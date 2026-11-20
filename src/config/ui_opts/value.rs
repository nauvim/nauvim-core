/// An owned snapshot of the selected UI-specific option dictionary.
///
/// Tables form a graph so shared references, cycles, and table-valued keys are
/// preserved. Strings retain their original bytes. Opaque values have local
/// identity tokens; they retain no runtime resources or executable behavior.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UiConfig {
    pub(super) root: UiTable,
    pub(super) tables: Vec<UiTable>,
}

/// An owned table containing key-value pairs in unspecified order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UiTable {
    pub(super) entries: Vec<(UiValue, UiValue)>,
}

/// An owned UI-specific option value.
#[derive(Debug, Clone, PartialEq)]
pub enum UiValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    /// Preserves string bytes without requiring UTF-8.
    String(Vec<u8>),
    /// Identifies another table in the same configuration snapshot.
    Table(UiTableId),
    /// Identifies a non-data object without retaining its runtime resources.
    Opaque(UiObjectId),
}

/// Identifies a table within one [`UiConfig`] snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UiTableId(pub(super) usize);

/// Identifies an opaque object within one [`UiConfig`] snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UiObjectId(pub(super) usize);

impl UiConfig {
    /// Borrows the selected dictionary, whose keys are string values.
    pub fn root(&self) -> &UiTable {
        &self.root
    }

    /// Borrows a root dictionary value by its exact UTF-8 key.
    pub fn get(&self, key: &str) -> Option<&UiValue> {
        self.root.get(key)
    }

    /// Resolves a table ID within this configuration snapshot.
    pub fn table(&self, id: UiTableId) -> Option<&UiTable> {
        match id.0.checked_sub(1) {
            None => Some(&self.root),
            Some(idx) => self.tables.get(idx),
        }
    }
}

impl UiTable {
    /// Iterates over the table's key-value pairs in unspecified order.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&UiValue, &UiValue)> {
        self.entries.iter().map(|(key, value)| (key, value))
    }

    /// Borrows a string-keyed value by its exact UTF-8 key.
    ///
    /// Use [`Self::entries()`] for byte-string keys that are not valid UTF-8 or
    /// for keys of other types.
    pub fn get(&self, key: &str) -> Option<&UiValue> {
        self.entries.iter().find_map(|(candidate, value)| {
            matches!(candidate, UiValue::String(bytes) if bytes == key.as_bytes()).then_some(value)
        })
    }
}
