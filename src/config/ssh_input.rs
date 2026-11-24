use crate::Error;

use mlua::{Table, Value};

#[derive(Clone, Copy)]
pub(super) enum Field {
    HostName,
    User,
    Port,
    IdentityFile,
    CertificateFile,
    IdentityAgent,
    IdentitiesOnly,
    ProxyJump,
    KexAlgorithms,
    HostKeyAlgorithms,
}

impl Field {
    fn from_entry(key: &[u8], value: &Value) -> Option<Self> {
        match (key, value) {
            (b"host_name", Value::String(_)) => Some(Self::HostName),
            (b"user", Value::String(_)) => Some(Self::User),
            (b"port", Value::Integer(_) | Value::String(_)) => Some(Self::Port),
            (b"identity_file", Value::String(_)) => Some(Self::IdentityFile),
            (b"certificate_file", Value::String(_)) => Some(Self::CertificateFile),
            (b"identity_agent", Value::String(_)) => Some(Self::IdentityAgent),
            (b"identities_only", Value::Boolean(_) | Value::String(_)) => {
                Some(Self::IdentitiesOnly)
            }
            (b"proxy_jump", Value::String(_) | Value::Table(_)) => Some(Self::ProxyJump),
            (b"kex_algorithms", Value::String(_) | Value::Table(_)) => Some(Self::KexAlgorithms),
            (b"host_key_algorithms", Value::String(_) | Value::Table(_)) => {
                Some(Self::HostKeyAlgorithms)
            }
            _ => None,
        }
    }
}

pub(super) fn entries(t: &Table) -> impl Iterator<Item = Result<(Field, Value), Error>> + '_ {
    t.pairs::<Value, Value>().filter_map(|entry| match entry {
        Ok((Value::String(key), value)) => {
            Field::from_entry(key.as_bytes().as_ref(), &value).map(|field| Ok((field, value)))
        }
        Ok(_) => None,
        Err(e) => Some(Err(e.into())),
    })
}

pub(super) fn inline_jump(t: &Table) -> Result<bool, Error> {
    for entry in t.pairs::<Value, Value>() {
        if matches!(entry?.0, Value::String(_)) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(not(feature = "ssh"))]
pub(super) fn validate_strings(t: &Table) -> Result<(), Error> {
    use super::lua_string;

    use std::collections::HashSet;

    let mut pending = vec![t.clone()];
    let mut visited = HashSet::new();
    while let Some(t) = pending.pop() {
        if !visited.insert(t.to_pointer()) {
            continue;
        }
        for entry in entries(&t) {
            let (field, value) = entry?;
            match value {
                Value::String(s) => {
                    lua_string(&s, "SSH setting")?;
                }
                Value::Table(t) if matches!(field, Field::ProxyJump) && inline_jump(&t)? => {
                    pending.push(t);
                }
                Value::Table(t) => {
                    for value in t.sequence_values::<Value>() {
                        match value? {
                            Value::String(s) => {
                                lua_string(&s, "SSH setting")?;
                            }
                            Value::Table(t) if matches!(field, Field::ProxyJump) => pending.push(t),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}
