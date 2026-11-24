use crate::Error;

use super::{lua_string, validate_string};

use mlua::{Table, Value};

mod convert;
mod value;
pub use value::{UiConfig, UiTable, UiTableId, UiValue};

pub(super) fn validate_id(ui_id: &str) -> Result<(), Error> {
    validate_string(ui_id, "UI implementation ID")?;
    require_nonempty(ui_id, "UI implementation ID")
}

fn require_nonempty(s: &str, field: &str) -> Result<(), Error> {
    if s.is_empty() {
        return Err(Error::Message(format!("{field} must be non-empty")));
    }
    Ok(())
}

pub(super) fn select(config: &Table, ui_id: &str) -> Result<UiConfig, Error> {
    let selector = match config.raw_get::<Value>("use_ui_opts")? {
        Value::String(s) => {
            let key = lua_string(&s, "use_ui_opts")?;
            require_nonempty(&key, "use_ui_opts")?;
            Some(key.to_string())
        }
        _ => None,
    };
    let selected_key = selector.as_deref().unwrap_or(ui_id);
    let mapping = match config.raw_get::<Value>("ui_opts")? {
        Value::Table(t) => t,
        _ => return Ok(UiConfig::default()),
    };
    let mut selected = UiConfig::default();
    for pair in mapping.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let Value::String(key) = key else {
            continue;
        };
        let key = lua_string(&key, "ui_opts key")?;
        require_nonempty(&key, "ui_opts key")?;
        let Value::Table(table) = value else {
            continue;
        };
        let opts = convert::from_table(table)?;
        if key.as_ref() == selected_key {
            selected = opts;
        }
    }
    Ok(selected)
}
