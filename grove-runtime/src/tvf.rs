
use std::borrow::Cow;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::os::raw::c_int;
use std::sync::{Arc, Mutex};

use rusqlite::types::Value as SqlValue;
use rusqlite::vtab::{
    Context as VtabContext, Filters, IndexInfo, Module, VTab, VTabConnection, VTabCursor,
    sqlite3_vtab, sqlite3_vtab_cursor,
};
use rusqlite::{Connection, Result};

pub type BulkSlot = Arc<Mutex<Vec<i64>>>;

pub type LitState = Arc<Mutex<HashMap<String, Vec<Vec<SqlValue>>>>>;

const BULK_MODULE: Module<BulkTab> = Module::eponymous_only_module();

#[repr(C)]
struct BulkTab {
    base: sqlite3_vtab,
    slot: BulkSlot,
}

unsafe impl<'vtab> VTab<'vtab> for BulkTab {
    type Aux = BulkSlot;
    type Cursor = BulkCursor<'vtab>;

    fn connect(
        _: &mut VTabConnection,
        aux: Option<&BulkSlot>,
        _: &[u8],
        _: &[u8],
        _: &[u8],
        _: &[&[u8]],
    ) -> Result<(Cow<'static, std::ffi::CStr>, Self)> {
        Ok((
            Cow::Borrowed(c"CREATE TABLE x(value)"),
            Self {
                base: sqlite3_vtab::default(),
                slot: aux.cloned().unwrap_or_default(),
            },
        ))
    }

    fn best_index(&self, info: &mut IndexInfo) -> Result<bool> {
        info.set_estimated_cost(1.0);
        Ok(true)
    }

    fn open(&mut self) -> Result<BulkCursor<'_>> {
        Ok(BulkCursor {
            base: sqlite3_vtab_cursor::default(),
            slot: self.slot.clone(),
            rows: Vec::new(),
            pos: 0,
            phantom: PhantomData,
        })
    }
}

#[repr(C)]
struct BulkCursor<'vtab> {
    base: sqlite3_vtab_cursor,
    slot: BulkSlot,
    rows: Vec<i64>,
    pos: usize,
    phantom: PhantomData<&'vtab BulkTab>,
}

unsafe impl VTabCursor for BulkCursor<'_> {
    fn filter(&mut self, _: c_int, _: Option<&str>, _: &Filters<'_>) -> Result<()> {
        self.rows = self.slot.lock().unwrap().clone();
        self.pos = 0;
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        self.pos += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.pos >= self.rows.len()
    }

    fn column(&self, ctx: &mut VtabContext, _: c_int) -> Result<()> {
        ctx.set_result(&self.rows[self.pos])
    }

    fn rowid(&self) -> Result<i64> {
        Ok(self.pos as i64)
    }
}

pub fn register_bulk(conn: &Connection, slot: BulkSlot) -> Result<()> {
    conn.create_module("$$bulk_params", &BULK_MODULE, Some(slot))
}


fn affinity(letter: u8) -> Option<&'static str> {
    match letter {
        b'I' => Some("INTEGER"),
        b'F' => Some("REAL"),
        b'T' => Some("TEXT"),
        b'B' => Some("BLOB"),
        _ => None,
    }
}

pub fn module_shape(name: &str) -> Option<(usize, Vec<u8>)> {
    let rest = name.strip_prefix("$$lit_")?;
    let (arity, sig) = rest.split_once('_')?;
    let arity: usize = arity.parse().ok()?;
    if sig.len() != arity || arity == 0 || !sig.bytes().all(|b| affinity(b).is_some()) {
        return None;
    }
    Some((arity, sig.bytes().collect()))
}

fn parse_module(name: &[u8]) -> Option<Vec<&'static str>> {
    let name = std::str::from_utf8(name).ok()?;
    let rest = name.strip_prefix("$$lit_")?;
    let (arity, sig) = rest.split_once('_')?;
    let arity: usize = arity.parse().ok()?;
    if sig.len() != arity || arity == 0 {
        return None;
    }
    let mut cols = Vec::with_capacity(arity);
    for letter in sig.bytes() {
        cols.push(affinity(letter)?);
    }
    Some(cols)
}

const LIT_MODULE: Module<LitTab> = Module::eponymous_only_module();

#[repr(C)]
struct LitTab {
    base: sqlite3_vtab,
    state: LitState,
}

unsafe impl<'vtab> VTab<'vtab> for LitTab {
    type Aux = LitState;
    type Cursor = LitCursor<'vtab>;

    fn connect(
        _: &mut VTabConnection,
        aux: Option<&LitState>,
        module_name: &[u8],
        _: &[u8],
        _: &[u8],
        _: &[&[u8]],
    ) -> Result<(Cow<'static, std::ffi::CStr>, Self)> {
        let Some(cols) = parse_module(module_name) else {
            return Err(rusqlite::Error::ModuleError(format!(
                "bad literal module name: {}",
                String::from_utf8_lossy(module_name)
            )));
        };
        let mut schema = String::from("CREATE TABLE x(id HIDDEN");
        for (i, affinity) in cols.iter().enumerate() {
            schema.push_str(&format!(", c{i} {affinity}"));
        }
        schema.push(')');
        let schema: &'static std::ffi::CStr = Box::leak(
            std::ffi::CString::new(schema)
                .expect("literal schema has no interior nul")
                .into_boxed_c_str(),
        );
        Ok((
            Cow::Borrowed(schema),
            Self {
                base: sqlite3_vtab::default(),
                state: aux.cloned().unwrap_or_default(),
            },
        ))
    }

    fn best_index(&self, info: &mut IndexInfo) -> Result<bool> {
        use rusqlite::vtab::IndexConstraintOp;
        let mut with_id = false;
        for (constraint, mut usage) in info.constraints_and_usages() {
            if constraint.column() == 0
                && constraint.operator() == IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ
                && constraint.is_usable()
            {
                usage.set_argv_index(1);
                usage.set_omit(true);
                with_id = true;
            }
        }
        info.set_idx_num(c_int::from(with_id));
        info.set_estimated_cost(1.0);
        Ok(true)
    }

    fn open(&mut self) -> Result<LitCursor<'_>> {
        Ok(LitCursor {
            base: sqlite3_vtab_cursor::default(),
            state: self.state.clone(),
            id: String::new(),
            rows: Vec::new(),
            pos: 0,
            phantom: PhantomData,
        })
    }
}

#[repr(C)]
struct LitCursor<'vtab> {
    base: sqlite3_vtab_cursor,
    state: LitState,
    id: String,
    rows: Vec<Vec<SqlValue>>,
    pos: usize,
    phantom: PhantomData<&'vtab LitTab>,
}

unsafe impl VTabCursor for LitCursor<'_> {
    fn filter(&mut self, idx_num: c_int, _: Option<&str>, args: &Filters<'_>) -> Result<()> {
        (self.id, self.rows) = if idx_num == 1 {
            let id: String = args.get(0)?;
            let rows = self
                .state
                .lock()
                .unwrap()
                .get(&id)
                .cloned()
                .unwrap_or_default();
            (id, rows)
        } else {
            (String::new(), Vec::new())
        };
        self.pos = 0;
        Ok(())
    }

    fn next(&mut self) -> Result<()> {
        self.pos += 1;
        Ok(())
    }

    fn eof(&self) -> bool {
        self.pos >= self.rows.len()
    }

    fn column(&self, ctx: &mut VtabContext, i: c_int) -> Result<()> {
        if i == 0 {
            ctx.set_result(&self.id)
        } else {
            ctx.set_result(&self.rows[self.pos][i as usize - 1])
        }
    }

    fn rowid(&self) -> Result<i64> {
        Ok(self.pos as i64)
    }
}

pub fn lit_modules_in(sql: &str) -> Vec<String> {
    let mut modules = Vec::new();
    let mut rest = sql;
    while let Some(start) = rest.find("\"$$lit_") {
        let after_open = &rest[start + 1..];
        if let Some(end) = after_open.find('"') {
            let name = &after_open[..end];
            if parse_module(name.as_bytes()).is_some() && !modules.contains(&name.to_string()) {
                modules.push(name.to_string());
            }
            rest = &after_open[end + 1..];
        } else {
            break;
        }
    }
    modules
}

pub fn lit_calls_in(sql: &str) -> Vec<(String, String)> {
    let mut calls = Vec::new();
    let mut rest = sql;
    while let Some(start) = rest.find("\"$$lit_") {
        let after_open = &rest[start + 1..];
        let Some(end) = after_open.find('"') else {
            break;
        };
        let module = &after_open[..end];
        let after_module = &after_open[end + 1..];
        let Some(paren) = after_module.find("('") else {
            rest = after_module;
            continue;
        };
        let after_paren = &after_module[paren + 2..];
        let Some(close) = after_paren.find("')") else {
            rest = after_paren;
            continue;
        };
        let id = &after_paren[..close];
        if parse_module(module.as_bytes()).is_some() {
            calls.push((module.to_string(), id.to_string()));
        }
        rest = &after_paren[close + 2..];
    }
    calls
}

pub fn register_lit_modules(conn: &Connection, modules: &[String], state: LitState) -> Result<()> {
    for module in modules {
        conn.create_module(module.as_str(), &LIT_MODULE, Some(state.clone()))?;
    }
    Ok(())
}
