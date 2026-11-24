use crate::Error;

use super::super::lua_string;
use super::value::{UiConfig, UiTable, UiTableId, UiValue};

use mlua::{Table, Value};

use std::collections::HashMap;
use std::ffi::c_void;

enum State {
    Pending,
    Active,
    Complete,
}

enum Visit {
    Table(Table),
    Finish(*const c_void),
}

pub(super) fn from_table(root: Table) -> Result<UiConfig, Error> {
    let mut config = UiConfig::default();
    let mut tables = HashMap::from([(root.to_pointer(), (UiTableId(0), State::Pending))]);
    // Visit children before completing their parent so an active table is
    // identified as a cycle and completed tables can be shared.
    let mut pending = vec![Visit::Table(root.clone())];
    while let Some(visit) = pending.pop() {
        let table = match visit {
            Visit::Table(table) => table,
            Visit::Finish(ptr) => {
                tables.get_mut(&ptr).unwrap().1 = State::Complete;
                continue;
            }
        };
        let ptr = table.to_pointer();
        let (id, state) = tables.get_mut(&ptr).unwrap();
        match state {
            State::Complete => continue,
            State::Active => {
                return Err(Error::Message("UI option tables must not be cyclic".into()));
            }
            State::Pending => *state = State::Active,
        }
        let id = *id;
        pending.push(Visit::Finish(ptr));

        let is_list = id.0 != 0 && list_form(&table)?;
        let mut entries = Vec::new();
        for pair in table.pairs::<Value, Value>() {
            let (key, value) = pair?;
            let key = match key {
                Value::Integer(n) if is_list => UiValue::Integer(n),
                Value::String(s) if !is_list => {
                    UiValue::String(lua_string(&s, "UI option dictionary key")?.to_string())
                }
                _ if !is_list => continue,
                _ => {
                    return Err(Error::Message(
                        "UI option lists must have integer keys".into(),
                    ));
                }
            };
            let value = match value {
                Value::Nil => UiValue::Nil,
                Value::Boolean(value) => UiValue::Boolean(value),
                Value::Integer(value) => UiValue::Integer(value),
                Value::Number(value) => UiValue::Number(value),
                Value::String(s) => {
                    UiValue::String(lua_string(&s, "UI option string")?.to_string())
                }
                Value::Table(table) => {
                    let (id, state) = tables.entry(table.to_pointer()).or_insert_with(|| {
                        config.tables.push(UiTable::default());
                        (UiTableId(config.tables.len()), State::Pending)
                    });
                    if !matches!(state, State::Complete) {
                        pending.push(Visit::Table(table));
                    }
                    UiValue::Table(*id)
                }
                _ if !is_list => continue,
                value => {
                    return Err(Error::Message(format!(
                        "Unsupported UI option list element type: {}",
                        value.type_name()
                    )));
                }
            };
            entries.push((key, value));
        }
        if is_list {
            let index = |(key, _): &(UiValue, UiValue)| match key {
                UiValue::Integer(n) => *n,
                _ => unreachable!("list keys were checked above"),
            };
            entries.sort_unstable_by_key(index);
            if !(1..)
                .zip(&entries)
                .all(|(expected, entry)| index(entry) == expected)
            {
                return Err(Error::Message(
                    "UI option lists must have contiguous indices starting at 1".into(),
                ));
            }
        }

        let table = UiTable { entries, is_list };
        match id.0.checked_sub(1) {
            None => config.root = table,
            Some(idx) => config.tables[idx] = table,
        }
    }
    Ok(config)
}

fn list_form(t: &Table) -> Result<bool, Error> {
    let mut has_index = false;
    for pair in t.pairs::<Value, Value>() {
        match pair?.0 {
            Value::String(_) => return Ok(false),
            Value::Integer(_) => has_index = true,
            _ => {}
        }
    }
    Ok(has_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    use mlua::Lua;

    #[test]
    fn snapshot_retains_data_after_source_runtime_is_dropped() {
        let config = {
            let lua = Lua::new();
            let table = lua
                .load("return {text='日本語',number=0/0,boolean=true}")
                .eval::<Table>()
                .unwrap();
            from_table(table).unwrap()
        };
        assert_eq!(config.get("text"), Some(&UiValue::String("日本語".into())));
        assert!(matches!(config.get("number"), Some(UiValue::Number(n)) if n.is_nan()));
        assert_eq!(config.get("boolean"), Some(&UiValue::Boolean(true)));
    }

    #[test]
    fn snapshot_rejects_invalid_list_entries() {
        let lua = Lua::new();
        for source in [
            "return {nested={function() end}}",
            "return {nested={[-1]=true}}",
            "return {nested={[2]=true}}",
            "return {nested={true,[false]=true}}",
        ] {
            let table = lua.load(source).eval::<Table>().unwrap();
            assert!(from_table(table).is_err(), "{source}");
        }
    }
}
