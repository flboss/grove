use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use grove_schema::validated::{
    EnumId, EnumUnderlying, EnumValue, Field, FieldId, Relation, RelationId, ScalarType,
    StorageTable, StructId, ValidatedSchema, ValueType,
};
use rusqlite::Connection;
use rusqlite::Statement;
use rusqlite::config::DbConfig;
use rusqlite::params_from_iter;
use rusqlite::types::{Value, ValueRef};

use crate::codec;
use crate::error::ExecError;
use crate::pushdown::{CmpOp, FetchConstraints, PushExpr, StringOp};
use crate::storage::{Key, Row, Storage};
use crate::value::{Scalar, Val};

pub struct SqliteBackend<'a> {
    conn: Connection,
    schema: &'a ValidatedSchema,
    sql_cache: RefCell<HashMap<u64, Vec<SqlCacheEntry>>>,
}

#[derive(Debug, Clone, PartialEq)]
enum SqlCacheKey {
    Fetch {
        struct_id: StructId,
        fields: Vec<FieldId>,
        constraints: FetchConstraints,
    },
    Related {
        rel: RelationId,
        fetch_from_first: bool,
        fields: Vec<FieldId>,
        constraints: FetchConstraints,
        parents: usize,
    },
}

impl SqlCacheKey {
    fn matches_fetch(
        &self,
        struct_id: StructId,
        fields: &[FieldId],
        constraints: &FetchConstraints,
    ) -> bool {
        matches!(
            self,
            SqlCacheKey::Fetch {
                struct_id: key_struct,
                fields: key_fields,
                constraints: key_constraints,
            } if *key_struct == struct_id
                && key_fields.as_slice() == fields
                && key_constraints == constraints
        )
    }

    fn matches_related(
        &self,
        rel: RelationId,
        fetch_from_first: bool,
        fields: &[FieldId],
        constraints: &FetchConstraints,
        parents: usize,
    ) -> bool {
        matches!(
            self,
            SqlCacheKey::Related {
                rel: key_rel,
                fetch_from_first: key_first,
                fields: key_fields,
                constraints: key_constraints,
                parents: key_parents,
            } if *key_rel == rel
                && *key_first == fetch_from_first
                && key_fields.as_slice() == fields
                && key_constraints == constraints
                && *key_parents == parents
        )
    }
}

#[derive(Debug)]
struct SqlCacheEntry {
    key: SqlCacheKey,
    cached: Rc<CachedSql>,
}

#[derive(Debug)]
struct CachedSql {
    sql: String,
    params: Vec<Value>,
}

fn fingerprint_fetch(
    struct_id: StructId,
    fields: &[FieldId],
    constraints: &FetchConstraints,
) -> u64 {
    let mut state = DefaultHasher::new();
    0u8.hash(&mut state);
    struct_id.hash(&mut state);
    fields.hash(&mut state);
    constraints.hash(&mut state);
    state.finish()
}

fn fingerprint_fetch_related(
    rel: RelationId,
    fetch_from_first: bool,
    fields: &[FieldId],
    constraints: &FetchConstraints,
    parents: usize,
) -> u64 {
    let mut state = DefaultHasher::new();
    1u8.hash(&mut state);
    rel.hash(&mut state);
    fetch_from_first.hash(&mut state);
    fields.hash(&mut state);
    constraints.hash(&mut state);
    parents.hash(&mut state);
    state.finish()
}

impl<'a> SqliteBackend<'a> {
    pub fn new(conn: Connection, schema: &'a ValidatedSchema) -> Result<Self, ExecError> {
        for option in [
            // Disable quoted identifiers degrading to string literals in some cases
            DbConfig::SQLITE_DBCONFIG_DQS_DML,
            DbConfig::SQLITE_DBCONFIG_DQS_DDL,
        ] {
            conn.set_db_config(option, false)
                .map_err(|err| Self::storage_error("disable double-quoted literals", err))?;
        }
        let backend = SqliteBackend {
            conn,
            schema,
            sql_cache: RefCell::new(HashMap::new()),
        };
        backend.verify_schema()?;
        Ok(backend)
    }

    pub fn open_in_memory(schema: &'a ValidatedSchema) -> Result<Self, ExecError> {
        let conn = Connection::open_in_memory()
            .map_err(|err| Self::storage_error("open in-memory database", err))?;
        Self::new(conn, schema)
    }

    fn storage_error(context: &str, err: rusqlite::Error) -> ExecError {
        ExecError::Storage {
            message: format!("{context}: {err}"),
        }
    }

    fn cached_sql(
        &self,
        fingerprint: u64,
        matches: impl Fn(&SqlCacheKey) -> bool,
        build_key: impl FnOnce() -> SqlCacheKey,
        build: impl FnOnce() -> Result<(String, Vec<Value>), ExecError>,
    ) -> Result<Rc<CachedSql>, ExecError> {
        if let Some(cached) = self
            .sql_cache
            .borrow()
            .get(&fingerprint)
            .and_then(|entries| {
                entries
                    .iter()
                    .find(|entry| matches(&entry.key))
                    .map(|entry| Rc::clone(&entry.cached))
            })
        {
            return Ok(cached);
        }
        let (sql, params) = build()?;
        let cached = Rc::new(CachedSql { sql, params });
        self.sql_cache
            .borrow_mut()
            .entry(fingerprint)
            .or_default()
            .push(SqlCacheEntry {
                key: build_key(),
                cached: Rc::clone(&cached),
            });
        Ok(cached)
    }

    /// Verify the live database matches the schema
    fn verify_schema(&self) -> Result<(), ExecError> {
        if rusqlite::version_number() < 3_037_000 {
            return Err(ExecError::Storage {
                message: "SQLite >= 3.37 required for schema verification".to_string(),
            });
        }
        let ctx = "verify database schema";
        let mut tables = self
            .conn
            .prepare("SELECT name, type, wr FROM pragma_table_list WHERE schema = 'main'")
            .map_err(|err| Self::storage_error(ctx, err))?;
        let live: Vec<(String, String, i64)> = tables
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|err| Self::storage_error(ctx, err))?
            .collect::<Result<_, _>>()
            .map_err(|err| Self::storage_error(ctx, err))?;
        for table in &self.schema.tables {
            let found = live
                .iter()
                .find(|(name, _, _)| name.eq_ignore_ascii_case(&table.name));
            let Some((_, kind, without_rowid)) = found else {
                return Err(ExecError::Storage {
                    message: format!("unknown table `{}`", table.name),
                });
            };
            if kind != "table" {
                return Err(ExecError::Storage {
                    message: format!("`{}` is a {kind}, rowid reads need a table", table.name),
                });
            }
            if *without_rowid != 0 {
                return Err(ExecError::Storage {
                    message: format!("table `{}` is WITHOUT ROWID", table.name),
                });
            }
            let mut columns = self
                .conn
                .prepare("SELECT name FROM pragma_table_xinfo(?)")
                .map_err(|err| Self::storage_error(ctx, err))?;
            let live_cols: Vec<String> = columns
                .query_map(
                    params_from_iter(std::iter::once(Value::Text(table.name.clone()))),
                    |row| row.get(0),
                )
                .map_err(|err| Self::storage_error(ctx, err))?
                .collect::<Result<_, _>>()
                .map_err(|err| Self::storage_error(ctx, err))?;
            for column in &table.columns {
                if !live_cols
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&column.name))
                {
                    return Err(ExecError::Storage {
                        message: format!(
                            "table `{}` is missing column `{}`",
                            table.name, column.name
                        ),
                    });
                }
            }
            if live_cols
                .iter()
                .any(|name| name.eq_ignore_ascii_case("_rowid_"))
            {
                return Err(ExecError::Storage {
                    message: format!(
                        "table `{}` has a column named `_rowid_`, shadowing the rowid alias",
                        table.name
                    ),
                });
            }
        }
        Ok(())
    }
}

fn stored_class(value: &ValueRef<'_>) -> &'static str {
    match value {
        ValueRef::Null => "NULL",
        ValueRef::Integer(_) => "INTEGER",
        ValueRef::Real(_) => "REAL",
        ValueRef::Text(_) => "TEXT",
        ValueRef::Blob(_) => "BLOB",
    }
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Encode a scalar literal as a SQLite value
fn scalar_value(value: &Scalar) -> Value {
    match value {
        Scalar::Int(n) => Value::Integer(*n),
        Scalar::Float(x) => Value::Real(*x),
        Scalar::Dec(d) => Value::Blob(Vec::from(codec::encode_dec(*d))),
        Scalar::String(s) => Value::Text(s.clone()),
        Scalar::Bytes(bytes) => Value::Blob(bytes.clone()),
        Scalar::Bool(b) => Value::Integer(if *b { 1 } else { 0 }),
        Scalar::Instant(t) => Value::Blob(Vec::from(codec::encode_instant(*t))),
        Scalar::Duration(d) => Value::Blob(Vec::from(codec::encode_duration(*d))),
    }
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// An owned link-key value for grouping array elements by parent row
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum LinkKey {
    Null,
    Int(i64),
    Real(u64),
    Text(String),
    Blob(Vec<u8>),
}

impl LinkKey {
    fn from_value(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => LinkKey::Null,
            ValueRef::Integer(n) => LinkKey::Int(n),
            ValueRef::Real(x) => LinkKey::Real(x.to_bits()),
            ValueRef::Text(bytes) => LinkKey::Text(String::from_utf8_lossy(bytes).into_owned()),
            ValueRef::Blob(bytes) => LinkKey::Blob(bytes.to_vec()),
        }
    }

    fn to_value(&self) -> Value {
        match self {
            LinkKey::Null => Value::Null,
            LinkKey::Int(n) => Value::Integer(*n),
            LinkKey::Real(bits) => Value::Real(f64::from_bits(*bits)),
            LinkKey::Text(s) => Value::Text(s.clone()),
            LinkKey::Blob(bytes) => Value::Blob(bytes.clone()),
        }
    }
}

/// Scalar leaves under a `ValueType`, depth-first left-to-right to match
/// the validated `columns` order
fn leaf_count(ty: &ValueType) -> usize {
    match ty {
        ValueType::Scalar(_) | ValueType::Enum(_) => 1,
        ValueType::Tuple(elements) => elements.iter().map(leaf_count).sum(),
        ValueType::Optional(inner) => leaf_count(inner),
        ValueType::Array { .. } => 0,
    }
}

/// Decode one node from the head of `vals`; returns value, columns consumed
fn decode_node(
    ty: &ValueType,
    vals: &[ValueRef<'_>],
    float_checks: bool,
    schema: &ValidatedSchema,
) -> Result<(Val, usize), ExecError> {
    match ty {
        ValueType::Scalar(scalar) => {
            decode_scalar(*scalar, vals[0], false, float_checks).map(|v| (v, 1))
        }
        ValueType::Enum(id) => decode_enum(*id, vals[0], schema).map(|v| (v, 1)),
        ValueType::Optional(inner) => {
            let len = leaf_count(inner);
            if vals[..len].iter().all(|v| matches!(v, ValueRef::Null)) {
                Ok((Val::None, len))
            } else {
                decode_node(inner, vals, float_checks, schema)
                    .map(|(v, n)| (Val::Some(Box::new(v)), n))
            }
        }
        ValueType::Tuple(elements) => {
            let mut items = Vec::with_capacity(elements.len());
            let mut used = 0;
            for element in elements {
                let (v, n) = decode_node(element, &vals[used..], float_checks, schema)?;
                items.push(v);
                used += n;
            }
            Ok((Val::Tuple(items), used))
        }
        ValueType::Array { .. } => unreachable!("array reached scalar decode"),
    }
}

fn decode_enum(
    enum_id: EnumId,
    value: ValueRef<'_>,
    schema: &ValidatedSchema,
) -> Result<Val, ExecError> {
    let enum_def = schema
        .enums
        .get(enum_id.index())
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown enum id {} (internal error)", enum_id.index()),
        })?;
    let invalid = |found: &str| ExecError::Integrity {
        message: format!("invalid value {found} for enum `{}`", enum_def.name),
    };
    let index = match (enum_def.underlying, value) {
        (EnumUnderlying::Int, ValueRef::Integer(n)) => enum_def
            .variants
            .iter()
            .position(|v| matches!(v.value, EnumValue::Int(m) if m == n))
            .ok_or_else(|| invalid(&n.to_string()))?,
        (EnumUnderlying::String, ValueRef::Text(bytes)) => {
            let text = str::from_utf8(bytes).map_err(|_| ExecError::Integrity {
                message: "expected String, found non-UTF8 TEXT".to_string(),
            })?;
            enum_def
                .variants
                .iter()
                .position(|v| matches!(&v.value, EnumValue::String(s) if s == text))
                .ok_or_else(|| invalid(&format!("{text:?}")))?
        }
        (underlying, other) => {
            return Err(ExecError::Integrity {
                message: format!(
                    "expected {:?}, found {}",
                    underlying.scalar_type(),
                    stored_class(&other)
                ),
            });
        }
    };
    Ok(Val::Enum(enum_id, index))
}

struct ValueSlot<'a> {
    label: String,
    ty: &'a ValueType,
    start: usize,
    len: usize,
}

struct ArraySlot<'conn> {
    label: String,
    link_idx: usize,
    nodes: Vec<ArrayNode>,
    stmts: Vec<Statement<'conn>>,
    subtree: Vec<usize>,
}

struct ValueArraySlot<'conn> {
    label: String,
    start: usize,
    len: usize,
    plan: ElemPlan,
    nodes: Vec<ArrayNode>,
    stmts: Vec<Statement<'conn>>,
    subtrees: Vec<(usize, Vec<usize>)>,
}

enum Slot<'a, 'conn> {
    Value(ValueSlot<'a>),
    Array(ArraySlot<'conn>),
    ValueArray(ValueArraySlot<'conn>),
}

/// One array level in an element type tree
struct ArrayNode {
    plan: ElemPlan,
    width: usize,
    parent: Option<usize>,
}

/// Decode plan mirroring an array element type
#[derive(Debug, Clone)]
enum ElemPlan {
    Scalar(ScalarType),
    #[allow(dead_code)] // TODO: enums
    Enum(EnumId),
    Tuple(Vec<ElemPlan>),
    Optional(Box<ElemPlan>),
    Link(usize),
}

struct NodeDraft<'a> {
    storage: &'a StorageTable,
    parent: Option<(usize, usize)>,
    plan: Option<ElemPlan>,
}

/// Walk a (element) type, registering nested arrays as nodes; returns the
/// decode plan. `counter` tracks value columns of the current level
/// Top-level arrays (no parent node) scope `key = ?` on their own link
fn walk_element<'a>(
    ty: &'a ValueType,
    self_node: Option<usize>,
    drafts: &mut Vec<NodeDraft<'a>>,
    counter: &mut usize,
) -> ElemPlan {
    match ty {
        ValueType::Scalar(scalar) => {
            *counter += 1;
            ElemPlan::Scalar(*scalar)
        }
        ValueType::Enum(id) => {
            *counter += 1;
            ElemPlan::Enum(*id)
        }
        ValueType::Tuple(elements) => ElemPlan::Tuple(
            elements
                .iter()
                .map(|e| walk_element(e, self_node, drafts, counter))
                .collect(),
        ),
        ValueType::Optional(inner) => {
            ElemPlan::Optional(Box::new(walk_element(inner, self_node, drafts, counter)))
        }
        ValueType::Array { element, storage } => {
            let pos = *counter;
            *counter += 1;
            let idx = drafts.len();
            drafts.push(NodeDraft {
                storage,
                parent: self_node.map(|n| (n, pos)),
                plan: None,
            });
            let mut sub = 0;
            let plan = walk_element(element, Some(idx), drafts, &mut sub);
            drafts[idx].plan = Some(plan);
            ElemPlan::Link(idx)
        }
    }
}

fn plan_width(plan: &ElemPlan) -> usize {
    match plan {
        ElemPlan::Scalar(_) | ElemPlan::Enum(_) | ElemPlan::Link(_) => 1,
        ElemPlan::Tuple(elements) => elements.iter().map(plan_width).sum(),
        ElemPlan::Optional(inner) => plan_width(inner),
    }
}

/// Scope predicate tying a node's rows to the parent row's top link
fn node_scope(
    idx: usize,
    drafts: &[NodeDraft<'_>],
    schema: &ValidatedSchema,
) -> Result<String, ExecError> {
    let alias = format!("g$a{idx}");
    let key = format!(
        "{alias}.{}",
        quote(schema.column_name(drafts[idx].storage.key))
    );
    match drafts[idx].parent {
        None => Ok(format!("{key} = ?")),
        Some((parent, pos)) => {
            let palias = format!("g$a{parent}");
            let ptable = quote(&schema.tables[drafts[parent].storage.table.index()].name);
            let pcol =
                drafts[parent]
                    .storage
                    .value
                    .get(pos)
                    .ok_or_else(|| ExecError::Integrity {
                        message: format!("array storage for `{ptable}` names no column {pos}"),
                    })?;
            let plink = format!("{palias}.{}", quote(schema.column_name(*pcol)));
            let pscope = node_scope(parent, drafts, schema)?;
            Ok(format!(
                "{key} IN (SELECT {plink} FROM {ptable} AS {palias} WHERE {pscope})"
            ))
        }
    }
}

fn node_sql(
    idx: usize,
    drafts: &[NodeDraft<'_>],
    schema: &ValidatedSchema,
) -> Result<String, ExecError> {
    let draft = &drafts[idx];
    let alias = format!("g$a{idx}");
    let table = quote(&schema.tables[draft.storage.table.index()].name);
    let key = format!("{alias}.{}", quote(schema.column_name(draft.storage.key)));
    let vals: Vec<String> = draft
        .storage
        .value
        .iter()
        .map(|col| format!("{alias}.{}", quote(schema.column_name(*col))))
        .collect();
    let scope = node_scope(idx, drafts, schema)?;
    Ok(format!(
        "SELECT {key}, {} FROM {table} AS {alias} WHERE {scope} ORDER BY _rowid_",
        vals.join(", ")
    ))
}

/// A decoded level row holding finished values or links into deeper nodes
enum SemiVal {
    Done(Val),
    Tuple(Vec<SemiVal>),
    Some(Box<SemiVal>),
    Deep { node: usize, key: LinkKey },
}

/// Decode one level row
fn semi(
    plan: &ElemPlan,
    cells: &[ValueRef<'_>],
    float_checks: bool,
    schema: &ValidatedSchema,
) -> Result<(SemiVal, usize), ExecError> {
    match plan {
        ElemPlan::Scalar(scalar) => {
            decode_scalar(*scalar, cells[0], false, float_checks).map(|v| (SemiVal::Done(v), 1))
        }
        ElemPlan::Enum(id) => decode_enum(*id, cells[0], schema).map(|v| (SemiVal::Done(v), 1)),
        ElemPlan::Tuple(plans) => {
            let mut items = Vec::with_capacity(plans.len());
            let mut used = 0;
            for plan in plans {
                let (v, n) = semi(plan, &cells[used..], float_checks, schema)?;
                items.push(v);
                used += n;
            }
            Ok((SemiVal::Tuple(items), used))
        }
        ElemPlan::Optional(inner) => {
            let width = plan_width(inner);
            if cells[..width].iter().all(|v| matches!(v, ValueRef::Null)) {
                Ok((SemiVal::Done(Val::None), width))
            } else {
                semi(inner, cells, float_checks, schema).map(|(s, n)| match s {
                    SemiVal::Done(v) => (SemiVal::Done(Val::Some(Box::new(v))), n),
                    other => (SemiVal::Some(Box::new(other)), n),
                })
            }
        }
        ElemPlan::Link(node) => {
            if matches!(cells[0], ValueRef::Null) {
                Ok((SemiVal::Done(Val::List(Vec::new())), 1))
            } else {
                Ok((
                    SemiVal::Deep {
                        node: *node,
                        key: LinkKey::from_value(cells[0]),
                    },
                    1,
                ))
            }
        }
    }
}

/// Resolve links against already-finalized deeper groups, keyed by node
fn finalize(semi: SemiVal, groups: &HashMap<usize, HashMap<LinkKey, Vec<Val>>>) -> Val {
    match semi {
        SemiVal::Done(v) => v,
        SemiVal::Tuple(items) => {
            Val::Tuple(items.into_iter().map(|s| finalize(s, groups)).collect())
        }
        SemiVal::Some(inner) => Val::Some(Box::new(finalize(*inner, groups))),
        SemiVal::Deep { node, key } => Val::List(
            groups
                .get(&node)
                .and_then(|g| g.get(&key))
                .cloned()
                .unwrap_or_default(),
        ),
    }
}

/// Partition nodes into per-root subtrees (root, ascending members)
fn split_roots(nodes: &[ArrayNode]) -> Vec<(usize, Vec<usize>)> {
    fn root_of(nodes: &[ArrayNode], mut n: usize) -> usize {
        while let Some(parent) = nodes[n].parent {
            n = parent;
        }
        n
    }
    let mut pos: HashMap<usize, usize> = HashMap::new();
    let mut subtrees: Vec<(usize, Vec<usize>)> = Vec::new();
    for i in 0..nodes.len() {
        let root = root_of(nodes, i);
        let parent = *pos.entry(root).or_insert_with(|| {
            subtrees.push((root, Vec::new()));
            subtrees.len() - 1
        });
        subtrees[parent].1.push(i);
    }
    subtrees
}

/// Find a root's link in a decoded field template
fn find_link(tree: &SemiVal, root: usize) -> Option<LinkKey> {
    match tree {
        SemiVal::Done(_) => None,
        SemiVal::Tuple(items) => items.iter().find_map(|s| find_link(s, root)),
        SemiVal::Some(inner) => find_link(inner, root),
        SemiVal::Deep { node, key } => (*node == root).then(|| key.clone()),
    }
}

/// Splice resolved root lists back into a decoded field template
fn splice(tree: SemiVal, resolved: &mut HashMap<usize, Val>) -> Val {
    match tree {
        SemiVal::Done(v) => v,
        SemiVal::Tuple(items) => {
            Val::Tuple(items.into_iter().map(|s| splice(s, resolved)).collect())
        }
        SemiVal::Some(inner) => Val::Some(Box::new(splice(*inner, resolved))),
        SemiVal::Deep { node, .. } => resolved.remove(&node).unwrap_or(Val::List(Vec::new())),
    }
}

/// Decode one scalar cell
fn decode_scalar(
    ty: ScalarType,
    value: ValueRef<'_>,
    nullable: bool,
    float_checks: bool,
) -> Result<Val, ExecError> {
    let mismatch = |found: &str| ExecError::Integrity {
        message: format!("expected {ty:?}, found {found}"),
    };
    match (ty, value) {
        (_, ValueRef::Null) if nullable => Ok(Val::None),
        // SQLite stores NaN as NULL; decoding prefers absence over NaN if nullable
        (ScalarType::Float, ValueRef::Null) => {
            if float_checks {
                Err(ExecError::Integrity {
                    message: "found non-finite float: NaN".to_string(),
                })
            } else {
                Ok(Val::Scalar(Scalar::Float(f64::NAN)))
            }
        }
        (ScalarType::Int, ValueRef::Integer(n)) => Ok(Val::Scalar(Scalar::Int(n))),
        (ScalarType::Float, ValueRef::Real(x)) => Ok(Val::Scalar(Scalar::Float(x))),
        (ScalarType::String, ValueRef::Text(bytes)) => String::from_utf8(bytes.to_vec())
            .map(|s| Val::Scalar(Scalar::String(s)))
            .map_err(|_| mismatch("non-UTF8 TEXT")),
        (ScalarType::Bytes, ValueRef::Blob(blob)) => Ok(Val::Scalar(Scalar::Bytes(blob.to_vec()))),
        (ScalarType::Bool, ValueRef::Integer(0)) => Ok(Val::Scalar(Scalar::Bool(false))),
        (ScalarType::Bool, ValueRef::Integer(1)) => Ok(Val::Scalar(Scalar::Bool(true))),
        (ScalarType::Dec, ValueRef::Blob(blob)) => {
            codec::decode_dec(blob).map(|d| Val::Scalar(Scalar::Dec(d)))
        }
        (ScalarType::Instant, ValueRef::Blob(blob)) => {
            codec::decode_instant(blob).map(|t| Val::Scalar(Scalar::Instant(t)))
        }
        (ScalarType::Duration, ValueRef::Blob(blob)) => {
            codec::decode_duration(blob).map(|d| Val::Scalar(Scalar::Duration(d)))
        }
        (_, other) => Err(mismatch(stored_class(&other))),
    }
}

impl SqliteBackend<'_> {
    /// Build decode plans and prepare one statement per array node in `drafts`
    fn prepare_array_nodes(
        &self,
        drafts: &[NodeDraft<'_>],
        label: &str,
        ctx: &str,
    ) -> Result<(Vec<ArrayNode>, Vec<Statement<'_>>), ExecError> {
        let mut nodes = Vec::with_capacity(drafts.len());
        let mut stmts = Vec::with_capacity(drafts.len());
        for (idx, draft) in drafts.iter().enumerate() {
            let sql = node_sql(idx, drafts, self.schema).map_err(|err| err.context(label))?;
            stmts.push(
                self.conn
                    .prepare(&sql)
                    .map_err(|err| Self::storage_error(ctx, err))?,
            );
            let plan = draft.plan.clone().expect("walk fills every draft");
            nodes.push(ArrayNode {
                width: plan_width(&plan),
                plan,
                parent: draft.parent.map(|(p, _)| p),
            });
        }
        Ok((nodes, stmts))
    }

    /// Run one root's subtree bound to a single link; root rows in order
    fn run_subtree(
        &self,
        nodes: &[ArrayNode],
        stmts: &mut [Statement<'_>],
        sub: &[usize],
        key: Value,
        label: &str,
        ctx: &str,
    ) -> Result<Vec<Val>, ExecError> {
        let Some(&root) = sub.first() else {
            return Ok(Vec::new());
        };
        let mut groups: HashMap<usize, HashMap<LinkKey, Vec<Val>>> = HashMap::new();
        let mut items = Vec::new();
        for &n in sub.iter().rev() {
            let mut srows = stmts[n]
                .query(params_from_iter(std::iter::once(key.clone())))
                .map_err(|err| Self::storage_error(ctx, err))?;
            while let Some(srow) = srows.next().map_err(|err| Self::storage_error(ctx, err))? {
                let link = LinkKey::from_value(
                    srow.get_ref(0)
                        .map_err(|err| Self::storage_error(ctx, err))?,
                );
                let width = nodes[n].width;
                let mut cells = Vec::with_capacity(width);
                for i in 0..width {
                    cells.push(
                        srow.get_ref(1 + i)
                            .map_err(|err| Self::storage_error(ctx, err))?,
                    );
                }
                let (semival, _) = semi(
                    &nodes[n].plan,
                    &cells,
                    self.schema.config.float_checks,
                    self.schema,
                )
                .map_err(|err| err.context(label))?;
                let val = finalize(semival, &groups);
                if n == root {
                    items.push(val);
                } else {
                    groups
                        .entry(n)
                        .or_default()
                        .entry(link)
                        .or_default()
                        .push(val);
                }
            }
        }
        Ok(items)
    }

    /// Validate a pushdown field and resolve its quoted column and scalar type
    fn pushdown_field(
        &self,
        struct_id: StructId,
        field: FieldId,
    ) -> Result<(String, ScalarType), ExecError> {
        if field.struct_id() != struct_id {
            return Err(ExecError::Integrity {
                message: format!(
                    "pushdown field {:?} does not belong to struct {} (internal error)",
                    field,
                    struct_id.index()
                ),
            });
        }
        let struct_def =
            self.schema
                .structs
                .get(struct_id.index())
                .ok_or_else(|| ExecError::Integrity {
                    message: format!("unknown struct id {} (internal error)", struct_id.index()),
                })?;
        match struct_def.fields.get(field.local()) {
            Some(Field::Value {
                ty: ValueType::Scalar(scalar),
                columns,
                ..
            }) if columns.len() == 1 => Ok((quote(self.schema.column_name(columns[0])), *scalar)),
            Some(Field::Value {
                ty: ValueType::Enum(id),
                columns,
                ..
            }) if columns.len() == 1 => {
                let enum_def =
                    self.schema
                        .enums
                        .get(id.index())
                        .ok_or_else(|| ExecError::Integrity {
                            message: format!("unknown enum id {} (internal error)", id.index()),
                        })?;
                Ok((
                    quote(self.schema.column_name(columns[0])),
                    enum_def.underlying.scalar_type(),
                ))
            }
            Some(other) => Err(ExecError::Integrity {
                message: format!(
                    "pushdown field `{}.{}` is not a single-column scalar (internal error)",
                    struct_def.name,
                    other.name()
                ),
            }),
            None => Err(ExecError::Integrity {
                message: format!("unknown field {field:?} (internal error)"),
            }),
        }
    }

    /// Render a pushdown expression
    fn push_expr_sql(
        &self,
        struct_id: StructId,
        expr: &PushExpr,
        qualifier: &str,
        params: &mut Vec<Value>,
    ) -> Result<String, ExecError> {
        match expr {
            PushExpr::Field(field) => {
                let (column, scalar) = self.pushdown_field(struct_id, *field)?;
                if scalar == ScalarType::Bool {
                    // TODO: reject non-0/1?
                    Ok(format!("({qualifier}.{column} = 1)"))
                } else {
                    Ok(format!("{qualifier}.{column}"))
                }
            }
            PushExpr::Literal(value) => {
                params.push(scalar_value(value));
                Ok("?".to_string())
            }
            PushExpr::Cmp { op, lhs, rhs } => {
                let lhs = self.push_expr_sql(struct_id, lhs, qualifier, params)?;
                let rhs = self.push_expr_sql(struct_id, rhs, qualifier, params)?;
                let op = match op {
                    CmpOp::Eq => "=",
                    CmpOp::Ne => "<>",
                    CmpOp::Lt => "<",
                    CmpOp::Le => "<=",
                    CmpOp::Gt => ">",
                    CmpOp::Ge => ">=",
                };
                Ok(format!("({lhs} {op} {rhs})"))
            }
            PushExpr::And(lhs, rhs) => {
                let lhs = self.push_expr_sql(struct_id, lhs, qualifier, params)?;
                let rhs = self.push_expr_sql(struct_id, rhs, qualifier, params)?;
                Ok(format!("({lhs} AND {rhs})"))
            }
            PushExpr::Or(lhs, rhs) => {
                let lhs = self.push_expr_sql(struct_id, lhs, qualifier, params)?;
                let rhs = self.push_expr_sql(struct_id, rhs, qualifier, params)?;
                Ok(format!("({lhs} OR {rhs})"))
            }
            PushExpr::Not(inner) => {
                let inner = self.push_expr_sql(struct_id, inner, qualifier, params)?;
                Ok(format!("(NOT {inner})"))
            }
            PushExpr::In { lhs, items } => {
                let lhs = self.push_expr_sql(struct_id, lhs, qualifier, params)?;
                let mut rendered = Vec::with_capacity(items.len());
                for item in items {
                    rendered.push(self.push_expr_sql(struct_id, item, qualifier, params)?);
                }
                Ok(format!("({lhs} IN ({}))", rendered.join(", ")))
            }
            PushExpr::StringOp { op, base, needle } => match op {
                StringOp::Contains => {
                    let base = self.push_expr_sql(struct_id, base, qualifier, params)?;
                    let needle = self.push_expr_sql(struct_id, needle, qualifier, params)?;
                    Ok(format!("(instr({base}, {needle}) > 0)"))
                }
                StringOp::StartsWith => {
                    let base = self.push_expr_sql(struct_id, base, qualifier, params)?;
                    let needle = self.push_expr_sql(struct_id, needle, qualifier, params)?;
                    Ok(format!("(instr({base}, {needle}) = 1)"))
                }
                StringOp::EndsWith => {
                    let needle_len = self.push_expr_sql(struct_id, needle, qualifier, params)?;
                    let base = self.push_expr_sql(struct_id, base, qualifier, params)?;
                    let needle_len2 = self.push_expr_sql(struct_id, needle, qualifier, params)?;
                    let needle = self.push_expr_sql(struct_id, needle, qualifier, params)?;
                    Ok(format!(
                        "(length({needle_len}) = 0 OR \
                        substr({base}, -length({needle_len2})) = {needle})"
                    ))
                }
            },
            PushExpr::StringLen(inner) => {
                let inner = self.push_expr_sql(struct_id, inner, qualifier, params)?;
                Ok(format!("(length({inner}))"))
            }
        }
    }

    /// Plan field slots, appending qualified columns to `select`
    fn plan_fields(
        &self,
        struct_id: StructId,
        fields: &[FieldId],
        qualifier: &str,
        select: &mut Vec<String>,
        ctx: &str,
    ) -> Result<Vec<Slot<'_, '_>>, ExecError> {
        let struct_def =
            self.schema
                .structs
                .get(struct_id.index())
                .ok_or_else(|| ExecError::Integrity {
                    message: format!("unknown struct id {}", struct_id.index()),
                })?;
        let struct_name = &struct_def.name;
        let mut slots = Vec::with_capacity(fields.len());
        for field_id in fields {
            if field_id.struct_id() != struct_id {
                return Err(ExecError::Integrity {
                    message: format!("fetch field {field_id:?} belongs to another struct"),
                });
            }
            let field =
                struct_def
                    .fields
                    .get(field_id.local())
                    .ok_or_else(|| ExecError::Integrity {
                        message: format!("unknown field {field_id:?} on struct `{struct_name}`"),
                    })?;
            match field {
                Field::Value { name, ty, columns } => {
                    let start = select.len();
                    for col in columns {
                        select.push(format!(
                            "{qualifier}.{}",
                            quote(self.schema.column_name(*col))
                        ));
                    }
                    let label = format!("{struct_name}.{name}");
                    if ty.has_array() {
                        let mut drafts = Vec::new();
                        let mut counter = 0;
                        let plan = walk_element(ty, None, &mut drafts, &mut counter);
                        let (nodes, stmts) = self.prepare_array_nodes(&drafts, &label, ctx)?;
                        let subtrees = split_roots(&nodes);
                        slots.push(Slot::ValueArray(ValueArraySlot {
                            label,
                            start,
                            len: columns.len(),
                            plan,
                            nodes,
                            stmts,
                            subtrees,
                        }));
                    } else {
                        slots.push(Slot::Value(ValueSlot {
                            label,
                            ty,
                            start,
                            len: columns.len(),
                        }));
                    }
                }
                Field::Array {
                    name,
                    element,
                    link,
                    storage,
                } => {
                    let link_idx = select.len();
                    select.push(format!(
                        "{qualifier}.{}",
                        quote(self.schema.column_name(*link))
                    ));
                    let label = format!("{struct_name}.{name}");
                    let mut drafts = vec![NodeDraft {
                        storage,
                        parent: None,
                        plan: None,
                    }];
                    let mut counter = 0;
                    let plan = walk_element(element, Some(0), &mut drafts, &mut counter);
                    drafts[0].plan = Some(plan);
                    let (nodes, stmts) = self.prepare_array_nodes(&drafts, &label, ctx)?;
                    let subtree: Vec<usize> = (0..nodes.len()).collect();
                    slots.push(Slot::Array(ArraySlot {
                        label,
                        link_idx,
                        nodes,
                        stmts,
                        subtree,
                    }));
                }
                Field::Ref { name, .. } => {
                    return Err(ExecError::Integrity {
                        message: format!(
                            "cannot fetch Ref field `{struct_name}.{name}`: navigation uses fetch_related"
                        ),
                    });
                }
            }
        }
        Ok(slots)
    }

    /// Decode one storage row at the planned columns
    fn decode_row(
        &self,
        slots: &mut [Slot<'_, '_>],
        row: &rusqlite::Row<'_>,
        ctx: &str,
    ) -> Result<(Key, Vec<Val>), ExecError> {
        let rowid: i64 = row.get(0).map_err(|err| Self::storage_error(ctx, err))?;
        let float_checks = self.schema.config.float_checks;
        let mut values = Vec::with_capacity(slots.len());
        for slot in slots.iter_mut() {
            match slot {
                Slot::Value(slot) => {
                    let mut cells = Vec::with_capacity(slot.len);
                    for i in 0..slot.len {
                        cells.push(
                            row.get_ref(slot.start + i)
                                .map_err(|err| Self::storage_error(ctx, err))?,
                        );
                    }
                    let (val, _) = decode_node(slot.ty, &cells, float_checks, self.schema)
                        .map_err(|err| err.context(&slot.label))?;
                    values.push(val);
                }
                Slot::Array(slot) => {
                    let link = row
                        .get_ref(slot.link_idx)
                        .map_err(|err| Self::storage_error(ctx, err))?;
                    let mut items = Vec::new();
                    if !matches!(link, ValueRef::Null) {
                        let top = LinkKey::from_value(link).to_value();
                        items = self.run_subtree(
                            &slot.nodes,
                            &mut slot.stmts,
                            &slot.subtree,
                            top,
                            &slot.label,
                            ctx,
                        )?;
                    }
                    values.push(Val::List(items));
                }
                Slot::ValueArray(slot) => {
                    let mut cells = Vec::with_capacity(slot.len);
                    for i in 0..slot.len {
                        cells.push(
                            row.get_ref(slot.start + i)
                                .map_err(|err| Self::storage_error(ctx, err))?,
                        );
                    }
                    let (tree, _) = semi(&slot.plan, &cells, float_checks, self.schema)
                        .map_err(|err| err.context(&slot.label))?;
                    let mut resolved: HashMap<usize, Val> = HashMap::new();
                    for (root, sub) in &slot.subtrees {
                        if let Some(key) = find_link(&tree, *root) {
                            let items = self.run_subtree(
                                &slot.nodes,
                                &mut slot.stmts,
                                sub,
                                key.to_value(),
                                &slot.label,
                                ctx,
                            )?;
                            resolved.insert(*root, Val::List(items));
                        }
                    }
                    values.push(splice(tree, &mut resolved));
                }
            }
        }
        Ok((Key(rowid), values))
    }
}

impl Storage for SqliteBackend<'_> {
    fn fetch(
        &self,
        struct_id: StructId,
        fields: &[FieldId],
        constraints: &FetchConstraints,
    ) -> Result<Vec<Row>, ExecError> {
        let struct_def =
            self.schema
                .structs
                .get(struct_id.index())
                .ok_or_else(|| ExecError::Integrity {
                    message: format!("unknown struct id {}", struct_id.index()),
                })?;
        let struct_name = &struct_def.name;
        let table = quote(&self.schema.tables[struct_def.table.index()].name);
        let ctx = format!("fetch {struct_name}");
        let mut select = vec!["_rowid_".to_string()];
        let mut slots = self.plan_fields(struct_id, fields, &table, &mut select, &ctx)?;
        let fingerprint = fingerprint_fetch(struct_id, fields, constraints);
        let cached = self.cached_sql(
            fingerprint,
            |key| key.matches_fetch(struct_id, fields, constraints),
            || SqlCacheKey::Fetch {
                struct_id,
                fields: fields.to_vec(),
                constraints: constraints.clone(),
            },
            || {
                let mut params = Vec::new();
                let mut filters = Vec::with_capacity(constraints.filters.len());
                for filter in &constraints.filters {
                    filters.push(self.push_expr_sql(struct_id, filter, &table, &mut params)?);
                }
                let mut sort = Vec::with_capacity(constraints.sort.len() + 1);
                for key in &constraints.sort {
                    let (column, _) = self.pushdown_field(struct_id, key.field)?;
                    let direction = if key.desc { "DESC" } else { "ASC" };
                    sort.push(format!("{table}.{column} {direction}"));
                }
                sort.push("_rowid_ ASC".to_string());
                let mut sql = format!("SELECT {} FROM {table}", select.join(", "));
                if !filters.is_empty() {
                    sql.push_str(" WHERE ");
                    sql.push_str(&filters.join(" AND "));
                }
                sql.push_str(" ORDER BY ");
                sql.push_str(&sort.join(", "));
                if let Some(limit) = constraints.limit {
                    sql.push_str(&format!(" LIMIT {limit}"));
                    if constraints.offset != 0 {
                        sql.push_str(&format!(" OFFSET {}", constraints.offset));
                    }
                } else if constraints.offset != 0 {
                    sql.push_str(&format!(" LIMIT -1 OFFSET {}", constraints.offset));
                }
                Ok((sql, params))
            },
        )?;
        let mut stmt = self
            .conn
            .prepare_cached(&cached.sql)
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut rows = stmt
            .query(params_from_iter(&cached.params))
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|err| Self::storage_error(&ctx, err))? {
            let (rowid, values) = self.decode_row(&mut slots, row, &ctx)?;
            out.push(Row { rowid, values });
        }
        Ok(out)
    }

    fn fetch_related(
        &self,
        rel: RelationId,
        fetch_from_first: bool,
        parents: &[Key],
        fields: &[FieldId],
        constraints: &FetchConstraints,
    ) -> Result<Vec<Vec<Row>>, ExecError> {
        let mut groups: Vec<Vec<Row>> = parents.iter().map(|_| Vec::new()).collect();
        if parents.is_empty() {
            return Ok(groups);
        }
        let schema = &self.schema;
        let relation = schema
            .relations
            .get(rel.index())
            .ok_or_else(|| ExecError::Integrity {
                message: format!("unknown relation id {}", rel.index()),
            })?;
        let fetch_ep = if fetch_from_first {
            relation.endpoints()[0]
        } else {
            relation.endpoints()[1]
        };
        // Parents are rows of the navigated endpoint's struct;
        // fetched rows are the endpoint target
        let parent_struct = fetch_ep.struct_id();
        let parent_def = &schema.structs[parent_struct.index()];
        let (target, field_name) = match parent_def.fields.get(fetch_ep.local()) {
            Some(Field::Ref { target, name, .. }) => (*target, name.clone()),
            _ => {
                return Err(ExecError::Integrity {
                    message: format!("relation endpoint {fetch_ep:?} is not a reference"),
                });
            }
        };
        let ctx = format!("fetch_related {}.{field_name}", parent_def.name);
        let mut select = vec!["g$f._rowid_".to_string()];
        let mut slots = self.plan_fields(target, fields, "g$f", &mut select, &ctx)?;
        let parent_col = select.len();
        let fingerprint =
            fingerprint_fetch_related(rel, fetch_from_first, fields, constraints, parents.len());
        let cached = self.cached_sql(
            fingerprint,
            |key| key.matches_related(rel, fetch_from_first, fields, constraints, parents.len()),
            || SqlCacheKey::Related {
                rel,
                fetch_from_first,
                fields: fields.to_vec(),
                constraints: constraints.clone(),
                parents: parents.len(),
            },
            || {
                let target_def = &schema.structs[target.index()];
                let fetched_table = quote(&schema.tables[target_def.table.index()].name);
                let known_table = quote(&schema.tables[parent_def.table.index()].name);
                let from = match relation {
                    Relation::OneToOne {
                        child_ref, fk, pk, ..
                    }
                    | Relation::ManyToOne {
                        child_ref, fk, pk, ..
                    } => {
                        let fk_col = quote(schema.column_name(*fk));
                        let pk_col = quote(schema.column_name(*pk));
                        let on = if fetch_ep != *child_ref {
                            format!("g$f.{fk_col} = g$k.{pk_col}")
                        } else {
                            format!("g$f.{pk_col} = g$k.{fk_col}")
                        };
                        format!("{fetched_table} AS g$f JOIN {known_table} AS g$k ON {on}")
                    }
                    Relation::OneToMany { b_ref, fk, pk, .. } => {
                        let fk_col = quote(schema.column_name(*fk));
                        let pk_col = quote(schema.column_name(*pk));
                        let on = if fetch_ep != *b_ref {
                            format!("g$f.{fk_col} = g$k.{pk_col}")
                        } else {
                            format!("g$f.{pk_col} = g$k.{fk_col}")
                        };
                        format!("{fetched_table} AS g$f JOIN {known_table} AS g$k ON {on}")
                    }
                    Relation::ManyToMany {
                        a_ref,
                        b_ref: _,
                        join_table,
                        a_col,
                        a_pk,
                        b_col,
                        b_pk,
                    } => {
                        // Endpoints point across (b endpoint's target is the a-side struct),
                        // so fetching through b reads a-side rows
                        let (x_col, x_pk, y_col, y_pk) = if fetch_ep != *a_ref {
                            (a_col, a_pk, b_col, b_pk)
                        } else {
                            (b_col, b_pk, a_col, a_pk)
                        };
                        let join_table = quote(&schema.tables[join_table.index()].name);
                        let x_col = quote(schema.column_name(*x_col));
                        let x_pk = quote(schema.column_name(*x_pk));
                        let y_col = quote(schema.column_name(*y_col));
                        let y_pk = quote(schema.column_name(*y_pk));
                        format!(
                            "{fetched_table} AS g$f \
                         JOIN {join_table} AS g$j ON g$j.{x_col} = g$f.{x_pk} \
                         JOIN {known_table} AS g$k ON g$j.{y_col} = g$k.{y_pk}"
                        )
                    }
                };
                let placeholders = vec!["?"; parents.len()].join(", ");
                let mut params = Vec::new();
                let mut filters = Vec::with_capacity(constraints.filters.len());
                for filter in &constraints.filters {
                    filters.push(self.push_expr_sql(target, filter, "g$f", &mut params)?);
                }
                let mut sort = Vec::with_capacity(constraints.sort.len() + 1);
                for key in &constraints.sort {
                    let (column, _) = self.pushdown_field(target, key.field)?;
                    let direction = if key.desc { "DESC" } else { "ASC" };
                    sort.push(format!("g$f.{column} {direction}"));
                }
                sort.push("g$f._rowid_ ASC".to_string());
                let mut where_clause = format!("g$k._rowid_ IN ({placeholders})");
                if !filters.is_empty() {
                    where_clause.push_str(" AND ");
                    where_clause.push_str(&filters.join(" AND "));
                }
                let caps = constraints.limit.is_some() || constraints.offset != 0;
                let sql = if caps {
                    let mut inner = select;
                    inner.push("g$k._rowid_ AS g$p".to_string());
                    inner.push(format!(
                        "ROW_NUMBER() OVER (PARTITION BY g$k._rowid_ ORDER BY {}) AS g$rn",
                        sort.join(", ")
                    ));
                    let mut outer_where = Vec::new();
                    if constraints.offset != 0 {
                        outer_where.push("g$rn > ?".to_string());
                        params.push(Value::Integer(saturating_i64(constraints.offset)));
                    }
                    if let Some(limit) = constraints.limit {
                        outer_where.push("g$rn <= ?".to_string());
                        params.push(Value::Integer(saturating_i64(
                            constraints.offset.saturating_add(limit),
                        )));
                    }
                    format!(
                        "SELECT * FROM (SELECT {} FROM {from} WHERE {where_clause}) \
                     WHERE {} ORDER BY g$p, g$rn",
                        inner.join(", "),
                        outer_where.join(" AND ")
                    )
                } else {
                    select.push("g$k._rowid_".to_string());
                    format!(
                        "SELECT {} FROM {from} WHERE {where_clause} ORDER BY {}",
                        select.join(", "),
                        sort.join(", ")
                    )
                };
                Ok((sql, params))
            },
        )?;
        let mut bind: Vec<Value> = parents.iter().map(|key| Value::Integer(key.0)).collect();
        bind.extend(cached.params.iter().cloned());
        let mut stmt = self
            .conn
            .prepare_cached(&cached.sql)
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut rows = stmt
            .query(params_from_iter(bind))
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut index: HashMap<Key, Vec<usize>> = HashMap::new();
        for (i, key) in parents.iter().enumerate() {
            index.entry(*key).or_default().push(i);
        }
        while let Some(row) = rows.next().map_err(|err| Self::storage_error(&ctx, err))? {
            let (rowid, values) = self.decode_row(&mut slots, row, &ctx)?;
            let parent: i64 = row
                .get(parent_col)
                .map_err(|err| Self::storage_error(&ctx, err))?;
            if let Some(positions) = index.get(&Key(parent)) {
                if positions.len() == 1 {
                    groups[positions[0]].push(Row { rowid, values });
                } else {
                    for &i in positions {
                        groups[i].push(Row {
                            rowid,
                            values: values.clone(),
                        });
                    }
                }
            }
        }
        Ok(groups)
    }
}

#[cfg(test)]
mod tests {
    use crate::pushdown::SortKey;
    use crate::testdata;

    use super::*;
    use chrono::{DateTime, TimeDelta};
    use rust_decimal::dec;

    #[test]
    fn open_in_memory_constructor() {
        assert!(SqliteBackend::open_in_memory(&ValidatedSchema::default()).is_ok());
    }

    #[test]
    fn decode_int_float_string() {
        assert_eq!(
            decode_scalar(ScalarType::Int, ValueRef::Integer(-7), false, true),
            Ok(Val::Scalar(Scalar::Int(-7)))
        );
        assert_eq!(
            decode_scalar(ScalarType::Float, ValueRef::Real(1.5), false, true),
            Ok(Val::Scalar(Scalar::Float(1.5)))
        );
        assert_eq!(
            decode_scalar(ScalarType::String, ValueRef::Text(b"hi"), false, true),
            Ok(Val::Scalar(Scalar::String("hi".to_string())))
        );
        assert!(decode_scalar(ScalarType::String, ValueRef::Text(b"\xFF"), false, true).is_err());
    }

    #[test]
    fn decode_bool_strict() {
        assert_eq!(
            decode_scalar(ScalarType::Bool, ValueRef::Integer(0), false, true),
            Ok(Val::Scalar(Scalar::Bool(false)))
        );
        assert_eq!(
            decode_scalar(ScalarType::Bool, ValueRef::Integer(1), false, true),
            Ok(Val::Scalar(Scalar::Bool(true)))
        );
        assert!(decode_scalar(ScalarType::Bool, ValueRef::Integer(2), false, true).is_err());
        assert!(decode_scalar(ScalarType::Bool, ValueRef::Real(1.0), false, true).is_err());
    }

    #[test]
    fn decode_null() {
        assert_eq!(
            decode_scalar(ScalarType::Int, ValueRef::Null, true, true),
            Ok(Val::None)
        );
        assert!(decode_scalar(ScalarType::Int, ValueRef::Null, false, true).is_err());
    }

    #[test]
    fn decode_null_float_nan() {
        assert_eq!(
            decode_scalar(ScalarType::Float, ValueRef::Null, true, true),
            Ok(Val::None)
        );
        assert_eq!(
            decode_scalar(ScalarType::Float, ValueRef::Null, true, false),
            Ok(Val::None)
        );
        match decode_scalar(ScalarType::Float, ValueRef::Null, false, false) {
            Ok(Val::Scalar(Scalar::Float(x))) => assert!(x.is_nan()),
            other => panic!("expected NaN, got {other:?}"),
        }
        assert!(decode_scalar(ScalarType::Float, ValueRef::Null, false, true).is_err());
    }

    #[test]
    fn fetch_null_float_checks() {
        use grove_schema::validated::{
            Column, ColumnId, Config, Field, FieldId, Struct, StructId, Table, TableId, ValueType,
        };
        fn schema_with_checks(float_checks: bool) -> ValidatedSchema {
            ValidatedSchema {
                config: Config {
                    float_checks,
                    ..Default::default()
                },
                tables: vec![Table {
                    name: "t".to_string(),
                    columns: vec![Column {
                        name: "x".to_string(),
                        ty: Some(ScalarType::Float),
                    }],
                }],
                structs: vec![Struct {
                    name: "S".to_string(),
                    table: TableId::new(0),
                    fields: vec![Field::Value {
                        name: "x".to_string(),
                        ty: ValueType::Scalar(ScalarType::Float),
                        columns: vec![ColumnId::new(TableId::new(0), 0)],
                    }],
                }],
                ..Default::default()
            }
        }
        fn fetch_null_float(float_checks: bool) -> Result<Vec<Row>, ExecError> {
            let schema = schema_with_checks(float_checks);
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch("CREATE TABLE t(x REAL); INSERT INTO t(x) VALUES (NULL);")
                .unwrap();
            let backend = SqliteBackend::new(conn, &schema).unwrap();
            let sid = StructId::new(0);
            backend.fetch(sid, &[FieldId::new(sid, 0)], &FetchConstraints::EMPTY)
        }
        match fetch_null_float(false).unwrap()[0].values[0] {
            Val::Scalar(Scalar::Float(x)) => assert!(x.is_nan()),
            ref other => panic!("expected NaN, got {other:?}"),
        }
        let err = fetch_null_float(true).unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
    }

    #[test]
    fn decode_bytes_blob() {
        let raw = [0x00, 0x01, 0xfe, 0xff];
        assert_eq!(
            decode_scalar(ScalarType::Bytes, ValueRef::Blob(&raw), false, true),
            Ok(Val::Scalar(Scalar::Bytes(raw.to_vec())))
        );
        assert_eq!(
            decode_scalar(ScalarType::Bytes, ValueRef::Blob(&[]), false, true),
            Ok(Val::Scalar(Scalar::Bytes(Vec::new())))
        );
        assert_eq!(
            scalar_value(&Scalar::Bytes(raw.to_vec())),
            Value::Blob(raw.to_vec())
        );
        assert!(decode_scalar(ScalarType::Bytes, ValueRef::Text(b"x"), false, true).is_err());
        assert_eq!(
            decode_scalar(ScalarType::Bytes, ValueRef::Null, true, true),
            Ok(Val::None)
        );
        assert!(decode_scalar(ScalarType::Bytes, ValueRef::Null, false, true).is_err());
    }

    #[test]
    fn fetch_bytes_blob() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let blobs = testdata::struct_id(&schema, "Blob");
        let data_fid = schema.struct_field(blobs, "data").unwrap().0;
        let maybe_fid = schema.struct_field(blobs, "maybe").unwrap().0;
        let rows = backend
            .fetch(blobs, &[data_fid, maybe_fid], &FetchConstraints::EMPTY)
            .unwrap();

        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].values,
            vec![
                Val::Scalar(Scalar::Bytes(vec![0, 1])),
                Val::Some(Box::new(Val::Scalar(Scalar::Bytes(vec![255])))),
            ]
        );
        assert_eq!(
            rows[1].values,
            vec![Val::Scalar(Scalar::Bytes(Vec::new())), Val::None]
        );
    }

    #[test]
    fn decode_storage_class_mismatch() {
        assert!(decode_scalar(ScalarType::String, ValueRef::Integer(1), false, true).is_err());
        assert!(decode_scalar(ScalarType::Int, ValueRef::Real(1.0), false, true).is_err());
        assert!(decode_scalar(ScalarType::Float, ValueRef::Integer(1), false, true).is_err());
        assert!(decode_scalar(ScalarType::Dec, ValueRef::Text(b"1.5"), false, true).is_err());
        assert!(decode_scalar(ScalarType::Instant, ValueRef::Integer(0), false, true).is_err());
    }

    #[test]
    fn decode_blobs_round_trip() {
        let d = dec!(30.00);
        assert_eq!(
            decode_scalar(
                ScalarType::Dec,
                ValueRef::Blob(&codec::encode_dec(d)),
                false,
                true
            ),
            Ok(Val::Scalar(Scalar::Dec(d.normalize())))
        );
        let t = DateTime::from_timestamp(1_700_000_000, 500).unwrap();
        assert_eq!(
            decode_scalar(
                ScalarType::Instant,
                ValueRef::Blob(&codec::encode_instant(t)),
                false,
                true
            ),
            Ok(Val::Scalar(Scalar::Instant(t)))
        );
        let span = TimeDelta::milliseconds(-1500);
        match decode_scalar(
            ScalarType::Duration,
            ValueRef::Blob(&codec::encode_duration(span)),
            false,
            true,
        ) {
            Ok(Val::Scalar(Scalar::Duration(back))) => {
                assert_eq!(back.num_seconds(), span.num_seconds());
                assert_eq!(back.subsec_nanos(), span.subsec_nanos());
            }
            other => panic!("unexpected decode result: {other:?}"),
        }
    }

    fn test_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let mut conn = Connection::open_in_memory().unwrap();
        testdata::seed(&mut conn).unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

    fn user_struct(schema: &ValidatedSchema) -> StructId {
        testdata::struct_id(schema, "User")
    }

    fn user_field(schema: &ValidatedSchema, sid: StructId, name: &str) -> FieldId {
        testdata::field_id(schema, sid, name)
    }

    fn some(v: Val) -> Val {
        Val::Some(Box::new(v))
    }

    fn int(n: i64) -> Val {
        Val::Scalar(Scalar::Int(n))
    }

    #[test]
    fn fetch_scalars_positional() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(
                sid,
                &[
                    user_field(&schema, sid, "name"),
                    user_field(&schema, sid, "id"),
                    user_field(&schema, sid, "balance"),
                    user_field(&schema, sid, "created"),
                    user_field(&schema, sid, "elapsed"),
                    user_field(&schema, sid, "active"),
                ],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].rowid, Key(1));
        assert_eq!(
            rows[0].values,
            vec![
                Val::Scalar(Scalar::String("Ada".to_string())),
                int(1),
                Val::Scalar(Scalar::Dec(dec!(30))),
                Val::Scalar(Scalar::Instant(
                    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
                )),
                Val::Scalar(Scalar::Duration(TimeDelta::seconds(90))),
                Val::Scalar(Scalar::Bool(true)),
            ]
        );
        assert_eq!(
            rows[2].values,
            vec![
                Val::Scalar(Scalar::String("Cyd".to_string())),
                int(3),
                Val::Scalar(Scalar::Dec(dec!(-5.25))),
                Val::Scalar(Scalar::Instant(
                    DateTime::from_timestamp(-1, 500_000_000).unwrap()
                )),
                Val::Scalar(Scalar::Duration(TimeDelta::milliseconds(-1500))),
                Val::Scalar(Scalar::Bool(false)),
            ]
        );
    }

    #[test]
    fn fetch_optional_and_tuples() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(
                sid,
                &[
                    user_field(&schema, sid, "id"),
                    user_field(&schema, sid, "nickname"),
                    user_field(&schema, sid, "pos"),
                    user_field(&schema, sid, "home"),
                ],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0].values,
            vec![
                int(1),
                some(Val::Scalar(Scalar::String("A".to_string()))),
                Val::Tuple(vec![
                    Val::Scalar(Scalar::Float(1.5)),
                    Val::Scalar(Scalar::Float(-2.5)),
                ]),
                some(Val::Tuple(vec![
                    Val::Scalar(Scalar::Float(3.0)),
                    Val::Scalar(Scalar::Float(4.0)),
                ])),
            ]
        );
        assert_eq!(
            rows[1].values,
            vec![
                int(2),
                Val::None,
                Val::Tuple(vec![
                    Val::Scalar(Scalar::Float(0.0)),
                    Val::Scalar(Scalar::Float(0.0)),
                ]),
                Val::None,
            ]
        );
    }

    #[test]
    fn fetch_partial_null_tuple_error() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        backend
            .conn
            .execute("UPDATE users SET hx = 3.0 WHERE id = 3", [])
            .unwrap();
        let sid = user_struct(&schema);
        let err = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "home")],
                &FetchConstraints::EMPTY,
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
        assert!(err.to_string().contains("User.home"), "{err}");
    }

    #[test]
    fn fetch_empty_table_empty_fields() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let rows = backend.fetch(sid, &[], &FetchConstraints::EMPTY).unwrap();
        assert_eq!(rows.len(), 5);
        assert!(rows[0].values.is_empty());
        assert_eq!(rows[4].rowid, Key(5));
        backend.conn.execute("DELETE FROM users", []).unwrap();
        let rows = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "id")],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn fetch_reads_rowid_order() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        backend
            .conn
            .execute("DELETE FROM users WHERE rowid = 2", [])
            .unwrap();
        backend
            .conn
            .execute("INSERT INTO users(id) VALUES (9)", [])
            .unwrap();
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "id")],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.rowid).collect::<Vec<_>>(),
            vec![Key(1), Key(3), Key(4), Key(5), Key(6)]
        );
        assert_eq!(
            rows.iter().map(|r| r.values.clone()).collect::<Vec<_>>(),
            vec![
                vec![int(1)],
                vec![int(3)],
                vec![int(4)],
                vec![int(5)],
                vec![int(9)],
            ]
        );
    }

    #[test]
    fn fetch_arrays_grouped_by_link_in_side_rowid_order() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(
                sid,
                &[
                    user_field(&schema, sid, "id"),
                    user_field(&schema, sid, "tags"),
                    user_field(&schema, sid, "scores"),
                    user_field(&schema, sid, "lucky"),
                ],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(rows.len(), 5);
        let str_list = |ss: &[&str]| {
            Val::List(
                ss.iter()
                    .map(|s| Val::Scalar(Scalar::String(s.to_string())))
                    .collect(),
            )
        };
        assert_eq!(
            rows[0].values,
            vec![
                int(1),
                str_list(&["a1", "a2"]),
                Val::List(vec![
                    Val::Tuple(vec![int(1), Val::Scalar(Scalar::String("one".to_string()))]),
                    Val::Tuple(vec![int(2), Val::Scalar(Scalar::String("two".to_string()))]),
                ]),
                Val::List(vec![some(int(5)), Val::None]),
            ]
        );
        assert_eq!(
            rows[1].values,
            vec![
                int(2),
                str_list(&["b2", "b1"]),
                Val::List(vec![]),
                Val::List(vec![]),
            ]
        );
        assert_eq!(
            rows[2].values,
            vec![
                int(3),
                Val::List(vec![]),
                Val::List(vec![]),
                Val::List(vec![])
            ]
        );
        assert_eq!(
            rows[3].values,
            vec![
                int(4),
                Val::List(vec![]),
                Val::List(vec![Val::Tuple(vec![
                    int(3),
                    Val::Scalar(Scalar::String("three".to_string()))
                ])]),
                Val::List(vec![]),
            ]
        );
        assert_eq!(
            rows[4].values,
            vec![
                int(5),
                str_list(&["a1", "a2"]),
                Val::List(vec![]),
                Val::List(vec![]),
            ]
        );
    }

    #[test]
    fn fetch_ref_field_error() {
        use grove_schema::validated::{Field, Struct, Table, TableId};
        let schema = ValidatedSchema {
            tables: vec![Table {
                name: "t".to_string(),
                columns: vec![],
            }],
            structs: vec![Struct {
                name: "S".to_string(),
                table: TableId::new(0),
                fields: vec![Field::Ref {
                    name: "r".to_string(),
                    target: StructId::new(0),
                    optional: false,
                    is_list: false,
                    owning: false,
                }],
            }],
            ..Default::default()
        };
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(x INTEGER)").unwrap();
        let backend = SqliteBackend::new(conn, &schema).unwrap();
        let err = backend
            .fetch(
                StructId::new(0),
                &[FieldId::new(StructId::new(0), 0)],
                &FetchConstraints::EMPTY,
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
        assert!(err.to_string().contains("Ref field `S.r`"), "{err}");
    }

    #[test]
    fn fetch_mismatched_types() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let id = user_field(&schema, sid, "id");
        let name = user_field(&schema, sid, "name");
        let active = user_field(&schema, sid, "active");
        let balance = user_field(&schema, sid, "balance");
        for (setup, field, label) in [
            (
                "INSERT INTO users(id, name) VALUES (1, x'313233')",
                name,
                "User.name",
            ),
            (
                "INSERT INTO users(id, name) VALUES (1.5, 'x')",
                id,
                "User.id",
            ),
            (
                "INSERT INTO users(id, name, active) VALUES (1, 'x', 2)",
                active,
                "User.active",
            ),
            (
                "INSERT INTO users(id, name, balance) VALUES (1, 'x', x'6a756e6b')",
                balance,
                "User.balance",
            ),
        ] {
            backend.conn.execute_batch("DELETE FROM users").unwrap();
            backend.conn.execute_batch(setup).unwrap();
            let err = backend
                .fetch(sid, &[field], &FetchConstraints::EMPTY)
                .unwrap_err();
            assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
            assert!(err.to_string().contains(label), "{err}");
        }
    }

    #[test]
    fn fetch_bad_ids_error() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        assert!(matches!(
            backend
                .fetch(StructId::new(99), &[], &FetchConstraints::EMPTY)
                .unwrap_err(),
            ExecError::Integrity { .. }
        ));
        assert!(matches!(
            backend
                .fetch(
                    sid,
                    &[FieldId::new(StructId::new(7), 0)],
                    &FetchConstraints::EMPTY
                )
                .unwrap_err(),
            ExecError::Integrity { .. }
        ));
    }

    #[test]
    fn quoted_identifiers_dont_degrade() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).unwrap();
        let err = backend
            .conn
            .prepare("SELECT \"no_such_column_xyz\"")
            .unwrap_err();
        assert!(err.to_string().contains("no such column"), "{err}");
    }

    fn verify_backend(tables: &[(&str, &[&str])], ddl: &str) -> Result<(), ExecError> {
        use grove_schema::validated::{Column, Table};
        let schema = ValidatedSchema {
            tables: tables
                .iter()
                .map(|(name, cols)| Table {
                    name: name.to_string(),
                    columns: cols
                        .iter()
                        .map(|c| Column {
                            name: c.to_string(),
                            ty: None,
                        })
                        .collect(),
                })
                .collect(),
            ..Default::default()
        };
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(ddl).unwrap();
        SqliteBackend::new(conn, &schema).map(|_| ())
    }

    #[test]
    fn verify_missing_table() {
        let err = verify_backend(&[("t", &["a"])], "CREATE TABLE other(x);").unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(err.to_string().contains("unknown table `t`"), "{err}");
    }

    #[test]
    fn verify_view_instead_of_table() {
        let err = verify_backend(
            &[("t", &["a"])],
            "CREATE TABLE base(x); CREATE VIEW t AS SELECT x AS a FROM base;",
        )
        .unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(err.to_string().contains("is a view"), "{err}");
    }

    #[test]
    fn verify_without_rowid_table() {
        let err = verify_backend(
            &[("t", &["a"])],
            "CREATE TABLE t(a INTEGER PRIMARY KEY) WITHOUT ROWID;",
        )
        .unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(err.to_string().contains("WITHOUT ROWID"), "{err}");
    }

    #[test]
    fn verify_missing_column() {
        let err = verify_backend(&[("t", &["a", "b"])], "CREATE TABLE t(a INTEGER);").unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(err.to_string().contains("missing column `b`"), "{err}");
    }

    #[test]
    fn verify_ignores_unknown_and_affinity() {
        assert!(
            verify_backend(
                &[("t", &["a"])],
                "CREATE TABLE t(a TEXT, extra BLOB); CREATE TABLE other(x);"
            )
            .is_ok()
        );
    }

    #[test]
    fn verify_case_insensitive_names() {
        assert!(verify_backend(&[("Users", &["A"])], "CREATE TABLE users(a INTEGER);").is_ok());
    }

    #[test]
    fn verify_rowid_shadow_column() {
        let err = verify_backend(&[("t", &["a"])], "CREATE TABLE t(a INTEGER, _rowid_ TEXT);")
            .unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(
            err.to_string().contains("shadowing the rowid alias"),
            "{err}"
        );
    }

    #[test]
    fn decode_node_nested_tuples() {
        use grove_schema::validated::ValueType;
        let ty = ValueType::Tuple(vec![
            ValueType::Scalar(ScalarType::Int),
            ValueType::Tuple(vec![
                ValueType::Scalar(ScalarType::String),
                ValueType::Scalar(ScalarType::Bool),
            ]),
        ]);
        let vals = [
            ValueRef::Integer(1),
            ValueRef::Text(b"a"),
            ValueRef::Integer(1),
        ];
        let (v, used) = decode_node(&ty, &vals, true, &ValidatedSchema::default()).unwrap();
        assert_eq!(used, 3);
        assert_eq!(
            v,
            Val::Tuple(vec![
                int(1),
                Val::Tuple(vec![
                    Val::Scalar(Scalar::String("a".to_string())),
                    Val::Scalar(Scalar::Bool(true)),
                ]),
            ])
        );
    }

    #[test]
    fn fetch_nested_via_array() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "matrix")],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0].values,
            vec![Val::List(vec![
                Val::List(vec![int(1), int(2)]),
                Val::List(vec![]),
            ])]
        );
        for row in &rows[1..] {
            assert_eq!(row.values, vec![Val::List(vec![])]);
        }
    }

    fn float(n: f64) -> Val {
        Val::Scalar(Scalar::Float(n))
    }

    fn bool(n: bool) -> Val {
        Val::Scalar(Scalar::Bool(n))
    }

    #[test]
    fn fetch_deep_branched_via_array() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(
                sid,
                &[
                    user_field(&schema, sid, "id"),
                    user_field(&schema, sid, "deep_branches"),
                ],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0].values,
            vec![
                int(1),
                Val::List(vec![Val::Tuple(vec![
                    Val::List(vec![Val::List(vec![int(1), int(2)]), Val::List(vec![])]),
                    Val::List(vec![float(2.5)]),
                    bool(true),
                ])]),
            ]
        );
        for row in &rows[1..] {
            assert_eq!(row.values[1], Val::List(vec![]));
        }
    }

    fn str(s: &str) -> Val {
        Val::Scalar(Scalar::String(s.to_string()))
    }

    #[test]
    fn fetch_tuple_embedded_array() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(
                sid,
                &[
                    user_field(&schema, sid, "id"),
                    user_field(&schema, sid, "extra"),
                ],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0].values,
            vec![
                int(1),
                Val::Tuple(vec![int(1), Val::List(vec![str("x"), str("y")])]),
            ]
        );
        assert_eq!(
            rows[1].values,
            vec![int(2), Val::Tuple(vec![int(3), Val::List(vec![])])]
        );
        assert_eq!(
            rows[2].values,
            vec![int(3), Val::Tuple(vec![int(4), Val::List(vec![])])]
        );
        assert_eq!(
            rows[3].values,
            vec![int(4), Val::Tuple(vec![int(2), Val::List(vec![str("z")])])]
        );
        assert_eq!(
            rows[4].values,
            vec![int(5), Val::Tuple(vec![int(5), Val::List(vec![])])]
        );
    }

    #[test]
    fn fetch_optional_tuple_embedded_array() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(
                sid,
                &[
                    user_field(&schema, sid, "id"),
                    user_field(&schema, sid, "opt_extra"),
                ],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(
            rows[0].values,
            vec![
                int(1),
                some(Val::Tuple(vec![int(5), Val::List(vec![str("p")])])),
            ]
        );
        assert_eq!(rows[1].values, vec![int(2), Val::None]);
        assert_eq!(rows[2].values, vec![int(3), Val::None]);
        assert_eq!(
            rows[3].values,
            vec![int(4), some(Val::Tuple(vec![int(7), Val::List(vec![])]))]
        );
        assert_eq!(rows[4].values, vec![int(5), Val::None]);
    }

    fn rel_named(
        schema: &ValidatedSchema,
        a_struct: &str,
        a_field: &str,
        b_struct: &str,
        b_field: &str,
    ) -> RelationId {
        let asid = testdata::struct_id(schema, a_struct);
        let bsid = testdata::struct_id(schema, b_struct);
        testdata::relation_id(
            schema,
            testdata::field_id(schema, asid, a_field),
            testdata::field_id(schema, bsid, b_field),
        )
    }

    fn firsts(groups: &[Vec<Row>]) -> Vec<Vec<Val>> {
        groups
            .iter()
            .map(|g| g.iter().map(|row| row.values[0].clone()).collect())
            .collect()
    }

    #[test]
    fn fetch_related_one_to_one() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let users = testdata::struct_id(&schema, "User");
        let profiles = testdata::struct_id(&schema, "Profile");
        let rel = rel_named(&schema, "Profile", "user", "User", "profile");
        let bio = testdata::field_id(&schema, profiles, "bio");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2), Key(3)],
                &[bio],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("AdaBio")], vec![], vec![str("CydBio")]]
        );
        let name = testdata::field_id(&schema, users, "name");
        let groups = backend
            .fetch_related(
                rel,
                true,
                &[Key(1), Key(2)],
                &[name],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(firsts(&groups), vec![vec![str("Ada")], vec![str("Cyd")]]);
    }

    #[test]
    fn fetch_related_many_to_one() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let users = testdata::struct_id(&schema, "User");
        let orders = testdata::struct_id(&schema, "Order");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");
        let name = testdata::field_id(&schema, users, "name");
        let groups = backend
            .fetch_related(
                rel,
                true,
                &[Key(1), Key(2), Key(3), Key(4)],
                &[name],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("Ada")], vec![str("Ada")], vec![], vec![str("Bob")]]
        );
        let amount = testdata::field_id(&schema, orders, "amount");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2), Key(3)],
                &[amount],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![int(100), int(200)], vec![int(400)], vec![]]
        );
    }

    #[test]
    fn fetch_related_constraints_filter_sort() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");
        let constraints = FetchConstraints {
            filters: vec![push_cmp(
                CmpOp::Gt,
                target_field(&schema, "Order", "amount"),
                PushExpr::Literal(Scalar::Int(150)),
            )],
            sort: vec![SortKey {
                field: amount,
                desc: true,
            }],
            offset: 0,
            limit: None,
        };
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2), Key(3)],
                &[amount],
                &constraints,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![int(200)], vec![int(400)], vec![]]
        );
    }

    #[test]
    fn fetch_related_caps_per_parent() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");

        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2), Key(3)],
                &[amount],
                &FetchConstraints {
                    filters: vec![],
                    sort: vec![],
                    offset: 0,
                    limit: Some(1),
                },
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![int(100)], vec![int(400)], vec![]]
        );

        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2), Key(3)],
                &[amount],
                &FetchConstraints {
                    filters: vec![],
                    sort: vec![],
                    offset: 1,
                    limit: Some(1),
                },
            )
            .unwrap();
        assert_eq!(firsts(&groups), vec![vec![int(200)], vec![], vec![]]);
    }

    #[test]
    fn fetch_related_non_owning_constraints() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let name = testdata::field_id(&schema, testdata::struct_id(&schema, "Role"), "name");
        let rel = rel_named(&schema, "Role", "users", "User", "roles");
        let constraints = FetchConstraints {
            filters: vec![push_cmp(
                CmpOp::Eq,
                target_field(&schema, "Role", "name"),
                PushExpr::Literal(Scalar::String("editor".to_string())),
            )],
            sort: vec![],
            offset: 0,
            limit: Some(1),
        };
        let groups = backend
            .fetch_related(rel, false, &[Key(1), Key(3), Key(2)], &[name], &constraints)
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("editor")], vec![str("editor")], vec![]]
        );
    }

    #[test]
    fn fetch_related_offset_only() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2)],
                &[amount],
                &FetchConstraints {
                    filters: vec![],
                    sort: vec![SortKey {
                        field: amount,
                        desc: false,
                    }],
                    offset: 1,
                    limit: None,
                },
            )
            .unwrap();
        assert_eq!(firsts(&groups), vec![vec![int(200)], vec![]]);
    }

    #[test]
    fn sql_cache_fetch_statements() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let fields = vec![user_field(&schema, sid, "name")];
        let constraints = FetchConstraints {
            filters: vec![push_cmp(CmpOp::Gt, push_field(&schema, "id"), push_int(2))],
            sort: vec![],
            offset: 0,
            limit: None,
        };

        backend.fetch(sid, &fields, &constraints).unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);
        backend.fetch(sid, &fields, &constraints).unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);

        let capped = FetchConstraints {
            offset: 1,
            ..constraints.clone()
        };
        backend.fetch(sid, &fields, &capped).unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 2);

        backend
            .fetch(sid, &[user_field(&schema, sid, "id")], &constraints)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 3);
    }

    #[test]
    fn fetch_enum_filter_and_cache() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = testdata::struct_id(&schema, "Setting");
        let name = testdata::field_id(&schema, sid, "name");
        let priority = FetchConstraints {
            filters: vec![push_cmp(
                CmpOp::Eq,
                target_field(&schema, "Setting", "priority"),
                push_str("high"),
            )],
            sort: vec![],
            offset: 0,
            limit: None,
        };
        let rows = backend.fetch(sid, &[name], &priority).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.values[0].clone())
                .collect::<Vec<_>>(),
            vec![str("alpha")]
        );
        assert_eq!(backend.sql_cache.borrow().len(), 1);
        backend.fetch(sid, &[name], &priority).unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);

        let level = FetchConstraints {
            filters: vec![PushExpr::In {
                lhs: Box::new(target_field(&schema, "Setting", "level")),
                items: vec![push_int(2), push_int(-1)],
            }],
            sort: vec![],
            offset: 0,
            limit: None,
        };
        let rows = backend.fetch(sid, &[name], &level).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.values[0].clone())
                .collect::<Vec<_>>(),
            vec![str("alpha"), str("beta")]
        );
        assert_eq!(backend.sql_cache.borrow().len(), 2);
    }

    #[test]
    fn sql_cache_fetch_related_statements() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");
        let constraints = FetchConstraints {
            filters: vec![push_cmp(
                CmpOp::Gt,
                target_field(&schema, "Order", "amount"),
                PushExpr::Literal(Scalar::Int(150)),
            )],
            sort: vec![SortKey {
                field: amount,
                desc: true,
            }],
            offset: 0,
            limit: Some(1),
        };

        backend
            .fetch_related(rel, false, &[Key(1), Key(2)], &[amount], &constraints)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);
        backend
            .fetch_related(rel, false, &[Key(1), Key(2)], &[amount], &constraints)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);

        let uncapped = FetchConstraints {
            limit: None,
            ..constraints.clone()
        };
        backend
            .fetch_related(rel, false, &[Key(1), Key(2)], &[amount], &uncapped)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 2);

        backend
            .fetch_related(rel, false, &[Key(1)], &[amount], &constraints)
            .unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 3);
    }

    #[test]
    fn fetch_related_one_to_many() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let images = testdata::struct_id(&schema, "Image");
        let messages = testdata::struct_id(&schema, "Message");
        let rel = rel_named(&schema, "Image", "messages", "Message", "image");
        let body = testdata::field_id(&schema, messages, "body");
        let groups = backend
            .fetch_related(
                rel,
                true,
                &[Key(1), Key(2)],
                &[body],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("nice"), str("wow")], vec![str("meh")]]
        );
        let title = testdata::field_id(&schema, images, "title");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(4)],
                &[title],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(firsts(&groups), vec![vec![str("Sunset")], vec![]]);
    }

    #[test]
    fn fetch_related_many_to_many() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let users = testdata::struct_id(&schema, "User");
        let roles = testdata::struct_id(&schema, "Role");
        let rel = rel_named(&schema, "Role", "users", "User", "roles");
        let rname = testdata::field_id(&schema, roles, "name");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(3), Key(2)],
                &[rname],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![
                vec![str("admin"), str("editor"), str("editor")],
                vec![str("editor")],
                vec![],
            ]
        );
        assert_eq!(
            groups[0].iter().map(|row| row.rowid).collect::<Vec<_>>(),
            vec![Key(1), Key(2), Key(2)]
        );
        let uname = testdata::field_id(&schema, users, "name");
        let groups = backend
            .fetch_related(
                rel,
                true,
                &[Key(2), Key(1)],
                &[uname],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("Ada"), str("Ada"), str("Cyd")], vec![str("Ada")]]
        );
        assert_eq!(
            groups[0].iter().map(|row| row.rowid).collect::<Vec<_>>(),
            vec![Key(1), Key(1), Key(3)]
        );
    }

    #[test]
    fn fetch_related_self_reference() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let users = testdata::struct_id(&schema, "User");
        let rel = rel_named(&schema, "User", "manager", "User", "subordinates");
        let name = testdata::field_id(&schema, users, "name");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(2), Key(1)],
                &[name],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("Ada"), str("Cyd")], vec![str("Dan")]]
        );
        let groups = backend
            .fetch_related(
                rel,
                true,
                &[Key(1), Key(4), Key(2)],
                &[name],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("Bob")], vec![str("Ada")], vec![]]
        );
    }

    #[test]
    fn fetch_related_with_array_fields() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let profiles = testdata::struct_id(&schema, "Profile");
        let rel = rel_named(&schema, "Profile", "user", "User", "profile");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(3)],
                &[
                    testdata::field_id(&schema, profiles, "bio"),
                    testdata::field_id(&schema, profiles, "notes"),
                ],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0][0].values,
            vec![str("AdaBio"), Val::List(vec![str("a-note")])]
        );
        assert_eq!(groups[1][0].values, vec![str("CydBio"), Val::List(vec![])]);
    }

    #[test]
    fn fetch_related_edge_cases() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let rel = rel_named(&schema, "Profile", "user", "User", "profile");
        // empty parents: no rows, fields arg irrelevant (never resolved)
        let groups = backend
            .fetch_related(rel, true, &[], &[], &FetchConstraints::EMPTY)
            .unwrap();
        assert!(groups.is_empty());
        let err = backend
            .fetch_related(
                RelationId::new(99),
                true,
                &[Key(1)],
                &[],
                &FetchConstraints::EMPTY,
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
    }

    fn push_field(schema: &ValidatedSchema, name: &str) -> PushExpr {
        PushExpr::Field(user_field(schema, user_struct(schema), name))
    }

    fn target_field(schema: &ValidatedSchema, struct_name: &str, name: &str) -> PushExpr {
        PushExpr::Field(testdata::field_id(
            schema,
            testdata::struct_id(schema, struct_name),
            name,
        ))
    }

    fn push_int(value: i64) -> PushExpr {
        PushExpr::Literal(Scalar::Int(value))
    }

    fn push_str(value: &str) -> PushExpr {
        PushExpr::Literal(Scalar::String(value.to_string()))
    }

    fn push_cmp(op: CmpOp, lhs: PushExpr, rhs: PushExpr) -> PushExpr {
        PushExpr::Cmp {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        }
    }

    fn user_names(
        backend: &SqliteBackend<'_>,
        schema: &ValidatedSchema,
        filters: Vec<PushExpr>,
        sort: Vec<SortKey>,
        offset: u64,
        limit: Option<u64>,
    ) -> Vec<String> {
        let sid = user_struct(schema);
        let rows = backend
            .fetch(
                sid,
                &[user_field(schema, sid, "name")],
                &FetchConstraints {
                    filters,
                    sort,
                    offset,
                    limit,
                },
            )
            .unwrap();
        rows.into_iter()
            .map(|row| match row.values.as_slice() {
                [Val::Scalar(Scalar::String(name))] => name.clone(),
                other => panic!("expected one name value, got {other:?}"),
            })
            .collect()
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn fetch_filter_comparisons() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);

        let filters = vec![push_cmp(CmpOp::Gt, push_field(&schema, "id"), push_int(2))];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Cyd", "Dan", "Eli"])
        );

        let filters = vec![push_cmp(
            CmpOp::Eq,
            push_field(&schema, "name"),
            push_str("Bob"),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Bob"])
        );

        let filters = vec![push_cmp(CmpOp::Ne, push_field(&schema, "id"), push_int(1))];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Bob", "Cyd", "Dan", "Eli"])
        );

        let filters = vec![push_cmp(
            CmpOp::Eq,
            push_field(&schema, "active"),
            PushExpr::Literal(Scalar::Bool(false)),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Cyd", "Eli"])
        );

        let filters = vec![PushExpr::In {
            lhs: Box::new(push_field(&schema, "id")),
            items: vec![push_int(1), push_int(3), push_int(5)],
        }];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Cyd", "Eli"])
        );

        let filters = vec![PushExpr::And(
            Box::new(push_field(&schema, "active")),
            Box::new(push_cmp(CmpOp::Gt, push_field(&schema, "id"), push_int(1))),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Bob", "Dan"])
        );

        let filters = vec![PushExpr::Or(
            Box::new(push_cmp(CmpOp::Eq, push_field(&schema, "id"), push_int(1))),
            Box::new(push_cmp(CmpOp::Eq, push_field(&schema, "id"), push_int(5))),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Eli"])
        );

        let filters = vec![PushExpr::Not(Box::new(push_field(&schema, "active")))];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Cyd", "Eli"])
        );
    }

    #[test]
    fn fetch_filter_string_operations() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);

        let string_op = |op: StringOp, needle: &str| {
            vec![PushExpr::StringOp {
                op,
                base: Box::new(push_field(&schema, "name")),
                needle: Box::new(push_str(needle)),
            }]
        };
        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::Contains, "a"),
                vec![],
                0,
                None
            ),
            names(&["Ada", "Dan"])
        );
        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::Contains, ""),
                vec![],
                0,
                None
            ),
            names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
        );

        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::StartsWith, "A"),
                vec![],
                0,
                None
            ),
            names(&["Ada"])
        );
        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::StartsWith, ""),
                vec![],
                0,
                None
            ),
            names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
        );

        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::EndsWith, "d"),
                vec![],
                0,
                None
            ),
            names(&["Cyd"])
        );
        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::EndsWith, ""),
                vec![],
                0,
                None
            ),
            names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
        );

        let filters = vec![push_cmp(
            CmpOp::Ge,
            PushExpr::StringLen(Box::new(push_field(&schema, "name"))),
            push_int(3),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
        );
    }

    #[test]
    fn fetch_filter_temporal_and_decimal() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);

        let filters = vec![push_cmp(
            CmpOp::Gt,
            push_field(&schema, "balance"),
            PushExpr::Literal(Scalar::Dec(dec!(30.00))),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Dan"])
        );

        let filters = vec![push_cmp(
            CmpOp::Ge,
            push_field(&schema, "balance"),
            PushExpr::Literal(Scalar::Dec(dec!(30.00))),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Dan"])
        );

        let filters = vec![push_cmp(
            CmpOp::Ge,
            push_field(&schema, "created"),
            PushExpr::Literal(Scalar::Instant(
                DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            )),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Dan", "Eli"])
        );

        let filters = vec![push_cmp(
            CmpOp::Gt,
            push_field(&schema, "elapsed"),
            PushExpr::Literal(Scalar::Duration(TimeDelta::zero())),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Dan", "Eli"])
        );
    }

    #[test]
    fn fetch_bytes_filter_sort_and_cache() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = testdata::struct_id(&schema, "Blob");
        let id = testdata::field_id(&schema, sid, "id");
        let data = testdata::field_id(&schema, sid, "data");
        let maybe = testdata::field_id(&schema, sid, "maybe");
        let chunks = testdata::field_id(&schema, sid, "chunks");

        let constraints = FetchConstraints {
            filters: vec![push_cmp(
                CmpOp::Gt,
                target_field(&schema, "Blob", "data"),
                PushExpr::Literal(Scalar::Bytes(vec![0x00, 0x00])),
            )],
            sort: vec![SortKey {
                field: data,
                desc: false,
            }],
            offset: 0,
            limit: None,
        };
        let rows = backend.fetch(sid, &[id], &constraints).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.values[0].clone())
                .collect::<Vec<_>>(),
            vec![int(1), int(3)]
        );
        assert_eq!(backend.sql_cache.borrow().len(), 1);
        backend.fetch(sid, &[id], &constraints).unwrap();
        assert_eq!(backend.sql_cache.borrow().len(), 1);

        let rows = backend
            .fetch(sid, &[maybe, chunks], &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(
            rows[0].values,
            vec![
                some(Val::Scalar(Scalar::Bytes(vec![0xff]))),
                Val::List(vec![
                    Val::Scalar(Scalar::Bytes(vec![0x01])),
                    Val::Scalar(Scalar::Bytes(vec![0x01, 0x00])),
                ]),
            ]
        );
        assert_eq!(rows[1].values, vec![Val::None, Val::List(vec![])]);
        assert_eq!(
            rows[2].values,
            vec![
                some(Val::Scalar(Scalar::Bytes(vec![0x00]))),
                Val::List(vec![]),
            ]
        );
    }

    #[test]
    fn fetch_sort_limit_offset() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);

        let sort = vec![SortKey {
            field: user_field(&schema, sid, "id"),
            desc: true,
        }];
        assert_eq!(
            user_names(&backend, &schema, vec![], sort, 0, None),
            names(&["Eli", "Dan", "Cyd", "Bob", "Ada"])
        );

        let sort = vec![
            SortKey {
                field: user_field(&schema, sid, "active"),
                desc: true,
            },
            SortKey {
                field: user_field(&schema, sid, "id"),
                desc: false,
            },
        ];
        assert_eq!(
            user_names(&backend, &schema, vec![], sort, 0, None),
            names(&["Ada", "Bob", "Dan", "Cyd", "Eli"])
        );

        let sort = vec![SortKey {
            field: user_field(&schema, sid, "active"),
            desc: false,
        }];
        assert_eq!(
            user_names(&backend, &schema, vec![], sort, 0, None),
            names(&["Cyd", "Eli", "Ada", "Bob", "Dan"])
        );

        assert_eq!(
            user_names(&backend, &schema, vec![], vec![], 0, Some(2)),
            names(&["Ada", "Bob"])
        );
        assert_eq!(
            user_names(&backend, &schema, vec![], vec![], 2, None),
            names(&["Cyd", "Dan", "Eli"])
        );
        assert_eq!(
            user_names(&backend, &schema, vec![], vec![], 3, Some(1)),
            names(&["Dan"])
        );
        assert_eq!(
            user_names(&backend, &schema, vec![], vec![], 0, Some(0)),
            Vec::<String>::new()
        );
        assert_eq!(
            user_names(&backend, &schema, vec![], vec![], 10, None),
            Vec::<String>::new()
        );

        let sort = vec![SortKey {
            field: user_field(&schema, sid, "name"),
            desc: true,
        }];
        assert_eq!(
            user_names(&backend, &schema, vec![], sort, 0, Some(2)),
            names(&["Eli", "Dan"])
        );
    }

    #[test]
    fn fetch_enums_optional_array_tuple() {
        let schema = testdata::schema();
        let sid = testdata::struct_id(&schema, "Setting");
        let priority = testdata::enum_id(&schema, "Priority");
        let level = testdata::enum_id(&schema, "Level");
        let fields = [
            testdata::field_id(&schema, sid, "name"),
            testdata::field_id(&schema, sid, "priority"),
            testdata::field_id(&schema, sid, "level"),
            testdata::field_id(&schema, sid, "fallback"),
            testdata::field_id(&schema, sid, "history"),
            testdata::field_id(&schema, sid, "pair"),
        ];
        let rows = test_backend(&schema)
            .fetch(sid, &fields, &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].values,
            vec![
                str("alpha"),
                Val::Enum(priority, 2),
                Val::Enum(level, 2),
                some(Val::Enum(priority, 0)),
                Val::List(vec![Val::Enum(priority, 0), Val::Enum(priority, 2)]),
                Val::Tuple(vec![Val::Enum(priority, 2), Val::Enum(level, 2)]),
            ]
        );
        assert_eq!(
            rows[1].values,
            vec![
                str("beta"),
                Val::Enum(priority, 0),
                Val::Enum(level, 0),
                Val::None,
                Val::List(vec![Val::Enum(priority, 2)]),
                Val::Tuple(vec![Val::Enum(priority, 0), Val::Enum(level, 0)]),
            ]
        );
        assert_eq!(
            rows[2].values,
            vec![
                str("gamma"),
                Val::Enum(priority, 1),
                Val::Enum(level, 1),
                some(Val::Enum(priority, 1)),
                Val::List(vec![]),
                Val::Tuple(vec![Val::Enum(priority, 1), Val::Enum(level, 1)]),
            ]
        );
    }

    #[test]
    fn fetch_invalid_enum_value() {
        let schema = testdata::schema();
        let sid = testdata::struct_id(&schema, "Setting");
        let priority = testdata::field_id(&schema, sid, "priority");

        let mut conn = Connection::open_in_memory().unwrap();
        testdata::seed(&mut conn).unwrap();
        conn.execute(
            "UPDATE settings SET priority = 'urgent' WHERE name = 'alpha'",
            [],
        )
        .unwrap();
        let backend = SqliteBackend::new(conn, &schema).unwrap();
        let err = backend
            .fetch(sid, &[priority], &FetchConstraints::EMPTY)
            .unwrap_err();
        match err {
            ExecError::Integrity { message } => {
                assert!(message.contains("Priority"), "{message}");
                assert!(message.contains("urgent"), "{message}");
            }
            other => panic!("expected integrity error, got {other:?}"),
        }
    }

    #[test]
    fn fetch_enum_mismatched_storage_class() {
        let schema = testdata::schema();
        let sid = testdata::struct_id(&schema, "Setting");
        let level = testdata::field_id(&schema, sid, "level");

        let mut conn = Connection::open_in_memory().unwrap();
        testdata::seed(&mut conn).unwrap();
        conn.execute(
            "UPDATE settings SET level = 'high' WHERE name = 'alpha'",
            [],
        )
        .unwrap();
        let backend = SqliteBackend::new(conn, &schema).unwrap();
        let err = backend
            .fetch(sid, &[level], &FetchConstraints::EMPTY)
            .unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
        assert!(err.to_string().contains("expected Int"), "{err}");
    }

    #[test]
    fn fetch_invalid_enum_array_value() {
        let schema = testdata::schema();
        let sid = testdata::struct_id(&schema, "Setting");
        let history = testdata::field_id(&schema, sid, "history");

        let mut conn = Connection::open_in_memory().unwrap();
        testdata::seed(&mut conn).unwrap();
        conn.execute(
            "UPDATE setting_history SET priority = 'urgent' WHERE setting_key = 700",
            [],
        )
        .unwrap();
        let backend = SqliteBackend::new(conn, &schema).unwrap();
        let err = backend
            .fetch(sid, &[history], &FetchConstraints::EMPTY)
            .unwrap_err();
        match err {
            ExecError::Integrity { message } => {
                assert!(message.contains("Priority"), "{message}");
                assert!(message.contains("urgent"), "{message}");
            }
            other => panic!("expected integrity error, got {other:?}"),
        }
    }

    #[test]
    fn fetch_constraints_rejected_fields() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);

        let order = testdata::struct_id(&schema, "Order");
        let amount = testdata::field_id(&schema, order, "amount");
        let err = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "name")],
                &FetchConstraints {
                    filters: vec![PushExpr::Field(amount)],
                    sort: vec![],
                    offset: 0,
                    limit: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");

        let pos = user_field(&schema, sid, "pos");
        let err = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "name")],
                &FetchConstraints {
                    filters: vec![PushExpr::Field(pos)],
                    sort: vec![],
                    offset: 0,
                    limit: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");

        let err = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "name")],
                &FetchConstraints {
                    filters: vec![],
                    sort: vec![SortKey {
                        field: pos,
                        desc: false,
                    }],
                    offset: 0,
                    limit: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
    }
}
