use crate::Error;

use super::value::{UiConfig, UiObjectId, UiTable, UiTableId, UiValue};

use mlua::{Table, Value};

use std::collections::HashMap;
use std::ffi::c_void;

pub(super) fn from_table(root: Table) -> Result<UiConfig, Error> {
    #[derive(Hash, PartialEq, Eq)]
    enum ObjectKey {
        Light(*mut c_void),
        Thread(*const c_void),
        UserData(*const c_void),
        Other(*const c_void),
    }

    #[derive(Default)]
    struct Converter {
        config: UiConfig,
        tables: HashMap<*const c_void, UiTableId>,
        pending: Vec<(UiTableId, Table)>,
        objects: HashMap<ObjectKey, UiObjectId>,
        next_object: usize,
    }

    impl Converter {
        fn object(&mut self, key: Option<ObjectKey>) -> Result<UiValue, Error> {
            if let Some(id) = key.as_ref().and_then(|key| self.objects.get(key)) {
                return Ok(UiValue::Opaque(*id));
            }
            let id = UiObjectId(self.next_object);
            self.next_object = self
                .next_object
                .checked_add(1)
                .ok_or_else(|| Error::Message("Too many opaque UI option values".into()))?;
            if let Some(key) = key {
                self.objects.insert(key, id);
            }
            Ok(UiValue::Opaque(id))
        }

        fn value(&mut self, value: Value) -> Result<UiValue, Error> {
            Ok(match value {
                Value::Nil => UiValue::Nil,
                Value::Boolean(value) => UiValue::Boolean(value),
                Value::Integer(value) => UiValue::Integer(value),
                Value::Number(value) => UiValue::Number(value),
                Value::String(value) => UiValue::String(value.as_bytes().to_vec()),
                Value::Table(table) => {
                    let ptr = table.to_pointer();
                    let id = match self.tables.get(&ptr) {
                        Some(id) => *id,
                        None => {
                            self.config.tables.push(UiTable::default());
                            let id = UiTableId(self.config.tables.len());
                            self.tables.insert(ptr, id);
                            self.pending.push((id, table));
                            id
                        }
                    };
                    UiValue::Table(id)
                }
                Value::Function(_) => {
                    return Err(Error::Message(
                        "UI option dictionaries must not contain functions".into(),
                    ));
                }
                Value::LightUserData(value) => {
                    return self.object(Some(ObjectKey::Light(value.0)));
                }
                Value::Thread(value) => {
                    return self.object(Some(ObjectKey::Thread(value.to_pointer())));
                }
                Value::UserData(value) => {
                    return self.object(Some(ObjectKey::UserData(value.to_pointer())));
                }
                Value::Other(value) => {
                    return self.object(Some(ObjectKey::Other(Value::Other(value).to_pointer())));
                }
                Value::Error(_) => return self.object(None),
            })
        }
    }

    let mut converter = Converter::default();
    converter.tables.insert(root.to_pointer(), UiTableId(0));
    converter.pending.push((UiTableId(0), root));
    while let Some((id, table)) = converter.pending.pop() {
        let mut entries = Vec::new();
        for pair in table.pairs::<Value, Value>() {
            let (key, value) = pair?;
            entries.push((converter.value(key)?, converter.value(value)?));
        }
        let table = UiTable { entries };
        match id.0.checked_sub(1) {
            None => converter.config.root = table,
            Some(idx) => {
                let target = converter.config.tables.get_mut(idx).ok_or_else(|| {
                    Error::Message("Missing UI option table during conversion".into())
                })?;
                *target = table;
            }
        }
    }
    Ok(converter.config)
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
                .load("return {text=string.char(255,0),number=0/0,boolean=true}")
                .eval::<Table>()
                .unwrap();
            from_table(table).unwrap()
        };
        assert_eq!(config.get("text"), Some(&UiValue::String(vec![255, 0])));
        assert!(matches!(config.get("number"), Some(UiValue::Number(n)) if n.is_nan()));
        assert_eq!(config.get("boolean"), Some(&UiValue::Boolean(true)));
    }

    #[test]
    fn snapshot_preserves_opaque_identity_without_retaining_objects() {
        let config = {
            let lua = Lua::new();
            let table = lua
                .load(
                    "local a=coroutine.create(function() end); local b=coroutine.create(function() end); return {a=a,alias=a,b=b}",
                )
                .eval::<Table>()
                .unwrap();
            from_table(table).unwrap()
        };
        let Some(UiValue::Opaque(a)) = config.get("a") else {
            panic!("Expected an opaque object token")
        };
        assert_eq!(config.get("alias"), Some(&UiValue::Opaque(*a)));
        assert_ne!(config.get("a"), config.get("b"));
    }

    #[test]
    fn snapshot_rejects_functions_in_nested_keys_and_values() {
        let lua = Lua::new();
        for source in [
            "return {nested={[function() end]=true}}",
            "return {nested={function() end}}",
            "return {nested={[{callback=function() end}]=true}}",
        ] {
            let table = lua.load(source).eval::<Table>().unwrap();
            assert!(
                matches!(from_table(table), Err(Error::Message(message)) if message == "UI option dictionaries must not contain functions"),
                "{source}"
            );
        }
    }
}
