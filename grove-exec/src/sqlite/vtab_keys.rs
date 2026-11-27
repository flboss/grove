use std::borrow::Cow;
use std::ffi::{CStr, c_int};
use std::sync::{Arc, RwLock};

use rusqlite::vtab::{
    Context, Filters, IndexConstraintOp, IndexFlags, IndexInfo, Module, VTab, VTabConnection,
    VTabCursor, sqlite3_vtab, sqlite3_vtab_cursor,
};
use rusqlite::{Connection, Result};

pub(super) const MODULE_NAME: &CStr = c"sqlite_grove_keys";

const MODULE: Module<KeysTab> = Module::eponymous_only_module();

/// Shared key list backing the vtab (sorted and deduplicated)
pub(super) struct KeySupply {
    keys: RwLock<Arc<[i64]>>,
}

impl KeySupply {
    pub(super) fn new() -> Self {
        Self {
            keys: RwLock::new(Arc::from(Vec::<i64>::new())),
        }
    }

    #[allow(dead_code)]
    pub(super) fn set(&self, keys: impl IntoIterator<Item = i64>) {
        let mut keys: Vec<i64> = keys.into_iter().collect();
        // enables binary search on keys
        keys.sort_unstable();
        // necessary for JOIN correctness
        keys.dedup();
        *self.keys.write().unwrap() = Arc::from(keys);
    }

    pub(super) fn snapshot(&self) -> Arc<[i64]> {
        Arc::clone(&self.keys.read().unwrap())
    }

    pub(super) fn len(&self) -> usize {
        self.keys.read().unwrap().len()
    }
}

/// Register the eponymous virtual keys table on `conn`
pub(super) fn register(conn: &Connection, supply: Arc<KeySupply>) -> Result<()> {
    conn.create_module(MODULE_NAME, &MODULE, Some(supply))
}

#[repr(C)]
struct KeysTab {
    /// must be first
    base: sqlite3_vtab,
    supply: Arc<KeySupply>,
}

#[repr(C)]
struct KeysCursor {
    /// must be first
    base: sqlite3_vtab_cursor,
    /// frozen key list
    snapshot: Arc<[i64]>,
    pos: usize,
    /// `Some(key)`: equality strategy chosen; `None`: full scan
    lookup: Option<i64>,
    found: bool,
}

impl KeysCursor {
    fn current(&self) -> i64 {
        self.lookup.unwrap_or(self.snapshot[self.pos])
    }
}

unsafe impl<'vtab> VTab<'vtab> for KeysTab {
    type Aux = Arc<KeySupply>;
    type Cursor = KeysCursor;

    fn connect(
        _db: &mut VTabConnection,
        aux: Option<&Self::Aux>,
        _module_name: &[u8],
        _database_name: &[u8],
        _table_name: &[u8],
        _args: &[&[u8]],
    ) -> Result<(Cow<'static, CStr>, Self)> {
        let supply = Arc::clone(aux.expect("keys module registered without a supply"));
        Ok((
            Cow::Borrowed(c"CREATE TABLE x(value INTEGER)"),
            Self {
                base: sqlite3_vtab::default(),
                supply,
            },
        ))
    }

    fn best_index(&self, info: &mut IndexInfo) -> Result<bool> {
        let mut equality = None;
        for (index, constraint) in info.constraints().enumerate() {
            // 0 is `value`; -1 is vtab rowid (=value)
            let on_key = constraint.column() == 0 || constraint.column() == -1;
            if constraint.is_usable()
                && on_key
                && constraint.operator() == IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ
                && !info.is_in_constraint(index)?
            {
                equality = Some(index);
                break;
            }
        }

        let len = self.supply.len();
        let n = len.max(1) as f64;
        if let Some(index) = equality {
            // equality lookup (single unique row)
            info.set_idx_num(1);
            let mut usage = info.constraint_usage(index);
            usage.set_argv_index(1);
            // instruct SQLite to skip rechecking equality
            usage.set_omit(true);
            // binary search over sorted list
            info.set_estimated_cost(1.0 + n.log2());
            info.set_estimated_rows(1);
            info.set_idx_flags(IndexFlags::SQLITE_INDEX_SCAN_UNIQUE);
        } else {
            // full scan
            info.set_idx_num(0);
            info.set_estimated_cost(n);
            info.set_estimated_rows(len.max(1) as i64);
        }
        Ok(true)
    }

    fn open(&'vtab mut self) -> Result<Self::Cursor> {
        Ok(KeysCursor {
            base: sqlite3_vtab_cursor::default(),
            snapshot: self.supply.snapshot(),
            pos: 0,
            lookup: None,
            found: false,
        })
    }
}

unsafe impl VTabCursor for KeysCursor {
    fn filter(&mut self, idx_num: c_int, _idx_str: Option<&str>, args: &Filters<'_>) -> Result<()> {
        self.pos = 0;
        self.lookup = None;
        self.found = false;
        // equality lookup
        if idx_num == 1 {
            let key = args.get::<i64>(0)?;
            self.lookup = Some(key);
            self.found = self.snapshot.binary_search(&key).is_ok();
        }
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        self.pos += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        match self.lookup {
            Some(_) => !self.found || self.pos > 0,
            None => self.pos >= self.snapshot.len(),
        }
    }

    fn column(&self, ctx: &mut Context, i: c_int) -> Result<()> {
        debug_assert_eq!(i, 0);
        ctx.set_result(self.current())
    }

    fn rowid(&self) -> Result<i64> {
        Ok(self.current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    const TABLE: &str = "\"sqlite_grove_keys\"";

    fn backend(keys: &[i64]) -> (Connection, Arc<KeySupply>) {
        let conn = Connection::open_in_memory().unwrap();
        let supply = Arc::new(KeySupply::new());
        supply.set(keys.iter().copied());
        register(&conn, Arc::clone(&supply)).unwrap();
        (conn, supply)
    }

    #[test]
    fn full_scan_sorted_and_deduplicated() {
        let (conn, _) = backend(&[3, 1, 3, 2]);
        let values: Vec<i64> = conn
            .prepare(&format!("SELECT value FROM {TABLE}"))
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(values, vec![1, 2, 3]);
    }

    #[test]
    fn equality_lookup_hit_miss() {
        let (conn, _) = backend(&[1, 2, 3]);
        let hit: Option<i64> = conn
            .query_row(
                &format!("SELECT value FROM {TABLE} WHERE value = ?"),
                [2],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        let miss: Option<i64> = conn
            .query_row(
                &format!("SELECT value FROM {TABLE} WHERE value = ?"),
                [9],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(hit, Some(2));
        assert_eq!(miss, None);
    }

    #[test]
    fn cached_statements_affected_by_key_supply_swap() {
        let (conn, supply) = backend(&[1, 2]);
        let mut stmt = conn
            .prepare_cached(&format!("SELECT value FROM {TABLE} ORDER BY value"))
            .unwrap();
        let first: Vec<i64> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        drop(stmt);

        supply.set([3, 4]);
        let second: Vec<i64> = conn
            .prepare_cached(&format!("SELECT value FROM {TABLE} ORDER BY value"))
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(first, vec![1, 2]);
        assert_eq!(second, vec![3, 4]);
    }

    #[test]
    fn join_keys_against_rowid_deduplicated() {
        let (conn, _) = backend(&[2, 1, 2]);
        conn.execute_batch(
            "CREATE TABLE t(id INTEGER PRIMARY KEY);
             INSERT INTO t(id) VALUES (1), (2), (3);",
        )
        .unwrap();
        let values: Vec<i64> = conn
            .prepare(&format!(
                "SELECT t.id FROM t JOIN {TABLE} AS k ON k.value = t._rowid_ ORDER BY t.id"
            ))
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        assert_eq!(values, vec![1, 2]);
    }
}
