use std::collections::HashMap;

use grove_schema::validated::{
    Field, FieldId, RelationId, ScalarType, StorageTable, StructId, ValidatedSchema, ValueType,
};
use rusqlite::Connection;
use rusqlite::Statement;
use rusqlite::config::DbConfig;
use rusqlite::params_from_iter;
use rusqlite::types::{Value, ValueRef};

use crate::codec;
use crate::error::ExecError;
use crate::storage::{Key, Row, Storage};
use crate::value::{Scalar, Val};

pub struct SqliteBackend {
    conn: Connection,
    schema: ValidatedSchema,
}

impl SqliteBackend {
    pub fn new(conn: Connection, schema: ValidatedSchema) -> Result<Self, ExecError> {
        for option in [
            // Disable quoted identifiers degrading to string literals in some cases
            DbConfig::SQLITE_DBCONFIG_DQS_DML,
            DbConfig::SQLITE_DBCONFIG_DQS_DDL,
        ] {
            conn.set_db_config(option, false)
                .map_err(|err| Self::storage_error("disable double-quoted literals", err))?;
        }
        Ok(SqliteBackend { conn, schema })
    }

    pub fn open_in_memory(schema: ValidatedSchema) -> Result<Self, ExecError> {
        let conn = Connection::open_in_memory()
            .map_err(|err| Self::storage_error("open in-memory database", err))?;
        Self::new(conn, schema)
    }

    fn storage_error(context: &str, err: rusqlite::Error) -> ExecError {
        ExecError::Storage {
            message: format!("{context}: {err}"),
        }
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
        ValueType::Scalar(_) => 1,
        ValueType::Tuple(elements) => elements.iter().map(leaf_count).sum(),
        ValueType::Optional(inner) => leaf_count(inner),
        ValueType::Array { .. } => 0,
    }
}

/// Decode one node from the head of `vals`; returns value, columns consumed
fn decode_node(ty: &ValueType, vals: &[ValueRef<'_>]) -> Result<(Val, usize), ExecError> {
    match ty {
        ValueType::Scalar(scalar) => decode_scalar(*scalar, vals[0], false).map(|v| (v, 1)),
        ValueType::Optional(inner) => {
            let len = leaf_count(inner);
            if vals[..len].iter().all(|v| matches!(v, ValueRef::Null)) {
                Ok((Val::None, len))
            } else {
                decode_node(inner, vals).map(|(v, n)| (Val::Some(Box::new(v)), n))
            }
        }
        ValueType::Tuple(elements) => {
            let mut items = Vec::with_capacity(elements.len());
            let mut used = 0;
            for element in elements {
                let (v, n) = decode_node(element, &vals[used..])?;
                items.push(v);
                used += n;
            }
            Ok((Val::Tuple(items), used))
        }
        ValueType::Array { .. } => Err(ExecError::Storage {
            message: "via array inside a value field is currently unsupported".to_string(),
        }),
    }
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
}

enum Slot<'a, 'conn> {
    Value(ValueSlot<'a>),
    Array(ArraySlot<'conn>),
}

/// One array level in an element type tree
struct ArrayNode {
    plan: ElemPlan,
    width: usize,
}

/// Decode plan mirroring an array element type
#[derive(Debug, Clone)]
enum ElemPlan {
    Scalar(ScalarType),
    Tuple(Vec<ElemPlan>),
    Optional(Box<ElemPlan>),
    Link(usize),
}

struct NodeDraft<'a> {
    storage: &'a StorageTable,
    parent: Option<(usize, usize)>,
    plan: Option<ElemPlan>,
}

/// Walk an element type, registering nested arrays as nodes; returns the
/// decode plan. `counter` tracks value columns of the current level.
fn walk_element<'a>(
    ty: &'a ValueType,
    self_node: usize,
    drafts: &mut Vec<NodeDraft<'a>>,
    counter: &mut usize,
) -> ElemPlan {
    match ty {
        ValueType::Scalar(scalar) => {
            *counter += 1;
            ElemPlan::Scalar(*scalar)
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
                parent: Some((self_node, pos)),
                plan: None,
            });
            let mut sub = 0;
            let plan = walk_element(element, idx, drafts, &mut sub);
            drafts[idx].plan = Some(plan);
            ElemPlan::Link(idx)
        }
    }
}

fn plan_width(plan: &ElemPlan) -> usize {
    match plan {
        ElemPlan::Scalar(_) | ElemPlan::Link(_) => 1,
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
    let alias = format!("__a{idx}");
    let key = format!(
        "{alias}.{}",
        quote(schema.column_name(drafts[idx].storage.key))
    );
    match drafts[idx].parent {
        None => Ok(format!("{key} = ?")),
        Some((parent, pos)) => {
            let palias = format!("__a{parent}");
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
    let alias = format!("__a{idx}");
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
        "SELECT {key}, {} FROM {table} AS {alias} WHERE {scope} ORDER BY rowid",
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
fn semi(plan: &ElemPlan, cells: &[ValueRef<'_>]) -> Result<(SemiVal, usize), ExecError> {
    match plan {
        ElemPlan::Scalar(scalar) => {
            decode_scalar(*scalar, cells[0], false).map(|v| (SemiVal::Done(v), 1))
        }
        ElemPlan::Tuple(plans) => {
            let mut items = Vec::with_capacity(plans.len());
            let mut used = 0;
            for plan in plans {
                let (v, n) = semi(plan, &cells[used..])?;
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
                semi(inner, cells).map(|(s, n)| match s {
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

/// Resolve links against already-finalized deeper groups
fn finalize(semi: SemiVal, groups: &[HashMap<LinkKey, Vec<Val>>]) -> Val {
    match semi {
        SemiVal::Done(v) => v,
        SemiVal::Tuple(items) => {
            Val::Tuple(items.into_iter().map(|s| finalize(s, groups)).collect())
        }
        SemiVal::Some(inner) => Val::Some(Box::new(finalize(*inner, groups))),
        SemiVal::Deep { node, key } => {
            Val::List(groups[node].get(&key).cloned().unwrap_or_default())
        }
    }
}

/// Decode one scalar cell
fn decode_scalar(ty: ScalarType, value: ValueRef<'_>, nullable: bool) -> Result<Val, ExecError> {
    let mismatch = |found: &str| ExecError::Integrity {
        message: format!("expected {ty:?}, found {found}"),
    };
    match (ty, value) {
        (_, ValueRef::Null) if nullable => Ok(Val::None),
        (ScalarType::Int, ValueRef::Integer(n)) => Ok(Val::Scalar(Scalar::Int(n))),
        (ScalarType::Float, ValueRef::Real(x)) => Ok(Val::Scalar(Scalar::Float(x))),
        (ScalarType::String, ValueRef::Text(bytes)) => String::from_utf8(bytes.to_vec())
            .map(|s| Val::Scalar(Scalar::String(s)))
            .map_err(|_| mismatch("non-UTF8 TEXT")),
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

impl Storage for SqliteBackend {
    fn fetch(&self, struct_id: StructId, fields: &[FieldId]) -> Result<Vec<Row>, ExecError> {
        let schema = &self.schema;
        let def = schema
            .structs
            .get(struct_id.index())
            .ok_or_else(|| ExecError::Storage {
                message: format!("unknown struct id {}", struct_id.index()),
            })?;
        let struct_name = &def.name;
        let table = quote(&schema.tables[def.table.index()].name);
        let ctx = format!("fetch {struct_name}");
        let mut select = vec!["rowid".to_string()];
        let mut slots = Vec::with_capacity(fields.len());
        for field_id in fields {
            if field_id.struct_id() != struct_id {
                return Err(ExecError::Integrity {
                    message: format!("fetch field {field_id:?} belongs to another struct"),
                });
            }
            let field = def
                .fields
                .get(field_id.local())
                .ok_or_else(|| ExecError::Integrity {
                    message: format!("unknown field {field_id:?} on struct `{struct_name}`"),
                })?;
            match field {
                Field::Value { name, ty, columns } => {
                    let start = select.len();
                    for col in columns {
                        select.push(quote(schema.column_name(*col)));
                    }
                    slots.push(Slot::Value(ValueSlot {
                        label: format!("{struct_name}.{name}"),
                        ty,
                        start,
                        len: columns.len(),
                    }));
                }
                Field::Array {
                    name,
                    element,
                    link,
                    storage,
                } => {
                    let link_idx = select.len();
                    select.push(quote(schema.column_name(*link)));
                    let label = format!("{struct_name}.{name}");
                    let mut drafts = vec![NodeDraft {
                        storage,
                        parent: None,
                        plan: None,
                    }];
                    let mut counter = 0;
                    let plan = walk_element(element, 0, &mut drafts, &mut counter);
                    drafts[0].plan = Some(plan);
                    let mut nodes = Vec::with_capacity(drafts.len());
                    let mut stmts = Vec::with_capacity(drafts.len());
                    for (idx, draft) in drafts.iter().enumerate() {
                        let sql =
                            node_sql(idx, &drafts, schema).map_err(|err| err.context(&label))?;
                        stmts.push(
                            self.conn
                                .prepare(&sql)
                                .map_err(|err| Self::storage_error(&ctx, err))?,
                        );
                        let plan = draft.plan.clone().expect("walk fills every draft");
                        nodes.push(ArrayNode {
                            width: plan_width(&plan),
                            plan,
                        });
                    }
                    slots.push(Slot::Array(ArraySlot {
                        label,
                        link_idx,
                        nodes,
                        stmts,
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
        let sql = format!("SELECT {} FROM {table} ORDER BY rowid", select.join(", "));
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut rows = stmt
            .query([])
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|err| Self::storage_error(&ctx, err))? {
            let rowid: i64 = row.get(0).map_err(|err| Self::storage_error(&ctx, err))?;
            let mut values = Vec::with_capacity(slots.len());
            for slot in &mut slots {
                match slot {
                    Slot::Value(slot) => {
                        let mut cells = Vec::with_capacity(slot.len);
                        for i in 0..slot.len {
                            cells.push(
                                row.get_ref(slot.start + i)
                                    .map_err(|err| Self::storage_error(&ctx, err))?,
                            );
                        }
                        let (val, _) =
                            decode_node(slot.ty, &cells).map_err(|err| err.context(&slot.label))?;
                        values.push(val);
                    }
                    Slot::Array(slot) => {
                        let link = row
                            .get_ref(slot.link_idx)
                            .map_err(|err| Self::storage_error(&ctx, err))?;
                        let mut items = Vec::new();
                        if !matches!(link, ValueRef::Null) {
                            let top = LinkKey::from_value(link).to_value();
                            let mut raws = Vec::with_capacity(slot.nodes.len());
                            for (n, stmt) in slot.stmts.iter_mut().enumerate() {
                                let mut srows = stmt
                                    .query(params_from_iter(std::iter::once(top.clone())))
                                    .map_err(|err| Self::storage_error(&ctx, err))?;
                                let mut rows = Vec::new();
                                while let Some(srow) =
                                    srows.next().map_err(|err| Self::storage_error(&ctx, err))?
                                {
                                    let key = LinkKey::from_value(
                                        srow.get_ref(0)
                                            .map_err(|err| Self::storage_error(&ctx, err))?,
                                    );
                                    let width = slot.nodes[n].width;
                                    let mut cells = Vec::with_capacity(width);
                                    for i in 0..width {
                                        cells.push(
                                            srow.get_ref(1 + i)
                                                .map_err(|err| Self::storage_error(&ctx, err))?,
                                        );
                                    }
                                    let (sv, _) = semi(&slot.nodes[n].plan, &cells)
                                        .map_err(|err| err.context(&slot.label))?;
                                    rows.push((key, sv));
                                }
                                raws.push(rows);
                            }
                            let mut groups: Vec<HashMap<LinkKey, Vec<Val>>> =
                                (0..slot.nodes.len()).map(|_| HashMap::new()).collect();
                            for n in (1..slot.nodes.len()).rev() {
                                for (key, sv) in raws[n].drain(..) {
                                    let v = finalize(sv, &groups);
                                    groups[n].entry(key).or_default().push(v);
                                }
                            }
                            items = raws[0]
                                .drain(..)
                                .map(|(_, sv)| finalize(sv, &groups))
                                .collect();
                        }
                        values.push(Val::List(items));
                    }
                }
            }
            out.push(Row {
                rowid: Key(rowid),
                values,
            });
        }
        Ok(out)
    }

    fn fetch_related(
        &self,
        _rel: RelationId,
        _fetch_from_first: bool,
        _parents: &[Key],
        _fields: &[FieldId],
    ) -> Result<Vec<Vec<Row>>, ExecError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeDelta};

    #[test]
    fn open_in_memory_constructor() {
        assert!(SqliteBackend::open_in_memory(ValidatedSchema::default()).is_ok());
    }

    #[test]
    fn decode_int_float_string() {
        assert_eq!(
            decode_scalar(ScalarType::Int, ValueRef::Integer(-7), false),
            Ok(Val::Scalar(Scalar::Int(-7)))
        );
        assert_eq!(
            decode_scalar(ScalarType::Float, ValueRef::Real(1.5), false),
            Ok(Val::Scalar(Scalar::Float(1.5)))
        );
        assert_eq!(
            decode_scalar(ScalarType::String, ValueRef::Text(b"hi"), false),
            Ok(Val::Scalar(Scalar::String("hi".to_string())))
        );
        assert!(decode_scalar(ScalarType::String, ValueRef::Text(b"\xFF"), false).is_err());
    }

    #[test]
    fn decode_bool_strict() {
        assert_eq!(
            decode_scalar(ScalarType::Bool, ValueRef::Integer(0), false),
            Ok(Val::Scalar(Scalar::Bool(false)))
        );
        assert_eq!(
            decode_scalar(ScalarType::Bool, ValueRef::Integer(1), false),
            Ok(Val::Scalar(Scalar::Bool(true)))
        );
        assert!(decode_scalar(ScalarType::Bool, ValueRef::Integer(2), false).is_err());
        assert!(decode_scalar(ScalarType::Bool, ValueRef::Real(1.0), false).is_err());
    }

    #[test]
    fn decode_null() {
        assert_eq!(
            decode_scalar(ScalarType::Int, ValueRef::Null, true),
            Ok(Val::None)
        );
        assert!(decode_scalar(ScalarType::Int, ValueRef::Null, false).is_err());
    }

    #[test]
    fn decode_storage_class_mismatch() {
        assert!(decode_scalar(ScalarType::String, ValueRef::Integer(1), false).is_err());
        assert!(decode_scalar(ScalarType::Int, ValueRef::Real(1.0), false).is_err());
        assert!(decode_scalar(ScalarType::Float, ValueRef::Integer(1), false).is_err());
        assert!(decode_scalar(ScalarType::Dec, ValueRef::Text(b"1.5"), false).is_err());
        assert!(decode_scalar(ScalarType::Instant, ValueRef::Integer(0), false).is_err());
    }

    #[test]
    fn decode_blobs_round_trip() {
        let d: rust_decimal::Decimal = "30.00".parse().unwrap();
        assert_eq!(
            decode_scalar(
                ScalarType::Dec,
                ValueRef::Blob(&codec::encode_dec(d)),
                false
            ),
            Ok(Val::Scalar(Scalar::Dec(d.normalize())))
        );
        let t = DateTime::from_timestamp(1_700_000_000, 500).unwrap();
        assert_eq!(
            decode_scalar(
                ScalarType::Instant,
                ValueRef::Blob(&codec::encode_instant(t)),
                false
            ),
            Ok(Val::Scalar(Scalar::Instant(t)))
        );
        let span = TimeDelta::milliseconds(-1500);
        match decode_scalar(
            ScalarType::Duration,
            ValueRef::Blob(&codec::encode_duration(span)),
            false,
        ) {
            Ok(Val::Scalar(Scalar::Duration(back))) => {
                assert_eq!(back.num_seconds(), span.num_seconds());
                assert_eq!(back.subsec_nanos(), span.subsec_nanos());
            }
            other => panic!("unexpected decode result: {other:?}"),
        }
    }

    fn test_backend() -> (SqliteBackend, ValidatedSchema) {
        let schema = crate::testdata::schema();
        let mut conn = Connection::open_in_memory().unwrap();
        crate::testdata::seed(&mut conn).unwrap();
        let backend = SqliteBackend::new(conn, schema.clone()).unwrap();
        (backend, schema)
    }

    fn user_struct(schema: &ValidatedSchema) -> StructId {
        crate::testdata::struct_id(schema, "User")
    }

    fn user_field(schema: &ValidatedSchema, sid: StructId, name: &str) -> FieldId {
        crate::testdata::field_id(schema, sid, name)
    }

    fn some(v: Val) -> Val {
        Val::Some(Box::new(v))
    }

    fn int(n: i64) -> Val {
        Val::Scalar(Scalar::Int(n))
    }

    #[test]
    fn fetch_scalars_positional() {
        let (backend, schema) = test_backend();
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
            )
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].rowid, Key(1));
        assert_eq!(
            rows[0].values,
            vec![
                Val::Scalar(Scalar::String("Ada".to_string())),
                int(1),
                Val::Scalar(Scalar::Dec("30".parse().unwrap())),
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
                Val::Scalar(Scalar::Dec("-5.25".parse().unwrap())),
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
        let (backend, schema) = test_backend();
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
        let (backend, schema) = test_backend();
        backend
            .conn
            .execute("UPDATE users SET hx = 3.0 WHERE id = 3", [])
            .unwrap();
        let sid = user_struct(&schema);
        let err = backend
            .fetch(sid, &[user_field(&schema, sid, "home")])
            .unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
        assert!(err.to_string().contains("User.home"), "{err}");
    }

    #[test]
    fn fetch_empty_table_empty_fields() {
        let (backend, schema) = test_backend();
        let sid = user_struct(&schema);
        let rows = backend.fetch(sid, &[]).unwrap();
        assert_eq!(rows.len(), 5);
        assert!(rows[0].values.is_empty());
        assert_eq!(rows[4].rowid, Key(5));
        backend.conn.execute("DELETE FROM users", []).unwrap();
        let rows = backend
            .fetch(sid, &[user_field(&schema, sid, "id")])
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn fetch_reads_rowid_order() {
        let (backend, schema) = test_backend();
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
            .fetch(sid, &[user_field(&schema, sid, "id")])
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
        let (backend, schema) = test_backend();
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
        let backend = SqliteBackend::open_in_memory(schema).unwrap();
        let err = backend
            .fetch(StructId::new(0), &[FieldId::new(StructId::new(0), 0)])
            .unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
        assert!(err.to_string().contains("Ref field `S.r`"), "{err}");
    }

    #[test]
    fn fetch_mismatched_types() {
        let (backend, schema) = test_backend();
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
            let err = backend.fetch(sid, &[field]).unwrap_err();
            assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
            assert!(err.to_string().contains(label), "{err}");
        }
    }

    #[test]
    fn fetch_bad_ids_error() {
        let (backend, schema) = test_backend();
        let sid = user_struct(&schema);
        assert!(matches!(
            backend.fetch(StructId::new(99), &[]).unwrap_err(),
            ExecError::Storage { .. }
        ));
        assert!(matches!(
            backend
                .fetch(sid, &[FieldId::new(StructId::new(7), 0)])
                .unwrap_err(),
            ExecError::Integrity { .. }
        ));
    }

    #[test]
    fn fetch_missing_column_error() {
        use grove_schema::validated::{
            Column, ColumnId, Field, ScalarType, Struct, Table, TableId, ValueType,
        };
        let ghost = ColumnId::new(TableId::new(0), 0);
        let schema = ValidatedSchema {
            tables: vec![Table {
                name: "t".to_string(),
                columns: vec![Column {
                    name: "ghost".to_string(),
                    ty: Some(ScalarType::Int),
                }],
            }],
            structs: vec![Struct {
                name: "S".to_string(),
                table: TableId::new(0),
                fields: vec![Field::Value {
                    name: "x".to_string(),
                    ty: ValueType::Scalar(ScalarType::Int),
                    columns: vec![ghost],
                }],
            }],
            ..Default::default()
        };
        let backend = SqliteBackend::open_in_memory(schema).unwrap();
        backend
            .conn
            .execute_batch("CREATE TABLE t(c INTEGER);")
            .unwrap();
        let err = backend
            .fetch(StructId::new(0), &[FieldId::new(StructId::new(0), 0)])
            .unwrap_err();
        assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
        assert!(err.to_string().contains("no such column"), "{err}");
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
        let (v, used) = decode_node(&ty, &vals).unwrap();
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
        let (backend, schema) = test_backend();
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(sid, &[user_field(&schema, sid, "matrix")])
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
}
