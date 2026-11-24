use crate::Error;

use super::lua_string;

use mlua::Value;

use std::time::Duration;

/// Selects the client-side clipboard backend and bounds utility operations.
///
/// Backend identifiers are validated after Lua configuration execution. Invalid
/// table timeout values produce diagnostics and retain the default timeout.
#[derive(Debug, Clone)]
pub struct ClipboardConfig {
    backend: String,
    timeout: Duration,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            backend: "auto".into(),
            timeout: Duration::from_secs(3),
        }
    }
}

impl ClipboardConfig {
    /// Returns the requested backend, including `"auto"` for detection.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Returns the positive operation timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub(crate) fn parse(value: Value, diagnostics: &mut Vec<String>) -> Result<Self, Error> {
        let mut ret = Self::default();
        match value {
            Value::String(s) => ret.backend = lua_string(&s, "clipboard")?.to_string(),
            Value::Table(t) => {
                if let Value::String(s) = t.get("backend")? {
                    ret.backend = lua_string(&s, "clipboard.backend")?.to_string();
                }
                match t.get::<Value>("timeout_ms")? {
                    Value::Nil => {}
                    Value::Integer(n) if n >= 1 => {
                        if let Ok(n) = u64::try_from(n) {
                            ret.timeout = Duration::from_millis(n);
                        }
                    }
                    _ => diagnostics.push(format!(
                        "clipboard.timeout_ms must have integer type and be at least 1; using {} ms",
                        ret.timeout.as_millis()
                    )),
                }
            }
            _ => {}
        }
        Ok(ret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use mlua::Lua;

    fn parse(src: &str) -> (ClipboardConfig, Vec<String>) {
        let lua = Lua::new();
        let mut diagnostics = Vec::new();
        let value = lua.load(src).eval().unwrap();
        let config = ClipboardConfig::parse(value, &mut diagnostics).unwrap();
        (config, diagnostics)
    }

    #[test]
    fn parses_backend_forms_and_default_timeout() {
        for src in [
            "return nil",
            "return 'auto'",
            "return {}",
            "return {backend=nil,timeout_ms=nil}",
            "return {backend='auto',timeout_ms=3000}",
        ] {
            let (config, diagnostics) = parse(src);
            assert_eq!(config.backend(), "auto");
            assert_eq!(config.timeout(), Duration::from_secs(3));
            assert!(diagnostics.is_empty());
        }
        let (config, diagnostics) = parse("return {backend='xsel',timeout_ms=1}");
        assert_eq!(config.backend(), "xsel");
        assert_eq!(config.timeout(), Duration::from_millis(1));
        assert!(diagnostics.is_empty());
        let (config, diagnostics) = parse("return 'not-a-backend'");
        assert_eq!(config.backend(), "not-a-backend");
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn reports_invalid_timeouts_without_rejecting_configuration() {
        for timeout in [
            "false",
            "true",
            "0",
            "-1",
            "3.0",
            "1.5",
            "'3000'",
            "{}",
            "function() return 3 end",
        ] {
            let (config, diagnostics) =
                parse(&format!("return {{backend='xclip',timeout_ms={timeout}}}"));
            assert_eq!(config.backend(), "xclip", "{timeout}");
            assert_eq!(config.timeout(), Duration::from_secs(3), "{timeout}");
            assert_eq!(diagnostics.len(), 1, "{timeout}");
            assert_eq!(
                diagnostics[0],
                "clipboard.timeout_ms must have integer type and be at least 1; using 3000 ms",
                "{timeout}"
            );
        }
    }

    #[test]
    fn omits_unsupported_clipboard_settings() {
        for (src, timeout) in [
            ("return false", 3000),
            ("return 42", 3000),
            ("return {backend=42,timeout_ms=3}", 3),
            ("return {backend='auto',timeout_ms=3,unknown=true}", 3),
            ("return {backend='auto',['timeout-ms']=3}", 3000),
        ] {
            let (config, diagnostics) = parse(src);
            assert_eq!(config.backend(), "auto", "{src}");
            assert_eq!(config.timeout(), Duration::from_millis(timeout), "{src}");
            assert!(diagnostics.is_empty(), "{src}");
        }
    }
}
