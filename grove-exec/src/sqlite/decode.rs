use std::collections::HashMap;

use grove_schema::validated::{
    EnumId, EnumUnderlying, EnumValue, ScalarType, StorageTable, ValidatedSchema, ValueType,
};
use rusqlite::Statement;
use rusqlite::types::{Value, ValueRef};

use super::SqliteBackend;
use super::types::{LinkKey, decode_scalar, leaf_count, quote, stored_class};
use crate::error::ExecError;
use crate::value::{Key, Val};

pub(super) fn decode_node(
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

pub(super) fn decode_enum(
    enum_id: EnumId,
    value: ValueRef<'_>,
    schema: &ValidatedSchema,
) -> Result<Val, ExecError> {
    let enum_def = schema
        .enums
        .get(enum_id.index())
        .ok_or_else(|| ExecError::Internal {
            message: format!("unknown enum id {}", enum_id.index()),
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

pub(super) struct ValueSlot<'a> {
    pub(super) label: String,
    pub(super) ty: &'a ValueType,
    pub(super) start: usize,
    pub(super) len: usize,
}

pub(super) struct ArraySlot<'conn> {
    pub(super) label: String,
    pub(super) link_idx: usize,
    pub(super) nodes: Vec<ArrayNode>,
    pub(super) stmts: Vec<Statement<'conn>>,
    pub(super) subtree: Vec<usize>,
}

pub(super) struct ValueArraySlot<'conn> {
    pub(super) label: String,
    pub(super) start: usize,
    pub(super) len: usize,
    pub(super) plan: ElemPlan,
    pub(super) nodes: Vec<ArrayNode>,
    pub(super) stmts: Vec<Statement<'conn>>,
    pub(super) subtrees: Vec<(usize, Vec<usize>)>,
}

pub(super) enum Slot<'a, 'conn> {
    Value(ValueSlot<'a>),
    Array(ArraySlot<'conn>),
    ValueArray(ValueArraySlot<'conn>),
}

/// One array level in an element type tree
pub(super) struct ArrayNode {
    pub(super) plan: ElemPlan,
    pub(super) width: usize,
    pub(super) parent: Option<usize>,
}

/// Decode plan mirroring an array element type
#[derive(Debug, Clone)]
pub(super) enum ElemPlan {
    Scalar(ScalarType),
    Enum(EnumId),
    Tuple(Vec<ElemPlan>),
    Optional(Box<ElemPlan>),
    Link(usize),
}

pub(super) struct NodeDraft<'a> {
    pub(super) storage: &'a StorageTable,
    pub(super) parent: Option<(usize, usize)>,
    pub(super) plan: Option<ElemPlan>,
}

/// Walk a (element) type, registering nested arrays as nodes; returns the
/// decode plan. `counter` tracks value columns of the current level
/// Top-level arrays (no parent node) scope `key = ?` on their own link
pub(super) fn walk_element<'a>(
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

pub(super) fn plan_width(plan: &ElemPlan) -> usize {
    match plan {
        ElemPlan::Scalar(_) | ElemPlan::Enum(_) | ElemPlan::Link(_) => 1,
        ElemPlan::Tuple(elements) => elements.iter().map(plan_width).sum(),
        ElemPlan::Optional(inner) => plan_width(inner),
    }
}

/// Scope predicate tying a node's rows to the parent row's top link
pub(super) fn node_scope(
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
                    .ok_or_else(|| ExecError::Internal {
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

pub(super) fn node_sql(
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
pub(super) enum SemiVal {
    Done(Val),
    Tuple(Vec<SemiVal>),
    Some(Box<SemiVal>),
    Deep { node: usize, key: LinkKey },
}

/// Decode one level row
pub(super) fn decode_semi(
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
                let (v, n) = decode_semi(plan, &cells[used..], float_checks, schema)?;
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
                decode_semi(inner, cells, float_checks, schema).map(|(s, n)| match s {
                    SemiVal::Done(v) => (SemiVal::Done(Val::Some(Box::new(v))), n),
                    other => (SemiVal::Some(Box::new(other)), n),
                })
            }
        }
        ElemPlan::Link(node) => {
            if matches!(cells[0], ValueRef::Null) {
                Ok((SemiVal::Done(Val::list(Vec::new())), 1))
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
pub(super) fn finalize(semi: SemiVal, groups: &HashMap<usize, HashMap<LinkKey, Vec<Val>>>) -> Val {
    match semi {
        SemiVal::Done(v) => v,
        SemiVal::Tuple(items) => {
            Val::Tuple(items.into_iter().map(|s| finalize(s, groups)).collect())
        }
        SemiVal::Some(inner) => Val::Some(Box::new(finalize(*inner, groups))),
        SemiVal::Deep { node, key } => Val::list(
            groups
                .get(&node)
                .and_then(|g| g.get(&key))
                .cloned()
                .unwrap_or_default(),
        ),
    }
}

/// Partition nodes into per-root subtrees (root, ascending members)
pub(super) fn split_roots(nodes: &[ArrayNode]) -> Vec<(usize, Vec<usize>)> {
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
pub(super) fn find_link(tree: &SemiVal, root: usize) -> Option<LinkKey> {
    match tree {
        SemiVal::Done(_) => None,
        SemiVal::Tuple(items) => items.iter().find_map(|s| find_link(s, root)),
        SemiVal::Some(inner) => find_link(inner, root),
        SemiVal::Deep { node, key } => (*node == root).then(|| key.clone()),
    }
}

/// Splice resolved root lists back into a decoded field template
pub(super) fn splice(tree: SemiVal, resolved: &mut HashMap<usize, Val>) -> Val {
    match tree {
        SemiVal::Done(v) => v,
        SemiVal::Tuple(items) => {
            Val::Tuple(items.into_iter().map(|s| splice(s, resolved)).collect())
        }
        SemiVal::Some(inner) => Val::Some(Box::new(splice(*inner, resolved))),
        SemiVal::Deep { node, .. } => resolved.remove(&node).unwrap_or(Val::list(Vec::new())),
    }
}

impl SqliteBackend<'_> {
    pub(super) fn prepare_array_nodes(
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
    pub(super) fn run_subtree(
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
                .query(rusqlite::params_from_iter(std::iter::once(key.clone())))
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
                let (semival, _) = decode_semi(
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

    pub(super) fn decode_row(
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
                    values.push(Val::list(items));
                }
                Slot::ValueArray(slot) => {
                    let mut cells = Vec::with_capacity(slot.len);
                    for i in 0..slot.len {
                        cells.push(
                            row.get_ref(slot.start + i)
                                .map_err(|err| Self::storage_error(ctx, err))?,
                        );
                    }
                    let (tree, _) = decode_semi(&slot.plan, &cells, float_checks, self.schema)
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
                            resolved.insert(*root, Val::list(items));
                        }
                    }
                    values.push(splice(tree, &mut resolved));
                }
            }
        }
        Ok((Key(rowid), values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::tests::*;
    use crate::storage::{FetchConstraints, Storage};
    use crate::testdata;
    use crate::value::Scalar;
    use rusqlite::Connection;

    #[test]
    fn decode_node_nested_tuples() {
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
            Val::Tuple(vec![int(1), Val::Tuple(vec![str("a"), bool(true)]),])
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
                Val::Tuple(vec![float(1.5), float(-2.5)]),
                some(Val::Tuple(vec![float(3.0), float(4.0)])),
            ]
        );
        assert_eq!(
            rows[1].values,
            vec![
                int(2),
                Val::None,
                Val::Tuple(vec![float(0.0), float(0.0)]),
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
        let str_list = |ss: &[&str]| Val::list(ss.iter().map(|s| str(s)).collect());
        assert_eq!(
            rows[0].values,
            vec![
                int(1),
                str_list(&["a1", "a2"]),
                Val::list(vec![
                    Val::Tuple(vec![int(1), str("one")]),
                    Val::Tuple(vec![int(2), str("two")]),
                ]),
                Val::list(vec![some(int(5)), Val::None]),
            ]
        );
        assert_eq!(
            rows[1].values,
            vec![
                int(2),
                str_list(&["b2", "b1"]),
                Val::list(vec![]),
                Val::list(vec![]),
            ]
        );
        assert_eq!(
            rows[2].values,
            vec![
                int(3),
                Val::list(vec![]),
                Val::list(vec![]),
                Val::list(vec![])
            ]
        );
        assert_eq!(
            rows[3].values,
            vec![
                int(4),
                Val::list(vec![]),
                Val::list(vec![Val::Tuple(vec![int(3), str("three")])]),
                Val::list(vec![]),
            ]
        );
        assert_eq!(
            rows[4].values,
            vec![
                int(5),
                str_list(&["a1", "a2"]),
                Val::list(vec![]),
                Val::list(vec![]),
            ]
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
            vec![Val::list(vec![
                Val::list(vec![int(1), int(2)]),
                Val::list(vec![]),
            ])]
        );
        for row in &rows[1..] {
            assert_eq!(row.values, vec![Val::list(vec![])]);
        }
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
                Val::list(vec![Val::Tuple(vec![
                    Val::list(vec![Val::list(vec![int(1), int(2)]), Val::list(vec![])]),
                    Val::list(vec![float(2.5)]),
                    bool(true),
                ])]),
            ]
        );
        for row in &rows[1..] {
            assert_eq!(row.values[1], Val::list(vec![]));
        }
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
                Val::Tuple(vec![int(1), Val::list(vec![str("x"), str("y")])]),
            ]
        );
        assert_eq!(
            rows[1].values,
            vec![int(2), Val::Tuple(vec![int(3), Val::list(vec![])])]
        );
        assert_eq!(
            rows[2].values,
            vec![int(3), Val::Tuple(vec![int(4), Val::list(vec![])])]
        );
        assert_eq!(
            rows[3].values,
            vec![int(4), Val::Tuple(vec![int(2), Val::list(vec![str("z")])])]
        );
        assert_eq!(
            rows[4].values,
            vec![int(5), Val::Tuple(vec![int(5), Val::list(vec![])])]
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
                some(Val::Tuple(vec![int(5), Val::list(vec![str("p")])])),
            ]
        );
        assert_eq!(rows[1].values, vec![int(2), Val::None]);
        assert_eq!(rows[2].values, vec![int(3), Val::None]);
        assert_eq!(
            rows[3].values,
            vec![int(4), some(Val::Tuple(vec![int(7), Val::list(vec![])]))]
        );
        assert_eq!(rows[4].values, vec![int(5), Val::None]);
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
                Val::list(vec![Val::Enum(priority, 0), Val::Enum(priority, 2)]),
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
                Val::list(vec![Val::Enum(priority, 2)]),
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
                Val::list(vec![]),
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
}
