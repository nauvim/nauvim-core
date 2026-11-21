/// An owned snapshot of the selected UI-specific option dictionary.
///
/// Nested dictionaries and lists can share table references. The snapshot
/// owns its data independently of the Lua runtime.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UiConfig {
    pub(super) root: UiTable,
    pub(super) tables: Vec<UiTable>,
}

/// An owned dictionary or list of UI option values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UiTable {
    pub(super) entries: Vec<(UiValue, UiValue)>,
    pub(super) is_list: bool,
}

/// An owned UI-specific option value.
#[derive(Debug, Clone, PartialEq)]
pub enum UiValue {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(f64),
    /// Text without NUL or an initial U+FEFF.
    String(String),
    /// Identifies another table in the same configuration snapshot.
    Table(UiTableId),
}

/// Identifies a table within one [`UiConfig`] snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UiTableId(pub(super) usize);

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
    /// Returns whether this table represents a non-empty ordered list.
    /// Empty Lua tables are represented as dictionaries.
    pub fn is_list(&self) -> bool {
        self.is_list
    }

    /// Iterates over ordered list elements with their one-based integer keys,
    /// or over dictionary entries with string keys in unspecified order.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = (&UiValue, &UiValue)> {
        self.entries.iter().map(|(key, value)| (key, value))
    }

    /// Borrows a string-keyed value by its exact UTF-8 key.
    pub fn get(&self, key: &str) -> Option<&UiValue> {
        self.entries.iter().find_map(|(candidate, value)| {
            matches!(candidate, UiValue::String(text) if text == key).then_some(value)
        })
    }
}
