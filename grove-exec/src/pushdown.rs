use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use grove_query::ast::{BinaryOp, Literal, SortDir, UnaryOp};
use grove_query::typecheck::types::{
    IdentBinding, QueryType, RecordSource, TypedExpr, TypedExprKind, TypedMethodArg,
    TypedQueryFile, TypedStatement,
};
use grove_schema::validated::{Field, FieldId, ScalarType, StructId, ValidatedSchema, ValueType};
use grove_types::NodeId;

use crate::eval::literals::{decode_literal, eval_type_constant};
use crate::value::{Scalar, Val, enum_variant_scalar};

/// Backend-agnostic constraint set attached to a storage read
#[derive(Debug, Clone, PartialEq, Default, Hash)]
pub struct FetchConstraints {
    /// WHERE predicates connected by AND
    pub filters: Vec<PushExpr>,
    /// ORDER BY keys in priority order
    pub sort: Vec<SortKey>,
    pub offset: u64,
    pub limit: Option<u64>,
}

impl FetchConstraints {
    pub const EMPTY: FetchConstraints = FetchConstraints {
        filters: Vec::new(),
        sort: Vec::new(),
        offset: 0,
        limit: None,
    };

    pub fn is_empty(&self) -> bool {
        self.filters.is_empty() && self.sort.is_empty() && self.offset == 0 && self.limit.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SortKey {
    pub field: FieldId,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq, Hash)]
pub enum PushExpr {
    /// Non-optional single-column scalar
    Field(FieldId),
    /// Literal evaluated at plan-build time
    Literal(Scalar),
    Cmp {
        op: CmpOp,
        lhs: Box<PushExpr>,
        rhs: Box<PushExpr>,
    },
    And(Box<PushExpr>, Box<PushExpr>),
    Or(Box<PushExpr>, Box<PushExpr>),
    Not(Box<PushExpr>),
    In {
        lhs: Box<PushExpr>,
        items: Vec<PushExpr>,
    },
    StringOp {
        op: StringOp,
        base: Box<PushExpr>,
        needle: Box<PushExpr>,
    },
    StringLen(Box<PushExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StringOp {
    Contains,
    StartsWith,
    EndsWith,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapRewrite {
    /// take/skip already folded into SQL LIMIT/OFFSET
    Skip,
    /// nth already folded; evaluate as `first`
    First,
}

/// Folded cap node rewrite mode, trigger fetch which applied it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapPlan {
    pub rewrite: CapRewrite,
    pub trigger: NodeId,
}

/// Annotation map for pushdown
#[derive(Debug, Clone, Default)]
pub struct PushdownPlan {
    fetches: HashMap<NodeId, FetchConstraints>,
    fields: HashMap<StructId, Vec<FieldId>>,
    caps: HashMap<NodeId, CapPlan>,
    #[allow(dead_code)] // prefetch does not yet consider this
    prefetch_skip: HashSet<FieldId>,
    enabled: bool,
}

impl PushdownPlan {
    /// Analyze a typed file and build the pushdown plan
    pub fn analyze(
        file: &TypedQueryFile,
        schema: &ValidatedSchema,
        now: DateTime<Utc>,
    ) -> PushdownPlan {
        let mut builder = PlanBuilder {
            schema,
            now,
            chains: HashMap::new(),
            ref_sites: HashMap::new(),
            usage: HashMap::new(),
            caps: HashMap::new(),
        };
        for statement in &file.statements {
            match statement {
                TypedStatement::Let(let_stmt) => {
                    builder.visit(&let_stmt.value, Need::Narrow);
                }
                TypedStatement::Expr(_) | TypedStatement::Mutation(_) => {}
            }
        }
        builder.visit(&file.result, Need::Whole);
        builder.finish()
    }

    pub fn disabled(schema: &ValidatedSchema) -> PushdownPlan {
        PushdownPlan {
            fetches: HashMap::new(),
            fields: all_struct_fields(schema),
            caps: HashMap::new(),
            prefetch_skip: HashSet::new(),
            enabled: false,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn constraints_for(&self, node: NodeId) -> Option<&FetchConstraints> {
        self.fetches.get(&node)
    }

    pub fn fields_for(&self, struct_id: StructId) -> &[FieldId] {
        self.fields
            .get(&struct_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn cap_rewrite(&self, node: NodeId) -> Option<CapRewrite> {
        self.caps.get(&node).map(|cap| cap.rewrite)
    }

    pub fn cap_plan(&self, node: NodeId) -> Option<CapPlan> {
        self.caps.get(&node).copied()
    }

    pub fn prefetch_skip(&self) -> &HashSet<FieldId> {
        &self.prefetch_skip
    }
}

/// Non-Ref fields of every struct in declaration order
pub fn all_struct_fields(schema: &ValidatedSchema) -> HashMap<StructId, Vec<FieldId>> {
    schema
        .structs
        .iter()
        .enumerate()
        .map(|(index, struct_def)| {
            let struct_id = StructId::new(index);
            let fields = struct_def
                .fields
                .iter()
                .enumerate()
                .filter(|(_, field)| !matches!(field, Field::Ref { .. }))
                .map(|(local, _)| FieldId::new(struct_id, local))
                .collect();
            (struct_id, fields)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    Narrow,
    Whole,
}

#[derive(Debug, Clone, Copy)]
struct ChainSummary {
    site: NodeId,
    struct_id: StructId,
}

#[derive(Debug, Default)]
struct FieldPlan {
    used: HashSet<FieldId>,
    whole: bool,
}

#[derive(Debug, Default)]
struct ChainState {
    filters: Vec<PushExpr>,
    sort: Vec<SortKey>,
    offset: u64,
    end: Option<u64>,
    cap_seen: bool,
    cap_closed: bool,
    unpushed_filter_seen: bool,
    unpushed_sort_seen: bool,
}

impl ChainState {
    fn into_constraints(self) -> FetchConstraints {
        FetchConstraints {
            filters: self.filters,
            sort: self.sort,
            offset: self.offset,
            limit: self.end.map(|end| end.saturating_sub(self.offset)),
        }
    }
}

struct PlanBuilder<'a> {
    schema: &'a ValidatedSchema,
    now: DateTime<Utc>,
    chains: HashMap<NodeId, ChainState>,
    ref_sites: HashMap<NodeId, FieldId>,
    usage: HashMap<StructId, FieldPlan>,
    caps: HashMap<NodeId, CapPlan>,
}

impl<'a> PlanBuilder<'a> {
    fn finish(self) -> PushdownPlan {
        let PlanBuilder {
            schema,
            chains,
            ref_sites,
            mut usage,
            caps,
            ..
        } = self;
        expand_whole(schema, &mut usage);
        let fields = schema
            .structs
            .iter()
            .enumerate()
            .map(|(index, struct_def)| {
                let struct_id = StructId::new(index);
                let plan = usage.get(&struct_id);
                let whole = plan.is_some_and(|plan| plan.whole);
                let subset = struct_def
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| !matches!(field, Field::Ref { .. }))
                    .map(|(local, _)| FieldId::new(struct_id, local))
                    .filter(|fid| whole || plan.is_some_and(|plan| plan.used.contains(fid)))
                    .collect();
                (struct_id, subset)
            })
            .collect();
        let fetches: HashMap<NodeId, FetchConstraints> = chains
            .into_iter()
            .map(|(node, state)| (node, state.into_constraints()))
            .collect();
        let prefetch_skip = ref_sites
            .into_iter()
            .filter_map(|(node, field)| {
                let constraints = fetches.get(&node)?;
                (!constraints.is_empty()).then_some(field)
            })
            .collect();
        PushdownPlan {
            fetches,
            fields,
            caps,
            prefetch_skip,
            enabled: true,
        }
    }

    /// Walk expression tree for column liveness (downward)
    /// and fetch pushdown chain recognition (upward)
    fn visit(&mut self, expr: &TypedExpr, need: Need) -> Option<ChainSummary> {
        if need == Need::Whole {
            self.mark_used_whole(&expr.ty);
        }
        match &expr.kind {
            TypedExprKind::Literal(_)
            | TypedExprKind::TypeConstant { .. }
            | TypedExprKind::EnumVariant { .. } => None,
            TypedExprKind::Ident { binding, .. } => self.visit_ident(expr, binding),
            TypedExprKind::Field { base, name, .. } => {
                self.visit_field_access(base, name);
                let trigger = base
                    .ty
                    .single_schema_record()
                    .and_then(|struct_id| self.schema.struct_field(struct_id, name))
                    .and_then(|(field, _)| {
                        self.list_ref_target(field).map(|target| (field, target))
                    });
                trigger.map(|(field, target)| self.start_ref_chain(expr.id, field, target))
            }
            TypedExprKind::TupleAccess { base, .. } => {
                self.visit(base, Need::Narrow);
                None
            }
            TypedExprKind::Method {
                base,
                name,
                args,
                opt_depth,
            } => {
                let base_need = method_base_need(name, need);
                let summary = self.visit(base, base_need);
                for arg in args {
                    self.visit(&arg.expr, Need::Whole);
                }
                self.extend_chain(expr.id, name, *opt_depth, args, summary)
            }
            TypedExprKind::Binary { lhs, rhs, .. } => {
                self.visit(lhs, Need::Whole);
                self.visit(rhs, Need::Whole);
                None
            }
            TypedExprKind::Unary { expr: inner, .. } | TypedExprKind::Cast { expr: inner, .. } => {
                self.visit(inner, Need::Whole);
                None
            }
            TypedExprKind::Some { value } => {
                self.visit(value, need);
                None
            }
            TypedExprKind::If { arms, default } => {
                for (condition, value) in arms {
                    self.visit(condition, Need::Whole);
                    self.visit(value, need);
                }
                self.visit(default, need);
                None
            }
            TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
                for element in elements {
                    self.visit(element, Need::Whole);
                }
                None
            }
            TypedExprKind::Struct { fields } => {
                for (_, value) in fields {
                    self.visit(value, Need::Whole);
                }
                None
            }
            TypedExprKind::Projection { base, items } => {
                self.visit(base, Need::Narrow);
                for item in items {
                    self.visit(&item.value, Need::Whole);
                }
                None
            }
        }
    }

    fn visit_ident(&mut self, expr: &TypedExpr, binding: &IdentBinding) -> Option<ChainSummary> {
        match binding {
            IdentBinding::Root { root_idx } => {
                let root = self.schema.roots.get(*root_idx)?;
                self.chains.entry(expr.id).or_default();
                Some(ChainSummary {
                    site: expr.id,
                    struct_id: root.struct_id,
                })
            }
            IdentBinding::Field(field) => {
                self.mark_used_field(*field);
                let target = self.list_ref_target(*field)?;
                Some(self.start_ref_chain(expr.id, *field, target))
            }
            IdentBinding::ProjectedField { .. }
            | IdentBinding::Prev { .. }
            | IdentBinding::ListElement
            | IdentBinding::Let => None,
        }
    }

    fn list_ref_target(&self, field: FieldId) -> Option<StructId> {
        match self.schema.field(field)? {
            Field::Ref {
                target,
                is_list: true,
                ..
            } => Some(*target),
            _ => None,
        }
    }

    fn start_ref_chain(&mut self, site: NodeId, field: FieldId, target: StructId) -> ChainSummary {
        self.ref_sites.insert(site, field);
        self.chains.entry(site).or_default();
        ChainSummary {
            site,
            struct_id: target,
        }
    }

    fn visit_field_access(&mut self, base: &TypedExpr, name: &str) {
        if let Some(struct_id) = base.ty.schema_record()
            && let Some((field, _)) = self.schema.struct_field(struct_id, name)
        {
            self.mark_used_field(field);
        }
        self.visit(base, Need::Narrow);
    }

    fn mark_used_field(&mut self, field: FieldId) {
        if matches!(
            self.schema.field(field),
            Some(Field::Value { .. } | Field::Array { .. })
        ) {
            self.usage
                .entry(field.struct_id())
                .or_default()
                .used
                .insert(field);
        }
    }

    fn mark_used_whole(&mut self, ty: &QueryType) {
        match ty {
            QueryType::Record(RecordSource::Schema(struct_id)) => {
                self.usage.entry(*struct_id).or_default().whole = true;
            }
            QueryType::Optional(inner) | QueryType::List(inner) => self.mark_used_whole(inner),
            QueryType::Tuple(elements) => {
                for element in elements {
                    self.mark_used_whole(element);
                }
            }
            QueryType::Record(RecordSource::Projection(fields)) => {
                for field in fields {
                    self.mark_used_whole(&field.ty);
                }
            }
            QueryType::Scalar(_) | QueryType::Enum(_) | QueryType::Void | QueryType::Unknown => {}
        }
    }

    /// Attempt to extend chain by a method; decide whether the chain ends
    fn extend_chain(
        &mut self,
        node: NodeId,
        method: &str,
        opt_depth: usize,
        args: &[TypedMethodArg],
        base: Option<ChainSummary>,
    ) -> Option<ChainSummary> {
        let chain = base?;
        if opt_depth > 0 {
            return None;
        }
        match method {
            "filter" => {
                let [arg] = args else {
                    return None;
                };
                let predicate = self.translate_expr(&arg.expr, chain.struct_id);
                let state = self.chains.get_mut(&chain.site)?;
                if state.cap_seen {
                    state.cap_closed = true;
                    return None;
                }
                match predicate {
                    Some(predicate) => state.filters.push(predicate),
                    None => state.unpushed_filter_seen = true,
                }
                Some(chain)
            }
            "sort" | "sort_asc" | "sort_desc" | "sort_over" | "sort_asc_over"
            | "sort_desc_over" => {
                let keyed = matches!(method, "sort_over" | "sort_asc_over" | "sort_desc_over");
                let default_desc = matches!(method, "sort_desc" | "sort_desc_over");
                let keys = if keyed {
                    self.translate_sort(args, chain.struct_id, default_desc)
                } else {
                    None
                };
                let state = self.chains.get_mut(&chain.site)?;
                if state.cap_seen {
                    state.cap_closed = true;
                    return None;
                }
                if state.unpushed_sort_seen {
                    return Some(chain);
                }
                match keys {
                    Some(keys) => {
                        state.sort.splice(0..0, keys);
                    }
                    None => state.unpushed_sort_seen = true,
                }
                Some(chain)
            }
            "take" | "skip" => {
                let state = self.chains.get_mut(&chain.site)?;
                state.cap_seen = true;
                let Some(bound) = const_int_arg(args) else {
                    state.cap_closed = true;
                    return None;
                };
                if state.cap_closed {
                    return None;
                }
                if state.unpushed_filter_seen || state.unpushed_sort_seen {
                    state.cap_closed = true;
                    return None;
                }
                if method == "take" {
                    apply_take(state, bound);
                } else {
                    apply_skip(state, bound);
                }
                self.caps.insert(
                    node,
                    CapPlan {
                        rewrite: CapRewrite::Skip,
                        trigger: chain.site,
                    },
                );
                Some(chain)
            }
            "first" => {
                let state = self.chains.get_mut(&chain.site)?;
                state.cap_seen = true;
                if !args.is_empty() {
                    state.cap_closed = true;
                    return None;
                }
                if state.cap_closed {
                    return None;
                }
                if state.unpushed_filter_seen || state.unpushed_sort_seen {
                    state.cap_closed = true;
                    return None;
                }
                apply_take(state, 1);
                Some(chain)
            }
            "nth" => {
                let state = self.chains.get_mut(&chain.site)?;
                state.cap_seen = true;
                let Some(index) = const_int_arg(args) else {
                    state.cap_closed = true;
                    return None;
                };
                if state.cap_closed {
                    return None;
                }
                if !state.cap_seen && (state.unpushed_filter_seen || state.unpushed_sort_seen) {
                    state.cap_closed = true;
                    return None;
                }
                apply_skip(state, index);
                apply_take(state, 1);
                self.caps.insert(
                    node,
                    CapPlan {
                        rewrite: CapRewrite::First,
                        trigger: chain.site,
                    },
                );
                Some(chain)
            }
            _ => None,
        }
    }

    /// Attempt to translate sort args into backend-agnostic simple sort keys
    fn translate_sort(
        &self,
        args: &[TypedMethodArg],
        source: StructId,
        default_desc: bool,
    ) -> Option<Vec<SortKey>> {
        if args.is_empty() {
            return None;
        }
        args.iter()
            .map(|arg| {
                let TypedExprKind::Ident {
                    binding: IdentBinding::Field(field),
                    depth: 0,
                    ..
                } = &arg.expr.kind
                else {
                    return None;
                };
                self.scalar_field(*field, source)?;
                let desc = arg
                    .direction
                    .as_ref()
                    .map_or(default_desc, |direction| direction.value == SortDir::Desc);
                Some(SortKey {
                    field: *field,
                    desc,
                })
            })
            .collect()
    }

    fn scalar_field(&self, field: FieldId, source: StructId) -> Option<ScalarType> {
        if field.struct_id() != source {
            return None;
        }
        match self.schema.field(field)? {
            Field::Value {
                ty: ValueType::Scalar(scalar),
                columns,
                ..
            } if columns.len() == 1 => Some(*scalar),
            Field::Value {
                ty: ValueType::Enum(id),
                columns,
                ..
            } if columns.len() == 1 => self
                .schema
                .enums
                .get(id.index())
                .map(|enum_def| enum_def.underlying.scalar_type()),
            _ => None,
        }
    }

    fn push_scalar_type(&self, ty: &QueryType) -> Option<ScalarType> {
        match ty {
            QueryType::Scalar(scalar) => Some(*scalar),
            QueryType::Enum(id) => self
                .schema
                .enums
                .get(id.index())
                .map(|enum_def| enum_def.underlying.scalar_type()),
            _ => None,
        }
    }

    /// Attempt to translate an expression into backend-agnostic simple PushExpr
    fn translate_expr(&self, expr: &TypedExpr, source: StructId) -> Option<PushExpr> {
        match &expr.kind {
            TypedExprKind::Literal(literal) => match decode_literal(literal, self.now) {
                Val::Scalar(scalar) => Some(PushExpr::Literal(scalar)),
                _ => None,
            },
            TypedExprKind::TypeConstant { ty, name } => {
                match eval_type_constant(ty.value, name.value) {
                    Val::Scalar(scalar) => Some(PushExpr::Literal(scalar)),
                    _ => None,
                }
            }
            TypedExprKind::EnumVariant { enum_id, variant } => {
                let enum_def = self.schema.enums.get(enum_id.index())?;
                enum_variant_scalar(enum_def, *variant).map(PushExpr::Literal)
            }
            TypedExprKind::Ident {
                binding: IdentBinding::Field(field),
                depth: 0,
                ..
            } => {
                self.scalar_field(*field, source)?;
                Some(PushExpr::Field(*field))
            }
            TypedExprKind::Binary { op, lhs, rhs } => match op.value {
                BinaryOp::Eq
                | BinaryOp::Ne
                | BinaryOp::Lt
                | BinaryOp::Le
                | BinaryOp::Gt
                | BinaryOp::Ge => {
                    let op = match op.value {
                        BinaryOp::Eq => CmpOp::Eq,
                        BinaryOp::Ne => CmpOp::Ne,
                        BinaryOp::Lt => CmpOp::Lt,
                        BinaryOp::Le => CmpOp::Le,
                        BinaryOp::Gt => CmpOp::Gt,
                        BinaryOp::Ge => CmpOp::Ge,
                        _ => unreachable!(),
                    };
                    Some(PushExpr::Cmp {
                        op,
                        lhs: Box::new(self.translate_expr(lhs, source)?),
                        rhs: Box::new(self.translate_expr(rhs, source)?),
                    })
                }
                BinaryOp::And => Some(PushExpr::And(
                    Box::new(self.translate_expr(lhs, source)?),
                    Box::new(self.translate_expr(rhs, source)?),
                )),
                BinaryOp::Or => Some(PushExpr::Or(
                    Box::new(self.translate_expr(lhs, source)?),
                    Box::new(self.translate_expr(rhs, source)?),
                )),
                BinaryOp::In => {
                    let translated_lhs = self.translate_expr(lhs, source)?;
                    let scalar = self.push_scalar_type(&lhs.ty)?;
                    let TypedExprKind::Tuple { elements } = &rhs.kind else {
                        return None;
                    };
                    if elements.is_empty() {
                        return None;
                    }
                    let mut items = Vec::with_capacity(elements.len());
                    for element in elements {
                        if self.push_scalar_type(&element.ty)? != scalar {
                            return None;
                        }
                        let PushExpr::Literal(value) = self.translate_expr(element, source)? else {
                            return None;
                        };
                        items.push(PushExpr::Literal(value));
                    }
                    Some(PushExpr::In {
                        lhs: Box::new(translated_lhs),
                        items,
                    })
                }
                _ => None,
            },
            TypedExprKind::Unary { op, expr: inner } if op.value == UnaryOp::Not => {
                Some(PushExpr::Not(Box::new(self.translate_expr(inner, source)?)))
            }
            TypedExprKind::Method {
                base,
                name,
                args,
                opt_depth: 0,
            } => {
                let QueryType::Scalar(ScalarType::String) = &base.ty else {
                    return None;
                };
                match name.value.as_str() {
                    "contains" | "starts_with" | "ends_with" if args.len() == 1 => {
                        let op = match name.value.as_str() {
                            "contains" => StringOp::Contains,
                            "starts_with" => StringOp::StartsWith,
                            "ends_with" => StringOp::EndsWith,
                            _ => unreachable!(),
                        };
                        Some(PushExpr::StringOp {
                            op,
                            base: Box::new(self.translate_expr(base, source)?),
                            needle: Box::new(self.translate_expr(&args[0].expr, source)?),
                        })
                    }
                    "len" if args.is_empty() => Some(PushExpr::StringLen(Box::new(
                        self.translate_expr(base, source)?,
                    ))),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// Whole records expose owning Refs at output/structural observation, so
/// whole-ness propagates through owning Ref fields (resolution is lazy)
fn expand_whole(schema: &ValidatedSchema, usage: &mut HashMap<StructId, FieldPlan>) {
    let mut queue: Vec<StructId> = usage
        .iter()
        .filter(|(_, plan)| plan.whole)
        .map(|(struct_id, _)| *struct_id)
        .collect();
    while let Some(struct_id) = queue.pop() {
        let Some(struct_def) = schema.structs.get(struct_id.index()) else {
            continue;
        };
        for field in &struct_def.fields {
            if let Field::Ref {
                owning: true,
                target,
                ..
            } = field
            {
                let plan = usage.entry(*target).or_default();
                if !plan.whole {
                    plan.whole = true;
                    queue.push(*target);
                }
            }
        }
    }
}

fn method_base_need(method: &str, output: Need) -> Need {
    match method {
        "map" | "sum_over" | "avg_over" | "max_by" | "min_by" | "sum" | "avg" | "max" | "min"
        | "len" | "is_some" | "is_none" => Need::Narrow,
        "contains" => Need::Whole,
        "filter" | "sort" | "sort_asc" | "sort_desc" | "sort_over" | "sort_asc_over"
        | "sort_desc_over" | "take" | "skip" | "first" | "nth" | "flatten" | "extend"
        | "unwrap" | "unwrap_or" => output,
        _ => Need::Whole,
    }
}

fn const_int_arg(args: &[TypedMethodArg]) -> Option<u64> {
    let [arg] = args else {
        return None;
    };
    match &arg.expr.kind {
        TypedExprKind::Literal(literal) => match literal.value {
            Literal::Int(value) if value >= 0 => Some(value as u64),
            _ => None,
        },
        _ => None,
    }
}

fn apply_take(state: &mut ChainState, bound: u64) {
    let end = state.offset.saturating_add(bound);
    state.end = Some(state.end.map_or(end, |current| current.min(end)));
}

fn apply_skip(state: &mut ChainState, bound: u64) {
    let offset = state.offset.saturating_add(bound);
    state.offset = state.end.map_or(offset, |end| offset.min(end));
}

#[cfg(test)]
mod tests {
    use crate::testdata;

    use super::*;
    use grove_types::Severity;

    const FIXED_NOW: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

    fn typed(src: &str, schema: &ValidatedSchema) -> TypedQueryFile {
        let (file, parse_diags) = grove_query::parse_query(src);
        assert!(
            parse_diags
                .iter()
                .all(|diag| diag.severity == Severity::Warning),
            "{src}: {parse_diags:?}"
        );
        let (typed, diags) = grove_query::typecheck::typecheck(file.expect("parse"), schema);
        assert!(diags.is_empty(), "{src}: {diags:?}");
        typed.expect("typecheck")
    }

    fn typed_and_plan(src: &str) -> (TypedQueryFile, PushdownPlan, ValidatedSchema) {
        let schema = testdata::schema();
        let file = typed(src, &schema);
        let plan = PushdownPlan::analyze(&file, &schema, FIXED_NOW);
        (file, plan, schema)
    }

    fn walk(expr: &TypedExpr, f: &mut dyn FnMut(&TypedExpr)) {
        f(expr);
        match &expr.kind {
            TypedExprKind::Literal(_)
            | TypedExprKind::TypeConstant { .. }
            | TypedExprKind::EnumVariant { .. } => {}
            TypedExprKind::Ident { .. } => {}
            TypedExprKind::Field { base, .. } | TypedExprKind::TupleAccess { base, .. } => {
                walk(base, f);
            }
            TypedExprKind::Method { base, args, .. } => {
                walk(base, f);
                for arg in args {
                    walk(&arg.expr, f);
                }
            }
            TypedExprKind::Binary { lhs, rhs, .. } => {
                walk(lhs, f);
                walk(rhs, f);
            }
            TypedExprKind::Unary { expr: inner, .. }
            | TypedExprKind::Cast { expr: inner, .. }
            | TypedExprKind::Some { value: inner } => walk(inner, f),
            TypedExprKind::If { arms, default } => {
                for (condition, value) in arms {
                    walk(condition, f);
                    walk(value, f);
                }
                walk(default, f);
            }
            TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
                for element in elements {
                    walk(element, f);
                }
            }
            TypedExprKind::Struct { fields } => {
                for (_, value) in fields {
                    walk(value, f);
                }
            }
            TypedExprKind::Projection { base, items } => {
                walk(base, f);
                for item in items {
                    walk(&item.value, f);
                }
            }
        }
    }

    fn walk_file(file: &TypedQueryFile, f: &mut dyn FnMut(&TypedExpr)) {
        for statement in &file.statements {
            match statement {
                TypedStatement::Let(let_stmt) => walk(&let_stmt.value, f),
                TypedStatement::Expr(expr) => walk(expr, f),
                TypedStatement::Mutation(_) => {}
            }
        }
        walk(&file.result, f);
    }

    fn root_nodes(file: &TypedQueryFile) -> Vec<NodeId> {
        let mut ids = Vec::new();
        walk_file(file, &mut |expr| {
            if matches!(
                expr.kind,
                TypedExprKind::Ident {
                    binding: IdentBinding::Root { .. },
                    ..
                }
            ) {
                ids.push(expr.id);
            }
        });
        ids
    }

    fn method_nodes(file: &TypedQueryFile, name: &str) -> Vec<NodeId> {
        let mut ids = Vec::new();
        walk_file(file, &mut |expr| {
            if let TypedExprKind::Method {
                name: method_name, ..
            } = &expr.kind
                && method_name.value == name
            {
                ids.push(expr.id);
            }
        });
        ids
    }

    fn field_ident_nodes(file: &TypedQueryFile, field: FieldId) -> Vec<NodeId> {
        let mut ids = Vec::new();
        walk_file(file, &mut |expr| {
            if let TypedExprKind::Ident {
                binding: IdentBinding::Field(bound),
                ..
            } = &expr.kind
                && *bound == field
            {
                ids.push(expr.id);
            }
        });
        ids
    }

    fn root_constraints<'a>(plan: &'a PushdownPlan, file: &TypedQueryFile) -> &'a FetchConstraints {
        let roots = root_nodes(file);
        assert_eq!(roots.len(), 1, "expected exactly one root");
        plan.constraints_for(roots[0])
            .expect("root trigger has a constraints entry")
    }

    fn field(schema: &ValidatedSchema, struct_name: &str, name: &str) -> FieldId {
        testdata::field_id(schema, testdata::struct_id(schema, struct_name), name)
    }

    fn non_ref_field_names<'a>(schema: &'a ValidatedSchema, struct_name: &str) -> Vec<&'a str> {
        let struct_id = testdata::struct_id(schema, struct_name);
        schema.structs[struct_id.index()]
            .fields
            .iter()
            .filter(|field| !matches!(field, Field::Ref { .. }))
            .map(|field| field.name())
            .collect()
    }

    fn assert_fields(
        plan: &PushdownPlan,
        schema: &ValidatedSchema,
        struct_name: &str,
        expected: &[&str],
    ) {
        let struct_id = testdata::struct_id(schema, struct_name);
        let mut actual: Vec<&str> = plan
            .fields_for(struct_id)
            .iter()
            .map(|field| schema.structs[struct_id.index()].fields[field.local()].name())
            .collect();
        actual.sort_unstable();
        let mut expected = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(actual, expected, "fields for {struct_name}");
    }

    #[test]
    fn matcher_simple_predicates() {
        let sources = [
            "users[id > 2]",
            "users[name == \"Ada\"]",
            "users[active]",
            "users[active == true]",
            "users[balance > 30.00]",
            "users[created > @now]",
            "users[name.contains(\"A\")]",
            "users[name.starts_with(\"A\")]",
            "users[name.ends_with(\"a\")]",
            "users[name.len() > 3]",
            "users[id in (1, 2, 3)]",
            "users[active && id > 1]",
            "users[!(active)]",
        ];
        for source in sources {
            let (file, plan, _schema) = typed_and_plan(source);
            let constraints = root_constraints(&plan, &file);
            assert_eq!(constraints.filters.len(), 1, "{source}: {constraints:?}");
            assert!(constraints.sort.is_empty(), "{source}");
            assert_eq!(constraints.offset, 0, "{source}");
            assert_eq!(constraints.limit, None, "{source}");
        }
    }

    #[test]
    fn matcher_predicate_translation() {
        let (file, plan, schema) = typed_and_plan("users[active]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::Field(field(&schema, "User", "active"))]
        );

        let (file, plan, schema) = typed_and_plan("users[id in (1, 2, 3)]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::In {
                lhs: Box::new(PushExpr::Field(field(&schema, "User", "id"))),
                items: vec![
                    PushExpr::Literal(Scalar::Int(1)),
                    PushExpr::Literal(Scalar::Int(2)),
                    PushExpr::Literal(Scalar::Int(3)),
                ],
            }]
        );

        let (file, plan, schema) = typed_and_plan("users[created > @now]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::Cmp {
                op: CmpOp::Gt,
                lhs: Box::new(PushExpr::Field(field(&schema, "User", "created"))),
                rhs: Box::new(PushExpr::Literal(Scalar::Instant(FIXED_NOW))),
            }]
        );

        let (file, plan, schema) = typed_and_plan("users[name.contains(\"A\")]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::StringOp {
                op: StringOp::Contains,
                base: Box::new(PushExpr::Field(field(&schema, "User", "name"))),
                needle: Box::new(PushExpr::Literal(Scalar::String("A".to_string()))),
            }]
        );
    }

    #[test]
    fn matcher_enum_predicates() {
        let (file, plan, schema) = typed_and_plan("settings[priority == Priority::High]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(PushExpr::Field(field(&schema, "Setting", "priority"))),
                rhs: Box::new(PushExpr::Literal(Scalar::String("high".to_string()))),
            }]
        );

        let (file, plan, schema) = typed_and_plan("settings[level in (Level::High, Level::Low)]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::In {
                lhs: Box::new(PushExpr::Field(field(&schema, "Setting", "level"))),
                items: vec![
                    PushExpr::Literal(Scalar::Int(2)),
                    PushExpr::Literal(Scalar::Int(-1)),
                ],
            }]
        );

        let (file, plan, schema) = typed_and_plan("settings[priority == Priority::High] { name }");
        assert_eq!(root_constraints(&plan, &file).filters.len(), 1);
        assert_fields(&plan, &schema, "Setting", &["name", "priority"]);
    }

    #[test]
    fn optional_method_chains_are_not_pushed() {
        let (file, plan, _schema) = typed_and_plan("users.first()?.orders?.take(1)");
        let takes = method_nodes(&file, "take");
        assert_eq!(takes.len(), 1);
        assert_eq!(plan.cap_rewrite(takes[0]), None);
    }

    #[test]
    fn matcher_unsupported_predicates() {
        let sources = [
            "users[nickname == some(\"A\")]",
            "users[profile?.bio?.len() == some(6)]",
            "users[pos.0 > 1.5f]",
            "users[orders.len() > 1]",
            "users[id + 1 > 2]",
            "users[name.to_upper() == \"ADA\"]",
        ];
        for source in sources {
            let (file, plan, _schema) = typed_and_plan(source);
            let constraints = root_constraints(&plan, &file);
            assert!(
                constraints.filters.is_empty(),
                "{source} should not push: {constraints:?}"
            );
        }
    }

    #[test]
    fn filters_composition_across_unpushed() {
        let (file, plan, schema) =
            typed_and_plan("users[id > 2][orders.len() > 1][name == \"Ada\"]");
        let constraints = root_constraints(&plan, &file);
        assert_eq!(
            constraints.filters,
            vec![
                PushExpr::Cmp {
                    op: CmpOp::Gt,
                    lhs: Box::new(PushExpr::Field(field(&schema, "User", "id"))),
                    rhs: Box::new(PushExpr::Literal(Scalar::Int(2))),
                },
                PushExpr::Cmp {
                    op: CmpOp::Eq,
                    lhs: Box::new(PushExpr::Field(field(&schema, "User", "name"))),
                    rhs: Box::new(PushExpr::Literal(Scalar::String("Ada".to_string()))),
                },
            ]
        );

        let (file, plan, _schema) = typed_and_plan("users[orders.len() > 1].take(3)");
        let constraints = root_constraints(&plan, &file);
        assert!(constraints.filters.is_empty());
        assert_eq!(constraints.limit, None);
        let take = method_nodes(&file, "take");
        assert_eq!(take.len(), 1);
        assert_eq!(plan.cap_rewrite(take[0]), None);
    }

    #[test]
    fn sort_keys() {
        let (file, plan, schema) = typed_and_plan("users.sort_over(name, desc id)");
        assert_eq!(
            root_constraints(&plan, &file).sort,
            vec![
                SortKey {
                    field: field(&schema, "User", "name"),
                    desc: false,
                },
                SortKey {
                    field: field(&schema, "User", "id"),
                    desc: true,
                },
            ]
        );

        let (file, plan, schema) = typed_and_plan("users.sort_desc_over(id)");
        assert_eq!(
            root_constraints(&plan, &file).sort,
            vec![SortKey {
                field: field(&schema, "User", "id"),
                desc: true,
            }]
        );
    }

    #[test]
    fn unpushed_sort_composition() {
        let (file, plan, schema) =
            typed_and_plan("users.sort_over(orders.len()).filter(id > 2).sort_over(name)");
        let constraints = root_constraints(&plan, &file);
        assert!(constraints.sort.is_empty());
        assert_eq!(
            constraints.filters,
            vec![PushExpr::Cmp {
                op: CmpOp::Gt,
                lhs: Box::new(PushExpr::Field(field(&schema, "User", "id"))),
                rhs: Box::new(PushExpr::Literal(Scalar::Int(2))),
            }]
        );
    }

    #[test]
    fn cap_folding() {
        let cases: &[(&str, u64, Option<u64>)] = &[
            ("users.take(10)", 0, Some(10)),
            ("users.skip(10)", 10, None),
            ("users.take(30).skip(10)", 10, Some(20)),
            ("users.skip(10).take(30)", 10, Some(30)),
            ("users.take(10).take(5)", 0, Some(5)),
            ("users.take(5).take(10)", 0, Some(5)),
            ("users.skip(10).skip(20)", 30, None),
            ("users.skip(30).take(10).skip(5)", 35, Some(5)),
        ];
        for &(source, offset, limit) in cases {
            let (file, plan, _schema) = typed_and_plan(source);
            let constraints = root_constraints(&plan, &file);
            assert_eq!(
                (constraints.offset, constraints.limit),
                (offset, limit),
                "{source}"
            );
            for name in ["take", "skip"] {
                for node in method_nodes(&file, name) {
                    assert_eq!(
                        plan.cap_rewrite(node),
                        Some(CapRewrite::Skip),
                        "{source}: {name} should be folded"
                    );
                }
            }
        }
    }

    #[test]
    fn first_nth() {
        let (file, plan, _schema) = typed_and_plan("users.first()");
        let constraints = root_constraints(&plan, &file);
        assert_eq!((constraints.offset, constraints.limit), (0, Some(1)));
        for node in method_nodes(&file, "first") {
            assert_eq!(plan.cap_rewrite(node), None);
        }

        let (file, plan, _schema) = typed_and_plan("users.skip(2).nth(3)");
        let constraints = root_constraints(&plan, &file);
        assert_eq!((constraints.offset, constraints.limit), (5, Some(1)));
        for node in method_nodes(&file, "skip") {
            assert_eq!(plan.cap_rewrite(node), Some(CapRewrite::Skip));
        }
        for node in method_nodes(&file, "nth") {
            assert_eq!(plan.cap_rewrite(node), Some(CapRewrite::First));
        }

        let (file, plan, _schema) = typed_and_plan("users.take(10).nth(3)");
        let constraints = root_constraints(&plan, &file);
        assert_eq!((constraints.offset, constraints.limit), (3, Some(1)));

        let (file, plan, _schema) = typed_and_plan("users.nth(-1)");
        let constraints = root_constraints(&plan, &file);
        assert_eq!((constraints.offset, constraints.limit), (0, None));
        for node in method_nodes(&file, "nth") {
            assert_eq!(plan.cap_rewrite(node), None);
        }
    }

    #[test]
    fn caps_composition() {
        let (file, plan, _schema) = typed_and_plan("users[id > 2].sort_over(name).take(3)");
        let constraints = root_constraints(&plan, &file);
        assert_eq!(constraints.filters.len(), 1);
        assert_eq!(constraints.sort.len(), 1);
        assert_eq!((constraints.offset, constraints.limit), (0, Some(3)));
        let roots = root_nodes(&file);
        assert_eq!(roots.len(), 1);
        for take in method_nodes(&file, "take") {
            assert_eq!(
                plan.cap_plan(take).expect("take cap planned").trigger,
                roots[0]
            );
        }

        let (file, plan, _schema) = typed_and_plan("users[id > 2].take(3).sort_over(name)");
        let constraints = root_constraints(&plan, &file);
        assert_eq!(constraints.filters.len(), 1);
        assert!(constraints.sort.is_empty());
        assert_eq!((constraints.offset, constraints.limit), (0, Some(3)));

        let (file, plan, _schema) = typed_and_plan("users.take(10).sort_over(id)");
        let constraints = root_constraints(&plan, &file);
        assert!(constraints.sort.is_empty());
        assert_eq!((constraints.offset, constraints.limit), (0, Some(10)));

        let (file, plan, _schema) = typed_and_plan("users.take(10)[id > 2]");
        let constraints = root_constraints(&plan, &file);
        assert!(constraints.filters.is_empty());
        assert_eq!((constraints.offset, constraints.limit), (0, Some(10)));
    }

    #[test]
    fn pushdown_chain_termination_on_projection() {
        let (file, plan, schema) = typed_and_plan("users[id > 2].take(3) { name }");
        let constraints = root_constraints(&plan, &file);
        assert_eq!(constraints.filters.len(), 1);
        assert_eq!((constraints.offset, constraints.limit), (0, Some(3)));
        assert_fields(&plan, &schema, "User", &["id", "name"]);

        let (file, plan, schema) = typed_and_plan("users { o = orders.take(2) }");
        let constraints = root_constraints(&plan, &file);
        assert_eq!((constraints.offset, constraints.limit), (0, None));
        let orders = testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "orders");
        let orders_sites = field_ident_nodes(&file, orders);
        assert_eq!(orders_sites.len(), 1);
        assert_eq!(
            plan.constraints_for(orders_sites[0]),
            Some(&FetchConstraints {
                filters: vec![],
                sort: vec![],
                offset: 0,
                limit: Some(2),
            })
        );
        let take = method_nodes(&file, "take");
        assert_eq!(take.len(), 1);
        assert_eq!(
            plan.cap_plan(take[0]),
            Some(CapPlan {
                rewrite: CapRewrite::Skip,
                trigger: orders_sites[0],
            })
        );
        assert!(plan.prefetch_skip.contains(&orders));
    }

    #[test]
    fn related_ident_chains() {
        let (file, plan, schema) =
            typed_and_plan("users { o = orders[amount > 100].take(1), r = roles.take(1) }");
        let user = testdata::struct_id(&schema, "User");
        let orders = testdata::field_id(&schema, user, "orders");
        let roles = testdata::field_id(&schema, user, "roles");

        let orders_sites = field_ident_nodes(&file, orders);
        assert_eq!(orders_sites.len(), 1);
        let orders_plan = plan
            .constraints_for(orders_sites[0])
            .expect("orders trigger constraints");
        assert_eq!(orders_plan.filters.len(), 1);
        assert_eq!(orders_plan.limit, Some(1));

        let roles_sites = field_ident_nodes(&file, roles);
        assert_eq!(roles_sites.len(), 1);
        let roles_plan = plan
            .constraints_for(roles_sites[0])
            .expect("roles trigger constraints");
        assert!(roles_plan.filters.is_empty());
        assert_eq!(roles_plan.limit, Some(1));

        assert!(plan.prefetch_skip.contains(&orders));
        assert!(plan.prefetch_skip.contains(&roles));
        for take in method_nodes(&file, "take") {
            let cap = plan.cap_plan(take).expect("take cap planned");
            assert_eq!(cap.rewrite, CapRewrite::Skip);
        }
    }

    #[test]
    fn related_empty_chain() {
        let (file, plan, schema) = typed_and_plan("users { o = orders.len() }");
        let orders = testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "orders");
        let sites = field_ident_nodes(&file, orders);
        assert_eq!(sites.len(), 1);
        let constraints = plan
            .constraints_for(sites[0])
            .expect("related chain entry exists");
        assert!(constraints.is_empty());
        assert!(!plan.prefetch_skip.contains(&orders));
        let len = method_nodes(&file, "len");
        assert_eq!(len.len(), 1);
        assert_eq!(plan.cap_plan(len[0]), None);
    }

    #[test]
    fn related_field_chains() {
        let (file, plan, schema) = typed_and_plan("users.map($.orders.take(2).len())");
        let orders = testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "orders");
        let take = method_nodes(&file, "take");
        assert_eq!(take.len(), 1);
        let cap = plan.cap_plan(take[0]).expect("take cap planned");
        assert_eq!(cap.rewrite, CapRewrite::Skip);
        let constraints = plan
            .constraints_for(cap.trigger)
            .expect("field trigger constraints");
        assert_eq!(constraints.limit, Some(2));
        assert!(plan.prefetch_skip.contains(&orders));

        let (file, plan, _schema) = typed_and_plan("users.map($.roles.skip(1).len())");
        let skip = method_nodes(&file, "skip");
        assert_eq!(skip.len(), 1);
        let cap = plan.cap_plan(skip[0]).expect("take cap planned");
        assert_eq!(
            plan.constraints_for(cap.trigger)
                .expect("field trigger constraints")
                .offset,
            1
        );
        let roles = testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "roles");
        assert!(plan.prefetch_skip.contains(&roles));
    }

    #[test]
    fn list_base_field_access_is_not_pushdown_trigger() {
        let (file, plan, _schema) = typed_and_plan("users.orders.take(2).len()");
        let take = method_nodes(&file, "take");
        assert_eq!(take.len(), 1);
        assert_eq!(plan.cap_plan(take[0]), None);
        assert!(plan.prefetch_skip.is_empty());
    }

    #[test]
    fn let_pushdown_chain() {
        let (file, plan, _schema) = typed_and_plan("let u = users[id > 2].take(3); u");
        let constraints = root_constraints(&plan, &file);
        assert_eq!(constraints.filters.len(), 1);
        assert_eq!((constraints.offset, constraints.limit), (0, Some(3)));
        for node in method_nodes(&file, "take") {
            assert_eq!(plan.cap_rewrite(node), Some(CapRewrite::Skip));
        }
    }

    #[test]
    fn discarded_statements_ignored() {
        let (file, plan, schema) = typed_and_plan("users[id > 2]; users { name }");
        let roots = root_nodes(&file);
        assert_eq!(roots.len(), 2);
        assert!(plan.constraints_for(roots[0]).is_none());
        assert!(plan.constraints_for(roots[1]).is_some());
        assert_eq!(plan.fetches.len(), 1);
        assert_fields(&plan, &schema, "User", &["name"]);
    }

    #[test]
    fn liveness_projection_prunes_fields() {
        let (_file, plan, schema) = typed_and_plan("users { name }");
        assert_fields(&plan, &schema, "User", &["name"]);
        assert_fields(&plan, &schema, "Order", &[]);
        assert_fields(&plan, &schema, "Profile", &[]);
    }

    #[test]
    fn liveness_whole_record() {
        let (_file, plan, schema) = typed_and_plan("users");
        assert_fields(
            &plan,
            &schema,
            "User",
            &non_ref_field_names(&schema, "User"),
        );
        assert_fields(
            &plan,
            &schema,
            "Profile",
            &non_ref_field_names(&schema, "Profile"),
        );
        assert_fields(
            &plan,
            &schema,
            "Order",
            &non_ref_field_names(&schema, "Order"),
        );
        assert_fields(&plan, &schema, "Role", &[]);

        let (_file, plan, schema) = typed_and_plan("users { m = manager }");
        assert_fields(
            &plan,
            &schema,
            "User",
            &non_ref_field_names(&schema, "User"),
        );

        let (_file, plan, schema) = typed_and_plan("let u = users; u");
        assert_fields(
            &plan,
            &schema,
            "User",
            &non_ref_field_names(&schema, "User"),
        );

        let (_file, plan, schema) = typed_and_plan("roles");
        assert_fields(
            &plan,
            &schema,
            "Role",
            &non_ref_field_names(&schema, "Role"),
        );
        assert_fields(&plan, &schema, "User", &[]);
    }

    #[test]
    fn liveness_partial() {
        let (_file, plan, schema) = typed_and_plan("users { o = orders { amount } }");
        assert_fields(&plan, &schema, "User", &[]);
        assert_fields(&plan, &schema, "Order", &["amount"]);

        let (_file, plan, schema) = typed_and_plan("users { p = profile?.bio }");
        assert_fields(&plan, &schema, "User", &[]);
        assert_fields(&plan, &schema, "Profile", &["bio"]);

        let (_file, plan, schema) = typed_and_plan("users { p = profile }");
        assert_fields(&plan, &schema, "User", &[]);
        assert_fields(
            &plan,
            &schema,
            "Profile",
            &non_ref_field_names(&schema, "Profile"),
        );

        let (_file, plan, schema) = typed_and_plan("users[orders.len() > 1]");
        assert_fields(
            &plan,
            &schema,
            "User",
            &non_ref_field_names(&schema, "User"),
        );
        assert_fields(
            &plan,
            &schema,
            "Order",
            &non_ref_field_names(&schema, "Order"),
        );

        let (_file, plan, schema) = typed_and_plan("users.len()");
        assert_fields(&plan, &schema, "User", &[]);

        let (_file, plan, schema) = typed_and_plan("users { c = orders.len() }");
        assert_fields(&plan, &schema, "User", &[]);
        assert_fields(&plan, &schema, "Order", &[]);

        let (_file, plan, schema) = typed_and_plan("users[active].sort_over(name) { id }");
        assert_fields(&plan, &schema, "User", &["id", "active", "name"]);

        let (_file, plan, schema) = typed_and_plan("users { e = extra.0 }");
        assert_fields(&plan, &schema, "User", &["extra"]);

        let (_file, plan, schema) = typed_and_plan("users.map($.name)");
        assert_fields(&plan, &schema, "User", &["name"]);
    }

    #[test]
    fn disabled_pushdown_plan() {
        let schema = testdata::schema();
        let plan = PushdownPlan::disabled(&schema);
        assert!(!plan.is_enabled());
        assert!(plan.fetches.is_empty());
        assert!(plan.caps.is_empty());
        assert!(plan.prefetch_skip.is_empty());
        for struct_def in &schema.structs {
            let struct_id = testdata::struct_id(&schema, &struct_def.name);
            let expected: Vec<&str> = struct_def
                .fields
                .iter()
                .filter(|field| !matches!(field, Field::Ref { .. }))
                .map(|field| field.name())
                .collect();
            assert_fields(&plan, &schema, &struct_def.name, &expected);
            assert_eq!(
                plan.fields_for(struct_id).len(),
                expected.len(),
                "{}",
                struct_def.name
            );
        }
    }
}
