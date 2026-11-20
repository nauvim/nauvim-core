use crate::Error;

use mlua::{Table, Value};

mod convert;
mod value;
pub use value::{UiConfig, UiObjectId, UiTable, UiTableId, UiValue};

use std::collections::HashSet;

pub(super) fn validate_id(ui_id: &str) -> Result<(), Error> {
    validate_key(ui_id, "UI implementation ID")
}

fn validate_key(s: &str, field: &str) -> Result<(), Error> {
    if s.is_empty() || s.starts_with('\u{feff}') || s.contains('\0') {
        return Err(Error::Message(format!(
            "{field} must be non-empty UTF-8 without leading U+FEFF or NUL"
        )));
    }
    Ok(())
}

pub(super) fn select(config: &Table, ui_id: &str) -> Result<UiConfig, Error> {
    let selector = match config.raw_get::<Value>("use_ui_opts")? {
        Value::Nil => None,
        Value::String(s) => {
            validate_key(&s.to_str()?, "use_ui_opts")?;
            Some(s)
        }
        _ => return Err(Error::Message("use_ui_opts must be a string or nil".into())),
    };
    let mapping = match config.raw_get::<Value>("ui_opts")? {
        Value::Nil => return Ok(UiConfig::default()),
        Value::Table(t) => t,
        _ => return Err(Error::Message("ui_opts must be a dictionary or nil".into())),
    };
    let mut pending = Vec::new();
    for pair in mapping.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let Value::String(key) = key else {
            return Err(Error::Message("ui_opts keys must be strings".into()));
        };
        validate_key(&key.to_str()?, "ui_opts key")?;
        let Value::Table(t) = value else {
            return Err(Error::Message(
                "ui_opts entries must be dictionaries".into(),
            ));
        };
        for pair in t.pairs::<Value, Value>() {
            if !matches!(pair?.0, Value::String(_)) {
                return Err(Error::Message(
                    "UI option dictionary keys must be strings".into(),
                ));
            }
        }
        pending.push(t);
    }

    let mut visited = HashSet::new();
    while let Some(t) = pending.pop() {
        if !visited.insert(t.to_pointer()) {
            continue;
        }
        for pair in t.pairs::<Value, Value>() {
            let (key, value) = pair?;
            for value in [key, value] {
                match value {
                    Value::Function(_) => {
                        return Err(Error::Message(
                            "UI option dictionaries must not contain functions".into(),
                        ));
                    }
                    Value::Table(t) => pending.push(t),
                    _ => {}
                }
            }
        }
    }

    let selected = match selector {
        Some(key) => mapping.raw_get::<Value>(key)?,
        None => mapping.raw_get::<Value>(ui_id)?,
    };
    match selected {
        Value::Nil => Ok(UiConfig::default()),
        Value::Table(t) => convert::from_table(t),
        _ => Err(Error::Message(
            "ui_opts entries must be dictionaries".into(),
        )),
    }
}
