use std::collections::BTreeMap;

use crate::typecheck::types::QueryType;

/// Type information for let bindings persisted across typecheck calls
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TypeBindings {
    entries: BTreeMap<String, QueryType>,
}

impl TypeBindings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn define(&mut self, name: impl Into<String>, ty: QueryType) {
        self.entries.insert(name.into(), ty);
    }

    pub fn get(&self, name: &str) -> Option<&QueryType> {
        self.entries.get(name)
    }

    pub fn remove(&mut self, name: &str) -> Option<QueryType> {
        self.entries.remove(name)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &QueryType)> + '_ {
        self.entries.iter().map(|(name, ty)| (name.as_str(), ty))
    }
}
