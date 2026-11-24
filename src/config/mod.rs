use crate::Error;

use mlua::{BorrowedStr, Function, Lua, Table, Value};

use directories::BaseDirs;

use std::collections::BTreeMap;
use std::env::consts;
use std::path::{Path, PathBuf};

mod info;
pub use info::{HostInfo, ProjectDirs};
mod clipboard;
pub use clipboard::ClipboardConfig;
mod policy;
pub use policy::Policies;
#[cfg(feature = "plugin-http")]
pub(crate) use policy::WebviewPolicy;
mod ui_opts;
pub use ui_opts::{UiConfig, UiTable, UiTableId, UiValue};
pub(crate) mod program;
use program::ProgramSetting;
#[cfg(feature = "ssh")]
pub(crate) mod ssh;
mod ssh_input;

pub(crate) enum Setting {
    Text(String),
    Function(Function),
}

impl Setting {
    fn parse(v: Value, field: &str) -> Result<Option<Self>, Error> {
        match v {
            Value::String(s) => Ok(Some(Self::Text(lua_string(&s, field)?.to_string()))),
            Value::Function(f) => Ok(Some(Self::Function(f))),
            _ => Ok(None),
        }
    }

    pub(crate) fn resolve(&self, info: Option<&HostInfo>, lua: &Lua) -> Result<String, Error> {
        match self {
            Self::Text(s) => Ok(s.clone()),
            Self::Function(f) => {
                let info =
                    info.ok_or_else(|| Error::Message("Host information is unavailable".into()))?;
                match f.call::<Value>(info.table(lua)?)? {
                    Value::String(s) => Ok(lua_string(&s, "socket callback result")?.to_string()),
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

impl Host {
    fn parse(t: &Table) -> Result<Self, Error> {
        let ssh = table(t.get("ssh")?);
        if let Some(t) = &ssh {
            #[cfg(feature = "ssh")]
            ssh::validate(t)?;
            #[cfg(not(feature = "ssh"))]
            ssh_input::validate_strings(t)?;
        }

        Ok(Self {
            nvim: ProgramSetting::parse(t.get("nvim")?)?.unwrap_or_default(),
            socket: Setting::parse(t.get("socket")?, "socket")?,
            policies: Policies::parse(table(t.raw_get("policies")?))?,
            #[cfg(feature = "ssh")]
            ssh,
            #[cfg(not(feature = "ssh"))]
            remote: ssh.is_some(),
        })
    }

    pub(crate) fn is_remote(&self) -> bool {
        #[cfg(feature = "ssh")]
        {
            self.ssh.is_some()
        }
        #[cfg(not(feature = "ssh"))]
        {
            self.remote
        }
    }
}

/// A startup destination, including the independent default host.
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
    /// Loads configuration and parses the selected UI-specific dictionary.
    ///
    /// `ui_id` identifies the frontend implementation. It must be non-empty,
    /// must not begin with U+FEFF, and must not contain NUL. `parse_ui_opts` interprets
    /// and validates only its selected dictionary and returns the frontend's
    /// settings alongside the core configuration. Pass `|opts| Ok(opts)` to
    /// receive the owned Rust dictionary directly.
    ///
    /// An absent implicit configuration file selects defaults without a
    /// diagnostic. Unsupported dictionary keys and value types are treated as
    /// omitted. Other loading or validation failures select the complete
    /// default configuration.
    /// The parser is then called with an empty dictionary. The engine emits the
    /// original failure through its error interface. An invalid `ui_id`, or a
    /// parser that rejects the empty default dictionary, returns an error.
    pub fn load<T>(
        path: Option<&Path>,
        ui_id: &str,
        mut parse_ui_opts: impl FnMut(UiConfig) -> Result<T, Error>,
    ) -> Result<(Self, T), Error> {
        ui_opts::validate_id(ui_id)?;
        match Self::try_load(path, ui_id, &mut parse_ui_opts) {
            Ok(Some(config)) => Ok(config),
            Ok(None) => {
                let opts = parse_ui_opts(UiConfig::default())
                    .map_err(|e| Error::UiOptions(Box::new(e)))?;
                Ok((Self::default(), opts))
            }
            Err(e) => {
                let mut config = Self::default();
                config
                    .diagnostics
                    .push(format!("Load Nauvim configuration: {e}"));
                let opts = parse_ui_opts(UiConfig::default()).map_err(|defaults| {
                    Error::DefaultUiOptions {
                        configuration: Box::new(e),
                        defaults: Box::new(defaults),
                    }
                })?;
                Ok((config, opts))
            }
        }
    }

    fn try_load<T>(
        path: Option<&Path>,
        ui_id: &str,
        parse_ui_opts: impl FnOnce(UiConfig) -> Result<T, Error>,
    ) -> Result<Option<(Self, T)>, Error> {
        use std::io::ErrorKind;
        use std::{env, fs};

        let is_implicit = path.is_none();
        let default_path;
        let selected = if let Some(path) = path {
            path
        } else {
            let config_root = env::var_os("XDG_CONFIG_HOME")
                .filter(|p| !p.is_empty())
                .map(PathBuf::from)
                .or_else(|| BaseDirs::new().map(|d| d.config_dir().to_path_buf()))
                .ok_or_else(|| {
                    Error::Message("Cannot determine the configuration directory".into())
                })?;
            default_path = config_root.join("nauvim/init.lua");
            &default_path
        };
        let read = fs::canonicalize(selected).and_then(|file| {
            let src = fs::read_to_string(&file)?;
            Ok((file, src))
        });
        let (file, src) = match read {
            Ok(config) => config,
            Err(e) if is_implicit && e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let info = HostInfo::local_with_config(Some(file))?;
        Self::evaluate(&src, info, ui_id, parse_ui_opts).map(Some)
    }

    /// Evaluates source and parses its selected UI-specific dictionary.
    ///
    /// Validates every `ui_opts` entry before selecting the exact `use_ui_opts`
    /// key, or `ui_id` when the selector is nil. A missing entry produces an empty
    /// dictionary. Option dictionaries are never merged. The parser runs after
    /// all core validation and receives only the selected dictionary.
    ///
    /// Returns evaluation, common-validation, and frontend schema errors
    /// directly. Use [`Self::load()`] for file loading with complete-default
    /// fallback and startup diagnostics.
    pub fn evaluate<T>(
        src: &str,
        info: HostInfo,
        ui_id: &str,
        parse_ui_opts: impl FnOnce(UiConfig) -> Result<T, Error>,
    ) -> Result<(Self, T), Error> {
        ui_opts::validate_id(ui_id)?;
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
        let client = info.table(&lua)?;
        client.set("ui_id", ui_id)?;
        client.set("os", consts::OS)?;
        client.set("os_family", consts::FAMILY)?;
        client.set("arch", consts::ARCH)?;
        loaded.set("nauvim-client-info", client)?;
        let t: Table = lua.load(src).set_name(file.to_string_lossy()).eval()?;

        let mut diagnostics = Vec::new();
        let clipboard = ClipboardConfig::parse(t.get("clipboard")?, &mut diagnostics)?;
        let (default_host, default_spec) = match t.get("default_host")? {
            Value::String(s) => {
                let s = lua_string(&s, "default_host")?;
                if s.is_empty() {
                    return Err(Error::Message("default_host must be non-empty".into()));
                }
                (Some(s.to_string()), Host::default())
            }
            Value::Table(t) => (None, Host::parse(&t)?),
            _ => (None, Host::default()),
        };
        let mut hosts = BTreeMap::new();
        if let Some(t) = table(t.get("hosts")?) {
            for entry in t.pairs::<Value, Value>() {
                let (key, value) = entry?;
                let Value::String(alias) = key else {
                    continue;
                };
                let alias = lua_string(&alias, "Host alias")?;
                if let Value::Table(h) = value {
                    hosts.insert(alias.to_string(), Host::parse(&h)?);
                }
            }
        }
        let (_ssh_config, _known_hosts) = if let Some(ssh) = table(t.get("ssh")?) {
            (
                global_path(&ssh, "config")?,
                global_path(&ssh, "known_hosts")?,
            )
        } else {
            (None, None)
        };
        let opts = parse_ui_opts(ui_opts::select(&t, ui_id)?)
            .map_err(|e| Error::UiOptions(Box::new(e)))?;
        Ok((
            Self {
                lua,
                hosts,
                default_spec,
                #[cfg(feature = "ssh")]
                ssh_config: _ssh_config.unwrap_or_else(|| info.home_dir.join(".ssh/config")),
                #[cfg(feature = "ssh")]
                known_hosts: _known_hosts.unwrap_or_else(|| info.home_dir.join(".ssh/known_hosts")),
                client: Some(info),
                default_host,
                clipboard,
                diagnostics,
            },
            opts,
        ))
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
    pub(crate) fn host(&self, alias: Option<&str>) -> Option<&Host> {
        match alias {
            Some(alias) => self.hosts.get(alias),
            None => Some(&self.default_spec),
        }
    }

    /// Selects automatic startup or leaves selection to the welcome page.
    pub fn startup_host(&self) -> Option<HostSelection<'_>> {
        match self.default_host.as_deref() {
            Some(alias) => Some(HostSelection::Alias(alias)),
            None if self.hosts.is_empty() => Some(HostSelection::Default),
            None => None,
        }
    }
}

pub(crate) use info::{local_identity, stable_hash};

fn table(value: Value) -> Option<Table> {
    match value {
        Value::Table(t) => Some(t),
        _ => None,
    }
}

fn global_path(t: &Table, field: &str) -> Result<Option<PathBuf>, Error> {
    match t.get::<Value>(field)? {
        Value::String(s) => {
            let path = PathBuf::from(lua_string(&s, field)?.as_ref());
            if !path.is_absolute() {
                return Err(Error::Message(format!(
                    "ssh.{field} must be a non-empty absolute path"
                )));
            }
            Ok(Some(path))
        }
        _ => Ok(None),
    }
}

fn validate_string(value: &str, field: &str) -> Result<(), Error> {
    if crate::is_spec_string(value) {
        Ok(())
    } else {
        Err(Error::Message(format!(
            "{field} must not begin with U+FEFF or contain NUL"
        )))
    }
}

fn lua_string<'a>(value: &'a mlua::String, field: &str) -> Result<BorrowedStr<'a>, Error> {
    let text = value
        .to_str()
        .map_err(|_| Error::Message(format!("{field} must be valid UTF-8")))?;
    validate_string(&text, field)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "ssh")]
    #[test]
    fn defaults_both_ssh_paths_when_global_settings_are_absent() {
        for source in ["return {}", "return {ssh=nil}", "return {ssh={}}"] {
            let info = HostInfo {
                host_name: "test".into(),
                user: "test".into(),
                config_file: Some("/tmp/nauvim-test/init.lua".into()),
                home_dir: "/home/nauvim-test".into(),
                project_dirs: ProjectDirs {
                    cache_dir: "/cache".into(),
                    data_dir: "/data".into(),
                    runtime_dir: "/runtime".into(),
                },
            };
            let (config, ()) =
                Config::evaluate(source, info, "org.example.nauvim.test", |_| Ok(())).unwrap();

            assert_eq!(
                config.ssh_config,
                Path::new("/home/nauvim-test/.ssh/config"),
                "{source}"
            );
            assert_eq!(
                config.known_hosts,
                Path::new("/home/nauvim-test/.ssh/known_hosts"),
                "{source}"
            );
        }
    }

    #[test]
    fn validates_socket_callback_results() {
        let lua = Lua::new();
        let info = HostInfo {
            host_name: "test".into(),
            user: "test".into(),
            config_file: None,
            home_dir: "/home".into(),
            project_dirs: ProjectDirs {
                cache_dir: "/cache".into(),
                data_dir: "/data".into(),
                runtime_dir: "/runtime".into(),
            },
        };
        for (source, expected) in [
            (r"return function(_) return 'so\0cket' end", None),
            (r"return function(_) return '\239\187\191socket' end", None),
            (r"return function(_) return '\255' end", None),
            (
                r"return function(_) return 'so\239\187\191cket' end",
                Some("so\u{feff}cket"),
            ),
        ] {
            let setting = Setting::parse(lua.load(source).eval().unwrap(), "socket")
                .unwrap()
                .unwrap();
            assert_eq!(
                setting.resolve(Some(&info), &lua).ok().as_deref(),
                expected,
                "{source}"
            );
        }
    }
}
