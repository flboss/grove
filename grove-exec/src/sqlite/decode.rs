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
pub(super) fn semi(
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
}
impl SqliteBackend<'_> {
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
