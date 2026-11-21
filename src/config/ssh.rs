use crate::Error;

use super::{Config, HostInfo, lua_string};

use mlua::{Table, Value};

use openrussh_client::config::{Auth, Chain, Dest, Host, RequestTty, Target};
use openrussh_client::russh::Preferred;
use openrussh_client::russh::kex;
use openrussh_client::russh::keys::{Algorithm as KeyAlgorithm, key::ALL_KEY_TYPES};
use openrussh_client::ssh2_config::{DefaultAlgorithms, ParseRule, SshConfig};

use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

pub(crate) struct Preferences {
    pub preferred: Preferred,
    pub host_keys_explicit: bool,
}

pub(crate) struct Resolved {
    pub chain: Chain,
    pub preferences: Vec<Preferences>,
}

#[derive(Clone)]
enum Jump {
    Alias(String),
    OpenSsh(String),
    Inline(Table),
}

#[derive(Clone, Default)]
struct Settings {
    host_name: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identities: Option<Vec<PathBuf>>,
    certificate: Option<PathBuf>,
    agent: Option<Option<PathBuf>>,
    identities_only: Option<bool>,
    jumps: Option<Vec<Jump>>,
    kex: Option<Vec<String>>,
    keys: Option<Vec<String>>,
}

impl Settings {
    fn has_absent(&self) -> bool {
        self.host_name.is_none()
            || self.user.is_none()
            || self.port.is_none()
            || self.identities.is_none()
            || self.certificate.is_none()
            || self.agent.is_none()
            || self.identities_only.is_none()
            || self.jumps.is_none()
            || self.kex.is_none()
            || self.keys.is_none()
    }

    fn defaults_from(mut self, defaults: Self) -> Self {
        self.host_name = self.host_name.or(defaults.host_name);
        self.user = self.user.or(defaults.user);
        self.port = self.port.or(defaults.port);
        self.identities = self.identities.or(defaults.identities);
        self.certificate = self.certificate.or(defaults.certificate);
        self.agent = self.agent.or(defaults.agent);
        self.identities_only = self.identities_only.or(defaults.identities_only);
        self.jumps = self.jumps.or(defaults.jumps);
        self.kex = self.kex.or(defaults.kex);
        self.keys = self.keys.or(defaults.keys);
        self
    }
}

fn message(s: impl Into<String>) -> Error {
    Error::Message(s.into())
}

fn string(value: Value, field: &str) -> Result<String, Error> {
    match value {
        Value::String(s) => Ok(lua_string(&s, &format!("SSH {field}"))?.to_string()),
        _ => Err(message(format!("SSH {field} must be a string"))),
    }
}

fn path(value: Value, field: &str) -> Result<PathBuf, Error> {
    let value = string(value, field)?;
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(message(format!(
            "SSH {field} must be a non-empty absolute path"
        )));
    }
    Ok(path)
}

fn sequence(table: Table, field: &str) -> Result<Vec<Value>, Error> {
    let len = table.raw_len();
    let mut count = 0_usize;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if !matches!(key, Value::Integer(n) if n >= 1 && usize::try_from(n).is_ok_and(|n| n <= len))
        {
            return Err(message(format!(
                "SSH {field} must be a contiguous sequence"
            )));
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| message("SSH sequence is too large"))?;
    }
    if count != len {
        return Err(message(format!(
            "SSH {field} must be a contiguous sequence"
        )));
    }
    table
        .sequence_values()
        .collect::<Result<_, _>>()
        .map_err(Error::from)
}

fn native(table: &Table, _depth: usize) -> Result<Settings, Error> {
    let mut settings = Settings::default();
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        let field = string(key, "field name")?;
        match field.as_str() {
            "host_name" => settings.host_name = Some(string(value, &field)?),
            "user" => settings.user = Some(string(value, &field)?),
            "port" => {
                let port = match value {
                    Value::Integer(n) => u16::try_from(n).ok(),
                    Value::String(s) => {
                        let s = lua_string(&s, "SSH port")?;
                        if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
                            None
                        } else {
                            s.parse().ok()
                        }
                    }
                    _ => None,
                }
                .ok_or_else(|| {
                    message("SSH port must be an integer or decimal string from 0 to 65535")
                })?;
                settings.port = Some(port);
            }
            "identity_file" => {
                settings.identities = Some(vec![path(value, &field)?]);
            }
            "certificate_file" => settings.certificate = Some(path(value, &field)?),
            "identity_agent" => settings.agent = Some(Some(path(value, &field)?)),
            "identities_only" => {
                settings.identities_only = Some(match value {
                    Value::Boolean(value) => value,
                    Value::String(value) => {
                        let value = lua_string(&value, "SSH identities_only")?;
                        if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("yes") {
                            true
                        } else if value.eq_ignore_ascii_case("false")
                            || value.eq_ignore_ascii_case("no")
                        {
                            false
                        } else {
                            return Err(message(
                                "SSH identities_only must be a boolean or true/false/yes/no string",
                            ));
                        }
                    }
                    _ => {
                        return Err(message(
                            "SSH identities_only must be a boolean or true/false/yes/no string",
                        ));
                    }
                })
            }
            "proxy_jump" => settings.jumps = Some(jumps(value)?),
            "kex_algorithms" | "host_key_algorithms" => {
                let values = match value {
                    Value::String(s) => lua_string(&s, &field)?
                        .split(',')
                        .map(str::to_owned)
                        .collect(),
                    Value::Table(table) => sequence(table, &field)?
                        .into_iter()
                        .map(|value| string(value, &field))
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => {
                        return Err(message(format!(
                            "SSH {field} must be a comma-separated string or sequence"
                        )));
                    }
                };
                let keys = field == "host_key_algorithms";
                apply_algorithms(&values, keys)?;
                if keys {
                    settings.keys = Some(values);
                } else {
                    settings.kex = Some(values);
                }
            }
            _ => return Err(message(format!("Unknown SSH field: {field}"))),
        }
    }
    Ok(settings)
}

fn jumps(value: Value) -> Result<Vec<Jump>, Error> {
    fn jump(value: Value) -> Result<Jump, Error> {
        match value {
            Value::String(s) => Ok(Jump::Alias(lua_string(&s, "SSH proxy_jump")?.to_string())),
            Value::Table(table) => Ok(Jump::Inline(table)),
            _ => Err(message(
                "SSH proxy_jump must contain host aliases or inline SSH tables",
            )),
        }
    }
    match value {
        Value::String(_) => Ok(vec![jump(value)?]),
        Value::Table(table) => {
            let mut has_named_field = false;
            for pair in table.clone().pairs::<Value, Value>() {
                if matches!(pair?.0, Value::String(_)) {
                    has_named_field = true;
                    break;
                }
            }
            if has_named_field {
                Ok(vec![jump(Value::Table(table))?])
            } else {
                sequence(table, "proxy_jump")?
                    .into_iter()
                    .map(jump)
                    .collect()
            }
        }
        _ => Err(message(
            "SSH proxy_jump must be a host alias, inline table or sequence",
        )),
    }
}

pub(crate) fn validate(table: &Table, _depth: usize) -> Result<(), Error> {
    let mut pending = vec![table.clone()];
    let mut validated = HashSet::new();
    while let Some(table) = pending.pop() {
        // Validate each Lua object once. Repeated references remain permitted;
        // this does not reject or diagnose cyclic proxy-jump definitions.
        if !validated.insert(table.to_pointer()) {
            continue;
        }
        let settings = native(&table, 0)?;
        if let Some(jumps) = settings.jumps {
            for jump in jumps {
                if let Jump::Inline(table) = jump {
                    pending.push(table);
                }
            }
        }
    }
    Ok(())
}

fn default_names(keys: bool) -> Vec<String> {
    if keys {
        Preferred::DEFAULT
            .key
            .iter()
            .map(ToString::to_string)
            .collect()
    } else {
        Preferred::DEFAULT
            .kex
            .iter()
            .filter(|name| !kex_signal(name.as_ref()))
            .map(|name| name.as_ref().to_owned())
            .collect()
    }
}

fn key_algorithm(name: &str) -> Option<(KeyAlgorithm, bool)> {
    for algorithm in ALL_KEY_TYPES {
        if algorithm.as_str() == name {
            return Some((algorithm.clone(), false));
        }
        if algorithm.to_certificate_type() == name {
            return Some((algorithm.clone(), true));
        }
    }
    None
}

fn kex_signal(name: &str) -> bool {
    matches!(
        name,
        "ext-info-c"
            | "ext-info-s"
            | "kex-strict-c-v00@openssh.com"
            | "kex-strict-s-v00@openssh.com"
    )
}

fn kex_algorithm(name: &str) -> Option<kex::Name> {
    kex::Name::try_from(name).ok()
}

fn apply_algorithms(input: &[String], keys: bool) -> Result<Vec<String>, Error> {
    use std::iter::once;

    let Some((first, rest)) = input.split_first() else {
        return Err(message("SSH algorithm list must not be empty"));
    };
    let (mode, first) = match first.as_bytes().first() {
        Some(b'+' | b'-' | b'^') => {
            let Some(first_name) = first.get(1..) else {
                return Err(message("Invalid algorithm modifier"));
            };
            (first.as_bytes().first().copied(), first_name)
        }
        _ => (None, first.as_str()),
    };
    let supplied = once(first).chain(rest.iter().map(String::as_str));
    let mut names = Vec::new();
    for name in supplied {
        if name.is_empty()
            || name.contains(['*', '?', '!'])
            || name
                .bytes()
                .any(|byte| byte <= b' ' || byte >= 0x7f || byte == b',')
        {
            return Err(message(
                "SSH algorithm names must be non-empty and cannot contain wildcards",
            ));
        }
        if !keys && kex_signal(name) {
            return Err(message(
                "KEX extension signals cannot be configured as algorithms",
            ));
        }
        if mode != Some(b'-')
            && if keys {
                key_algorithm(name).is_none()
            } else {
                kex_algorithm(name).is_none()
            }
        {
            return Err(message(format!("Unknown SSH algorithm: {name}")));
        }
        names.push(name.to_owned());
    }
    let ret = match mode {
        Some(b'+') => {
            let mut defaults = default_names(keys);
            defaults.extend(names);
            defaults
        }
        Some(b'^') => {
            names.extend(default_names(keys));
            names
        }
        Some(b'-') => default_names(keys)
            .into_iter()
            .filter(|name| !names.contains(name))
            .collect(),
        _ => names,
    };
    if ret.is_empty() {
        return Err(message(
            "SSH algorithm list must not resolve to an empty list",
        ));
    }
    if keys {
        let mut plain = false;
        for name in &ret {
            let Some((_, certificate)) = key_algorithm(name) else {
                return Err(message(format!("Unknown SSH host-key algorithm: {name}")));
            };
            if certificate && plain {
                return Err(message(
                    "SSH certificate algorithms must precede plain host-key algorithms",
                ));
            }
            plain |= !certificate;
        }
    }
    Ok(ret)
}

struct Resolver<'a> {
    config: &'a Config,
    openssh: Option<SshConfig>,
    user: String,
}

impl Resolver<'_> {
    fn openssh(&mut self, alias: &str) -> Result<Settings, Error> {
        if self.openssh.is_none() {
            let mut defaults = DefaultAlgorithms::empty();
            defaults.kex_algorithms = default_names(false);
            defaults.host_key_algorithms = default_names(true);
            let file = File::open(&self.config.ssh_config).map_err(|e| {
                message(format!(
                    "Read SSH configuration {}: {e}",
                    self.config.ssh_config.display()
                ))
            })?;
            let mut parser = SshConfig::default().default_algorithms(defaults);
            if let Some(info) = &self.config.client {
                parser = parser.home_dir(&info.home_dir);
            }
            self.openssh = Some(
                parser
                    .parse(
                        &mut BufReader::new(file),
                        ParseRule::ALLOW_UNSUPPORTED_FIELDS,
                    )
                    .map_err(|e| {
                        message(format!(
                            "Parse SSH configuration {}: {e}",
                            self.config.ssh_config.display()
                        ))
                    })?,
            );
        }
        let Some(config) = &self.openssh else {
            return Err(message("SSH configuration is unavailable"));
        };
        let mut params = config.query(alias);
        let host_name = params.host_name.clone();
        let user = params.user.clone();
        let port = params.port;
        let identities_supplied = params.identity_file.is_some();
        let certificate_supplied = params.certificate_file.is_some();
        let agent_supplied = params.unsupported_fields.contains_key("identityagent");
        let agent_disabled = params
            .unsupported_fields
            .get("identityagent")
            .and_then(|values| values.first())
            .is_some_and(|value| value.eq_ignore_ascii_case("none"));
        let identities_only = params
            .unsupported_fields
            .get("identitiesonly")
            .and_then(|v| v.first())
            .map(|s| s.eq_ignore_ascii_case("yes") || s.eq_ignore_ascii_case("true"));
        let jumps = params.proxy_jump.clone().map(|values| {
            values
                .into_iter()
                .filter(|name| !name.eq_ignore_ascii_case("none"))
                .map(Jump::OpenSsh)
                .collect()
        });
        let kex = (!params.kex_algorithms.is_default())
            .then(|| params.kex_algorithms.algorithms().to_vec());
        let keys = (!params.host_key_algorithms.is_default())
            .then(|| params.host_key_algorithms.algorithms().to_vec());
        params.user = Some(self.user.clone());
        let parsed = Host::parse_openssh(
            &mut params,
            Target {
                host: alias,
                port: None,
                user: None,
            },
        )
        .map_err(|e| message(format!("Resolve SSH host {alias}: {e:?}")))?;
        Ok(Settings {
            host_name,
            user,
            port,
            identities: identities_supplied.then_some(parsed.auth.identities.unwrap_or_default()),
            certificate: if certificate_supplied {
                parsed.auth.cert
            } else {
                None
            },
            agent: if agent_disabled {
                Some(None)
            } else if agent_supplied {
                Some(parsed.auth.agent)
            } else {
                None
            },
            identities_only,
            jumps,
            kex,
            keys,
        })
    }

    fn alias(&mut self, alias: &str) -> Result<Settings, Error> {
        let native_table = self
            .config
            .hosts
            .get(alias)
            .and_then(|host| host.ssh.as_ref());
        let Some(table) = native_table else {
            return self.openssh(alias);
        };
        let settings = native(table, 0)?;
        let name = settings
            .host_name
            .clone()
            .unwrap_or_else(|| alias.to_owned());
        self.supplement(&name, settings)
    }

    fn supplement(&mut self, name: &str, mut settings: Settings) -> Result<Settings, Error> {
        if let Some(table) = self.config.hosts.get(name).and_then(|h| h.ssh.as_ref()) {
            settings = settings.defaults_from(native(table, 0)?);
        }
        if settings.has_absent() {
            settings = settings.defaults_from(self.openssh(name)?);
        }
        Ok(settings)
    }

    fn host(
        &self,
        alias: &str,
        settings: Settings,
    ) -> Result<(Host, Preferences, Vec<Jump>), Error> {
        let mut preferred = Preferred::default();
        if let Some(input) = settings.kex {
            let mut algorithms = apply_algorithms(&input, false)?
                .iter()
                .map(|name| {
                    kex_algorithm(name)
                        .ok_or_else(|| message(format!("Unknown SSH KEX algorithm: {name}")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            algorithms.extend(
                Preferred::DEFAULT
                    .kex
                    .iter()
                    .copied()
                    .filter(|name| kex_signal(name.as_ref())),
            );
            preferred.kex = Cow::Owned(algorithms);
        }
        let host_keys_explicit = settings.keys.is_some();
        if let Some(input) = settings.keys {
            let names = apply_algorithms(&input, true)?;
            let mut keys = Vec::new();
            let mut certificates = Vec::new();
            for name in names {
                let Some((algorithm, certificate)) = key_algorithm(&name) else {
                    return Err(message(format!("Unknown SSH host-key algorithm: {name}")));
                };
                if certificate {
                    certificates.push(algorithm);
                } else {
                    keys.push(algorithm);
                }
            }
            preferred.key = Cow::Owned(keys);
            preferred.host_key_certificates = Cow::Owned(certificates);
        }
        Ok((
            Host {
                dest: Dest {
                    name: settings.host_name.unwrap_or_else(|| alias.to_owned()),
                    user: settings.user.unwrap_or_else(|| self.user.clone()),
                    port: settings.port.unwrap_or(22),
                },
                auth: Auth {
                    identities: settings.identities,
                    cert: settings.certificate,
                    agent: settings.agent.flatten(),
                    identities_only: settings.identities_only.unwrap_or(false),
                    ..Auth::default()
                },
                tty: RequestTty::Never,
            },
            Preferences {
                preferred,
                host_keys_explicit,
            },
            settings.jumps.unwrap_or_default(),
        ))
    }
}

pub(crate) fn resolve(config: &Config, alias: &str) -> Result<Resolved, Error> {
    let user = match &config.client {
        Some(info) => info.user.clone(),
        None => HostInfo::local(None)?.user,
    };
    let mut resolver = Resolver {
        config,
        openssh: None,
        user,
    };
    let settings = resolver.alias(alias)?;
    let (target, preferred, mut jumps) = resolver.host(alias, settings)?;
    let mut preferences = vec![preferred];
    let mut bastions = Vec::new();
    while !jumps.is_empty() {
        let mut first_ancestors = Vec::new();
        for (idx, jump) in jumps.into_iter().enumerate().rev() {
            let (name, settings) = match jump {
                Jump::Alias(name) => {
                    let settings = resolver.alias(&name)?;
                    (name, settings)
                }
                Jump::OpenSsh(name) => {
                    let target = Target::parse(&name);
                    let mut settings = resolver.alias(target.host)?;
                    if let Some(user) = target.user {
                        settings.user = Some(user.to_owned());
                    }
                    if let Some(port) = target.port {
                        settings.port = Some(port);
                    }
                    (target.host.to_owned(), settings)
                }
                Jump::Inline(table) => {
                    let settings = native(&table, 0)?;
                    let name = settings
                        .host_name
                        .clone()
                        .ok_or_else(|| message("Inline SSH jump host requires host_name"))?;
                    let settings = resolver.supplement(&name, settings)?;
                    (name, settings)
                }
            };
            let (host, preferred, ancestors) = resolver.host(&name, settings)?;
            if idx == 0 {
                first_ancestors = ancestors;
            }
            preferences.push(preferred);
            bastions.push(host);
        }
        jumps = first_ancestors;
    }
    Ok(Resolved {
        chain: Chain {
            target,
            bastions: if bastions.is_empty() {
                None
            } else {
                Some(bastions.into_boxed_slice())
            },
        },
        preferences,
    })
}

#[cfg(test)]
mod tests {
    use crate::ProjectDirs;
    use crate::config::Host as ConfigHost;

    use super::*;

    use tempfile::tempdir;

    use std::fs;

    fn config(src: &str) -> Config {
        let mut config = Config::default();
        let hosts: Table = config.lua.load(src).eval().unwrap();
        config.hosts.clear();
        for entry in hosts.pairs::<String, Table>() {
            let (alias, ssh) = entry.unwrap();
            let host = ConfigHost {
                ssh: Some(ssh),
                ..ConfigHost::default()
            };
            config.hosts.insert(alias, host);
        }
        config.client = Some(HostInfo {
            host_name: "client".to_owned(),
            user: "client-user".to_owned(),
            config_file: None,
            home_dir: PathBuf::from("/home/client"),
            project_dirs: ProjectDirs {
                cache_dir: PathBuf::from("/cache"),
                data_dir: PathBuf::from("/data"),
                runtime_dir: PathBuf::from("/runtime"),
            },
        });
        config.ssh_config = PathBuf::from("/dev/null");
        config
    }

    #[test]
    fn merges_native_fields_once_and_preserves_supplied_false_and_empty_lists() {
        let config = config(
            r#"return {
            base = { host_name = "must-not-be-resolved", user = "base-user", port = 2222,
                identity_file = "/id/base", identity_agent = "/agent", identities_only = true,
                proxy_jump = {"unresolved-jump"} },
            requested = { host_name = "base", identity_file = "/id/override", identities_only = false, proxy_jump = {} },
        }"#,
        );
        let resolved = resolve(&config, "requested").unwrap();
        assert_eq!(
            resolved.chain.target.dest,
            Dest {
                name: "base".to_owned(),
                user: "base-user".to_owned(),
                port: 2222
            }
        );
        assert_eq!(
            resolved.chain.target.auth.identities,
            Some(vec![PathBuf::from("/id/override")])
        );
        assert_eq!(
            resolved.chain.target.auth.agent,
            Some(PathBuf::from("/agent"))
        );
        assert!(!resolved.chain.target.auth.identities_only);
        assert_eq!(resolved.chain.target.tty, RequestTty::Never);
        assert!(resolved.chain.bastions.is_none());
    }

    #[test]
    fn native_self_merge_retains_literal_name_and_applies_defaults_after_source() {
        let config = config(r#"return { ["literal*?!"] = { port = "0" } }"#);
        let resolved = resolve(&config, "literal*?!").unwrap();
        assert_eq!(resolved.chain.target.dest.name, "literal*?!");
        assert_eq!(resolved.chain.target.dest.user, "client-user");
        assert_eq!(resolved.chain.target.dest.port, 0);
        assert!(resolved.chain.target.auth.agent.is_none());
        assert_eq!(
            resolve(&config, "LITERAL*?!")
                .unwrap()
                .chain
                .target
                .dest
                .name,
            "LITERAL*?!"
        );
    }

    #[test]
    fn validates_native_types_paths_and_unused_jump_tables() {
        use mlua::Lua;

        let lua = Lua::new();
        for src in [
            "return {request_tty=false}",
            "return {port=65536}",
            "return {port=22.5}",
            "return {port='-1'}",
            "return {identity_agent='none'}",
            "return {identity_file='~/.ssh/id'}",
            "return {certificate_file=''}",
            "return {identities_only='maybe'}",
            "return {user=1}",
            "return {kex_algorithms={}}",
            "return {proxy_jump={{host_name='jump',unsupported=true}}}",
            "return {proxy_jump={{host_name='jump',proxy_jump={identity_agent='relative'}}}}",
        ] {
            let table = lua.load(src).eval::<Table>().unwrap();
            assert!(validate(&table, 0).is_err(), "{src}");
        }
        for src in [
            "return {identities_only='YES',port='65535'}",
            "return {identities_only='nO',identity_file='/missing/id'}",
            "return {proxy_jump={}}",
        ] {
            assert!(
                validate(&lua.load(src).eval::<Table>().unwrap(), 0).is_ok(),
                "{src}"
            );
        }
        let table = lua
            .load("local t = {}; t.proxy_jump=t; return t")
            .eval::<Table>()
            .unwrap();
        assert!(validate(&table, 0).is_ok());
    }

    #[test]
    fn resolves_openssh_and_merges_native_overrides_without_early_defaults() {
        let dir = tempdir().unwrap();
        let mut config = config(r#"return { native = {host_name='real',identities_only=false} }"#);
        config.ssh_config = dir.path().join("config");
        fs::write(&config.ssh_config, "Host real\n HostName server.test\n User ssh-user\n Port 2345\n IdentitiesOnly yes\n RequestTTY force\n IdentityAgent none\n IdentityFile /ssh/id\n").unwrap();
        let native = resolve(&config, "native").unwrap();
        assert_eq!(
            native.chain.target.dest,
            Dest {
                name: "real".to_owned(),
                user: "ssh-user".to_owned(),
                port: 2345
            }
        );
        assert_eq!(
            native.chain.target.auth.identities,
            Some(vec![PathBuf::from("/ssh/id")])
        );
        assert!(!native.chain.target.auth.identities_only);
        assert!(native.chain.target.auth.agent.is_none());
        assert_eq!(native.chain.target.tty, RequestTty::Never);
        let external = resolve(&config, "real").unwrap();
        assert_eq!(external.chain.target.dest.name, "server.test");
        assert!(external.chain.target.auth.identities_only);
        assert_eq!(external.chain.target.tty, RequestTty::Never);
        fs::write(&config.ssh_config, "Host real\n Port invalid\n").unwrap();
        assert!(resolve(&config, "native").is_err());
    }

    #[test]
    fn expands_only_first_jump_hosts_ancestors() {
        let config = config(
            r#"return {
            target = {proxy_jump={'first','second'}},
            first = {proxy_jump={'ancestor'}},
            second = {proxy_jump={'must-not-resolve'}},
            ancestor = {},
        }"#,
        );
        let resolved = resolve(&config, "target").unwrap();
        let bastions = resolved.chain.bastions.unwrap();
        assert_eq!(
            bastions
                .iter()
                .map(|host| host.dest.name.as_str())
                .collect::<Vec<_>>(),
            ["second", "first", "ancestor"]
        );
        assert!(bastions.iter().all(|host| host.tty == RequestTty::Never));
    }

    #[test]
    fn supplements_native_self_merge_and_inline_hosts_from_required_source() {
        let dir = tempdir().unwrap();
        let mut config = config(
            r#"return {
            target={proxy_jump={host_name='jump',user='inline-user'}},
            jump={user='native-user',port=2022},
        }"#,
        );
        config.ssh_config = dir.path().join("config");
        fs::write(&config.ssh_config, "Host target\n Port 2222\nHost jump\n IdentityFile /ssh/jump-id\n User openssh-user\n Port 23\n").unwrap();
        let resolved = resolve(&config, "target").unwrap();
        assert_eq!(resolved.chain.target.dest.port, 2222);
        let bastions = resolved.chain.bastions.unwrap();
        let jump = bastions.first().unwrap();
        assert_eq!(jump.dest.name, "jump");
        assert_eq!(jump.dest.user, "inline-user");
        assert_eq!(jump.dest.port, 2022);
        assert_eq!(
            jump.auth.identities,
            Some(vec![PathBuf::from("/ssh/jump-id")])
        );
        fs::remove_file(&config.ssh_config).unwrap();
        assert!(
            resolve(&config, "target")
                .err()
                .unwrap()
                .to_string()
                .contains("Read SSH configuration")
        );
    }

    #[test]
    fn keeps_kex_extension_signals_out_of_user_algorithm_lists() {
        for name in [
            "ext-info-c",
            "ext-info-s",
            "kex-strict-c-v00@openssh.com",
            "kex-strict-s-v00@openssh.com",
        ] {
            for modifier in ["", "+", "-", "^"] {
                assert!(apply_algorithms(&[format!("{modifier}{name}")], false).is_err());
            }
        }
        let config = config("return {target={kex_algorithms='curve25519-sha256'}}");
        let resolved = resolve(&config, "target").unwrap();
        let preferred = &resolved.preferences.first().unwrap().preferred;
        assert_eq!(preferred.kex.first().unwrap().as_ref(), "curve25519-sha256");
        assert!(
            preferred
                .kex
                .iter()
                .any(|name| name.as_ref() == "ext-info-c")
        );
    }

    #[test]
    fn applies_algorithm_modifiers_and_enforces_certificate_order() {
        let defaults = default_names(true);
        let removed = apply_algorithms(&["-ssh-rsa,unknown".to_owned()], true);
        // Arrays represent distinct algorithm names; commas belong in the
        // string-valued input parser rather than inside an array element.
        assert!(removed.is_err());
        assert_eq!(
            apply_algorithms(&["-not-in-defaults".to_owned()], true).unwrap(),
            defaults
        );
        let prepended =
            apply_algorithms(&["^ssh-ed25519-cert-v01@openssh.com".to_owned()], true).unwrap();
        assert_eq!(
            prepended.first().unwrap(),
            "ssh-ed25519-cert-v01@openssh.com"
        );
        assert_eq!(prepended.get(1..).unwrap(), defaults);
        for input in [
            vec!["+ssh-ed25519-cert-v01@openssh.com".to_owned()],
            vec![
                "ssh-ed25519".to_owned(),
                "ssh-ed25519-cert-v01@openssh.com".to_owned(),
            ],
            vec!["unknown".to_owned()],
            vec!["-ssh-*".to_owned()],
        ] {
            assert!(apply_algorithms(&input, true).is_err(), "{input:?}");
        }
        let config = config(
            r#"return {target={host_key_algorithms={'ssh-ed25519-cert-v01@openssh.com','ssh-ed25519'}}}"#,
        );
        let resolved = resolve(&config, "target").unwrap();
        let preferred = resolved.preferences.first().unwrap();
        assert!(preferred.host_keys_explicit);
        assert_eq!(
            preferred.preferred.host_key_certificates.as_ref(),
            [KeyAlgorithm::Ed25519]
        );
        assert_eq!(preferred.preferred.key.as_ref(), [KeyAlgorithm::Ed25519]);
    }
}
