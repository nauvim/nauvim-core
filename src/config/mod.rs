use crate::Error;

use mlua::{Function, Lua, Table, Value};

use directories::BaseDirs;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod info;
pub use info::{HostInfo, ProjectDirs};
mod clipboard;
pub use clipboard::ClipboardConfig;
pub(crate) mod program;
use program::ProgramSetting;
#[cfg(feature = "ssh")]
pub(crate) mod ssh;

/// A configured fallback font, in priority order.
#[derive(Debug, Clone)]
pub struct Font {
    pub family: String,
    pub size: i64,
}

/// Client resource permissions for one attachment.
#[derive(Debug, Clone, Copy)]
pub struct Policies {
    pub local_forward: bool,
    pub filesystem: bool,
    pub http: bool,
    pub clipboard: bool,
}

impl Default for Policies {
    fn default() -> Self {
        Self {
            local_forward: true,
            filesystem: true,
            http: true,
            clipboard: true,
        }
    }
}

pub(crate) enum Setting {
    Text(String),
    Function(Function),
}

impl Setting {
    fn parse(v: Value, field: &str) -> Result<Option<Self>, Error> {
        match v {
            Value::Nil => Ok(None),
            Value::String(s) => Ok(Some(Self::Text(s.to_str()?.to_string()))),
            Value::Function(f) => Ok(Some(Self::Function(f))),
            _ => Err(Error::Message(format!(
                "{field} must be a string or function"
            ))),
        }
    }

    pub(crate) fn resolve(&self, info: Option<&HostInfo>, lua: &Lua) -> Result<String, Error> {
        match self {
            Self::Text(s) => Ok(s.clone()),
            Self::Function(f) => {
                let info =
                    info.ok_or_else(|| Error::Message("Host information is unavailable".into()))?;
                match f.call::<Value>(info.table(lua)?)? {
                    Value::String(s) => Ok(s.to_str()?.to_string()),
                    _ => Err(Error::Message(
                        "Host path callback must return a string".into(),
                    )),
                }
            }
        }
    }

    pub(crate) fn needs_info(&self) -> bool {
        matches!(self, Self::Function(_))
    }
}

#[derive(Default)]
pub(crate) struct Host {
    pub nvim: ProgramSetting,
    pub socket: Option<Setting>,
    pub policies: Policies,
    #[cfg(feature = "ssh")]
    pub ssh: Option<Table>,
    #[cfg(not(feature = "ssh"))]
    pub remote: bool,
}

/// A startup destination, including the independent default local host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSelection<'a> {
    Default,
    Alias(&'a str),
}

/// Evaluated client-side Lua configuration.
///
/// Function-valued host paths stay in this Lua environment and are evaluated
/// only when the selected host's information is available. Unused native SSH
/// tables are validated while loading the configuration.
pub struct Config {
    pub(crate) lua: Lua,
    pub(crate) hosts: BTreeMap<String, Host>,
    pub(crate) default_spec: Host,
    pub(crate) client: Option<HostInfo>,
    default_host: Option<String>,
    fonts: Vec<Font>,
    clipboard: ClipboardConfig,
    diagnostics: Vec<String>,
    #[cfg(feature = "ssh")]
    pub(crate) ssh_config: PathBuf,
    #[cfg(feature = "ssh")]
    pub(crate) known_hosts: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        #[cfg(feature = "ssh")]
        let home = BaseDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .unwrap_or_default();
        Self {
            lua: Lua::new(),
            hosts: BTreeMap::new(),
            default_spec: Host::default(),
            client: None,
            default_host: None,
            fonts: vec![Font {
                family: "monospace".into(),
                size: 12,
            }],
            clipboard: ClipboardConfig::default(),
            diagnostics: Vec::new(),
            #[cfg(feature = "ssh")]
            ssh_config: home.join(".ssh/config"),
            #[cfg(feature = "ssh")]
            known_hosts: home.join(".ssh/known_hosts"),
        }
    }
}

impl Config {
    /// Loads a Lua configuration and retains failures as startup diagnostics.
    ///
    /// Reading, execution, validation, or client-information failures select
    /// the complete default configuration. The engine emits the diagnostic
    /// through its error interface.
    pub fn load(path: Option<&Path>) -> Result<Self, Error> {
        match Self::try_load(path) {
            Ok(config) => Ok(config),
            Err(e) => {
                let mut config = Self::default();
                config
                    .diagnostics
                    .push(format!("Load Nauvim configuration: {e}"));
                Ok(config)
            }
        }
    }

    fn try_load(path: Option<&Path>) -> Result<Self, Error> {
        use std::{env, fs};

        let dirs = BaseDirs::new()
            .ok_or_else(|| Error::Message("Cannot determine the configuration directory".into()))?;
        let config_root = env::var_os("XDG_CONFIG_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs.config_dir().to_path_buf());
        let default_path = config_root.join("nauvim/init.lua");
        let selected = path.unwrap_or(&default_path);
        let info = HostInfo::local(Some(selected))?;
        let config_file = info
            .config_file
            .as_deref()
            .ok_or_else(|| Error::Message("Missing configuration path".into()))?;
        let src = fs::read_to_string(config_file)?;
        Self::evaluate(&src, info)
    }

    /// Evaluates configuration text with the supplied complete client information.
    pub fn evaluate(src: &str, info: HostInfo) -> Result<Self, Error> {
        let info = info.resolved()?;
        let lua = Lua::new();
        let file = info
            .config_file
            .as_deref()
            .ok_or_else(|| Error::Message("Client HostInfo requires config_file".into()))?;
        let root = file
            .parent()
            .ok_or_else(|| Error::Message("Configuration path has no parent".into()))?;
        let package: Table = lua.globals().get("package")?;
        let old: String = package.get("path")?;
        package.set(
            "path",
            format!(
                "{}/lua/?.lua;{}/lua/?/init.lua;{old}",
                root.display(),
                root.display()
            ),
        )?;
        let loaded: Table = package.get("loaded")?;
        loaded.set("nauvim-client-info", info.table(&lua)?)?;
        let t: Table = lua.load(src).set_name(file.to_string_lossy()).eval()?;
        fields(
            &t,
            &["clipboard", "default_host", "hosts", "ssh", "fonts"],
            "configuration",
        )?;
        let mut diagnostics = Vec::new();
        let clipboard = ClipboardConfig::parse(t.get("clipboard")?, &mut diagnostics)?;
        let mut hosts = BTreeMap::new();
        let default_host = match t.get::<Value>("default_host")? {
            Value::Nil => None,
            Value::String(s) => {
                let s = s.to_str()?.to_string();
                if s.is_empty() {
                    return Err(Error::Message("default_host must be non-empty".into()));
                }
                Some(s)
            }
            _ => {
                return Err(Error::Message(
                    "default_host must be a string or nil".into(),
                ));
            }
        };
        if let Some(t) = t.get::<Option<Table>>("hosts")? {
            for entry in t.pairs::<Value, Table>() {
                let (alias, h) = entry?;
                let Value::String(alias) = alias else {
                    return Err(Error::Message("Host aliases must be strings".into()));
                };
                let alias = alias.to_str()?.to_string();
                fields(&h, &["nvim", "socket", "ssh", "policies"], "host")?;
                let remote = h.get::<Option<Table>>("ssh")?;
                #[cfg(feature = "ssh")]
                if let Some(s) = &remote {
                    ssh::validate(s, 0)?;
                }
                #[cfg(not(feature = "ssh"))]
                if remote.is_some() {
                    validate_disabled_ssh(remote.as_ref())?;
                }
                let policies = match h.get::<Option<Table>>("policies")? {
                    Some(p) => {
                        reject_kebab(&p)?;
                        Policies {
                            local_forward: p.get::<Option<bool>>("local_forward")?.unwrap_or(true),
                            filesystem: p.get::<Option<bool>>("filesystem")?.unwrap_or(true),
                            http: p.get::<Option<bool>>("http")?.unwrap_or(true),
                            clipboard: p.get::<Option<bool>>("clipboard")?.unwrap_or(true),
                        }
                    }
                    None => Policies::default(),
                };
                hosts.insert(
                    alias,
                    Host {
                        nvim: ProgramSetting::parse(h.get("nvim")?)?.unwrap_or_default(),
                        socket: Setting::parse(h.get("socket")?, "socket")?,
                        policies,
                        #[cfg(feature = "ssh")]
                        ssh: remote,
                        #[cfg(not(feature = "ssh"))]
                        remote: remote.is_some(),
                    },
                );
            }
        }
        let fonts = parse_fonts(t.get("fonts")?)?;
        let (_ssh_config, _known_hosts) = {
            let ssh: Option<Table> = t.get("ssh")?;
            if let Some(s) = &ssh {
                fields(s, &["config", "known_hosts"], "ssh")?;
            }
            (
                ssh.as_ref()
                    .map(|s| global_path(s, "config"))
                    .transpose()?
                    .flatten()
                    .unwrap_or_else(|| info.home_dir.join(".ssh/config")),
                ssh.as_ref()
                    .map(|s| global_path(s, "known_hosts"))
                    .transpose()?
                    .flatten()
                    .unwrap_or_else(|| info.home_dir.join(".ssh/known_hosts")),
            )
        };
        Ok(Self {
            lua,
            hosts,
            default_spec: Host::default(),
            client: Some(info),
            default_host,
            fonts,
            clipboard,
            diagnostics,
            #[cfg(feature = "ssh")]
            ssh_config: _ssh_config,
            #[cfg(feature = "ssh")]
            known_hosts: _known_hosts,
        })
    }

    /// Borrows the client information acquired before configuration execution.
    pub fn client_info(&self) -> Option<&HostInfo> {
        self.client.as_ref()
    }

    /// Borrows the client-side clipboard backend and operation timeout settings.
    pub fn clipboard(&self) -> &ClipboardConfig {
        &self.clipboard
    }

    /// Returns configuration diagnostics that do not prevent editor startup.
    ///
    /// Invalid clipboard timeout values use the default timeout and leave a
    /// diagnostic here. [`crate::Engine`] emits these as client error events.
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Returns configured host aliases in lexical order.
    pub fn hosts(&self) -> impl Iterator<Item = &str> {
        self.hosts.keys().map(String::as_str)
    }
    /// Returns the top-level automatic startup alias, if configured.
    pub fn default_host(&self) -> Option<&str> {
        self.default_host.as_deref()
    }
    /// Selects automatic startup or leaves selection to the welcome page.
    pub fn startup_host(&self) -> Option<HostSelection<'_>> {
        match self.default_host.as_deref() {
            Some(alias) => Some(HostSelection::Alias(alias)),
            None if self.hosts.is_empty() => Some(HostSelection::Default),
            None => None,
        }
    }
    /// Returns the ordered font configuration.
    pub fn fonts(&self) -> &[Font] {
        &self.fonts
    }
}

fn reject_kebab(t: &Table) -> Result<(), Error> {
    for pair in t.clone().pairs::<String, Value>() {
        if pair?.0.contains('-') {
            return Err(Error::Message(
                "Configuration field names must use snake_case".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(not(feature = "ssh"))]
fn validate_disabled_ssh(t: Option<&Table>) -> Result<(), Error> {
    if let Some(t) = t {
        reject_kebab(t)?;
    }
    Ok(())
}

pub(crate) use info::stable_hash as info_hash;

fn fields(t: &Table, accepted: &[&str], context: &str) -> Result<(), Error> {
    for pair in t.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::String(key) = key else {
            return Err(Error::Message(format!(
                "{context} field names must be strings"
            )));
        };
        let key = key.to_str()?;
        if !accepted.contains(&key.as_ref()) {
            return Err(Error::Message(format!("Unknown {context} field: {key}")));
        }
    }
    Ok(())
}

fn parse_fonts(value: Value) -> Result<Vec<Font>, Error> {
    fn font(value: Value) -> Result<Font, Error> {
        match value {
            Value::String(family) => Ok(Font {
                family: family.to_str()?.to_string(),
                size: 12,
            }),
            Value::Table(t) => {
                fields(&t, &["family", "size"], "font")?;
                let Value::String(family) = t.get::<Value>("family")? else {
                    return Err(Error::Message("font.family must be a string".into()));
                };
                let size = match t.get::<Value>("size")? {
                    Value::Nil => 12,
                    Value::Integer(n) => n,
                    _ => return Err(Error::Message("font.size must have integer type".into())),
                };
                Ok(Font {
                    family: family.to_str()?.to_string(),
                    size,
                })
            }
            _ => Err(Error::Message(
                "fonts must be a string, font table, or font sequence".into(),
            )),
        }
    }
    match value {
        Value::Nil => Ok(vec![Font {
            family: "monospace".into(),
            size: 12,
        }]),
        Value::String(_) => Ok(vec![font(value)?]),
        Value::Table(t) if t.contains_key("family")? || t.contains_key("size")? => {
            Ok(vec![font(Value::Table(t))?])
        }
        Value::Table(t) => {
            let len = t.raw_len();
            for pair in t.clone().pairs::<Value, Value>() {
                let (key, _) = pair?;
                if !matches!(key, Value::Integer(n) if n >= 1 && usize::try_from(n).is_ok_and(|n| n <= len))
                {
                    return Err(Error::Message(
                        "fonts must be a contiguous font sequence".into(),
                    ));
                }
            }
            let mut ret = Vec::with_capacity(len);
            for idx in 1..=len {
                let value = t.raw_get::<Value>(idx)?;
                if !matches!(value, Value::Table(_)) {
                    return Err(Error::Message(
                        "A font sequence must contain font tables".into(),
                    ));
                }
                ret.push(font(value)?);
            }
            Ok(ret)
        }
        _ => Err(Error::Message(
            "fonts must be a string, font table, or font sequence".into(),
        )),
    }
}

fn global_path(t: &Table, field: &str) -> Result<Option<PathBuf>, Error> {
    match t.get::<Value>(field)? {
        Value::Nil => Ok(None),
        Value::String(s) => {
            let path = PathBuf::from(s.to_str()?.as_ref());
            if !path.is_absolute() {
                return Err(Error::Message(format!(
                    "ssh.{field} must be a non-empty absolute path"
                )));
            }
            Ok(Some(path))
        }
        _ => Err(Error::Message(format!(
            "ssh.{field} must be an absolute path string"
        ))),
    }
}
