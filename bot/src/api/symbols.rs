// symbols.rs — cache of cTrader symbol_id ↔ symbol_name.
//
// Populated once after account authentication via ProtoOASymbolsListReq.
// Lookups always return a string so serialisation can't fail; unknown
// ids are logged once per id and rendered as "SYMBOL_<id>".

use dashmap::DashMap;
use std::sync::Arc;
use tracing::warn;

#[derive(Default)]
pub struct SymbolCatalog {
    map: DashMap<i64, String>,
    reverse: DashMap<String, i64>,
}

impl SymbolCatalog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Populate the catalog from a `ProtoOaSymbolsListRes` response.
    pub fn populate<'a, I>(&self, entries: I) -> usize
    where
        I: IntoIterator<Item = (i64, &'a str)>,
    {
        let mut count = 0usize;
        for (id, name) in entries {
            self.map.insert(id, name.to_string());
            self.reverse.insert(name.to_string(), id);
            count += 1;
        }
        count
    }

    /// Reverse lookup: symbol name to id. Case-sensitive exact match.
    pub fn id(&self, name: &str) -> Option<i64> {
        self.reverse.get(name).map(|e| *e.value())
    }

    /// Resolve an id to a symbol name. Unknown ids get a stable placeholder
    /// and are logged at WARN so a catalog miss is visible.
    pub fn name(&self, id: i64) -> String {
        if let Some(entry) = self.map.get(&id) {
            return entry.clone();
        }
        let fallback = format!("SYMBOL_{id}");
        warn!(symbol_id = id, "symbol catalog miss; using placeholder");
        fallback
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    // Required by clippy's `len_without_is_empty` for any type with a
    // public `len()`. Not called today; present to satisfy the lint.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn populate_and_lookup() {
        let cat = SymbolCatalog::new();
        cat.populate([(1, "EURUSD"), (2, "GBPUSD")]);
        assert_eq!(cat.name(1), "EURUSD");
        assert_eq!(cat.name(2), "GBPUSD");
        assert_eq!(cat.len(), 2);
    }

    #[test]
    fn missing_lookup_returns_placeholder() {
        let cat = SymbolCatalog::new();
        assert_eq!(cat.name(99), "SYMBOL_99");
    }

    #[test]
    fn reverse_lookup_roundtrips() {
        let cat = SymbolCatalog::new();
        cat.populate([(7, "XAUUSD")]);
        assert_eq!(cat.id("XAUUSD"), Some(7));
        assert!(cat.id("UNKNOWN").is_none());
    }
}
