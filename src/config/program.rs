use crate::Error;

use super::{HostInfo, lua_string};

use mlua::{Function, Lua, Value};

use std::borrow::Cow;
use std::collections::BTreeMap;

/// A resolved host program and its configured argument and environment values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Program {
    pub prog: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Default for Program {
    fn default() -> Self {
        Self {
            prog: "nvim".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

pub(crate) enum ProgramSetting {
    Value(Program),
    Function(Function),
}

impl Default for ProgramSetting {
    fn default() -> Self {
        Self::Value(Program::default())
    }
}

impl ProgramSetting {
    pub(crate) fn parse(value: Value) -> Result<Option<Self>, Error> {
        match value {
            Value::Nil => Ok(None),
            Value::Function(f) => Ok(Some(Self::Function(f))),
            value => program(value).map(Self::Value).map(Some),
        }
    }

    pub(crate) fn needs_info(&self) -> bool {
        matches!(self, Self::Function(_))
    }

    pub(crate) fn resolve(
        &self,
        info: Option<&HostInfo>,
        lua: &Lua,
    ) -> Result<Cow<'_, Program>, Error> {
        match self {
            Self::Value(value) => Ok(Cow::Borrowed(value)),
            Self::Function(f) => {
                let info = info.ok_or_else(|| {
                    Error::Message("Host information is unavailable for the Neovim program".into())
                })?;
                program(f.call::<Value>(info.table(lua)?)?).map(Cow::Owned)
            }
        }
    }
}

fn string(value: Value, field: &str) -> Result<String, Error> {
    match value {
        Value::String(value) => Ok(lua_string(&value, field)?.to_string()),
        _ => Err(Error::Message(format!("{field} must be a string"))),
    }
}

fn sequence(value: Value) -> Result<Vec<String>, Error> {
    let Value::Table(table) = value else {
        return Err(Error::Message("nvim.args must be a string sequence".into()));
    };
    let len = table.raw_len();
    let mut count = 0_usize;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if !matches!(key, Value::Integer(n) if n >= 1 && usize::try_from(n).is_ok_and(|n| n <= len))
        {
            return Err(Error::Message(
                "nvim.args must be a contiguous string sequence".into(),
            ));
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| Error::Message("Too many Neovim arguments".into()))?;
    }
    if count != len {
        return Err(Error::Message(
            "nvim.args must be a contiguous string sequence".into(),
        ));
    }
    table
        .sequence_values::<Value>()
        .map(|value| string(value?, "nvim.args element"))
        .collect()
}

fn environment(value: Value) -> Result<BTreeMap<String, String>, Error> {
    let Value::Table(table) = value else {
        return Err(Error::Message(
            "nvim.env must map names to string values".into(),
        ));
    };
    let mut env = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let key = string(key, "nvim.env name")?;
        if key.is_empty() || key.contains('=') {
            return Err(Error::Message(
                "nvim.env names must be nonempty and must not contain '='".into(),
            ));
        }
        env.insert(key, string(value, "nvim.env value")?);
    }
    Ok(env)
}

fn program(value: Value) -> Result<Program, Error> {
    match value {
        Value::String(value) => Ok(Program {
            prog: lua_string(&value, "nvim")?.to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }),
        Value::Table(table) => {
            for pair in table.clone().pairs::<Value, Value>() {
                let field = string(pair?.0, "nvim field name")?;
                if !matches!(field.as_str(), "prog" | "args" | "env") {
                    return Err(Error::Message(format!("Unknown nvim field: {field}")));
                }
            }
            Ok(Program {
                prog: match table.get::<Value>("prog")? {
                    Value::Nil => "nvim".to_owned(),
                    value => string(value, "nvim.prog")?,
                },
                args: match table.get("args")? {
                    Value::Nil => Vec::new(),
                    value => sequence(value)?,
                },
                env: match table.get("env")? {
                    Value::Nil => BTreeMap::new(),
                    value => environment(value)?,
                },
            })
        }
        _ => Err(Error::Message(
            "nvim must be a string, program table, or function returning either".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lua: &Lua, src: &str) -> Result<ProgramSetting, Error> {
        ProgramSetting::parse(lua.load(src).eval()?)?
            .ok_or_else(|| Error::Message("Missing test program".into()))
    }

    #[test]
    fn retains_literal_program_arguments_and_environment() {
        let lua = Lua::new();
        let direct = parse(&lua, r#"return {prog='spaced nvim', args={'-O3','a b',"'quoted'",''},env={A='literal $HOME',B=''}}"#).unwrap();
        assert!(!direct.needs_info());
        assert_eq!(
            direct.resolve(None, &lua).unwrap().as_ref(),
            &Program {
                prog: "spaced nvim".into(),
                args: vec!["-O3".into(), "a b".into(), "'quoted'".into(), "".into()],
                env: BTreeMap::from([
                    ("A".into(), "literal $HOME".into()),
                    ("B".into(), "".into())
                ]),
            }
        );
        let string = parse(&lua, "return 'spaced nvim'").unwrap();
        assert_eq!(string.resolve(None, &lua).unwrap().prog, "spaced nvim");
    }

    #[test]
    fn rejects_invalid_program_shapes_without_coercion() {
        let lua = Lua::new();
        for src in [
            "return true",
            "return 123",
            "return {prog=1,args={},env={}}",
            "return {prog='nvim',args={1},env={}}",
            "return {prog='nvim',args={[2]='hole'},env={}}",
            "return {prog='nvim',args={other='x'},env={}}",
            "return {prog='nvim',args={},env={A=false}}",
            "return {prog='nvim',args={},env={[1]='value'}}",
            "return {prog='nvim',args={},env={['A=B']='value'}}",
            "return {prog='nvim',args={},env={},wrapper={}}",
        ] {
            assert!(parse(&lua, src).is_err(), "{src}");
        }
    }

    #[test]
    fn defaults_omitted_structured_program_fields() {
        let lua = Lua::new();
        for src in ["return {}", "return {prog=nil,args=nil,env=nil}"] {
            let setting = parse(&lua, src).unwrap();
            assert_eq!(
                setting.resolve(None, &lua).unwrap().as_ref(),
                &Program::default()
            );
        }
        let setting = parse(&lua, "return {args={'-u','NONE'}}").unwrap();
        let program = setting.resolve(None, &lua).unwrap();
        assert_eq!(program.prog, "nvim");
        assert_eq!(program.args, ["-u", "NONE"]);
        assert!(program.env.is_empty());
    }

    #[test]
    fn resolves_function_results_with_host_information() {
        use crate::ProjectDirs;

        let lua = Lua::new();
        let function = parse(&lua, "return function(host) assert(host.config_file == nil); return {prog=host.home_dir..'/bin/nvim',args={'-O3'},env={USER_NAME=host.user}} end").unwrap();
        assert!(function.needs_info());
        assert!(function.resolve(None, &lua).is_err());
        let info = HostInfo {
            host_name: "test".into(),
            user: "host_user".into(),
            config_file: None,
            home_dir: "/host/home".into(),
            project_dirs: ProjectDirs {
                cache_dir: "/host/cache".into(),
                data_dir: "/host/data".into(),
                runtime_dir: "/host/runtime".into(),
            },
        };
        let scalar = parse(
            &lua,
            "return function(host) return host.home_dir .. '/nvim' end",
        )
        .unwrap();
        assert_eq!(
            scalar.resolve(Some(&info), &lua).unwrap().as_ref(),
            &Program {
                prog: "/host/home/nvim".into(),
                args: Vec::new(),
                env: BTreeMap::new(),
            }
        );
        let resolved = function.resolve(Some(&info), &lua).unwrap();
        assert_eq!(resolved.prog, "/host/home/bin/nvim");
        assert_eq!(resolved.args, ["-O3"]);
        assert_eq!(
            resolved.env.get("USER_NAME").map(String::as_str),
            Some("host_user")
        );
        for src in [
            "return function(_) return 42 end",
            "return function(_) return {prog='nvim',args={},env={A=true}} end",
            r"return function(_) return 'nv\0im' end",
            r"return function(_) return {args={'\239\187\191argument'}} end",
        ] {
            assert!(
                parse(&lua, src)
                    .unwrap()
                    .resolve(Some(&info), &lua)
                    .is_err()
            );
        }
    }
}
