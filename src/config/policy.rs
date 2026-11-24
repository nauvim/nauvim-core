use crate::Error;

use super::{lua_string, table};

use mlua::{Table, Value};
use url::Url;

use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;

/// Client resource permissions for one attachment. The default grants no permissions.
#[derive(Debug, Clone, Default)]
pub struct Policies {
    pub filesystem_copy: bool,
    pub http_serve_file: bool,
    pub clipboard_copy: bool,
    tcp: AllowList<TcpEndpoint>,
    socket: AllowList<SocketEndpoint>,
    #[cfg(feature = "plugin-http")]
    pub(crate) webview: WebviewPolicy,
}

impl Policies {
    pub(crate) fn parse(t: Option<Table>) -> Result<Self, Error> {
        let mut policies = Self::default();
        let Some(t) = t else {
            return Ok(policies);
        };
        let Some(plugin) = table(t.raw_get("plugin")?) else {
            return Ok(policies);
        };

        if let Some(t) = table(plugin.raw_get("filesystem")?) {
            policies.filesystem_copy = flag(&t, "copy")?;
        }
        if let Some(t) = table(plugin.raw_get("clipboard")?) {
            policies.clipboard_copy = flag(&t, "copy")?;
        }
        if let Some(t) = table(plugin.raw_get("http")?) {
            policies.http_serve_file = flag(&t, "serve_file")?;
            if let Some(t) = table(t.raw_get("open_webview")?) {
                let _origins = AllowList::parse(
                    t.raw_get("origins")?,
                    "plugin.http.open_webview.origins",
                    |value| parse_origin(value).map(Some),
                )?;
                let _served_files = flag(&t, "served_files")?;
                #[cfg(feature = "plugin-http")]
                {
                    policies.webview = WebviewPolicy {
                        origins: _origins,
                        served_files: _served_files,
                    };
                }
            }
        }
        if let Some(t) = table(plugin.raw_get("local_forward")?) {
            policies.tcp = AllowList::parse(
                t.raw_get("tcp")?,
                "plugin.local_forward.tcp",
                TcpEndpoint::parse,
            )?;
            policies.socket = AllowList::parse(
                t.raw_get("socket")?,
                "plugin.local_forward.socket",
                SocketEndpoint::parse,
            )?;
        }

        Ok(policies)
    }

    /// Reports whether the policy permits the supplied TCP forward endpoint.
    pub fn allows_tcp(&self, client_port: u16, host_addr: &str, host_port: u16) -> bool {
        host_port != 0
            && crate::is_spec_string(host_addr)
            && self.tcp.allows(|entry| {
                entry.client_port == client_port
                    && entry.host_addr == host_addr
                    && entry.host_port == host_port
            })
    }

    /// Reports whether the policy permits the socket forward after client path confinement.
    ///
    /// `client_relative` is the resolved client path relative to the process temporary directory.
    pub fn allows_socket(&self, client_relative: &Path, host_socket_path: &str) -> bool {
        crate::is_spec_string(host_socket_path)
            && self.socket.allows(|entry| {
                client_relative.as_os_str() == entry.client_socket_path.as_str()
                    && host_socket_path == entry.host_socket_path
            })
    }
}

#[cfg(feature = "plugin-http")]
#[derive(Debug, Clone, Default)]
pub(crate) struct WebviewPolicy {
    origins: AllowList<Url>,
    served_files: bool,
}

#[cfg(feature = "plugin-http")]
impl WebviewPolicy {
    pub(crate) fn allows_origin(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && self.origins.allows(|entry| {
                entry.scheme() == url.scheme()
                    && entry.host_str() == url.host_str()
                    && entry.port_or_known_default() == url.port_or_known_default()
            })
    }

    pub(crate) fn permits_served_files(&self) -> bool {
        self.served_files
    }
}

#[derive(Debug, Default)]
enum AllowList<T> {
    #[default]
    None,
    All,
    Only(Arc<[T]>),
}

impl<T> Clone for AllowList<T> {
    fn clone(&self) -> Self {
        match self {
            Self::None => Self::None,
            Self::All => Self::All,
            Self::Only(entries) => Self::Only(entries.clone()),
        }
    }
}

impl<T> AllowList<T> {
    fn parse(
        value: Value,
        context: &str,
        parse_entry: impl Fn(Value) -> Result<Option<T>, Error>,
    ) -> Result<Self, Error> {
        let table = match value {
            Value::String(s) => {
                if lua_string(&s, context)?.as_ref() == "all" {
                    return Ok(Self::All);
                }
                return Err(Error::Message(format!("{context} must be a list or 'all'")));
            }
            Value::Table(t) => t,
            _ => return Ok(Self::None),
        };

        let mut entries = Vec::new();
        let mut last = 0;
        let mut count = 0_usize;
        for pair in table.pairs::<Value, Value>() {
            let (idx, value) = pair?;
            let idx = match idx {
                Value::Integer(idx) => usize::try_from(idx).ok().filter(|&idx| idx != 0),
                _ => None,
            };
            let idx = idx.ok_or_else(|| invalid_list(context))?;
            last = last.max(idx);
            count = count.checked_add(1).ok_or_else(|| invalid_list(context))?;
            if let Some(entry) = parse_entry(value)? {
                entries.push(entry);
            }
        }
        if last != count {
            return Err(invalid_list(context));
        }

        Ok(if entries.is_empty() {
            Self::None
        } else {
            Self::Only(entries.into())
        })
    }

    fn allows(&self, matches_entry: impl FnMut(&T) -> bool) -> bool {
        match self {
            Self::None => false,
            Self::All => true,
            Self::Only(entries) => entries.iter().any(matches_entry),
        }
    }
}

fn invalid_list(context: &str) -> Error {
    Error::Message(format!(
        "{context} must be a list indexed consecutively from 1"
    ))
}

#[derive(Debug)]
struct TcpEndpoint {
    client_port: u16,
    host_addr: String,
    host_port: u16,
}

impl TcpEndpoint {
    fn parse(value: Value) -> Result<Option<Self>, Error> {
        let Value::Table(t) = value else {
            return Err(Error::Message("TCP policy entry must be a table".into()));
        };
        let client_port = t.raw_get("client_port")?;
        let host_addr = t.raw_get("host_addr")?;
        let host_port = t.raw_get("host_port")?;
        if client_port == Value::Nil {
            return Err(invalid_port("client_port", 0..=u16::MAX));
        }
        if host_addr == Value::Nil {
            return Err(Error::Message("TCP policy entry requires host_addr".into()));
        }
        if host_port == Value::Nil {
            return Err(invalid_port("host_port", 1..=u16::MAX));
        }
        let (Value::Integer(client_port), Value::String(host_addr), Value::Integer(host_port)) =
            (client_port, host_addr, host_port)
        else {
            return Ok(None);
        };

        Ok(Some(Self {
            client_port: port(client_port, "client_port", 0..=u16::MAX)?,
            host_addr: lua_string(&host_addr, "host_addr")?.to_string(),
            host_port: port(host_port, "host_port", 1..=u16::MAX)?,
        }))
    }
}

#[derive(Debug)]
struct SocketEndpoint {
    client_socket_path: String,
    host_socket_path: String,
}

impl SocketEndpoint {
    fn parse(value: Value) -> Result<Option<Self>, Error> {
        let Value::Table(t) = value else {
            return Err(Error::Message("Socket policy entry must be a table".into()));
        };
        let client = t.raw_get("client_socket_path")?;
        let host = t.raw_get("host_socket_path")?;
        if client == Value::Nil || host == Value::Nil {
            return Err(Error::Message(
                "Socket policy entry requires client_socket_path and host_socket_path".into(),
            ));
        }
        let (Value::String(client), Value::String(host)) = (client, host) else {
            return Ok(None);
        };
        let client_socket_path = lua_string(&client, "client_socket_path")?.to_string();
        let host_socket_path = lua_string(&host, "host_socket_path")?.to_string();
        if client_socket_path.is_empty() || client_socket_path.starts_with('/') {
            return Err(Error::Message("Socket policy client_socket_path must be relative to the process temporary directory".into()));
        }
        if !host_socket_path.starts_with('/') {
            return Err(Error::Message(
                "Socket policy host_socket_path must be an absolute POSIX path".into(),
            ));
        }

        Ok(Some(Self {
            client_socket_path,
            host_socket_path,
        }))
    }
}

fn flag(t: &Table, key: &str) -> Result<bool, Error> {
    Ok(matches!(t.raw_get(key)?, Value::Boolean(true)))
}

fn port(value: i64, key: &str, range: RangeInclusive<u16>) -> Result<u16, Error> {
    u16::try_from(value)
        .ok()
        .filter(|n| range.contains(n))
        .ok_or_else(|| invalid_port(key, range))
}

fn invalid_port(key: &str, range: RangeInclusive<u16>) -> Error {
    Error::Message(format!(
        "TCP policy {key} must be an integer from {} to {}",
        range.start(),
        range.end()
    ))
}

fn parse_origin(value: Value) -> Result<Url, Error> {
    const CONTEXT: &str = "Webview origin policy entry";

    fn invalid_origin() -> Error {
        Error::Message(format!(
            "{CONTEXT} must be an HTTP(S) origin without a path, query, fragment, or user information"
        ))
    }

    let Value::String(s) = value else {
        return Err(invalid_origin());
    };
    let origin = lua_string(&s, CONTEXT)?;
    let (_, authority) = origin.split_once("://").ok_or_else(invalid_origin)?;
    if authority.is_empty()
        || authority.ends_with(':')
        || authority.contains(['/', '?', '#', '@', '\\'])
        || origin.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(invalid_origin());
    }
    let url = Url::parse(&origin).map_err(|_| invalid_origin())?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return Err(invalid_origin());
    }

    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    use mlua::Lua;

    fn policies(source: &str) -> Policies {
        let lua = Lua::new();
        Policies::parse(lua.load(source).eval().unwrap()).unwrap()
    }

    #[test]
    fn denies_permissions_by_default() {
        for source in [
            "return nil",
            "return {}",
            "return {plugin=nil}",
            "return {plugin={filesystem={},http={},local_forward={},clipboard={}}}",
            "return {plugin={filesystem={copy=false}, http={serve_file=false},
                local_forward={tcp={}, socket={}}, clipboard={copy=false}}}",
        ] {
            let policy = policies(source);
            assert!(!policy.filesystem_copy, "{source}");
            assert!(!policy.http_serve_file, "{source}");
            assert!(!policy.clipboard_copy, "{source}");
            assert!(!policy.allows_tcp(0, "example.test", 80), "{source}");
            assert!(
                !policy.allows_socket(Path::new("foo"), "/host/foo"),
                "{source}"
            );
            #[cfg(feature = "plugin-http")]
            {
                let url = Url::parse("http://example.test").unwrap();
                assert!(!policy.webview.allows_origin(&url), "{source}");
                assert!(!policy.webview.permits_served_files(), "{source}");
            }
        }
    }

    #[test]
    fn matches_exact_forward_endpoints() {
        let policy = policies(
            r#"return { plugin = {
                filesystem = { copy = true },
                http = { serve_file = true },
                clipboard = { copy = true },
                local_forward = {
                    tcp = {
                        { client_port = 0, host_addr = 'example.test', host_port = 8443 },
                        { client_port = 65535, host_addr = '127.0.0.1', host_port = 1 },
                    },
                    socket = {
                        { client_socket_path = 'nested/a.sock', host_socket_path = '/host/a.sock' },
                        { client_socket_path = 'b.sock', host_socket_path = '/host/./b.sock' },
                    },
                },
            } }"#,
        );
        assert!(policy.filesystem_copy);
        assert!(policy.http_serve_file);
        assert!(policy.clipboard_copy);
        for (client_port, addr, host_port, allowed) in [
            (0, "example.test", 8443, true),
            (65535, "127.0.0.1", 1, true),
            (1, "example.test", 8443, false),
            (0, "Example.test", 8443, false),
            (0, "example.test", 1, false),
        ] {
            assert_eq!(policy.allows_tcp(client_port, addr, host_port), allowed);
        }
        for (client, host, allowed) in [
            ("nested/a.sock", "/host/a.sock", true),
            ("b.sock", "/host/./b.sock", true),
            ("a.sock", "/host/a.sock", false),
            ("nested/a.sock", "/host/b.sock", false),
            ("b.sock", "/host/b.sock", false),
        ] {
            assert_eq!(policy.allows_socket(Path::new(client), host), allowed);
        }
    }

    #[test]
    fn omits_unsupported_policy_fields_without_dropping_other_permissions() {
        let policy = policies(
            r#"return {plugin={
            clipboard={copy=0},
            filesystem={copy=true,future=true},
            http=false,
            local_forward={
                tcp={
                    {client_port='0',host_addr='ignored',host_port=80},
                    {client_port=0,host_addr='kept',host_port=80,future=true},
                },
                socket={
                    {client_socket_path=false,host_socket_path='/ignored.sock'},
                    {client_socket_path='kept.sock',host_socket_path='/kept.sock'},
                },
            },
        }}"#,
        );
        assert!(!policy.clipboard_copy);
        assert!(!policy.http_serve_file);
        assert!(policy.filesystem_copy);
        assert!(policy.allows_tcp(0, "kept", 80));
        assert!(!policy.allows_tcp(0, "ignored", 80));
        assert!(policy.allows_socket(Path::new("kept.sock"), "/kept.sock"));
        assert!(!policy.allows_socket(Path::new("ignored.sock"), "/ignored.sock"));
    }

    #[test]
    fn allows_unrestricted_forwarding() {
        let policy = policies("return {plugin={local_forward={tcp='all',socket='all'}}}");

        assert!(policy.allows_tcp(0, "example.test", 65535));
        assert!(!policy.allows_tcp(0, "example.test", 0));
        assert!(policy.allows_socket(Path::new("nested/a.sock"), "/host/a.sock"));
        assert!(!policy.allows_socket(Path::new("nested/a.sock"), "/host/\0.sock"));
    }

    #[cfg(feature = "plugin-http")]
    #[test]
    fn matches_origins_by_scheme_host_and_effective_port() {
        let policy = policies(
            r#"return {plugin={http={open_webview={
                origins={'https://DOCS.example.test:443', 'http://[::1]:8080'},
                served_files=true,
            }}}}"#,
        );
        let all = policies("return {plugin={http={open_webview={origins='all'}}}}");

        for (url, allowed, all_allowed) in [
            ("https://docs.example.test/guide?a=b#c", true, true),
            ("https://DOCS.example.test:443", true, true),
            ("http://[0:0:0:0:0:0:0:1]:8080/", true, true),
            ("http://[::1]/", false, true),
            ("http://docs.example.test/", false, true),
            ("https://docs.example.test:8443/", false, true),
            ("https://other.example.test/", false, true),
            ("ftp://docs.example.test/", false, false),
        ] {
            let url = Url::parse(url).unwrap();
            assert_eq!(policy.webview.allows_origin(&url), allowed, "{url}");
            assert_eq!(all.webview.allows_origin(&url), all_allowed, "{url}");
        }
        assert!(policy.webview.permits_served_files());
        assert!(!all.webview.permits_served_files());
    }
}
