
use std::collections::HashMap;

use super::builder::{
    AliasGen, FromClause, MiddlePart, NamedColumn, OrderDir, OrderItem, SelectBuilder, SelectItem,
    SqlBinOp, SqlExpr,
};
use super::model::{
    CROSS_PAIR_TAG, Cardinality, ChildRef, ColumnRole, OutputColumn, ParamValue, ParentRef,
    ResultShape, RowCount, StatementKind, StatementShape, SunkStatement,
};
use crate::ast::{BinaryOp, ConstantName, Literal, SortDir, TypeName, UnaryOp};
use crate::typecheck::types::{
    IdentBinding, ProjectionField, QueryType, RecordSource, TypedExpr, TypedExprKind,
    TypedMethodArg, TypedProjectionItem,
};
use grove_schema::validated::{
    Field, FieldId, Relation, ScalarType, StructId, ValidatedSchema, ValueType,
};

pub struct Context<'s> {
    pub schema: &'s ValidatedSchema,
    pub aliases: AliasGen,
    pub statements: Vec<SunkStatement>,
    pub literals: HashMap<String, Vec<Vec<ParamValue>>>,
    pub no_outer: bool,
    pub in_take_arg: bool,
    pub corr: bool,
    pub child: Option<ChildCtx>,
    pub nest_depth: usize,
    pub temp_collect: Option<TempCollect>,
    pub in_array_temp: bool,
}

pub struct ChildCtx {
    pub base: Option<Level>,
    pub per_row: bool,
    pub partition: Option<String>,
    pub anchor: Option<StructId>,
}

fn row_struct(ty: &QueryType) -> Option<StructId> {
    match peel_optional(ty) {
        QueryType::List(inner) => match peel_optional(inner) {
            QueryType::Record(RecordSource::Schema(sid)) => Some(*sid),
            _ => None,
        },
        _ => None,
    }
}

fn tail_row_struct(ctx: &Context, tail: &TypedExpr) -> Option<StructId> {
    match tail_root_ident(tail)? {
        IdentBinding::Field(fid) => {
            match &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()] {
                Field::Ref { target, .. } => Some(*target),
                _ => None,
            }
        }
        IdentBinding::Root { root_idx } => Some(ctx.schema.roots[*root_idx].struct_id),
        _ => None,
    }
}

pub struct TempCollect {
    pub temp: String,
    pub child_fk: String,
    pub refs: Vec<String>,
}

impl<'s> Context<'s> {
    pub fn new(schema: &'s ValidatedSchema) -> Self {
        Context {
            schema,
            aliases: AliasGen::default(),
            statements: Vec::new(),
            literals: HashMap::new(),
            no_outer: false,
            in_take_arg: false,
            corr: false,
            child: None,
            nest_depth: 0,
            temp_collect: None,
            in_array_temp: false,
        }
    }

    pub fn sink(
        &mut self,
        builder: SelectBuilder,
        shape: StatementShape,
        kind: StatementKind,
    ) -> usize {
        let idx = self.statements.len() + 1;
        self.statements.push(SunkStatement {
            builder,
            shape,
            kind,
        });
        idx
    }
}

#[allow(dead_code)]
pub enum ParentLink {
    Root { root_idx: usize },
}

pub struct PendingSplit {
    pub child_from: FromClause,
    pub child_col: String,
    pub parent_col: String,
    pub multi_parent: bool,
    pub next: Option<Box<PendingSplit>>,
    pub middle_parts: Option<Vec<MiddlePart>>,
}

pub struct Level {
    pub builder: SelectBuilder,
    pub element: QueryType,
    pub parent: ParentLink,
    pub pending: Option<PendingSplit>,
    pub stmt_bound: bool,
    pub output_shapes: Option<Vec<(String, Option<ResultShape>)>>,
    pub root_parts: Option<Vec<MiddlePart>>,
}

fn struct_field_value<'a>(base: &'a TypedExpr, name: &str) -> Option<&'a TypedExpr> {
    match &base.kind {
        TypedExprKind::Struct { fields } => fields
            .iter()
            .find(|(field_name, _)| field_name.value == name)
            .map(|(_, expr)| expr),
        _ => None,
    }
}

fn beta_reduce_tail(tail: &TypedExpr) -> std::borrow::Cow<'_, TypedExpr> {
    let mut current = std::borrow::Cow::Borrowed(tail);
    loop {
        match reduce_step(&current) {
            None => return current,
            Some(next) => current = std::borrow::Cow::Owned(next),
        }
    }
}

fn reduce_step(node: &TypedExpr) -> Option<TypedExpr> {
    match &node.kind {
        TypedExprKind::Field { base, name, .. } => {
            if let Some(value) = struct_field_value(base, &name.value) {
                return Some(value.clone());
            }
            match beta_reduce_tail(base) {
                std::borrow::Cow::Borrowed(_) => None,
                std::borrow::Cow::Owned(base) => {
                    let mut reduced = node.clone();
                    if let TypedExprKind::Field { base: slot, .. } = &mut reduced.kind {
                        **slot = base;
                    }
                    Some(reduced)
                }
            }
        }
        TypedExprKind::Method { base, .. } | TypedExprKind::Projection { base, .. } => {
            match beta_reduce_tail(base) {
                std::borrow::Cow::Borrowed(_) => None,
                std::borrow::Cow::Owned(base) => {
                    let mut reduced = node.clone();
                    match &mut reduced.kind {
                        TypedExprKind::Method { base: slot, .. }
                        | TypedExprKind::Projection { base: slot, .. } => {
                            **slot = base;
                        }
                        _ => unreachable!("tail spine changed shape under rebuild"),
                    }
                    Some(reduced)
                }
            }
        }
        _ => None,
    }
}

pub fn scalar(ctx: &mut Context, expr: &TypedExpr) -> SqlExpr {
    match &expr.kind {
        TypedExprKind::Literal(lit) => scalar_literal(&lit.value),
        TypedExprKind::TypeConstant { ty, name } => scalar_constant(ty.value, name.value),
        TypedExprKind::Ident { binding, depth, .. } => scalar_ident(ctx, binding, *depth),
        TypedExprKind::Field { base, name, .. } => {
            if let Some(value) = struct_field_value(base, &name.value) {
                return scalar(ctx, value);
            }
            scalar_field(ctx, base, name)
        }
        TypedExprKind::Method {
            base,
            name,
            args,
            optional,
        } => scalar_method(ctx, base, name, args, *optional),
        TypedExprKind::Binary { op, lhs, rhs } => scalar_binary(ctx, op, lhs, rhs),
        TypedExprKind::Unary { op, expr } => scalar_unary(op, scalar(ctx, expr), &expr.ty),
        TypedExprKind::Cast { expr, ty } => scalar_cast(scalar(ctx, expr), &expr.ty, ty.value),
        TypedExprKind::If { arms, default } => SqlExpr::Case {
            arms: arms
                .iter()
                .map(|(cond, then)| (scalar(ctx, cond), scalar(ctx, then)))
                .collect(),
            else_: Box::new(scalar(ctx, default)),
        },
        TypedExprKind::Some { value } => scalar(ctx, value),
        TypedExprKind::Tuple { .. }
        | TypedExprKind::Array { .. }
        | TypedExprKind::Struct { .. }
        | TypedExprKind::Projection { .. } => {
            unreachable!(
                "scalar() received {} {}: callers dispatch on node type, composite/row nodes never enter scalar demand",
                kind_name(expr),
                expr.ty
            )
        }
    }
}

fn kind_name(expr: &TypedExpr) -> &'static str {
    match &expr.kind {
        TypedExprKind::Literal(_) => "literal",
        TypedExprKind::Ident { .. } => "ident",
        TypedExprKind::Field { .. } => "field access",
        TypedExprKind::Method { .. } => "method call",
        TypedExprKind::Binary { .. } => "binary operator",
        TypedExprKind::Unary { .. } => "unary operator",
        TypedExprKind::Cast { .. } => "cast",
        TypedExprKind::If { .. } => "if",
        TypedExprKind::Some { .. } => "some()",
        TypedExprKind::Tuple { .. } => "tuple",
        TypedExprKind::Array { .. } => "array",
        TypedExprKind::Struct { .. } => "struct literal",
        TypedExprKind::Projection { .. } => "projection",
        TypedExprKind::TypeConstant { .. } => "type constant",
    }
}

pub fn rows(ctx: &mut Context, expr: &TypedExpr) -> Level {
    let mut level = match &expr.kind {
        TypedExprKind::Ident { binding, .. } => rows_ident(ctx, binding, &expr.ty),
        TypedExprKind::Field {
            base,
            name,
            optional,
        } => rows_field(ctx, base, name, *optional),
        TypedExprKind::Method {
            base,
            name,
            args,
            optional,
        } => rows_method(ctx, base, name, args, *optional),
        TypedExprKind::Projection { base, items } => rows_projection(ctx, base, items),
        TypedExprKind::Array { .. } => return rows_array(ctx, expr),
        TypedExprKind::If { .. } => return rows_if(ctx, expr),
        TypedExprKind::Tuple { .. } => unreachable!(
            "tuple-typed node in row demand: row slots admit lists/records only — Tuple-kind nodes infer Tuple types, which no method base, field base, or projection base signature admits"
        ),
        TypedExprKind::Struct { .. } => unreachable!(
            "struct-literal node in row demand: row slots admit lists/records only — Struct-kind nodes infer Record types, which no row-slot signature admits (struct literals route via lower_struct_result)"
        ),
        TypedExprKind::Some { value } => {
            let mut level = rows(ctx, value);
            level.element = value.ty.clone();
            return level;
        }
        TypedExprKind::Literal(_)
        | TypedExprKind::TypeConstant { .. }
        | TypedExprKind::Binary { .. }
        | TypedExprKind::Unary { .. }
        | TypedExprKind::Cast { .. } => {
            unreachable!(
                "row demand received scalar {}: callers dispatch on node type",
                expr.ty
            )
        }
    };
    level.element = expr.ty.clone();
    level
}

fn rows_ident(ctx: &mut Context, binding: &IdentBinding, ty: &QueryType) -> Level {
    if let Some(tail) = ctx.child.as_mut() {
        if let Some(base) = tail.base.take() {
            return base;
        }
        match binding {
            IdentBinding::Root { .. } => todo!("M4b cross-root query inside child tail"),
            IdentBinding::Field(_) => todo!("M4b nested collection root inside child tail"),
            IdentBinding::ProjectedField { .. } => todo!("M4b projected root inside child tail"),
            IdentBinding::Prev { .. } => todo!("mutations era: prev"),
        }
    }
    match binding {
        IdentBinding::Root { root_idx } => fresh_root_level(ctx, *root_idx, ty),
        IdentBinding::Field(fid) if ctx.corr => corr_base_level(ctx, *fid, ty),
        _ => todo!("M4b scalar aggregation over derived rows"),
    }
}

fn corr_base_level(ctx: &mut Context, fid: FieldId, ty: &QueryType) -> Level {
    let link = resolve_tail_link(ctx, fid);
    if link.m2m_join.is_some() {
        let parts = m2m_parts(ctx.schema, fid).unwrap_or_else(|| {
            unreachable!("M2M link without junction parts: resolve_tail_link produced them");
        });
        let junction_alias = ctx.aliases.table(&parts.junction_table);
        let mut subquery = SelectBuilder::new();
        subquery.select = vec![SelectItem::Column {
            depth: 0,
            column: parts.junction_target_col.clone(),
            output: parts.junction_target_col.clone(),
            ty: None,
        }];
        subquery.from = Some(FromClause::Table {
            table: parts.junction_table.clone(),
            alias: junction_alias.clone(),
        });
        subquery.where_ = vec![SqlExpr::Binary {
            op: SqlBinOp::Eq,
            lhs: Box::new(SqlExpr::Column {
                depth: 0,
                column: parts.junction_parent_col.clone(),
            }),
            rhs: Box::new(SqlExpr::Column {
                depth: 2,
                column: link.parent_col.clone(),
            }),
        }];
        let target_alias = ctx.aliases.table(&parts.target_table);
        let mut builder = SelectBuilder::new();
        builder.from = Some(FromClause::Table {
            table: parts.target_table.clone(),
            alias: target_alias,
        });
        builder.where_ = vec![SqlExpr::InSubquery {
            expr: Box::new(SqlExpr::Column {
                depth: 0,
                column: parts.target_pk.clone(),
            }),
            query: Box::new(subquery),
        }];
        return Level {
            builder,
            element: ty.clone(),
            parent: ParentLink::Root { root_idx: 0 },
            pending: None,
            stmt_bound: true,
            output_shapes: None,
            root_parts: None,
        };
    }
    let mut builder = SelectBuilder::new();
    builder.from = Some(link.child_from);
    builder.where_ = vec![SqlExpr::Binary {
        op: SqlBinOp::Eq,
        lhs: Box::new(SqlExpr::Column {
            depth: 0,
            column: link.child_col,
        }),
        rhs: Box::new(SqlExpr::Column {
            depth: 1,
            column: link.parent_col,
        }),
    }];
    Level {
        builder,
        element: ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    }
}

fn fresh_root_level(ctx: &mut Context, root_idx: usize, ty: &QueryType) -> Level {
    let root = &ctx.schema.roots[root_idx];
    let table = ctx.schema.tables[ctx.schema.structs[root.struct_id.index()].table.index()]
        .name
        .clone();
    let mut builder = SelectBuilder::new();
    builder.from = Some(FromClause::Table {
        alias: ctx.aliases.table(&table),
        table,
    });
    Level {
        builder,
        element: ty.clone(),
        parent: ParentLink::Root { root_idx },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    }
}

fn rows_field(
    ctx: &mut Context,
    base: &TypedExpr,
    name: &grove_types::Spanned<String>,
    optional: bool,
) -> Level {
    let _ = optional;
    if let Some(value) = struct_field_value(base, &name.value) {
        return rows(ctx, value);
    }
    if matches!(base.ty, QueryType::Record(_)) {
        todo!("M4b navigation from single-record base");
    }
    let mut level = rows(ctx, base);
    let inner = match &base.ty {
        QueryType::List(inner) => inner.as_ref(),
        QueryType::Optional(inner) => match inner.as_ref() {
            QueryType::List(inner) => inner.as_ref(),
            record @ QueryType::Record(_) => record,
            other => {
                unreachable!("field base ?{other}: optionals wrap records or lists only")
            }
        },
        QueryType::Record(_) => {
            unreachable!("single-record bases divert above")
        }
        other => {
            unreachable!(
                "field base {other} is not a list: the checker maps fields over lists only"
            )
        }
    };
    match inner {
        QueryType::Record(RecordSource::Schema(struct_id)) => {
            let (fid, field) = ctx
                .schema
                .struct_field(*struct_id, &name.value)
                .unwrap_or_else(|| {
                    unreachable!("resolved field {} vanished from the schema", name.value)
                });
            match field {
                Field::Value { columns, ty, .. } => {
                    let [column] = columns.as_slice() else {
                        if level.pending.is_some() {
                            unreachable!(
                                "tuple field over split-pending rows: field access over nested lists is rejected upstream"
                            )
                        }
                        let ValueType::Tuple(items) = ty else {
                            unreachable!(
                                "multi-column field with non-tuple type: storage claims one column per scalar"
                            )
                        };
                        for (column, item_ty) in columns.iter().zip(items.iter()) {
                            level.builder.select.push(SelectItem::Column {
                                depth: 0,
                                column: ctx.schema.column_name(*column).to_string(),
                                output: ctx.schema.column_name(*column).to_string(),
                                ty: value_scalar(item_ty),
                            });
                        }
                        return level;
                    };
                    level.builder.select.push(SelectItem::Column {
                        depth: 0,
                        column: ctx.schema.column_name(*column).to_string(),
                        output: name.value.clone(),
                        ty: value_scalar(ty),
                    });
                    level
                }
                Field::Ref {
                    target,
                    is_list,
                    optional: field_optional,
                    ..
                } => navigate_ref(
                    ctx,
                    level,
                    matches!(base.ty, QueryType::List(_)),
                    fid,
                    *target,
                    *is_list,
                    *field_optional,
                ),
                Field::Array {
                    element,
                    link,
                    storage,
                    ..
                } => navigate_array(ctx, level, element, *link, storage),
            }
        }
        QueryType::Record(RecordSource::Projection(fields)) => {
            let field = fields
                .iter()
                .find(|f| f.name == name.value)
                .unwrap_or_else(|| {
                    unreachable!("resolved projection field {} vanished", name.value)
                });
            if !field.ty.is_scalar() {
                todo!("M4 non-scalar projection field access");
            }
            level.builder.select.push(SelectItem::Column {
                depth: 0,
                column: name.value.clone(),
                output: name.value.clone(),
                ty: scalar_of(&field.ty),
            });
            level
        }
        QueryType::List(_) => {
            unreachable!("field access over nested lists is rejected upstream")
        }
        other => {
            unreachable!("field access over {other} is rejected upstream")
        }
    }
}

fn navigate_ref(
    ctx: &mut Context,
    mut level: Level,
    multi_parent: bool,
    fid: FieldId,
    target: StructId,
    is_list: bool,
    field_optional: bool,
) -> Level {
    if !level.builder.unions.is_empty() {
        level = wrap_level(ctx, level);
    }
    let table = ctx.schema.tables[ctx.schema.structs[target.index()].table.index()]
        .name
        .clone();
    let child_from = FromClause::Table {
        table: table.clone(),
        alias: ctx.aliases.table(&table),
    };
    let (child_col, parent_col) = match link_cols(ctx.schema, fid) {
        LinkCols::ParentFk {
            parent_fk,
            child_pk,
        } => (child_pk, parent_fk),
        LinkCols::TargetFk {
            child_fk,
            parent_pk,
        } => (child_fk, parent_pk),
        LinkCols::ManyToMany => todo!("M4 many-to-many navigation"),
    };
    if is_list {
        if field_optional {
            todo!("M4 R-presence: optional inner list");
        }
        let Level {
            builder,
            element,
            parent,
            pending,
            stmt_bound,
            output_shapes,
            root_parts,
        } = level;
        if pending.is_some() {
            unreachable!("collection navigation over a split-pending level");
        }
        Level {
            builder,
            element,
            parent,
            pending: Some(PendingSplit {
                child_from,
                child_col,
                parent_col,
                multi_parent,
                next: None,
                middle_parts: None,
            }),
            stmt_bound,
            output_shapes,
            root_parts,
        }
    } else {
        merge_navigate(ctx, child_from, child_col, level, parent_col)
    }
}

fn navigate_array(
    ctx: &mut Context,
    mut level: Level,
    element: &grove_schema::validated::ValueType,
    link: grove_schema::validated::ColumnId,
    storage: &grove_schema::validated::StorageTable,
) -> Level {
    use grove_schema::validated::ValueType;
    if !level.builder.unions.is_empty() {
        level = wrap_level(ctx, level);
    }
    let table = ctx.schema.tables[storage.table.index()].name.clone();
    let child_from = FromClause::Table {
        table: table.clone(),
        alias: ctx.aliases.table(&table),
    };
    let child_col = ctx.schema.column_name(storage.key).to_string();
    let parent_col = ctx.schema.column_name(link).to_string();
    match element {
        ValueType::Optional(_) => todo!("M4 R-presence: optional inner list"),
        ValueType::Array { .. } => {
            let Level {
                builder,
                element,
                parent,
                pending,
                stmt_bound,
                output_shapes,
                root_parts,
            } = level;
            if pending.is_some() {
                unreachable!("collection navigation over a split-pending level");
            }
            Level {
                builder,
                element,
                parent,
                pending: Some(PendingSplit {
                    child_from,
                    child_col,
                    parent_col,
                    multi_parent: true,
                    next: None,
                    middle_parts: None,
                }),
                stmt_bound,
                output_shapes,
                root_parts,
            }
        }
        _ => {
            if ctx.child.is_some() {
                level.element = QueryType::List(Box::new(QueryType::from(element)));
                return merge_navigate(ctx, child_from, child_col, level, parent_col);
            }
            let Level {
                builder,
                element,
                parent,
                pending,
                stmt_bound,
                output_shapes,
                root_parts,
            } = level;
            if pending.is_some() {
                unreachable!("collection navigation over a split-pending level");
            }
            Level {
                builder,
                element,
                parent,
                pending: Some(PendingSplit {
                    child_from,
                    child_col,
                    parent_col,
                    multi_parent: true,
                    next: None,
                    middle_parts: None,
                }),
                stmt_bound,
                output_shapes,
                root_parts,
            }
        }
    }
}

fn merge_navigate(
    ctx: &mut Context,
    child_from: FromClause,
    child_col: String,
    level: Level,
    parent_col: String,
) -> Level {
    let _ = ctx;
    let Level {
        builder,
        element,
        parent,
        pending,
        stmt_bound,
        output_shapes,
        root_parts,
    } = level;
    if pending.is_some() {
        unreachable!("merge over a split-pending level");
    }
    Level {
        builder: merge_builder(
            child_from,
            child_col,
            builder,
            parent_col,
            ctx.schema,
            matches!(peel_optional(&element), QueryType::List(sub)
            if matches!(
                peel_optional(sub),
                QueryType::Scalar(_) | QueryType::Tuple(_)
            )),
        ),
        element,
        parent,
        pending: None,
        stmt_bound,
        output_shapes,
        root_parts,
    }
}

fn parts_named(parts: &[MiddlePart]) -> bool {
    parts.iter().any(|part| match part {
        MiddlePart::Value { name, .. } | MiddlePart::Nested { name, .. } => name.is_some(),
        MiddlePart::Tuple(inner) => parts_named(inner),
        MiddlePart::Record { fields, .. } => parts_named(fields),
    })
}

fn narrow_element(element: QueryType) -> QueryType {
    match peel_optional(&element).clone() {
        QueryType::List(sub) => *sub,
        _ => element,
    }
}

fn merge_builder(
    child_from: FromClause,
    child_col: String,
    mut parent_q: SelectBuilder,
    parent_col: String,
    schema: &ValidatedSchema,
    project_values: bool,
) -> SelectBuilder {
    let parent_ty = output_ty(&parent_q, &parent_col, schema);
    parent_q.select = vec![SelectItem::Column {
        depth: 0,
        column: parent_col.clone(),
        output: parent_col,
        ty: parent_ty,
    }];
    let mut child = SelectBuilder::new();
    if let FromClause::Literal {
        columns,
        middle_parts,
        ..
    }
    | FromClause::Computed {
        columns,
        middle_parts,
        ..
    } = &child_from
    {
        if middle_parts
            .as_ref()
            .is_some_and(|parts| parts_named(parts))
        {
            todo!("M7 flatten over record literals");
        }
        child.select = columns
            .iter()
            .filter(|c| c.name != child_col)
            .enumerate()
            .map(|(i, column)| SelectItem::Column {
                depth: 0,
                column: column.name.clone(),
                output: format!("c{i}"),
                ty: column.ty,
            })
            .collect();
        if child.select.is_empty() {
            unreachable!(
                "merge over a key-only literal level: the depth guard keeps merges on leaves"
            );
        }
    } else if project_values {
        child.select = from_columns_typed(&Some(child_from.clone()), schema)
            .into_iter()
            .filter(|(name, _)| name != &child_col)
            .map(|(name, ty)| SelectItem::Column {
                depth: 0,
                column: name.clone(),
                output: name,
                ty,
            })
            .collect();
        if child.select.is_empty() {
            unreachable!(
                "merge over a key-only storage level: scalar/tuple elements always serve a value column"
            );
        }
    }
    child.from = Some(child_from);
    child.where_ = vec![SqlExpr::InSubquery {
        expr: Box::new(SqlExpr::Column {
            depth: 0,
            column: child_col,
        }),
        query: Box::new(parent_q),
    }];
    child
}

fn global_first_hop(
    ctx: &mut Context,
    parent: Level,
    source: StructId,
    fid: FieldId,
) -> (StructId, SelectBuilder) {
    let field = &ctx.schema.structs[source.index()].fields[fid.local()];
    let target = match field {
        Field::Ref { target, .. } => *target,
        _ => unreachable!("global hop over non-reference field"),
    };
    let (child_col, parent_col) = match link_cols(ctx.schema, fid) {
        LinkCols::ParentFk {
            parent_fk,
            child_pk,
        } => (child_pk, parent_fk),
        LinkCols::TargetFk {
            child_fk,
            parent_pk,
        } => (child_fk, parent_pk),
        LinkCols::ManyToMany => {
            todo!("M4 many-to-many navigation");
        }
    };
    let table = ctx.schema.tables[ctx.schema.structs[target.index()].table.index()]
        .name
        .clone();
    let child_from = FromClause::Table {
        table: table.clone(),
        alias: ctx.aliases.table(&table),
    };
    if parent.pending.is_some() {
        unreachable!("global root with split-pending rows");
    }
    (
        target,
        merge_builder(
            child_from,
            child_col,
            parent.builder,
            parent_col,
            ctx.schema,
            false,
        ),
    )
}

fn rows_method(
    ctx: &mut Context,
    base: &TypedExpr,
    name: &grove_types::Spanned<String>,
    args: &[TypedMethodArg],
    optional: bool,
) -> Level {
    let _ = optional;
    let mut level = rows(ctx, base);
    match name.value.as_str() {
        "filter" => {
            if level.pending.is_some() {
                unreachable!(
                    "filter over split-pending level: pendings imply List inners, which the filter signature (Record-only) rejects"
                )
            }
            if needs_wrap(&level) {
                level = wrap_narrow_rn(ctx, level);
            }
            level
                .builder
                .where_
                .insert(0, scalar(ctx, one_arg(args, name)));
            level
        }
        "sort" | "sort_asc" | "sort_desc" => {
            if let Some(pending) = level.pending.as_ref() {
                if pending.multi_parent {
                    todo!("H1 QT0028: sort over nested lists");
                }
                todo!("M4 per-parent sort over single-row parent");
            }
            if needs_wrap(&level) {
                level = wrap_narrow_rn(ctx, level);
            }
            let default = if name.value == "sort_desc" {
                OrderDir::Desc
            } else {
                OrderDir::Asc
            };
            let mut new_keys = Vec::new();
            for arg in args {
                let dir = match &arg.direction {
                    Some(dir) => match dir.value {
                        SortDir::Asc => OrderDir::Asc,
                        SortDir::Desc => OrderDir::Desc,
                    },
                    None => default,
                };
                new_keys.extend(sort_keys(ctx, &level, &arg.expr, dir));
            }
            level.builder.order_by.extend(new_keys);
            level
        }
        "take" | "skip" | "first" | "nth" => {
            if let Some(fk) = ctx
                .child
                .as_ref()
                .filter(|tail| tail.per_row)
                .and_then(|tail| tail.partition.clone())
            {
                if name.value == "take" || name.value == "skip" {
                    return window_take(ctx, level, &fk, &name.value, args);
                }
                if name.value == "first" || name.value == "nth" {
                    let bound = lower_collapse_bound(ctx, &name.value, args);
                    return window_take_bound(ctx, level, &fk, &name.value, bound);
                }
                todo!("M4b per-parent collapse inside child tail");
            }
            if level.pending.is_some() && !matches!(level.element, QueryType::List(_)) {
                todo!("M4 per-parent take over single-row parent");
            }
            compose_limit(&mut level.builder, &name.value, args, ctx);
            level
        }
        "flatten" => match level.pending.take() {
            Some(pending) => {
                let PendingSplit {
                    child_from,
                    child_col,
                    parent_col,
                    next,
                    ..
                } = pending;
                level.element = narrow_element(level.element);
                let carry = match ctx.child.as_ref().filter(|tail| tail.per_row) {
                    Some(tail) => tail.partition.clone(),
                    None => None,
                };
                let single_source = !matches!(level.builder.from, Some(FromClause::Join { .. }));
                let carry = carry.filter(|_| single_source);
                let mid_from = carry.as_ref().and_then(|_| level.builder.from.clone());
                let mid_ty = match (&carry, &mid_from) {
                    (Some(fk), Some(mid_from)) => {
                        from_columns_typed(&Some(mid_from.clone()), ctx.schema)
                            .into_iter()
                            .find(|(name, _)| name == fk)
                            .and_then(|(_, ty)| ty)
                    }
                    _ => None,
                };
                let mut level = merge_navigate(
                    ctx,
                    child_from,
                    child_col.clone(),
                    level,
                    parent_col.clone(),
                );
                if let (Some(fk), Some(mid_from), Some(mid_ty)) = (carry, mid_from, mid_ty) {
                    let served = level.builder.select.iter().any(|item| match item {
                        SelectItem::Column { output, .. } | SelectItem::Expr { output, .. } => {
                            output == &fk
                        }
                        SelectItem::AllFrom { .. } => false,
                    });
                    if !served {
                        if level.builder.select.is_empty() {
                            level.builder.select.push(SelectItem::AllFrom { depth: 0 });
                        }
                        level.builder.select.push(SelectItem::Expr {
                            expr: SqlExpr::Subquery {
                                query: Box::new(SelectBuilder {
                                    select: vec![SelectItem::Column {
                                        depth: 0,
                                        column: fk.clone(),
                                        output: fk.clone(),
                                        ty: Some(mid_ty),
                                    }],
                                    from: Some(mid_from),
                                    where_: vec![SqlExpr::Binary {
                                        op: SqlBinOp::Eq,
                                        lhs: Box::new(SqlExpr::Column {
                                            depth: 0,
                                            column: parent_col.clone(),
                                        }),
                                        rhs: Box::new(SqlExpr::Column {
                                            depth: 1,
                                            column: child_col.clone(),
                                        }),
                                    }],
                                    order_by: Vec::new(),
                                    limit: None,
                                    offset: None,
                                    unions: Vec::new(),
                                }),
                            },
                            output: fk,
                            ty: Some(mid_ty),
                        });
                        level = wrap_level(ctx, level);
                    }
                }
                level.pending = next.map(|boxed| *boxed);
                level
            }
            None => {
                unreachable!("flatten over flat list is rejected upstream (strict sig)")
            }
        },
        "max_by" | "min_by" => {
            let dir = if name.value == "max_by" {
                OrderDir::Desc
            } else {
                OrderDir::Asc
            };
            if let Some(fk) = ctx
                .child
                .as_ref()
                .filter(|tail| tail.per_row)
                .and_then(|tail| tail.partition.clone())
            {
                let order_arg = over_arg(ctx, args, name).1;
                let mut level = window_wrap(
                    ctx,
                    level,
                    &fk,
                    vec![OrderItem {
                        expr: order_arg,
                        dir,
                    }],
                );
                level.builder.where_.push(SqlExpr::Binary {
                    op: SqlBinOp::Le,
                    lhs: Box::new(SqlExpr::Column {
                        depth: 0,
                        column: "$$rn".to_string(),
                    }),
                    rhs: Box::new(SqlExpr::Param(ParamValue::Int(1))),
                });
                return level;
            }
            if level.pending.is_some() {
                unreachable!(
                    "max_by over split-pending level: pendings imply List inners, which the max_by signature (Record-only) rejects"
                )
            }
            if needs_wrap(&level) {
                level = wrap_narrow_rn(ctx, level);
            }
            level.builder.order_by = vec![OrderItem {
                expr: over_arg(ctx, args, name).1,
                dir,
            }];
            compose_limit(&mut level.builder, "first", &[], ctx);
            level
        }
        "unwrap_or" if matches!(peel_optional(&level.element), QueryType::Record(_)) => {
            todo!("M4 presence-directed unwrap over records")
        }
        "unwrap" => assert_present_rows(ctx, level, "unwrap failed: value was none".to_string()),
        "unwrap_or" => {
            match peel_optional(&level.element) {
                QueryType::List(_) => {
                    let (level, probe_from) = match &level.pending {
                        Some(_) => match fold_literal_pick(base) {
                            Some(Some(picked)) => {
                                let values = rows(ctx, &picked);
                                if values.pending.is_some() {
                                    todo!("M4b presence: unwrap over list in row demand")
                                }
                                let outer = SelectBuilder {
                                    select: vec![SelectItem::AllFrom { depth: 0 }],
                                    from: level.builder.from.clone(),
                                    where_: level.builder.where_.clone(),
                                    order_by: Vec::new(),
                                    limit: level.builder.limit.clone(),
                                    offset: level.builder.offset.clone(),
                                    unions: Vec::new(),
                                };
                                (
                                    values,
                                    Some(FromClause::Subquery {
                                        query: Box::new(outer),
                                        alias: ctx.aliases.subquery(),
                                    }),
                                )
                            }
                            Some(None) => return rows(ctx, one_arg(args, name)),
                            None => todo!("M4b presence: unwrap over list in row demand"),
                        },
                        None => (level, None),
                    };
                    unwrap_or_rows(ctx, level, one_arg(args, name), probe_from)
                }
                QueryType::Record(_) => {
                    todo!("M4 presence-directed unwrap over records")
                }
                _ => todo!("M4b presence: unwrap over list in row demand"),
            }
        }
        _ => unreachable!(
            "method {} in row demand: no other value method returns row types",
            name.value
        ),
    }
}

fn needs_wrap(level: &Level) -> bool {
    level.builder.limit.is_some()
        || level.builder.offset.is_some()
        || !level.builder.select.is_empty()
        || !level.builder.unions.is_empty()
}

fn wrap_narrow_rn(ctx: &mut Context, level: Level) -> Level {
    let served: Vec<(String, Option<ScalarType>)> =
        builder_output_types(&level.builder, ctx.schema);
    let mut wrapped = wrap_level(ctx, level);
    if served.iter().any(|(name, _)| name == "$$rn") {
        wrapped.builder.select = served
            .into_iter()
            .filter(|(name, _)| name != "$$rn")
            .map(|(name, ty)| SelectItem::Column {
                depth: 0,
                column: name.clone(),
                output: name,
                ty,
            })
            .collect();
    }
    wrapped
}

fn wrap_level(ctx: &mut Context, level: Level) -> Level {
    let Level {
        mut builder,
        element,
        parent,
        pending,
        output_shapes,
        root_parts,
        ..
    } = level;
    if builder.select.is_empty() {
        builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let floor = nest_floor(&builder.from);
    builder = shift_nest_builder(builder, floor);
    let alias = ctx.aliases.subquery();
    let mut outer = SelectBuilder::new();
    outer.from = Some(FromClause::Subquery {
        query: Box::new(builder),
        alias,
    });
    Level {
        builder: outer,
        element,
        parent,
        pending,
        stmt_bound: false,
        output_shapes,
        root_parts,
    }
}

fn sort_keys(ctx: &mut Context, parent: &Level, expr: &TypedExpr, dir: OrderDir) -> Vec<OrderItem> {
    if !matches!(expr.ty, QueryType::Tuple(_)) {
        return vec![OrderItem {
            expr: scalar(ctx, expr),
            dir,
        }];
    }
    if let TypedExprKind::If { arms, default } = &expr.kind {
        let QueryType::Tuple(tys) = &expr.ty else {
            unreachable!("if with non-tuple type in tuple sort key: caller checks the type")
        };
        match distribute_if_tuple(arms, default, tys, expr.span, expr.ty.clone()) {
            Some(synthetic) => return sort_keys(ctx, parent, &synthetic, dir),
            None => todo!("M7 sort-key tuple expansion"),
        }
    }
    match &expr.kind {
        TypedExprKind::Tuple { elements } => elements
            .iter()
            .flat_map(|e| sort_keys(ctx, parent, e, dir))
            .collect(),
        TypedExprKind::Ident {
            binding: IdentBinding::Field(fid),
            depth,
            ..
        } => {
            let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
            match field {
                Field::Value { columns, .. } => columns
                    .iter()
                    .map(|column| OrderItem {
                        expr: SqlExpr::Column {
                            depth: *depth,
                            column: ctx.schema.column_name(*column).to_string(),
                        },
                        dir,
                    })
                    .collect(),
                _ => unreachable!("tuple sort key over non-value field: tuples are value-typed"),
            }
        }
        TypedExprKind::Ident {
            binding: IdentBinding::ProjectedField { name },
            depth,
            ..
        } => {
            if *depth != 0 {
                todo!("M7 projected composite across levels");
            }
            let Some(shape) = projected_shape(parent, name) else {
                unreachable!(
                    "projected composite {} missing from enclosing outputs: depth-0 projected idents are always projected in the current level",
                    name
                )
            };
            shape_leaves(shape)
                .into_iter()
                .map(|column| OrderItem {
                    expr: SqlExpr::Column {
                        depth: *depth,
                        column,
                    },
                    dir,
                })
                .collect()
        }
        _ => unreachable!(
            "remaining tuple sort-key shapes: Tuple/If/ProjectedField nodes route above; Field access over tuples is checker-rejected (QT0002) and no method returns bare tuples"
        ),
    }
}

fn shape_leaves(shape: &ResultShape) -> Vec<String> {
    match shape {
        ResultShape::Column(name) => vec![name.clone()],
        ResultShape::Tuple(shapes) => shapes.iter().flat_map(shape_leaves).collect(),
        ResultShape::Record(_) | ResultShape::List { .. } | ResultShape::Single { .. } => {
            unreachable!(
                "non-inline shape in sort key: sort keys are checker-ordered, which lists/records never satisfy"
            )
        }
    }
}

fn compose_limit(
    builder: &mut SelectBuilder,
    method: &str,
    args: &[TypedMethodArg],
    ctx: &mut Context,
) {
    let was = std::mem::replace(&mut ctx.in_take_arg, true);
    compose_limit_inner(builder, method, args, ctx);
    ctx.in_take_arg = was;
}

fn compose_limit_inner(
    builder: &mut SelectBuilder,
    method: &str,
    args: &[TypedMethodArg],
    ctx: &mut Context,
) {
    match method {
        "take" => {
            let arg = match args {
                [arg] => scalar(ctx, &arg.expr),
                _ => unreachable!("wrong arity for take: the checker enforces Fixed arg counts"),
            };
            let guard = non_neg_guard(arg, "take");
            builder.limit = Some(match builder.limit.take() {
                Some(existing) => SqlExpr::Func {
                    name: "min".to_string(),
                    args: vec![guard, existing],
                },
                None => guard,
            });
        }
        "skip" => {
            let arg = match args {
                [arg] => scalar(ctx, &arg.expr),
                _ => unreachable!("wrong arity for skip: the checker enforces Fixed arg counts"),
            };
            let guard = non_neg_guard(arg, "skip");
            builder.offset = Some(match builder.offset.take() {
                Some(existing) => SqlExpr::Func {
                    name: "$$add".to_string(),
                    args: vec![guard.clone(), existing],
                },
                None => guard.clone(),
            });
            if let Some(limit) = builder.limit.take() {
                builder.limit = Some(SqlExpr::Func {
                    name: "max".to_string(),
                    args: vec![
                        SqlExpr::Func {
                            name: "$$sub".to_string(),
                            args: vec![limit, guard],
                        },
                        SqlExpr::Param(ParamValue::Int(0)),
                    ],
                });
            }
        }
        "first" => {
            let one = SqlExpr::Param(ParamValue::Int(1));
            builder.limit = Some(match builder.limit.take() {
                Some(existing) => SqlExpr::Func {
                    name: "min".to_string(),
                    args: vec![one, existing],
                },
                None => one,
            });
        }
        "nth" => {
            let arg = match args {
                [arg] => scalar(ctx, &arg.expr),
                _ => unreachable!("wrong arity for nth: the checker enforces Fixed arg counts"),
            };
            let guard = non_neg_guard(arg, "nth");
            builder.offset = Some(match builder.offset.take() {
                Some(existing) => SqlExpr::Func {
                    name: "$$add".to_string(),
                    args: vec![guard.clone(), existing],
                },
                None => guard.clone(),
            });
            let one = SqlExpr::Param(ParamValue::Int(1));
            builder.limit = Some(match builder.limit.take() {
                Some(existing) => SqlExpr::Func {
                    name: "min".to_string(),
                    args: vec![
                        one,
                        SqlExpr::Func {
                            name: "max".to_string(),
                            args: vec![
                                SqlExpr::Func {
                                    name: "$$sub".to_string(),
                                    args: vec![existing, guard],
                                },
                                SqlExpr::Param(ParamValue::Int(0)),
                            ],
                        },
                    ],
                },
                None => one,
            });
        }
        _ => unreachable!("{method} is not a take-family method"),
    }
}

fn non_neg_guard(expr: SqlExpr, method: &str) -> SqlExpr {
    SqlExpr::Case {
        arms: vec![(
            SqlExpr::Binary {
                op: SqlBinOp::Ge,
                lhs: Box::new(expr.clone()),
                rhs: Box::new(SqlExpr::Param(ParamValue::Int(0))),
            },
            expr,
        )],
        else_: Box::new(SqlExpr::Func {
            name: "$$panic".to_string(),
            args: vec![SqlExpr::Param(ParamValue::String(format!(
                "{method} requires a non-negative integer"
            )))],
        }),
    }
}

fn window_take(
    ctx: &mut Context,
    level: Level,
    fk: &str,
    method: &str,
    args: &[TypedMethodArg],
) -> Level {
    let bound = lower_take_bound(ctx, method, args);
    window_take_bound(ctx, level, fk, method, bound)
}

fn lower_take_bound(ctx: &mut Context, method: &str, args: &[TypedMethodArg]) -> SqlExpr {
    let [arg] = args else {
        unreachable!("wrong arity for {method}: the checker enforces Fixed arg counts")
    };
    let was = std::mem::replace(&mut ctx.in_take_arg, true);
    let lowered = scalar(ctx, &arg.expr);
    ctx.in_take_arg = was;
    lowered
}

fn lower_collapse_bound(ctx: &mut Context, method: &str, args: &[TypedMethodArg]) -> SqlExpr {
    if method == "first" {
        return SqlExpr::Param(ParamValue::Int(1));
    }
    let [arg] = args else {
        unreachable!("wrong arity for {method}: the checker enforces Fixed arg counts")
    };
    let was = std::mem::replace(&mut ctx.in_take_arg, true);
    let lowered = collapse_index(ctx, &arg.expr);
    ctx.in_take_arg = was;
    lowered
}

fn window_take_bound(
    ctx: &mut Context,
    mut level: Level,
    fk: &str,
    method: &str,
    bound: SqlExpr,
) -> Level {
    if has_window_bound(&level.builder) {
        conjoin_window_bound(&mut level.builder, method, bound);
        return level;
    }
    let order = std::mem::take(&mut level.builder.order_by);
    let mut level = window_wrap(ctx, level, fk, order);
    conjoin_window_bound(&mut level.builder, method, bound);
    level
}

fn has_window_bound(builder: &SelectBuilder) -> bool {
    builder.where_.iter().any(|w| match w {
        SqlExpr::Binary {
            op: SqlBinOp::Le | SqlBinOp::Gt | SqlBinOp::Eq,
            lhs,
            ..
        } => matches!(
            lhs.as_ref(),
            SqlExpr::Column { column, .. } if column == "$$rn"
        ),
        _ => false,
    })
}

fn conjoin_window_bound(builder: &mut SelectBuilder, method: &str, bound: SqlExpr) {
    let mut offset: Option<SqlExpr> = None;
    for w in &builder.where_ {
        if let SqlExpr::Binary { op, lhs, rhs } = w
            && let SqlExpr::Column { column, .. } = lhs.as_ref()
            && column == "$$rn"
        {
            match op {
                SqlBinOp::Gt => offset = Some(rhs.as_ref().clone()),
                SqlBinOp::Eq => {
                    offset = Some(SqlExpr::Func {
                        name: "$$sub".to_string(),
                        args: vec![rhs.as_ref().clone(), SqlExpr::Param(ParamValue::Int(1))],
                    });
                }
                _ => {}
            }
        }
    }
    let rn = || {
        Box::new(SqlExpr::Column {
            depth: 0,
            column: "$$rn".to_string(),
        })
    };
    let rel = |b: SqlExpr| match offset {
        Some(l) => SqlExpr::Func {
            name: "$$add".to_string(),
            args: vec![l, b],
        },
        None => b,
    };
    let bound = match method {
        "take" | "skip" => {
            let guard = non_neg_guard(bound, method);
            SqlExpr::Binary {
                op: if method == "take" {
                    SqlBinOp::Le
                } else {
                    SqlBinOp::Gt
                },
                lhs: rn(),
                rhs: Box::new(rel(guard)),
            }
        }
        "first" => SqlExpr::Binary {
            op: SqlBinOp::Le,
            lhs: rn(),
            rhs: Box::new(rel(SqlExpr::Param(ParamValue::Int(1)))),
        },
        "nth" => SqlExpr::Binary {
            op: SqlBinOp::Eq,
            lhs: rn(),
            rhs: Box::new(rel(bound)),
        },
        _ => unreachable!("{method} is not a take-family method"),
    };
    builder.where_.push(bound);
}
fn window_wrap(ctx: &mut Context, level: Level, fk: &str, order: Vec<OrderItem>) -> Level {
    let Level {
        mut builder,
        element,
        parent,
        pending,
        stmt_bound,
        output_shapes,
        root_parts,
    } = level;
    if builder
        .select
        .iter()
        .any(|item| matches!(item, SelectItem::Expr { output, .. } if output == "$$rn"))
    {
        todo!("M4b stacked per-parent take");
    }
    if builder.select.is_empty() {
        builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    ensure_key(&mut builder, fk, ctx.schema);
    builder.select.push(SelectItem::Expr {
        expr: SqlExpr::Window {
            func: "ROW_NUMBER".to_string(),
            partition: vec![SqlExpr::Column {
                depth: 0,
                column: fk.to_string(),
            }],
            order,
        },
        output: "$$rn".to_string(),
        ty: Some(ScalarType::Int),
    });
    let level = Level {
        builder,
        element,
        parent,
        pending,
        stmt_bound,
        output_shapes,
        root_parts,
    };
    wrap_level(ctx, level)
}

const PENDING_PARENT_STMT: usize = usize::MAX;

fn patch_pending_parents(shape: &mut ResultShape, stmt: usize) {
    match shape {
        ResultShape::Column(_) => {}
        ResultShape::Tuple(elements) => {
            for e in elements {
                patch_pending_parents(e, stmt);
            }
        }
        ResultShape::Record(fields) => {
            for (_, e) in fields {
                patch_pending_parents(e, stmt);
            }
        }
        ResultShape::List { element, child } | ResultShape::Single { element, child } => {
            patch_pending_parents(element, stmt);
            if let Some(parent) = child.parent.as_mut()
                && parent.stmt == PENDING_PARENT_STMT
            {
                parent.stmt = stmt;
            }
        }
    }
}

pub fn has_pending_parent(shape: &ResultShape) -> bool {
    match shape {
        ResultShape::Column(_) => false,
        ResultShape::Tuple(elements) => elements.iter().any(has_pending_parent),
        ResultShape::Record(fields) => fields.iter().any(|(_, e)| has_pending_parent(e)),
        ResultShape::List { element, child } | ResultShape::Single { element, child } => {
            has_pending_parent(element)
                || child
                    .parent
                    .as_ref()
                    .is_some_and(|p| p.stmt == PENDING_PARENT_STMT)
        }
    }
}

fn nested_bulk_lane(parent: &Level, tail: &TypedExpr) -> bool {
    matches!(tail_root_ident(tail), Some(IdentBinding::Field(_)))
        && parent.pending.is_none()
        && parent.builder.unions.is_empty()
        && !tail_uses_outer(tail)
        && !take_bound_ident(tail)
}

fn b1_optional_list(
    ctx: &mut Context,
    parent: &mut Level,
    value: &TypedExpr,
) -> (Vec<SelectItem>, ResultShape, Vec<String>) {
    if matches!(&value.kind, TypedExprKind::Field { optional: true, .. }) {
        return b1_chained(ctx, parent, value);
    }
    if let TypedExprKind::Method { base, name, .. } = &value.kind
        && (name.value == "first" || name.value == "nth")
        && matches!(&base.kind, TypedExprKind::Array { .. })
    {
        if has_scope_ident(base) {
            todo!("M4b presence: optional list item");
        }
        return b1_literal(ctx, value);
    }
    if let TypedExprKind::Some { value: inner } = &value.kind {
        if matches!(inner.ty, QueryType::Optional(_)) {
            todo!("M4b presence: optional list item");
        }
        let (shape, parent_key) = rvalue_shape(ctx, parent, inner);
        return (Vec::new(), shape, parent_key.into_iter().collect());
    }
    if let TypedExprKind::Method {
        base: inner,
        name,
        args,
        ..
    } = &value.kind
        && (name.value == "first" || name.value == "nth")
        && matches!(tail_root_ident(inner), Some(IdentBinding::Field(_)))
        && !tail_uses_outer(value)
    {
        let collapse = if name.value == "first" {
            CollapseKind::First
        } else {
            CollapseKind::Nth(one_arg(args, name))
        };
        return b1_nested_collapse(ctx, value, inner, collapse);
    }
    todo!("M4b presence: optional list item")
}

fn b1_literal(ctx: &mut Context, value: &TypedExpr) -> (Vec<SelectItem>, ResultShape, Vec<String>) {
    let mut root = rows(ctx, value);
    let hops = root.pending.take().map(drain_pending).unwrap_or_default();
    if hops.is_empty() {
        todo!("M4b presence: optional list item");
    }
    if root.builder.select.is_empty() {
        root.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&root.builder, ctx.schema), |_| false);
    let root_creation = ctx.statements.len() + hops.len() + 1;
    let layers = sink_chain(ctx, &hops, root_creation);
    let inner = nest_layers(
        ctx.schema,
        layers,
        &value.ty,
        false,
        root.root_parts.clone(),
        root_creation,
    );
    let actual = ctx.sink(
        root.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::ZeroOrOne,
        },
        StatementKind::Query,
    );
    debug_assert_eq!(
        actual, root_creation,
        "collapse root sank at an unexpected index: sink order changed"
    );
    let shape = ResultShape::Single {
        element: Box::new(inner),
        child: ChildRef {
            stmt: root_creation,
            parent: None,
            cardinality: Cardinality::One,
            tag: None,
        },
    };
    (Vec::new(), shape, Vec::new())
}

fn b1_chained(
    ctx: &mut Context,
    parent: &mut Level,
    value: &TypedExpr,
) -> (Vec<SelectItem>, ResultShape, Vec<String>) {
    let _ = parent;
    if !matches!(parent.element, QueryType::List(_))
        || parent.pending.is_some()
        || tail_uses_outer(value)
        || take_bound_ident(value)
    {
        todo!("M4b presence: optional list item");
    }
    let Some((rewritten, collapse, tail_fid)) = split_opt_chain(ctx, value) else {
        todo!("M4b presence: optional list item");
    };
    let Some(peeled) = peel_windowed_collapse(collapse, false) else {
        todo!("M4b presence: optional list item");
    };
    if !peeled.keyed {
        todo!("M4b presence: optional list item");
    }
    let tail_link = resolve_tail_link(ctx, tail_fid);
    let (outer_level, outer_parent_key, outer_child_col) = build_capped_level(ctx, peeled);
    let mut inner_builder = SelectBuilder::new();
    if let Some((target_pk, junction_target_col)) = &tail_link.m2m_join {
        inner_builder.from = Some(tail_link.child_from.clone());
        inner_builder.where_ = vec![
            SqlExpr::InSubquery {
                expr: Box::new(SqlExpr::Column {
                    depth: 1,
                    column: tail_link.child_col.clone(),
                }),
                query: Box::new(bulk_query()),
            },
            SqlExpr::Binary {
                op: SqlBinOp::Eq,
                lhs: Box::new(SqlExpr::Column {
                    depth: 0,
                    column: target_pk.clone(),
                }),
                rhs: Box::new(SqlExpr::Column {
                    depth: 1,
                    column: junction_target_col.clone(),
                }),
            },
        ];
        m2m_seed_selects(ctx.schema, &mut inner_builder, &tail_link.child_col);
        let alias = ctx.aliases.subquery();
        let mut outer = SelectBuilder::new();
        outer.from = Some(FromClause::Subquery {
            query: Box::new(inner_builder),
            alias,
        });
        inner_builder = outer;
    } else {
        inner_builder.from = Some(tail_link.child_from.clone());
        inner_builder.where_ = vec![SqlExpr::InSubquery {
            expr: Box::new(SqlExpr::Column {
                depth: 0,
                column: tail_link.child_col.clone(),
            }),
            query: Box::new(bulk_query()),
        }];
    }
    let inner_level = Level {
        builder: inner_builder,
        element: peel_optional(&value.ty).clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    };
    let mut inner = lower_tail(
        ctx,
        &rewritten,
        inner_level,
        true,
        Some(tail_link.child_col.clone()),
        false,
    );
    if inner.builder.select.is_empty() {
        inner.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    ensure_key(&mut inner.builder, &tail_link.child_col, ctx.schema);
    let outputs = builder_outputs(&inner.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&inner.builder, ctx.schema), |name| {
            name == tail_link.child_col
        });
    let item_ty = match peel_optional(&value.ty) {
        QueryType::List(sub) => sub.as_ref(),
        _ => peel_optional(&value.ty),
    };
    let mut element = child_element(
        ctx.schema,
        inner.output_shapes.as_ref(),
        item_ty,
        &outputs,
        |name| name == tail_link.child_col,
    );
    let idx_inner = ctx.sink(
        inner.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    patch_pending_parents(&mut element, idx_inner);
    let mut outer = outer_level;
    if outer.builder.select.is_empty() {
        outer.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let outer_shape = StatementShape {
        columns: shape_columns(builder_output_types(&outer.builder, ctx.schema), |name| {
            name == outer_child_col
        }),
        rows: RowCount::Many,
    };
    let idx_outer = ctx.sink(outer.builder, outer_shape, StatementKind::Query);
    let inner_list = ResultShape::List {
        element: Box::new(element),
        child: ChildRef {
            stmt: idx_inner,
            parent: Some(ParentRef {
                stmt: idx_outer,
                parent_key: tail_link.parent_col.clone(),
                child_key: tail_link.child_col.clone(),
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    let shape = ResultShape::Single {
        element: Box::new(inner_list),
        child: ChildRef {
            stmt: idx_outer,
            parent: outer_parent_key.clone().map(|parent_key| ParentRef {
                stmt: if ctx.child.is_some() {
                    PENDING_PARENT_STMT
                } else {
                    0
                },
                parent_key,
                child_key: outer_child_col,
                negated: false,
            }),
            cardinality: Cardinality::One,
            tag: None,
        },
    };
    (Vec::new(), shape, outer_parent_key.into_iter().collect())
}

fn b1_nested_collapse(
    ctx: &mut Context,
    value: &TypedExpr,
    inner: &TypedExpr,
    collapse: CollapseKind<'_>,
) -> (Vec<SelectItem>, ResultShape, Vec<String>) {
    let (child_from, child_col, parent_key) = match tail_root_ident(inner) {
        Some(IdentBinding::Field(fid)) => {
            let link = resolve_tail_link(ctx, *fid);
            if link.m2m_join.is_some() {
                todo!("M4 many-to-many navigation");
            }
            (link.child_from, link.child_col, Some(link.parent_col))
        }
        _ => unreachable!("exotic nested collapse base: the b1 arm admits keyed field tails only"),
    };
    let child_level = Level {
        builder: {
            let mut child_builder = SelectBuilder::new();
            child_builder.from = Some(child_from);
            child_builder.where_ = vec![SqlExpr::InSubquery {
                expr: Box::new(SqlExpr::Column {
                    depth: 0,
                    column: child_col.clone(),
                }),
                query: Box::new(bulk_query()),
            }];
            child_builder
        },
        element: inner.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    };
    let mut child = lower_tail(ctx, inner, child_level, true, Some(child_col.clone()), true);
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    ensure_key(&mut child.builder, &child_col, ctx.schema);
    let mut child = if has_window_bound(&child.builder) {
        child
    } else {
        window_wrap(ctx, child, &child_col, Vec::new())
    };
    match collapse {
        CollapseKind::First => {
            conjoin_window_bound(
                &mut child.builder,
                "first",
                SqlExpr::Param(ParamValue::Int(1)),
            );
        }
        CollapseKind::Nth(arg) => {
            conjoin_window_bound(&mut child.builder, "nth", collapse_index(ctx, arg));
        }
    }
    let Some(pending) = child.pending.take() else {
        todo!("M4b presence: optional list item");
    };
    let (inner_nest, child_idx) = drain_pending_chain(ctx, pending, &value.ty);
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&child.builder, ctx.schema), |name| {
            name == child_col
        });
    let idx_outer = ctx.sink(
        child.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    debug_assert_eq!(
        idx_outer, child_idx,
        "nested collapse sank at an unexpected index: sink order changed"
    );
    let shape = ResultShape::Single {
        element: Box::new(inner_nest),
        child: ChildRef {
            stmt: idx_outer,
            parent: parent_key.clone().map(|parent_key| ParentRef {
                stmt: if ctx.child.is_some() {
                    PENDING_PARENT_STMT
                } else {
                    0
                },
                parent_key,
                child_key: child_col,
                negated: false,
            }),
            cardinality: Cardinality::One,
            tag: None,
        },
    };
    (Vec::new(), shape, parent_key.into_iter().collect())
}

fn split_opt_chain<'a>(
    ctx: &mut Context,
    value: &'a TypedExpr,
) -> Option<(TypedExpr, &'a TypedExpr, FieldId)> {
    fn is_collapse(node: &TypedExpr) -> bool {
        matches!(
            &node.kind,
            TypedExprKind::Method { name, .. }
                if name.value == "first" || name.value == "nth"
        )
    }
    fn contains_opt_collapse(node: &TypedExpr) -> bool {
        match &node.kind {
            TypedExprKind::Field {
                base,
                optional: true,
                ..
            } if is_collapse(base) => true,
            TypedExprKind::Field { base, .. }
            | TypedExprKind::Method { base, .. }
            | TypedExprKind::Projection { base, .. } => contains_opt_collapse(base),
            _ => false,
        }
    }
    fn go<'a>(
        ctx: &mut Context,
        node: &'a TypedExpr,
        list_ty: &QueryType,
        span: grove_types::Span,
    ) -> Option<(TypedExpr, &'a TypedExpr, FieldId)> {
        match &node.kind {
            TypedExprKind::Field {
                base,
                name,
                optional: true,
            } if is_collapse(base) => {
                if contains_opt_collapse(base) {
                    return None;
                }
                let sid = match peel_optional(&base.ty) {
                    QueryType::Record(RecordSource::Schema(sid)) => *sid,
                    _ => return None,
                };
                let (fid, _) = ctx.schema.struct_field(sid, &name.value)?;
                let dummy = TypedExpr {
                    kind: TypedExprKind::Ident {
                        name: grove_types::Spanned {
                            span,
                            value: name.value.clone(),
                        },
                        binding: IdentBinding::Field(fid),
                        depth: 0,
                    },
                    ty: list_ty.clone(),
                    span,
                };
                Some((dummy, base.as_ref(), fid))
            }
            TypedExprKind::Field {
                base,
                name,
                optional,
            } => {
                let (rw, collapse, fid) = go(ctx, base, list_ty, span)?;
                Some((
                    TypedExpr {
                        kind: TypedExprKind::Field {
                            base: Box::new(rw),
                            name: name.clone(),
                            optional: *optional,
                        },
                        ty: node.ty.clone(),
                        span: node.span,
                    },
                    collapse,
                    fid,
                ))
            }
            TypedExprKind::Method {
                base,
                name,
                args,
                optional,
            } => {
                let (rw, collapse, fid) = go(ctx, base, list_ty, span)?;
                Some((
                    TypedExpr {
                        kind: TypedExprKind::Method {
                            base: Box::new(rw),
                            name: name.clone(),
                            args: args.clone(),
                            optional: *optional,
                        },
                        ty: node.ty.clone(),
                        span: node.span,
                    },
                    collapse,
                    fid,
                ))
            }
            TypedExprKind::Projection { base, items } => {
                let (rw, collapse, fid) = go(ctx, base, list_ty, span)?;
                Some((
                    TypedExpr {
                        kind: TypedExprKind::Projection {
                            base: Box::new(rw),
                            items: items.clone(),
                        },
                        ty: node.ty.clone(),
                        span: node.span,
                    },
                    collapse,
                    fid,
                ))
            }
            _ => None,
        }
    }
    let list_ty = match peel_optional(&value.ty) {
        QueryType::List(_) => peel_optional(&value.ty).clone(),
        _ => return None,
    };
    go(ctx, value, &list_ty, value.span)
}

fn assert_present_rows(ctx: &mut Context, level: Level, msg: String) -> Level {
    let Level {
        mut builder,
        element,
        parent,
        pending,
        stmt_bound,
        output_shapes,
        root_parts,
    } = level;
    if builder.select.is_empty() {
        builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let mut base_inner = SelectBuilder::new();
    base_inner.from = Some(FromClause::Subquery {
        query: Box::new(builder),
        alias: ctx.aliases.subquery(),
    });
    base_inner.select = vec![SelectItem::AllFrom { depth: 0 }];
    let ncols = builder_outputs(&base_inner, ctx.schema).len();
    let mut base_inner = shift_outer_builder(base_inner, 2);
    let guard_base = shift_outer_builder(base_inner.clone(), 2);
    base_inner.select.push(SelectItem::Expr {
        expr: SqlExpr::Param(ParamValue::Null),
        output: "$$abort".to_string(),
        ty: Some(ScalarType::Int),
    });
    builder = base_inner;
    let mut arm = SelectBuilder::new();
    arm.select = (0..ncols)
        .map(|i| SelectItem::Expr {
            expr: SqlExpr::Param(ParamValue::Null),
            output: format!("c{i}"),
            ty: None,
        })
        .chain(std::iter::once(SelectItem::Expr {
            expr: SqlExpr::Param(ParamValue::Null),
            output: "$$abort".to_string(),
            ty: Some(ScalarType::Int),
        }))
        .collect();
    let probe = {
        let mut probe = SelectBuilder::new();
        probe.select = vec![SelectItem::Expr {
            expr: SqlExpr::Param(ParamValue::Int(1)),
            output: "c0".to_string(),
            ty: Some(ScalarType::Int),
        }];
        probe.from = Some(FromClause::Subquery {
            query: Box::new(guard_base),
            alias: ctx.aliases.subquery(),
        });
        probe
    };
    arm.where_ = vec![SqlExpr::Binary {
        op: SqlBinOp::IsNot,
        lhs: Box::new(SqlExpr::Case {
            arms: vec![(
                SqlExpr::Not(Box::new(SqlExpr::Exists(Box::new(probe)))),
                SqlExpr::Func {
                    name: "$$panic".to_string(),
                    args: vec![SqlExpr::Param(ParamValue::String(msg))],
                },
            )],
            else_: Box::new(SqlExpr::Param(ParamValue::Null)),
        }),
        rhs: Box::new(SqlExpr::Param(ParamValue::Null)),
    }];
    builder.unions.push(arm);
    wrap_level(
        ctx,
        Level {
            builder,
            element,
            parent,
            pending,
            stmt_bound,
            output_shapes,
            root_parts,
        },
    )
}

fn fold_literal_pick(base: &TypedExpr) -> Option<Option<TypedExpr>> {
    let TypedExprKind::Method {
        base: arr,
        name,
        args,
        ..
    } = &base.kind
    else {
        return None;
    };
    if name.value != "first" && name.value != "nth" {
        return None;
    }
    let TypedExprKind::Array { elements } = &arr.kind else {
        return None;
    };
    if has_scope_ident(base) || uses_outer(base) || take_bound_ident(base) {
        return None;
    }
    let idx = if name.value == "first" {
        0
    } else {
        let [arg] = args.as_slice() else {
            return None;
        };
        match &arg.expr.kind {
            TypedExprKind::Literal(lit) => match &lit.value {
                crate::ast::Literal::Int(n) if *n >= 0 => *n as usize,
                _ => return None,
            },
            _ => return None,
        }
    };
    let Some(elem) = elements.get(idx) else {
        return Some(None);
    };
    if !matches!(&elem.ty, QueryType::List(inner) if matches!(inner.as_ref(), QueryType::Scalar(_)))
    {
        return None;
    }
    Some(Some(elem.clone()))
}

fn unwrap_or_rows(
    ctx: &mut Context,
    level: Level,
    default: &TypedExpr,
    probe_from: Option<FromClause>,
) -> Level {
    if uses_outer(default) || has_scope_ident(default) || take_bound_ident(default) {
        todo!("M4b presence: unwrap over list in row demand");
    }
    let Level {
        mut builder,
        element,
        parent,
        pending,
        stmt_bound,
        output_shapes,
        root_parts,
    } = level;
    let _ = pending;
    if builder.select.is_empty() {
        builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let mut base_inner = SelectBuilder::new();
    base_inner.from = Some(FromClause::Subquery {
        query: Box::new(builder),
        alias: ctx.aliases.subquery(),
    });
    base_inner.select = vec![SelectItem::AllFrom { depth: 0 }];
    let base_inner = shift_outer_builder(base_inner, 2);
    let guard_base = shift_outer_builder(base_inner.clone(), 2);
    builder = base_inner;
    let default_level = rows(ctx, default);
    if default_level.pending.is_some() || !default_level.builder.unions.is_empty() {
        todo!("M4b presence: unwrap over list in row demand");
    }
    let mut probe = SelectBuilder::new();
    probe.select = vec![SelectItem::Expr {
        expr: SqlExpr::Param(ParamValue::Int(1)),
        output: "c0".to_string(),
        ty: Some(ScalarType::Int),
    }];
    probe.from = Some(match probe_from {
        Some(from) => from,
        None => FromClause::Subquery {
            query: Box::new(guard_base),
            alias: ctx.aliases.subquery(),
        },
    });
    let mut arm = default_level.builder;
    if arm.select.is_empty() {
        arm.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    arm.where_
        .push(SqlExpr::Not(Box::new(SqlExpr::Exists(Box::new(probe)))));
    builder.unions.push(arm);
    wrap_level(
        ctx,
        Level {
            builder,
            element,
            parent,
            pending: None,
            stmt_bound,
            output_shapes,
            root_parts,
        },
    )
}

fn lower_value(
    ctx: &mut Context,
    parent: &mut Level,
    prefix: String,
    value: &TypedExpr,
) -> (Vec<SelectItem>, ResultShape, Vec<String>) {
    if let TypedExprKind::Field { base, name, .. } = &value.kind
        && let Some(field) = struct_field_value(base, &name.value)
    {
        return lower_value(ctx, parent, prefix, field);
    }
    if value.ty.is_scalar() {
        if let Some((select, shape, keys)) = scalar_temp_array(ctx, parent, &prefix, value) {
            return (select, shape, keys);
        }
        let value_ty = scalar_of(&value.ty);
        let item = match scalar(ctx, value) {
            SqlExpr::Column { depth, column } => SelectItem::Column {
                depth,
                column,
                output: prefix.clone(),
                ty: value_ty,
            },
            expr => SelectItem::Expr {
                expr,
                output: prefix.clone(),
                ty: value_ty,
            },
        };
        return (vec![item], ResultShape::Column(prefix), Vec::new());
    }
    if let TypedExprKind::Struct { fields } = &value.kind {
        let QueryType::Record(RecordSource::Projection(_)) = &value.ty else {
            unreachable!(
                "struct literal with non-projection type {}: the checker infers anonymous records",
                value.ty
            )
        };
        let mut select = Vec::new();
        let mut record = Vec::with_capacity(fields.len());
        let mut keys = Vec::new();
        for (name, field) in fields {
            let (mut sub_select, sub_shape, mut sub_keys) =
                lower_value(ctx, parent, format!("{prefix}_{}", name.value), field);
            select.append(&mut sub_select);
            record.push((name.value.clone(), sub_shape));
            keys.append(&mut sub_keys);
        }
        return (select, ResultShape::Record(record), keys);
    }
    match &value.ty {
        QueryType::List(_) => {
            if ctx.child.is_some() {
                if !nested_bulk_lane(parent, value) {
                    todo!("M4b nested list item");
                }
                ctx.nest_depth += 1;
                let result = rvalue_shape(ctx, parent, value);
                ctx.nest_depth -= 1;
                let (shape, parent_key) = result;
                return (Vec::new(), shape, parent_key.into_iter().collect());
            }
            let (shape, parent_key) = rvalue_shape(ctx, parent, value);
            (Vec::new(), shape, parent_key.into_iter().collect())
        }
        QueryType::Record(_) => {
            if ctx.child.is_some() {
                if !nested_bulk_lane(parent, value) {
                    todo!("M4b nested record item");
                }
                ctx.nest_depth += 1;
                let result = rscalar_shape(ctx, parent, value);
                ctx.nest_depth -= 1;
                let (shape, parent_key) = result;
                return (Vec::new(), shape, parent_key.into_iter().collect());
            }
            if matches!(value.kind, TypedExprKind::If { .. }) {
                todo!("M7 If over records");
            }
            let (shape, parent_key) = rscalar_shape(ctx, parent, value);
            (Vec::new(), shape, parent_key.into_iter().collect())
        }
        QueryType::Optional(inner) => match inner.as_ref() {
            QueryType::Record(_) => {
                if ctx.child.is_some() {
                    if !nested_bulk_lane(parent, value) {
                        todo!("M4b nested record item");
                    }
                    ctx.nest_depth += 1;
                    let result = rscalar_shape(ctx, parent, value);
                    ctx.nest_depth -= 1;
                    let (shape, parent_key) = result;
                    return (Vec::new(), shape, parent_key.into_iter().collect());
                }
                if matches!(value.kind, TypedExprKind::If { .. }) {
                    todo!("M7 If over records");
                }
                let (shape, parent_key) = rscalar_shape(ctx, parent, value);
                (Vec::new(), shape, parent_key.into_iter().collect())
            }
            QueryType::List(_) => {
                if ctx.child.is_some() {
                    if let TypedExprKind::Method { base, name, .. } = &value.kind
                        && (name.value == "first" || name.value == "nth")
                        && matches!(&base.kind, TypedExprKind::Array { .. })
                        && !has_scope_ident(value)
                        && !uses_outer(value)
                    {
                        let was_child = ctx.child.take();
                        let result = b1_literal(ctx, value);
                        ctx.child = was_child;
                        return result;
                    }
                    let bulk_middle = ctx.child.as_ref().is_some_and(|tail| tail.per_row);
                    if !bulk_middle || matches!(&value.kind, TypedExprKind::Some { .. }) {
                        todo!("M4b presence: optional list item");
                    }
                    ctx.nest_depth += 1;
                    let result = b1_optional_list(ctx, parent, value);
                    ctx.nest_depth -= 1;
                    return result;
                }
                b1_optional_list(ctx, parent, value)
            }
            QueryType::Tuple(_) => unreachable!(
                "optional-tuple value: Optional(Tuple) node types never arise — wrap_optional maps tuples element-wise, Some-of-tuple is rejected by the final check (QT0027)"
            ),
            _ => todo!("M4b exotic optional item"),
        },
        QueryType::Tuple(tys) => {
            if let TypedExprKind::Ident {
                binding: IdentBinding::Field(fid),
                depth,
                ..
            } = &value.kind
            {
                let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
                match field {
                    Field::Value { columns, .. } => {
                        if columns.len() != tys.len() || tys.iter().any(|t| !t.is_scalar()) {
                            todo!("M7 schema tuple with collection element");
                        }
                        let mut select = Vec::with_capacity(columns.len());
                        let mut shapes = Vec::with_capacity(columns.len());
                        for (i, column) in columns.iter().enumerate() {
                            let output = format!("{prefix}_c{i}");
                            select.push(SelectItem::Column {
                                depth: *depth,
                                column: ctx.schema.column_name(*column).to_string(),
                                output: output.clone(),
                                ty: tys.get(i).and_then(scalar_of),
                            });
                            shapes.push(ResultShape::Column(output));
                        }
                        return (select, ResultShape::Tuple(shapes), Vec::new());
                    }
                    _ => unreachable!("tuple-typed non-value field: schema tuples are value-typed"),
                }
            }
            if let TypedExprKind::Ident {
                binding: IdentBinding::ProjectedField { name },
                depth,
                ..
            } = &value.kind
            {
                if *depth != 0 {
                    todo!("M7 projected composite across levels");
                }
                let shape = projected_shape(parent, name);
                let Some(shape) = shape else {
                    unreachable!(
                        "projected composite {} missing from enclosing outputs: depth-0 projected idents are always projected in the current level",
                        name
                    )
                };
                let (select, shape) = reslot_shape(shape, *depth, &prefix, parent, ctx.schema);
                return (select, shape, Vec::new());
            }
            let TypedExprKind::Tuple { elements } = &value.kind else {
                if let TypedExprKind::If { arms, default } = &value.kind {
                    match distribute_if_tuple(arms, default, tys, value.span, value.ty.clone()) {
                        Some(synthetic) => {
                            return lower_value(ctx, parent, prefix, &synthetic);
                        }
                        None => todo!("M7 non-literal tuple value"),
                    }
                }
                if let TypedExprKind::Method {
                    base, name, args, ..
                } = &value.kind
                    && ["first", "nth"].contains(&name.value.as_str())
                {
                    return collapse_tuple_method(ctx, prefix, base, name, args, tys);
                }
                unreachable!(
                    "remaining tuple-typed value shapes: Tuple/If/ProjectedField nodes and collapses route above; field access over tuples is checker-rejected (QT0002) and no method returns bare tuples"
                )
            };
            if elements.len() != tys.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            let mut select = Vec::new();
            let mut shapes = Vec::with_capacity(elements.len());
            let mut keys = Vec::new();
            for (i, elem) in elements.iter().enumerate() {
                let (mut sub_select, sub_shape, mut sub_keys) =
                    lower_value(ctx, parent, format!("{prefix}_c{i}"), elem);
                select.append(&mut sub_select);
                shapes.push(sub_shape);
                keys.append(&mut sub_keys);
            }
            (select, ResultShape::Tuple(shapes), keys)
        }
        QueryType::Void | QueryType::Unknown => {
            unreachable!("void/unknown value: the checker excludes them")
        }
        _ => unreachable!("scalar values match the first arm"),
    }
}

fn projected_shape<'a>(parent: &'a Level, name: &str) -> Option<&'a ResultShape> {
    parent
        .output_shapes
        .as_ref()?
        .iter()
        .find_map(|(alias, shape)| (alias.as_str() == name).then_some(shape.as_ref()).flatten())
}

fn reslot_shape(
    shape: &ResultShape,
    depth: usize,
    prefix: &str,
    parent: &Level,
    schema: &ValidatedSchema,
) -> (Vec<SelectItem>, ResultShape) {
    match shape {
        ResultShape::Column(name) => {
            unreachable!(
                "reslot over scalar shape {name}: callers reslot composites positionally, scalars inline directly"
            )
        }
        ResultShape::Tuple(shapes) => {
            let mut select = Vec::new();
            let mut out = Vec::with_capacity(shapes.len());
            for (i, sub) in shapes.iter().enumerate() {
                let (mut sub_select, sub_shape) =
                    reslot_position(sub, depth, &format!("{prefix}_c{i}"), parent, schema);
                select.append(&mut sub_select);
                out.push(sub_shape);
            }
            (select, ResultShape::Tuple(out))
        }
        ResultShape::Record(fields) => {
            let mut select = Vec::new();
            let mut out = Vec::with_capacity(fields.len());
            for (name, sub) in fields {
                let (mut sub_select, sub_shape) =
                    reslot_position(sub, depth, &format!("{prefix}_{name}"), parent, schema);
                select.append(&mut sub_select);
                out.push((name.clone(), sub_shape));
            }
            (select, ResultShape::Record(out))
        }
        ResultShape::List { .. } | ResultShape::Single { .. } => (Vec::new(), shape.clone()),
    }
}

fn reslot_position(
    shape: &ResultShape,
    depth: usize,
    output: &str,
    parent: &Level,
    schema: &ValidatedSchema,
) -> (Vec<SelectItem>, ResultShape) {
    match shape {
        ResultShape::Column(name) => (
            vec![SelectItem::Column {
                depth,
                column: name.clone(),
                output: output.to_string(),
                ty: output_ty(&parent.builder, name, schema),
            }],
            ResultShape::Column(output.to_string()),
        ),
        ResultShape::Tuple(_) | ResultShape::Record(_) => {
            reslot_shape(shape, depth, output, parent, schema)
        }
        ResultShape::List { .. } | ResultShape::Single { .. } => (Vec::new(), shape.clone()),
    }
}

fn rows_projection(ctx: &mut Context, base: &TypedExpr, items: &[TypedProjectionItem]) -> Level {
    let mut level = rows(ctx, base);
    if level.pending.is_some() {
        unreachable!("projection over nested/optional lists is rejected upstream");
    }
    let mut select = Vec::with_capacity(items.len());
    let mut fields: Vec<(String, Option<ResultShape>)> = Vec::with_capacity(items.len());
    let mut parent_keys: Vec<String> = Vec::new();
    for item in items {
        let alias = item.alias.value.clone();
        let (mut item_select, shape, mut keys) =
            lower_value(ctx, &mut level, alias.clone(), &item.value);
        select.append(&mut item_select);
        fields.push((
            alias,
            match shape {
                ResultShape::Column(_) => None,
                shape => Some(shape),
            },
        ));
        parent_keys.append(&mut keys);
    }
    if !level.builder.select.is_empty() || !level.builder.unions.is_empty() {
        level = wrap_level(ctx, level);
    }
    level.builder.select = select;
    for key in parent_keys {
        ensure_key(&mut level.builder, &key, ctx.schema);
    }
    level.output_shapes = Some(fields);
    level
}

fn ensure_key(builder: &mut SelectBuilder, col: &str, schema: &ValidatedSchema) {
    if builder
        .select
        .iter()
        .any(|item| matches!(item, SelectItem::AllFrom { .. }))
    {
        return;
    }
    if builder.select.iter().any(|item| match item {
        SelectItem::AllFrom { .. } => false,
        SelectItem::Column { output, .. } | SelectItem::Expr { output, .. } => output == col,
    }) {
        return;
    }
    let ty = output_ty(builder, col, schema);
    builder.select.push(SelectItem::Column {
        depth: 0,
        column: col.to_string(),
        output: col.to_string(),
        ty,
    });
}

fn output_ty(builder: &SelectBuilder, col: &str, schema: &ValidatedSchema) -> Option<ScalarType> {
    let mut found: Option<Option<ScalarType>> = None;
    for item in &builder.select {
        match item {
            SelectItem::AllFrom { .. } => {}
            SelectItem::Column { output, ty, .. } | SelectItem::Expr { output, ty, .. }
                if output == col =>
            {
                found = Some(*ty);
            }
            _ => {}
        }
    }
    if let Some(ty) = found {
        return ty;
    }
    for (name, ty) in from_columns_typed(&builder.from, schema) {
        if name == col {
            return ty;
        }
    }
    None
}

fn table_column_ty(schema: &ValidatedSchema, table: &str, column: &str) -> Option<ScalarType> {
    schema
        .tables
        .iter()
        .find(|t| t.name == table)
        .and_then(|t| t.columns.iter().find(|c| c.name == column))
        .and_then(|c| c.ty)
}

fn temp_column_ty(ctx: &Context, temp: &str, column: &str) -> Option<ScalarType> {
    ctx.statements
        .iter()
        .find(|sunk| matches!(&sunk.kind, StatementKind::Temp { name } if name == temp))
        .and_then(|sunk| {
            sunk.shape
                .columns
                .iter()
                .find(|c| c.name == column)
                .and_then(|c| c.ty)
        })
}

fn tail_root_ident(tail: &TypedExpr) -> Option<&IdentBinding> {
    let mut node = tail;
    loop {
        match &node.kind {
            TypedExprKind::Field { base, .. }
            | TypedExprKind::Method { base, .. }
            | TypedExprKind::Projection { base, .. } => node = base,
            TypedExprKind::Ident { binding, .. } => return Some(binding),
            _ => return None,
        }
    }
}

struct ChildLink {
    child_from: FromClause,
    child_col: String,
    parent_col: String,
    m2m_join: Option<(String, String)>,
}

struct M2mParts {
    junction_table: String,
    junction_parent_col: String,
    junction_target_col: String,
    target_table: String,
    target_pk: String,
    parent_pk: String,
}

fn m2m_parts(schema: &ValidatedSchema, fid: FieldId) -> Option<M2mParts> {
    let relation = &schema.relations[schema.relation_of_field(fid)?.index()];
    let Relation::ManyToMany {
        a_ref,
        b_ref,
        join_table,
        a_col,
        a_pk,
        b_col,
        b_pk,
    } = relation
    else {
        return None;
    };
    let name = |c| schema.column_name(c).to_string();
    let table_of = |fid: FieldId| {
        schema.tables[schema.structs[fid.struct_id().index()].table.index()]
            .name
            .clone()
    };
    let (parent_pk, junction_parent_col, target, target_pk, junction_target_col) = if fid == *a_ref
    {
        (
            name(*a_pk),
            name(*a_col),
            table_of(*b_ref),
            name(*b_pk),
            name(*b_col),
        )
    } else if fid == *b_ref {
        (
            name(*b_pk),
            name(*b_col),
            table_of(*a_ref),
            name(*a_pk),
            name(*a_col),
        )
    } else {
        unreachable!("hop field is not an endpoint of its relation")
    };
    Some(M2mParts {
        junction_table: schema.tables[join_table.index()].name.clone(),
        junction_parent_col,
        junction_target_col,
        target_table: target,
        target_pk,
        parent_pk,
    })
}

fn resolve_tail_link(ctx: &mut Context, fid: FieldId) -> ChildLink {
    let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
    match field {
        Field::Ref { target, .. } => {
            if let Some(parts) = m2m_parts(ctx.schema, fid) {
                let junction_alias = ctx.aliases.table(&parts.junction_table);
                let target_alias = ctx.aliases.table(&parts.target_table);
                return ChildLink {
                    child_from: FromClause::Join {
                        left: Box::new(FromClause::Table {
                            table: parts.junction_table,
                            alias: junction_alias,
                        }),
                        right: Box::new(FromClause::Table {
                            table: parts.target_table,
                            alias: target_alias,
                        }),
                    },
                    child_col: parts.junction_parent_col,
                    parent_col: parts.parent_pk,
                    m2m_join: Some((parts.target_pk, parts.junction_target_col)),
                };
            }
            let table = ctx.schema.tables[ctx.schema.structs[target.index()].table.index()]
                .name
                .clone();
            let (child_col, parent_col) = match link_cols(ctx.schema, fid) {
                LinkCols::ParentFk {
                    parent_fk,
                    child_pk,
                } => (child_pk, parent_fk),
                LinkCols::TargetFk {
                    child_fk,
                    parent_pk,
                } => (child_fk, parent_pk),
                LinkCols::ManyToMany => unreachable!("M2M tails route above"),
            };
            ChildLink {
                child_from: FromClause::Table {
                    table: table.clone(),
                    alias: ctx.aliases.table(&table),
                },
                child_col,
                parent_col,
                m2m_join: None,
            }
        }
        Field::Array {
            element,
            link,
            storage,
            ..
        } => {
            if matches!(element, grove_schema::validated::ValueType::Optional(_)) {
                todo!("M4 R-presence: optional inner list");
            }
            let table = ctx.schema.tables[storage.table.index()].name.clone();
            ChildLink {
                child_from: FromClause::Table {
                    table: table.clone(),
                    alias: ctx.aliases.table(&table),
                },
                child_col: ctx.schema.column_name(storage.key).to_string(),
                parent_col: ctx.schema.column_name(*link).to_string(),
                m2m_join: None,
            }
        }
        Field::Value { .. } => {
            unreachable!("scalar tail root: scalar methods never return collections")
        }
    }
}

fn scalar_ident_temp(ctx: &mut Context, binding: &IdentBinding) -> SqlExpr {
    let (temp, child_fk) = ctx
        .temp_collect
        .as_ref()
        .map(|c| (c.temp.clone(), c.child_fk.clone()))
        .unwrap_or_else(|| unreachable!("temp record outside temp mode"));
    let column = match binding {
        IdentBinding::Field(fid) => {
            let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
            match field {
                Field::Value { columns, .. } => match columns.as_slice() {
                    [col] => ctx.schema.column_name(*col).to_string(),
                    _ => unreachable!(
                        "temp ref to tuple field: temp recording fires in scalar demand, whose callers admit scalar-typed nodes only"
                    ),
                },
                _ => unreachable!(
                    "temp ref to non-value field: temp recording fires in scalar demand, whose callers admit scalar-typed nodes only"
                ),
            }
        }
        IdentBinding::ProjectedField { name } => name.clone(),
        IdentBinding::Root { .. } => {
            unreachable!("root idents are List-typed: scalar() only receives scalar inputs")
        }
        IdentBinding::Prev { .. } => todo!("mutations era: prev"),
    };
    record_temp_ref(ctx, &column);
    temp_lookup(ctx, &temp, &column, &child_fk)
}

fn record_temp_ref(ctx: &mut Context, column: &str) {
    let collect = ctx
        .temp_collect
        .as_mut()
        .unwrap_or_else(|| unreachable!("temp record outside temp mode"));
    if !collect.refs.iter().any(|r| r == column) {
        collect.refs.push(column.to_string());
    }
}

fn temp_lookup(ctx: &Context, temp: &str, column: &str, child_fk: &str) -> SqlExpr {
    temp_lookup_at(ctx, temp, column, child_fk, 1)
}

fn temp_lookup_at(
    ctx: &Context,
    temp: &str,
    column: &str,
    child_fk: &str,
    fk_depth: usize,
) -> SqlExpr {
    let mut query = SelectBuilder::new();
    query.select = vec![SelectItem::Column {
        depth: 0,
        column: column.to_string(),
        output: column.to_string(),
        ty: temp_column_ty(ctx, temp, column),
    }];
    query.from = Some(FromClause::Table {
        table: temp.to_string(),
        alias: temp.to_string(),
    });
    query.where_ = vec![SqlExpr::Binary {
        op: SqlBinOp::Eq,
        lhs: Box::new(SqlExpr::Column {
            depth: 0,
            column: "$$key".to_string(),
        }),
        rhs: Box::new(SqlExpr::Column {
            depth: fk_depth,
            column: child_fk.to_string(),
        }),
    }];
    SqlExpr::Subquery {
        query: Box::new(query),
    }
}

fn free_columns(expr: &SqlExpr, level: usize) -> Vec<String> {
    let mut out = Vec::new();
    free_columns_into(expr, level, &mut out);
    out
}

fn free_columns_into(expr: &SqlExpr, level: usize, out: &mut Vec<String>) {
    match expr {
        SqlExpr::Column { depth, column } => {
            if *depth == level && !out.iter().any(|c| c == column) {
                out.push(column.clone());
            }
        }
        SqlExpr::Param(_) | SqlExpr::Count => {}
        SqlExpr::Binary { lhs, rhs, .. } => {
            free_columns_into(lhs, level, out);
            free_columns_into(rhs, level, out);
        }
        SqlExpr::Not(inner) | SqlExpr::Cast { expr: inner, .. } => {
            free_columns_into(inner, level, out)
        }
        SqlExpr::Func { args, .. } => {
            for arg in args {
                free_columns_into(arg, level, out);
            }
        }
        SqlExpr::Case { arms, else_ } => {
            for (cond, then) in arms {
                free_columns_into(cond, level, out);
                free_columns_into(then, level, out);
            }
            free_columns_into(else_, level, out);
        }
        SqlExpr::In { expr, items } => {
            free_columns_into(expr, level, out);
            for item in items {
                free_columns_into(item, level, out);
            }
        }
        SqlExpr::Coalesce(items) => {
            for item in items {
                free_columns_into(item, level, out);
            }
        }
        SqlExpr::InSubquery { expr, query } => {
            free_columns_into(expr, level, out);
            free_builder_columns(query, level + 1, out);
        }
        SqlExpr::Subquery { query } | SqlExpr::Exists(query) => {
            free_builder_columns(query, level + 1, out)
        }
        SqlExpr::Window {
            partition, order, ..
        } => {
            for part in partition {
                free_columns_into(part, level, out);
            }
            for item in order {
                free_columns_into(&item.expr, level, out);
            }
        }
    }
}

fn free_builder_columns(builder: &SelectBuilder, level: usize, out: &mut Vec<String>) {
    for item in &builder.select {
        match item {
            SelectItem::AllFrom { .. } => {}
            SelectItem::Column { depth, column, .. } => {
                if *depth == level && !out.iter().any(|c| c == column) {
                    out.push(column.clone());
                }
            }
            SelectItem::Expr { expr, .. } => free_columns_into(expr, level, out),
        }
    }
    free_from_columns(&builder.from, level, out);
    for cond in &builder.where_ {
        free_columns_into(cond, level, out);
    }
    for item in &builder.order_by {
        free_columns_into(&item.expr, level, out);
    }
    if let Some(limit) = &builder.limit {
        free_columns_into(limit, level, out);
    }
    if let Some(offset) = &builder.offset {
        free_columns_into(offset, level, out);
    }
    for arm in &builder.unions {
        free_builder_columns(arm, level, out);
    }
}

fn free_from_columns(from: &Option<FromClause>, level: usize, out: &mut Vec<String>) {
    match from {
        None
        | Some(FromClause::Table { .. })
        | Some(FromClause::Temp { .. })
        | Some(FromClause::Literal { .. })
        | Some(FromClause::TableFunction { .. }) => {}
        Some(FromClause::Subquery { query, .. }) => free_builder_columns(query, level + 1, out),
        Some(FromClause::Computed { query, .. }) => free_builder_columns(query, level + 1, out),
        Some(FromClause::Join { left, right }) => {
            free_from_columns(&Some((**left).clone()), level, out);
            free_from_columns(&Some((**right).clone()), level, out);
        }
    }
}

fn lower_tail(
    ctx: &mut Context,
    tail: &TypedExpr,
    child_level: Level,
    per_row: bool,
    partition: Option<String>,
    allow_pending: bool,
) -> Level {
    let anchor = tail_row_struct(ctx, tail);
    let was_child = ctx.child.replace(ChildCtx {
        base: Some(child_level),
        per_row,
        partition,
        anchor,
    });
    let was_outer = std::mem::replace(&mut ctx.no_outer, true);
    let child = rows(ctx, tail);
    ctx.child = was_child;
    ctx.no_outer = was_outer;
    if child.pending.is_some() && !allow_pending {
        todo!("M4b nested collection inside child");
    }
    child
}

fn shift_depths(expr: SqlExpr, by: usize) -> SqlExpr {
    if by == 0 {
        return expr;
    }
    match expr {
        SqlExpr::Column { depth, column } => SqlExpr::Column {
            depth: depth + by,
            column,
        },
        SqlExpr::Param(_) => expr,
        SqlExpr::Count => expr,
        SqlExpr::Binary { op, lhs, rhs } => SqlExpr::Binary {
            op,
            lhs: Box::new(shift_depths(*lhs, by)),
            rhs: Box::new(shift_depths(*rhs, by)),
        },
        SqlExpr::Not(x) => SqlExpr::Not(Box::new(shift_depths(*x, by))),
        SqlExpr::Func { name, args } => SqlExpr::Func {
            name,
            args: args.into_iter().map(|a| shift_depths(a, by)).collect(),
        },
        SqlExpr::Case { arms, else_ } => SqlExpr::Case {
            arms: arms
                .into_iter()
                .map(|(c, t)| (shift_depths(c, by), shift_depths(t, by)))
                .collect(),
            else_: Box::new(shift_depths(*else_, by)),
        },
        SqlExpr::In { expr, items } => SqlExpr::In {
            expr: Box::new(shift_depths(*expr, by)),
            items: items.into_iter().map(|i| shift_depths(i, by)).collect(),
        },
        SqlExpr::InSubquery { expr, query } => SqlExpr::InSubquery {
            expr: Box::new(shift_depths(*expr, by)),
            query: Box::new(shift_builder(*query, by)),
        },
        SqlExpr::Subquery { query } => SqlExpr::Subquery {
            query: Box::new(shift_builder(*query, by)),
        },
        SqlExpr::Exists(query) => SqlExpr::Exists(Box::new(shift_builder(*query, by))),
        SqlExpr::Cast { expr, target } => SqlExpr::Cast {
            expr: Box::new(shift_depths(*expr, by)),
            target,
        },
        SqlExpr::Coalesce(values) => {
            SqlExpr::Coalesce(values.into_iter().map(|v| shift_depths(v, by)).collect())
        }
        SqlExpr::Window {
            func,
            partition,
            order,
        } => SqlExpr::Window {
            func,
            partition: partition.into_iter().map(|p| shift_depths(p, by)).collect(),
            order: order
                .into_iter()
                .map(|item| OrderItem {
                    expr: shift_depths(item.expr, by),
                    dir: item.dir,
                })
                .collect(),
        },
    }
}

fn shift_outer_depths(expr: SqlExpr, by: usize) -> SqlExpr {
    if by == 0 {
        return expr;
    }
    match expr {
        SqlExpr::Column { depth, column } if depth > 0 => SqlExpr::Column {
            depth: depth + by,
            column,
        },
        SqlExpr::InSubquery { expr, query } => SqlExpr::InSubquery {
            expr: Box::new(shift_outer_depths(*expr, by)),
            query: Box::new(shift_outer_builder(*query, by)),
        },
        SqlExpr::Subquery { query } => SqlExpr::Subquery {
            query: Box::new(shift_outer_builder(*query, by)),
        },
        SqlExpr::Exists(query) => SqlExpr::Exists(Box::new(shift_outer_builder(*query, by))),
        SqlExpr::Binary { op, lhs, rhs } => SqlExpr::Binary {
            op,
            lhs: Box::new(shift_outer_depths(*lhs, by)),
            rhs: Box::new(shift_outer_depths(*rhs, by)),
        },
        SqlExpr::Not(x) => SqlExpr::Not(Box::new(shift_outer_depths(*x, by))),
        SqlExpr::Func { name, args } => SqlExpr::Func {
            name,
            args: args
                .into_iter()
                .map(|a| shift_outer_depths(a, by))
                .collect(),
        },
        SqlExpr::Case { arms, else_ } => SqlExpr::Case {
            arms: arms
                .into_iter()
                .map(|(c, t)| (shift_outer_depths(c, by), shift_outer_depths(t, by)))
                .collect(),
            else_: Box::new(shift_outer_depths(*else_, by)),
        },
        SqlExpr::In { expr, items } => SqlExpr::In {
            expr: Box::new(shift_outer_depths(*expr, by)),
            items: items
                .into_iter()
                .map(|i| shift_outer_depths(i, by))
                .collect(),
        },
        SqlExpr::Cast { expr, target } => SqlExpr::Cast {
            expr: Box::new(shift_outer_depths(*expr, by)),
            target,
        },
        SqlExpr::Coalesce(values) => SqlExpr::Coalesce(
            values
                .into_iter()
                .map(|v| shift_outer_depths(v, by))
                .collect(),
        ),
        SqlExpr::Window {
            func,
            partition,
            order,
        } => SqlExpr::Window {
            func,
            partition: partition
                .into_iter()
                .map(|p| shift_outer_depths(p, by))
                .collect(),
            order: order
                .into_iter()
                .map(|item| OrderItem {
                    expr: shift_outer_depths(item.expr, by),
                    dir: item.dir,
                })
                .collect(),
        },
        other => other,
    }
}

fn shift_outer_builder(mut builder: SelectBuilder, by: usize) -> SelectBuilder {
    builder.select = builder
        .select
        .into_iter()
        .map(|item| match item {
            SelectItem::AllFrom { depth } if depth > 0 => SelectItem::AllFrom { depth: depth + by },
            SelectItem::Column {
                depth,
                column,
                output,
                ty,
            } if depth > 0 => SelectItem::Column {
                depth: depth + by,
                column,
                output,
                ty,
            },
            SelectItem::Expr { expr, output, ty } => SelectItem::Expr {
                expr: shift_outer_depths(expr, by),
                output,
                ty,
            },
            item => item,
        })
        .collect();
    builder.where_ = builder
        .where_
        .into_iter()
        .map(|w| shift_outer_depths(w, by))
        .collect();
    builder.order_by = builder
        .order_by
        .into_iter()
        .map(|item| OrderItem {
            expr: shift_outer_depths(item.expr, by),
            dir: item.dir,
        })
        .collect();
    if let Some(limit) = builder.limit.take() {
        builder.limit = Some(shift_outer_depths(limit, by));
    }
    if let Some(offset) = builder.offset.take() {
        builder.offset = Some(shift_outer_depths(offset, by));
    }
    builder.unions = builder
        .unions
        .into_iter()
        .map(|arm| shift_outer_builder(arm, by))
        .collect();
    if let Some(from) = builder.from.take() {
        builder.from = Some(shift_outer_from(from, by));
    }
    builder
}

fn shift_nest_expr(expr: SqlExpr, floor: usize) -> SqlExpr {
    match expr {
        SqlExpr::Column { depth, column } if depth > floor => SqlExpr::Column {
            depth: depth + 1,
            column,
        },
        SqlExpr::InSubquery { expr, query } => SqlExpr::InSubquery {
            expr: Box::new(shift_nest_expr(*expr, floor)),
            query: Box::new(shift_nest_builder(*query, floor + 1)),
        },
        SqlExpr::Subquery { query } => SqlExpr::Subquery {
            query: Box::new(shift_nest_builder(*query, floor + 1)),
        },
        SqlExpr::Exists(query) => SqlExpr::Exists(Box::new(shift_nest_builder(*query, floor + 1))),
        SqlExpr::Binary { op, lhs, rhs } => SqlExpr::Binary {
            op,
            lhs: Box::new(shift_nest_expr(*lhs, floor)),
            rhs: Box::new(shift_nest_expr(*rhs, floor)),
        },
        SqlExpr::Not(x) => SqlExpr::Not(Box::new(shift_nest_expr(*x, floor))),
        SqlExpr::Func { name, args } => SqlExpr::Func {
            name,
            args: args
                .into_iter()
                .map(|a| shift_nest_expr(a, floor))
                .collect(),
        },
        SqlExpr::Case { arms, else_ } => SqlExpr::Case {
            arms: arms
                .into_iter()
                .map(|(c, t)| (shift_nest_expr(c, floor), shift_nest_expr(t, floor)))
                .collect(),
            else_: Box::new(shift_nest_expr(*else_, floor)),
        },
        SqlExpr::In { expr, items } => SqlExpr::In {
            expr: Box::new(shift_nest_expr(*expr, floor)),
            items: items
                .into_iter()
                .map(|i| shift_nest_expr(i, floor))
                .collect(),
        },
        SqlExpr::Cast { expr, target } => SqlExpr::Cast {
            expr: Box::new(shift_nest_expr(*expr, floor)),
            target,
        },
        SqlExpr::Coalesce(values) => SqlExpr::Coalesce(
            values
                .into_iter()
                .map(|v| shift_nest_expr(v, floor))
                .collect(),
        ),
        SqlExpr::Window {
            func,
            partition,
            order,
        } => SqlExpr::Window {
            func,
            partition: partition
                .into_iter()
                .map(|p| shift_nest_expr(p, floor))
                .collect(),
            order: order
                .into_iter()
                .map(|item| OrderItem {
                    expr: shift_nest_expr(item.expr, floor),
                    dir: item.dir,
                })
                .collect(),
        },
        other => other,
    }
}

fn nest_floor(from: &Option<FromClause>) -> usize {
    match from {
        Some(FromClause::Join { .. }) => 1,
        _ => 0,
    }
}

fn shift_nest_builder(mut builder: SelectBuilder, floor: usize) -> SelectBuilder {
    builder.select = builder
        .select
        .into_iter()
        .map(|item| match item {
            SelectItem::AllFrom { depth } if depth > floor => {
                SelectItem::AllFrom { depth: depth + 1 }
            }
            SelectItem::Column {
                depth,
                column,
                output,
                ty,
            } if depth > floor => SelectItem::Column {
                depth: depth + 1,
                column,
                output,
                ty,
            },
            SelectItem::Expr { expr, output, ty } => SelectItem::Expr {
                expr: shift_nest_expr(expr, floor),
                output,
                ty,
            },
            item => item,
        })
        .collect();
    builder.where_ = builder
        .where_
        .into_iter()
        .map(|w| shift_nest_expr(w, floor))
        .collect();
    builder.order_by = builder
        .order_by
        .into_iter()
        .map(|item| OrderItem {
            expr: shift_nest_expr(item.expr, floor),
            dir: item.dir,
        })
        .collect();
    if let Some(limit) = builder.limit.take() {
        builder.limit = Some(shift_nest_expr(limit, floor));
    }
    if let Some(offset) = builder.offset.take() {
        builder.offset = Some(shift_nest_expr(offset, floor));
    }
    builder.unions = builder
        .unions
        .into_iter()
        .map(|arm| {
            let floor = nest_floor(&arm.from);
            shift_nest_builder(arm, floor)
        })
        .collect();
    if let Some(from) = builder.from.take() {
        builder.from = Some(shift_nest_from(from));
    }
    builder
}

fn shift_nest_from(from: FromClause) -> FromClause {
    match from {
        FromClause::Join { left, right } => FromClause::Join {
            left: Box::new(shift_nest_from(*left)),
            right: Box::new(shift_nest_from(*right)),
        },
        from => from,
    }
}

fn shift_outer_from(from: FromClause, by: usize) -> FromClause {
    match from {
        FromClause::Join { left, right } => FromClause::Join {
            left: Box::new(shift_outer_from(*left, by)),
            right: Box::new(shift_outer_from(*right, by)),
        },
        FromClause::Subquery { query, alias } => FromClause::Subquery {
            query: Box::new(shift_outer_builder(*query, by)),
            alias,
        },
        FromClause::Computed {
            query,
            alias,
            columns,
            middle_parts,
        } => FromClause::Computed {
            query: Box::new(shift_outer_builder(*query, by)),
            alias,
            columns,
            middle_parts,
        },
        from => from,
    }
}

fn shift_builder(mut builder: SelectBuilder, by: usize) -> SelectBuilder {
    builder.select = builder
        .select
        .into_iter()
        .map(|item| match item {
            SelectItem::AllFrom { depth } => SelectItem::AllFrom { depth: depth + by },
            SelectItem::Column {
                depth,
                column,
                output,
                ty,
            } => SelectItem::Column {
                depth: depth + by,
                column,
                output,
                ty,
            },
            SelectItem::Expr { expr, output, ty } => SelectItem::Expr {
                expr: shift_depths(expr, by),
                output,
                ty,
            },
        })
        .collect();
    builder.where_ = builder
        .where_
        .into_iter()
        .map(|w| shift_depths(w, by))
        .collect();
    builder.order_by = builder
        .order_by
        .into_iter()
        .map(|item| OrderItem {
            expr: shift_depths(item.expr, by),
            dir: item.dir,
        })
        .collect();
    if let Some(limit) = builder.limit.take() {
        builder.limit = Some(shift_depths(limit, by));
    }
    if let Some(offset) = builder.offset.take() {
        builder.offset = Some(shift_depths(offset, by));
    }
    builder.unions = builder
        .unions
        .into_iter()
        .map(|arm| shift_builder(arm, by))
        .collect();
    if let Some(from) = builder.from.take() {
        builder.from = Some(shift_from(from, by));
    }
    builder
}

fn shift_from(from: FromClause, by: usize) -> FromClause {
    match from {
        FromClause::Table { .. } | FromClause::TableFunction { .. } => from,
        FromClause::Temp { .. } => from,
        FromClause::Literal { .. } => from,
        FromClause::Join { left, right } => FromClause::Join {
            left: Box::new(shift_from(*left, by)),
            right: Box::new(shift_from(*right, by)),
        },
        FromClause::Subquery { query, alias } => FromClause::Subquery {
            query: Box::new(shift_builder(*query, by)),
            alias,
        },
        FromClause::Computed {
            query,
            alias,
            columns,
            middle_parts,
        } => FromClause::Computed {
            query: Box::new(shift_builder(*query, by)),
            alias,
            columns,
            middle_parts,
        },
    }
}
fn bulk_query() -> SelectBuilder {
    SelectBuilder {
        select: vec![SelectItem::Column {
            depth: 0,
            column: "value".to_string(),
            output: "value".to_string(),
            ty: Some(ScalarType::Int),
        }],
        from: Some(FromClause::TableFunction {
            name: "$$bulk_params".to_string(),
        }),
        where_: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        unions: Vec::new(),
    }
}

fn available_cols(builder: &SelectBuilder, schema: &ValidatedSchema) -> Vec<String> {
    if builder.select.is_empty() && !builder.unions.is_empty() {
        return union_outputs(builder, schema);
    }
    if builder
        .select
        .iter()
        .any(|item| matches!(item, SelectItem::AllFrom { .. }))
    {
        return available_from_cols(&builder.from, schema);
    }
    builder
        .select
        .iter()
        .filter_map(|item| match item {
            SelectItem::AllFrom { .. } => None,
            SelectItem::Column { output, .. } | SelectItem::Expr { output, .. } => {
                Some(output.clone())
            }
        })
        .collect()
}

fn available_from_cols(from: &Option<FromClause>, schema: &ValidatedSchema) -> Vec<String> {
    match from {
        Some(FromClause::Table { table, .. }) => schema
            .tables
            .iter()
            .find(|t| &t.name == table)
            .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default(),
        Some(FromClause::Subquery { query, .. }) => available_cols(query, schema),
        Some(FromClause::Temp { columns, .. }) => columns.iter().map(|c| c.name.clone()).collect(),
        Some(FromClause::Literal { columns, .. }) | Some(FromClause::Computed { columns, .. }) => {
            columns.iter().map(|c| c.name.clone()).collect()
        }
        Some(FromClause::TableFunction { .. }) => vec!["value".to_string()],
        Some(FromClause::Join { left, right }) => {
            let mut cols = available_from_cols(&Some((**left).clone()), schema);
            cols.extend(available_from_cols(&Some((**right).clone()), schema));
            cols
        }
        None => Vec::new(),
    }
}

struct CorrCond {
    outer_col: String,
    outer_struct: StructId,
    op: SqlBinOp,
    child: TypedExpr,
    nav: Option<NavCarry>,
}

struct NavCarry {
    carry: String,
    target: StructId,
    parent_col: String,
    target_col: String,
    endpoint: String,
    endpoint_ty: ScalarType,
    default: Option<SqlExpr>,
}

enum OuterRef {
    Field(FieldId),
    Projected(String),
    Nav(NavCarry),
}

fn split_cross_tail(
    ctx: &mut Context,
    tail: &TypedExpr,
    temp_form: bool,
) -> (TypedExpr, Vec<CorrCond>, Vec<TypedExpr>) {
    let mut correlated: Vec<CorrCond> = Vec::new();
    let mut broadcast: Vec<TypedExpr> = Vec::new();
    let pruned = prune_cross_filters(ctx, tail, &mut correlated, &mut broadcast, temp_form);
    (pruned, correlated, broadcast)
}

fn prune_cross_filters(
    ctx: &mut Context,
    node: &TypedExpr,
    correlated: &mut Vec<CorrCond>,
    broadcast: &mut Vec<TypedExpr>,
    temp_form: bool,
) -> TypedExpr {
    match &node.kind {
        TypedExprKind::Method {
            base,
            name,
            args,
            optional,
        } if name.value == "filter" => {
            let arg = one_arg(args, name);
            let mut locals: Vec<TypedExpr> = Vec::new();
            for conjunct in split_and(arg) {
                if uses_outer(&conjunct) {
                    if matches!(&conjunct.kind, TypedExprKind::Binary { op, .. } if op.value == BinaryOp::Or)
                    {
                        let (conds, arms) = split_disjuncts(ctx, &conjunct, temp_form);
                        correlated.extend(conds);
                        broadcast.extend(arms);
                    } else {
                        correlated.push(check_corr(ctx, &conjunct, temp_form));
                    }
                } else {
                    locals.push(conjunct);
                }
            }
            let pruned_base = prune_cross_filters(ctx, base, correlated, broadcast, temp_form);
            match locals.as_slice() {
                [] => pruned_base,
                [single] => rebuild_filter(node, pruned_base, single.clone()),
                _ => {
                    let mut folded = locals.remove(0);
                    for next in locals {
                        folded = and_expr(&folded, &next);
                    }
                    rebuild_filter(node, pruned_base, folded)
                }
            }
        }
        TypedExprKind::Method { base, name, .. }
            if ["sort", "sort_asc", "sort_desc"].contains(&name.value.as_str()) =>
        {
            let pruned_base = prune_cross_filters(ctx, base, correlated, broadcast, temp_form);
            rebuild_same(node, pruned_base)
        }
        TypedExprKind::Method { base, name, .. }
            if ["take", "skip"].contains(&name.value.as_str()) =>
        {
            let pruned_base = prune_cross_filters(ctx, base, correlated, broadcast, temp_form);
            rebuild_same(node, pruned_base)
        }
        TypedExprKind::Projection { base, items } => {
            for item in items {
                if !item.value.ty.is_scalar() {
                    todo!("M4b cross projection correlation");
                }
                if uses_outer(&item.value) {
                    todo!("M4b cross projection correlation");
                }
            }
            let pruned_base = prune_cross_filters(ctx, base, correlated, broadcast, temp_form);
            rebuild_same(node, pruned_base)
        }
        TypedExprKind::Ident { .. } => node.clone(),
        _ => todo!("M4b cross tail shape"),
    }
}

fn peel_cross_window(
    node: &TypedExpr,
    suffix: &mut Vec<(String, Vec<TypedMethodArg>)>,
    outer_bounds: bool,
) -> TypedExpr {
    match &node.kind {
        TypedExprKind::Method {
            base,
            name,
            args,
            optional,
        } if ["take", "skip"].contains(&name.value.as_str()) && !optional => {
            if !outer_bounds {
                for arg in args {
                    if uses_outer(&arg.expr) {
                        todo!("M4b temp-bound per-row limit");
                    }
                }
            }
            suffix.push((name.value.clone(), args.clone()));
            peel_cross_window(base, suffix, outer_bounds)
        }
        TypedExprKind::Method { .. } | TypedExprKind::Projection { .. } => rebuild_same(
            node,
            peel_cross_window(tail_base(node), suffix, outer_bounds),
        ),
        _ => node.clone(),
    }
}

fn stacked_takes_foldable(tail: &TypedExpr) -> bool {
    let mut node = tail;
    let mut seen_filter_sort = false;
    loop {
        match &node.kind {
            TypedExprKind::Method {
                base,
                name,
                optional,
                ..
            } if ["take", "skip"].contains(&name.value.as_str()) && !optional => {
                if seen_filter_sort {
                    return false;
                }
                node = base;
            }
            TypedExprKind::Projection { base, .. } => {
                node = base;
            }
            TypedExprKind::Method { base, .. } => {
                seen_filter_sort = true;
                node = base;
            }
            _ => return true,
        }
    }
}

fn fold_broadcast_take(
    tail_ty: &QueryType,
    tail: &TypedExpr,
    suffix: &[(String, Vec<TypedMethodArg>)],
) -> Option<(usize, Option<usize>)> {
    if !matches!(tail_ty, QueryType::List(_)) {
        return None;
    }
    if !stacked_takes_foldable(tail) {
        return None;
    }
    let mut node = tail;
    loop {
        match &node.kind {
            TypedExprKind::Method { base, name, .. }
                if ["sort", "sort_asc", "sort_desc"].contains(&name.value.as_str()) =>
            {
                return None;
            }
            TypedExprKind::Method { base, .. } | TypedExprKind::Projection { base, .. } => {
                node = base;
            }
            _ => break,
        }
    }
    if !stacked_takes_foldable(tail) {
        return None;
    }
    let mut offset: usize = 0;
    let mut limit: Option<usize> = None;
    for (method, args) in suffix.iter().rev() {
        let [arg] = args.as_slice() else {
            return None;
        };
        let TypedExprKind::Literal(lit) = &arg.expr.kind else {
            return None;
        };
        let crate::ast::Literal::Int(n) = &lit.value else {
            return None;
        };
        let n: usize = (*n).try_into().ok()?;
        match method.as_str() {
            "take" => limit = Some(limit.unwrap_or(usize::MAX).min(n)),
            "skip" => {
                offset = offset.saturating_add(n);
                limit = limit.map(|l| l.saturating_sub(n));
            }
            _ => return None,
        }
    }
    Some((offset, limit))
}

fn tail_base(node: &TypedExpr) -> &TypedExpr {
    match &node.kind {
        TypedExprKind::Method { base, .. } | TypedExprKind::Projection { base, .. } => base,
        _ => unreachable!("tail_base below spine hops: prune admits no such tails"),
    }
}

fn tail_take_trailing(tail: &TypedExpr) -> bool {
    let mut node = tail;
    let mut above = false;
    loop {
        match &node.kind {
            TypedExprKind::Method {
                base,
                name,
                optional,
                ..
            } => {
                if (name.value == "take" || name.value == "skip") && !optional {
                    return !above;
                }
                if name.value != "take" && name.value != "skip" {
                    above = true;
                }
                node = base;
            }
            TypedExprKind::Projection { .. } => {
                node = tail_base(node);
            }
            _ => return true,
        }
    }
}

fn peel_cross_sorts(node: &TypedExpr, sorts: &mut Vec<(String, Vec<TypedMethodArg>)>) -> TypedExpr {
    match &node.kind {
        TypedExprKind::Method {
            base,
            name,
            args,
            optional,
        } if ["sort", "sort_asc", "sort_desc"].contains(&name.value.as_str()) && !optional => {
            if args.iter().any(|a| uses_outer(&a.expr)) {
                sorts.push((name.value.clone(), args.clone()));
                return peel_cross_sorts(base, sorts);
            }
            rebuild_same(node, peel_cross_sorts(base, sorts))
        }
        TypedExprKind::Method { .. } | TypedExprKind::Projection { .. } => {
            rebuild_same(node, peel_cross_sorts(tail_base(node), sorts))
        }
        _ => node.clone(),
    }
}

fn tail_has_limit_op(tail: &TypedExpr) -> bool {
    match &tail.kind {
        TypedExprKind::Method {
            base, name, args, ..
        } if ["take", "skip", "nth"].contains(&name.value.as_str()) => true,
        TypedExprKind::Method { base, args, .. } => {
            tail_has_limit_op(base) || args.iter().any(|a| tail_has_limit_op(&a.expr))
        }
        TypedExprKind::Projection { base, items } => {
            tail_has_limit_op(base) || items.iter().any(|i| tail_has_limit_op(&i.value))
        }
        TypedExprKind::Field { base, .. } => tail_has_limit_op(base),
        TypedExprKind::Binary { lhs, rhs, .. } => tail_has_limit_op(lhs) || tail_has_limit_op(rhs),
        TypedExprKind::Unary { expr, .. } | TypedExprKind::Cast { expr, .. } => {
            tail_has_limit_op(expr)
        }
        TypedExprKind::If { arms, default } => {
            arms.iter()
                .any(|(c, t)| tail_has_limit_op(c) || tail_has_limit_op(t))
                || tail_has_limit_op(default)
        }
        _ => false,
    }
}

fn sort_key_divertible(key: &TypedExpr) -> bool {
    match &key.kind {
        TypedExprKind::Ident {
            binding: IdentBinding::Field(_),
            ..
        } => true,
        TypedExprKind::Ident { .. } => !uses_outer(key),
        TypedExprKind::Field { base, .. }
        | TypedExprKind::Method { base, .. }
        | TypedExprKind::Projection { base, .. } => sort_key_divertible(base),
        TypedExprKind::Binary { lhs, rhs, .. } => {
            sort_key_divertible(lhs) && sort_key_divertible(rhs)
        }
        TypedExprKind::Unary { expr, .. } | TypedExprKind::Cast { expr, .. } => {
            sort_key_divertible(expr)
        }
        TypedExprKind::If { arms, default } => {
            arms.iter()
                .all(|(c, t)| sort_key_divertible(c) && sort_key_divertible(t))
                && sort_key_divertible(default)
        }
        _ => !uses_outer(key),
    }
}

fn split_or(expr: &TypedExpr) -> Vec<TypedExpr> {
    match &expr.kind {
        TypedExprKind::Binary { op, lhs, rhs } if op.value == BinaryOp::Or => {
            let mut out = split_or(lhs);
            out.extend(split_or(rhs));
            out
        }
        _ => vec![expr.clone()],
    }
}

fn split_disjuncts(
    ctx: &mut Context,
    conjunct: &TypedExpr,
    temp_form: bool,
) -> (Vec<CorrCond>, Vec<TypedExpr>) {
    let mut conds = Vec::new();
    let mut locals = Vec::new();
    for disjunct in split_or(conjunct) {
        if uses_outer(&disjunct) {
            conds.push(check_corr(ctx, &disjunct, temp_form));
        } else {
            locals.push(disjunct);
        }
    }
    (conds, locals)
}

fn split_and(expr: &TypedExpr) -> Vec<TypedExpr> {
    match &expr.kind {
        TypedExprKind::Binary { op, lhs, rhs } if op.value == BinaryOp::And => {
            let mut out = split_and(lhs);
            out.extend(split_and(rhs));
            out
        }
        _ => vec![expr.clone()],
    }
}

fn nav_carry(ctx: &mut Context, outer_side: &TypedExpr) -> NavCarry {
    let (base, endpoint) = match &outer_side.kind {
        TypedExprKind::Field { base, name, .. } => (base.as_ref(), name.value.clone()),
        _ => todo!("M4b complex cross-root correlation"),
    };
    let ref_fid = match &base.kind {
        TypedExprKind::Ident {
            binding: IdentBinding::Field(fid),
            depth,
            ..
        } if *depth > 0 => match &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()] {
            Field::Ref { .. } => *fid,
            _ => todo!("M4b complex cross-root correlation"),
        },
        _ => todo!("M4b complex cross-root correlation"),
    };
    let target = match &ctx.schema.structs[ref_fid.struct_id().index()].fields[ref_fid.local()] {
        Field::Ref { target, .. } => *target,
        _ => unreachable!("nav root re-resolved non-Ref: checked above"),
    };
    let (parent_col, target_col) = match link_cols(ctx.schema, ref_fid) {
        LinkCols::ParentFk {
            parent_fk,
            child_pk,
        } => (parent_fk, child_pk),
        LinkCols::TargetFk {
            child_fk,
            parent_pk,
        } => (parent_pk, child_fk),
        LinkCols::ManyToMany => todo!("M4b complex cross-root correlation"),
    };
    let (endpoint_col, endpoint_ty) = match ctx.schema.struct_field(target, &endpoint) {
        Some((_, Field::Value { ty, columns, .. })) => match columns.as_slice() {
            [col] => match value_scalar(ty) {
                Some(scalar) => (ctx.schema.column_name(*col).to_string(), scalar),
                None => todo!("M4b complex cross-root correlation"),
            },
            _ => todo!("M4b complex cross-root correlation"),
        },
        _ => todo!("M4b complex cross-root correlation"),
    };
    NavCarry {
        carry: ctx.aliases.nav(),
        target,
        parent_col,
        target_col,
        endpoint: endpoint_col,
        endpoint_ty,
        default: None,
    }
}

fn nav_carry_default(ctx: &mut Context, outer_side: &TypedExpr) -> NavCarry {
    let (base, default) = match &outer_side.kind {
        TypedExprKind::Method {
            base, name, args, ..
        } if name.value == "unwrap_or" => (base.as_ref(), one_arg(args, name)),
        _ => todo!("M4b complex cross-root correlation"),
    };
    let mut carry = nav_carry(ctx, base);
    carry.default = Some(scalar(ctx, default));
    carry
}

fn tail_uses_nav_outer(ctx: &Context, tail: &TypedExpr) -> bool {
    match &tail.kind {
        TypedExprKind::Field { base, .. } => {
            if let TypedExprKind::Ident {
                binding: IdentBinding::Field(fid),
                depth,
                ..
            } = &base.kind
                && *depth > 0
                && let Field::Ref { .. } =
                    &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()]
            {
                return true;
            }
            tail_uses_nav_outer(ctx, base)
        }
        TypedExprKind::Method { base, args, .. } => {
            tail_uses_nav_outer(ctx, base) || args.iter().any(|a| tail_uses_nav_outer(ctx, &a.expr))
        }
        TypedExprKind::Binary { lhs, rhs, .. } => {
            tail_uses_nav_outer(ctx, lhs) || tail_uses_nav_outer(ctx, rhs)
        }
        TypedExprKind::Unary { expr, .. } | TypedExprKind::Cast { expr, .. } => {
            tail_uses_nav_outer(ctx, expr)
        }
        TypedExprKind::If { arms, default } => {
            arms.iter()
                .any(|(c, t)| tail_uses_nav_outer(ctx, c) || tail_uses_nav_outer(ctx, t))
                || tail_uses_nav_outer(ctx, default)
        }
        TypedExprKind::Projection { base, items } => {
            tail_uses_nav_outer(ctx, base)
                || items.iter().any(|i| tail_uses_nav_outer(ctx, &i.value))
        }
        TypedExprKind::Some { value } => tail_uses_nav_outer(ctx, value),
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            elements.iter().any(|e| tail_uses_nav_outer(ctx, e))
        }
        TypedExprKind::Struct { fields } => fields.iter().any(|(_, e)| tail_uses_nav_outer(ctx, e)),
        _ => false,
    }
}

fn nav_carry_item(ctx: &mut Context, nav: &NavCarry) -> SelectItem {
    let target_table = ctx.schema.tables[ctx.schema.structs[nav.target.index()].table.index()]
        .name
        .clone();
    let target_alias = ctx.aliases.table(&target_table);
    let mut nav_q = SelectBuilder::new();
    nav_q.select = vec![SelectItem::Column {
        depth: 0,
        column: nav.endpoint.clone(),
        output: nav.endpoint.clone(),
        ty: Some(nav.endpoint_ty),
    }];
    nav_q.from = Some(FromClause::Table {
        table: target_table,
        alias: target_alias,
    });
    nav_q.where_ = vec![SqlExpr::Binary {
        op: SqlBinOp::Eq,
        lhs: Box::new(SqlExpr::Column {
            depth: 0,
            column: nav.target_col.clone(),
        }),
        rhs: Box::new(SqlExpr::Column {
            depth: 1,
            column: nav.parent_col.clone(),
        }),
    }];
    SelectItem::Expr {
        expr: match &nav.default {
            Some(default) => SqlExpr::Coalesce(vec![
                SqlExpr::Subquery {
                    query: Box::new(nav_q),
                },
                default.clone(),
            ]),
            None => SqlExpr::Subquery {
                query: Box::new(nav_q),
            },
        },
        output: nav.carry.clone(),
        ty: Some(nav.endpoint_ty),
    }
}

fn check_corr(ctx: &mut Context, conjunct: &TypedExpr, temp_form: bool) -> CorrCond {
    match &conjunct.kind {
        TypedExprKind::Binary { op, lhs, rhs } => {
            let sql_op = match op.value {
                BinaryOp::Eq => SqlBinOp::Eq,
                BinaryOp::Ne => SqlBinOp::Ne,
                BinaryOp::Lt => SqlBinOp::Lt,
                BinaryOp::Gt => SqlBinOp::Gt,
                BinaryOp::Le => SqlBinOp::Le,
                BinaryOp::Ge => SqlBinOp::Ge,
                _ => todo!("M4b complex cross-root correlation"),
            };
            let (outer_side, child_side, swapped) = match (uses_outer(lhs), uses_outer(rhs)) {
                (true, false) => (lhs.as_ref(), rhs.as_ref(), false),
                (false, true) => (rhs.as_ref(), lhs.as_ref(), true),
                _ => todo!("M4b complex cross-root correlation"),
            };
            let sql_op = if swapped {
                match sql_op {
                    SqlBinOp::Lt => SqlBinOp::Gt,
                    SqlBinOp::Gt => SqlBinOp::Lt,
                    SqlBinOp::Le => SqlBinOp::Ge,
                    SqlBinOp::Ge => SqlBinOp::Le,
                    symmetric => symmetric,
                }
            } else {
                sql_op
            };
            if uses_outer(child_side) {
                todo!("M4b nested cross-root correlation");
            }
            let outer_ref = match &outer_side.kind {
                TypedExprKind::Ident {
                    binding: IdentBinding::Field(fid),
                    ..
                } => OuterRef::Field(*fid),
                TypedExprKind::Ident {
                    binding: IdentBinding::ProjectedField { name },
                    ..
                } if temp_form => OuterRef::Projected(name.clone()),
                TypedExprKind::Field { base, name, .. }
                    if matches!(
                        &base.kind,
                        TypedExprKind::Ident {
                            binding: IdentBinding::Field(_),
                            depth,
                            ..
                        } if *depth > 0
                    ) =>
                {
                    if !temp_form {
                        todo!("M4b complex cross-root correlation");
                    }
                    OuterRef::Nav(nav_carry(ctx, outer_side))
                }
                TypedExprKind::Method { name, .. } if name.value == "unwrap_or" && temp_form => {
                    OuterRef::Nav(nav_carry_default(ctx, outer_side))
                }
                _ => todo!("M4b complex cross-root correlation"),
            };
            let (outer_struct, outer_col, nav) = match outer_ref {
                OuterRef::Field(fid) => {
                    let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
                    match field {
                        Field::Value { columns, .. } => match columns.as_slice() {
                            [col] => (
                                fid.struct_id(),
                                ctx.schema.column_name(*col).to_string(),
                                None,
                            ),
                            _ => todo!("M4b complex cross-root correlation"),
                        },
                        _ => todo!("M4b complex cross-root correlation"),
                    }
                }
                OuterRef::Projected(name) => {
                    (StructId::new(0), name, None)
                }
                OuterRef::Nav(carry) => {
                    let outer_col = carry.carry.clone();
                    (StructId::new(0), outer_col, Some(carry))
                }
            };
            CorrCond {
                outer_struct,
                outer_col,
                op: sql_op,
                child: child_side.clone(),
                nav,
            }
        }
        _ => todo!("M4b complex cross-root correlation"),
    }
}

fn rebuild_filter(node: &TypedExpr, base: TypedExpr, pred: TypedExpr) -> TypedExpr {
    match &node.kind {
        TypedExprKind::Method { name, optional, .. } => TypedExpr {
            kind: TypedExprKind::Method {
                base: Box::new(base),
                name: name.clone(),
                args: vec![TypedMethodArg {
                    direction: None,
                    expr: pred,
                }],
                optional: *optional,
            },
            ty: node.ty.clone(),
            span: node.span,
        },
        _ => unreachable!("rebuild_filter on non-filter node"),
    }
}

fn rebuild_same(node: &TypedExpr, base: TypedExpr) -> TypedExpr {
    let mut rebuilt = node.clone();
    match &mut rebuilt.kind {
        TypedExprKind::Method { base: b, .. } => **b = base,
        TypedExprKind::Projection { base: b, .. } => **b = base,
        _ => unreachable!("rebuild_same on other node"),
    }
    rebuilt
}

fn and_expr(lhs: &TypedExpr, rhs: &TypedExpr) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::Binary {
            op: grove_types::Spanned {
                value: BinaryOp::And,
                span: lhs.span,
            },
            lhs: Box::new(lhs.clone()),
            rhs: Box::new(rhs.clone()),
        },
        ty: QueryType::Scalar(ScalarType::Bool),
        span: lhs.span,
    }
}

fn pkey_arm_cond(ctx: &mut Context, corr: &CorrCond) -> SqlExpr {
    let was_outer = std::mem::replace(&mut ctx.no_outer, true);
    let child_frag = shift_depths(scalar(ctx, &corr.child), 1);
    ctx.no_outer = was_outer;
    let op = match corr.op {
        SqlBinOp::Ne => SqlBinOp::Eq,
        op => op,
    };
    SqlExpr::Binary {
        op,
        lhs: Box::new(SqlExpr::Column {
            depth: 0,
            column: corr.outer_col.clone(),
        }),
        rhs: Box::new(child_frag),
    }
}

fn exclude_broadcast(ctx: &mut Context, cond: SqlExpr, broadcast: &[TypedExpr]) -> SqlExpr {
    let was_outer = std::mem::replace(&mut ctx.no_outer, true);
    let mut preds = broadcast.iter();
    let first = shift_depths(scalar(ctx, preds.next().expect("broadcast nonempty")), 1);
    let or_pred = preds.fold(first, |acc, pred| SqlExpr::Binary {
        op: SqlBinOp::Or,
        lhs: Box::new(acc),
        rhs: Box::new(shift_depths(scalar(ctx, pred), 1)),
    });
    ctx.no_outer = was_outer;
    SqlExpr::Binary {
        op: SqlBinOp::And,
        lhs: Box::new(cond),
        rhs: Box::new(SqlExpr::Not(Box::new(or_pred))),
    }
}

fn pkey_query(
    ctx: &mut Context,
    source_name: &str,
    temp_idx: Option<usize>,
    outer_col: &str,
    cond: SqlExpr,
) -> SelectBuilder {
    let mut pkey_q = SelectBuilder::new();
    let pkey_inner_ty = if temp_idx.is_some() {
        temp_column_ty(ctx, source_name, outer_col)
    } else {
        table_column_ty(ctx.schema, source_name, outer_col)
    };
    pkey_q.select = vec![SelectItem::Column {
        depth: 0,
        column: outer_col.to_string(),
        output: outer_col.to_string(),
        ty: pkey_inner_ty,
    }];
    let pkey_alias = if temp_idx.is_some() {
        source_name.to_string()
    } else {
        ctx.aliases.table(source_name)
    };
    pkey_q.from = Some(FromClause::Table {
        table: source_name.to_string(),
        alias: pkey_alias,
    });
    pkey_q.where_ = vec![cond];
    pkey_q
}

fn rcross_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
    cardinality: Cardinality,
) -> (ResultShape, Option<String>) {
    let reduced_tail = beta_reduce_tail(tail);
    let tail: &TypedExpr = &reduced_tail;
    if let Some((shape, key)) = const_proj_cross_shape(ctx, parent, tail, cardinality) {
        return (shape, key);
    }
    let bare = match &parent.builder.from {
        Some(FromClause::Table { .. }) => {
            parent.builder.where_.is_empty()
                && parent.builder.order_by.is_empty()
                && parent.builder.limit.is_none()
                && parent.builder.offset.is_none()
                && !tail_uses_projected_outer(tail)
                && !tail_uses_nav_outer(ctx, tail)
        }
        _ => false,
    };
    let (local_tail, correlated, broadcast) = split_cross_tail(ctx, tail, !bare);
    if correlated.is_empty() {
        if broadcast.is_empty()
            && let Some(lane) = sortable_cross_tail(ctx, parent, &local_tail)
        {
            return sorted_cross_shape(ctx, parent, lane, cardinality);
        }
        todo!("M4b composite cross-root correlation");
    }
    if correlated.len() > 1
        && correlated.iter().any(|c| {
            c.outer_col != correlated[0].outer_col || c.outer_struct != correlated[0].outer_struct
        })
    {
        todo!("M4b complex cross-root correlation");
    }
    let corr = &correlated[0];
    let mut suffix: Vec<(String, Vec<TypedMethodArg>)> = Vec::new();
    let peeled_tail = peel_cross_window(&local_tail, &mut suffix, false);
    let broadcast_truncate: Option<(usize, Option<usize>)> =
        if broadcast.is_empty() || suffix.is_empty() {
            None
        } else {
            fold_broadcast_take(&local_tail.ty, tail, &suffix)
        };
    if (!suffix.is_empty() && !broadcast.is_empty() && broadcast_truncate.is_none())
        || (suffix.len() > 1 && !stacked_takes_foldable(&local_tail))
    {
        todo!("M4b cross tail shape");
    }
    if !correlated.is_empty()
        && correlated.iter().all(|c| c.nav.is_none())
        && matches!(&local_tail.ty, QueryType::List(_))
        && !tail_uses_projected_outer(tail)
        && !tail_uses_nav_outer(ctx, tail)
        && ctx.nest_depth < 1
    {
        let has_range = correlated.iter().any(|c| {
            matches!(
                c.op,
                SqlBinOp::Lt | SqlBinOp::Gt | SqlBinOp::Le | SqlBinOp::Ge
            )
        });
        let mut peeled_sorts = Vec::new();
        peel_cross_sorts(tail, &mut peeled_sorts);
        let has_outer_sorts = peeled_sorts
            .iter()
            .any(|(_, args)| args.iter().any(|a| uses_outer(&a.expr)));
        let has_ne_composition = correlated.iter().any(|c| c.op == SqlBinOp::Ne)
            && (correlated.len() > 1 || !suffix.is_empty() || !broadcast.is_empty());
        if (has_range || has_outer_sorts || has_ne_composition)
            && (suffix.is_empty() || stacked_takes_foldable(tail))
        {
            let mut carries: Vec<(String, ScalarType)> = Vec::new();
            let carry_col = |col: &str, carries: &mut Vec<(String, ScalarType)>| -> bool {
                if carries.iter().any(|(c, _)| c == col) {
                    return true;
                }
                match output_ty(&parent.builder, col, ctx.schema) {
                    Some(ty) => {
                        carries.push((col.to_string(), ty));
                        true
                    }
                    None => false,
                }
            };
            let mut ok = true;
            for corr in correlated.iter() {
                ok = ok && carry_col(&corr.outer_col, &mut carries);
            }
            for (_, args) in &peeled_sorts {
                for arg in args {
                    let mut fids = Vec::new();
                    any_ident(&arg.expr, &mut |e| {
                        if let TypedExprKind::Ident {
                            binding: IdentBinding::Field(fid),
                            depth,
                            ..
                        } = &e.kind
                            && *depth > 0
                        {
                            fids.push(*fid);
                        }
                        false
                    });
                    for fid in fids {
                        let field =
                            &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
                        match field {
                            Field::Value { columns, .. } => match columns.as_slice() {
                                [col] => {
                                    ok =
                                        ok && carry_col(ctx.schema.column_name(*col), &mut carries);
                                }
                                _ => ok = false,
                            },
                            _ => ok = false,
                        }
                    }
                }
            }
            if tail_uses_optional_sort(tail) {
            } else if ok {
                let trunc: Option<(usize, Option<usize>)> =
                    if broadcast.is_empty() || suffix.is_empty() {
                        None
                    } else {
                        match fold_broadcast_take(&local_tail.ty, tail, &suffix) {
                            Some(t) => Some(t),
                            None => todo!("M4b cross tail shape"),
                        }
                    };
                return predicate_join_shape(
                    ctx,
                    parent,
                    PredLane {
                        peeled: &peeled_tail,
                        conds: &correlated,
                        carries,
                        broadcast: &broadcast,
                        suffix: &suffix,
                        trunc,
                        cardinality,
                    },
                );
            }
        }
    }
    if correlated.iter().any(|c| c.op == SqlBinOp::Ne)
        && (correlated.len() > 1
            || !broadcast.is_empty()
            || !suffix.is_empty()
            || cardinality != Cardinality::Many)
    {
        todo!("M4b complex cross-root correlation");
    }
    let global_take = suffix.len() == 1 && !tail_take_trailing(tail);
    let lowering_tail = if global_take {
        &local_tail
    } else {
        &peeled_tail
    };
    let (source_name, temp_idx): (String, Option<usize>) = if bare {
        let table = ctx.schema.tables[ctx.schema.structs[corr.outer_struct.index()].table.index()]
            .name
            .clone();
        (table, None)
    } else {
        let temp = ctx.aliases.temp();
        let mut create_q = parent.builder.clone();
        if create_q.select.is_empty() {
            create_q.select = vec![SelectItem::AllFrom { depth: 0 }];
        }
        for corr in &correlated {
            if let Some(nav) = &corr.nav {
                create_q.select.push(nav_carry_item(ctx, nav));
            }
        }
        if corr.nav.is_none() && !available_cols(&create_q, ctx.schema).contains(&corr.outer_col) {
            todo!("M4b key threading through subqueries");
        }
        let create_outputs = builder_output_types(&create_q, ctx.schema);
        let idx = ctx.sink(
            create_q,
            StatementShape {
                columns: shape_columns(create_outputs.clone(), |name| name == corr.outer_col),
                rows: RowCount::Many,
            },
            StatementKind::Temp { name: temp.clone() },
        );
        swap_parent_to_temp(parent, &temp, &create_outputs);
        (temp, Some(idx))
    };
    let mut child_builder = SelectBuilder::new();
    child_builder.from = Some(cross_root_scan(ctx, tail));
    let child_level = Level {
        builder: child_builder,
        element: local_tail.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    };
    let anchor = tail_row_struct(ctx, tail);
    let was_child = ctx.child.replace(ChildCtx {
        base: Some(child_level),
        per_row: false,
        partition: None,
        anchor,
    });
    let was_outer = std::mem::replace(&mut ctx.no_outer, true);
    let mut child = rows(ctx, lowering_tail);
    ctx.child = was_child;
    ctx.no_outer = was_outer;
    if child.pending.is_some() {
        todo!("M4b nested collection inside cross child");
    }
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let pkey_where = if correlated.len() > 1 {
        let mut conds = correlated.iter();
        let first = pkey_arm_cond(ctx, conds.next().expect("correlated nonempty"));
        conds.fold(first, |acc, extra| SqlExpr::Binary {
            op: SqlBinOp::Or,
            lhs: Box::new(acc),
            rhs: Box::new(pkey_arm_cond(ctx, extra)),
        })
    } else {
        pkey_arm_cond(ctx, corr)
    };
    let pkey_where = if broadcast.is_empty() {
        pkey_where
    } else {
        exclude_broadcast(ctx, pkey_where, &broadcast)
    };
    let scan_template = if broadcast.is_empty() {
        None
    } else {
        Some(child.builder.clone())
    };
    let pkey_q = pkey_query(ctx, &source_name, temp_idx, &corr.outer_col, pkey_where);
    child.builder.select.push(SelectItem::Expr {
        expr: SqlExpr::Subquery {
            query: Box::new(pkey_q),
        },
        output: "$$pkey".to_string(),
        ty: scalar_of(&corr.child.ty),
    });
    if !broadcast.is_empty() {
        child.builder.select.push(SelectItem::Expr {
            expr: SqlExpr::Param(ParamValue::Int(0)),
            output: CROSS_PAIR_TAG.to_string(),
            ty: Some(ScalarType::Int),
        });
        let mut arm = scan_template.expect("scan template for broadcast arm");
        let was_outer = std::mem::replace(&mut ctx.no_outer, true);
        let mut preds = broadcast.iter();
        let first = scalar(ctx, preds.next().expect("broadcast nonempty"));
        let or_pred = preds.fold(first, |acc, pred| SqlExpr::Binary {
            op: SqlBinOp::Or,
            lhs: Box::new(acc),
            rhs: Box::new(scalar(ctx, pred)),
        });
        ctx.no_outer = was_outer;
        arm.where_.push(or_pred);
        arm.select.push(SelectItem::Expr {
            expr: SqlExpr::Param(ParamValue::Null),
            output: "$$pkey".to_string(),
            ty: None,
        });
        arm.select.push(SelectItem::Expr {
            expr: SqlExpr::Param(ParamValue::Int(1)),
            output: CROSS_PAIR_TAG.to_string(),
            ty: Some(ScalarType::Int),
        });
        child.builder.unions.push(arm);
    }
    if !global_take && broadcast_truncate.is_none() {
        for (method, args) in suffix.iter().rev() {
            if has_window_bound(&child.builder) {
                let bound = lower_take_bound(ctx, method, args);
                conjoin_window_bound(&mut child.builder, method, bound);
                continue;
            }
            let order = std::mem::take(&mut child.builder.order_by);
            let mut wrapped = wrap_level(ctx, child);
            wrapped.builder.order_by = order;
            child = window_take(ctx, wrapped, "$$pkey", method, args);
        }
    }
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let outputs = builder_outputs(&child.builder, ctx.schema);
    let is_pairing = |name: &str| name == "$$pkey" || name == CROSS_PAIR_TAG;
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&child.builder, ctx.schema), is_pairing);
    let item_ty = match &local_tail.ty {
        QueryType::List(sub) => sub.as_ref(),
        _ => &local_tail.ty,
    };
    let element = child_element(
        ctx.schema,
        child.output_shapes.as_ref(),
        item_ty,
        &outputs,
        is_pairing,
    );
    let idx = ctx.sink(
        child.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    let child_ref = ChildRef {
        stmt: idx,
        parent: Some(ParentRef {
            stmt: temp_idx.unwrap_or(0),
            parent_key: corr.outer_col.clone(),
            child_key: "$$pkey".to_string(),
            negated: corr.op == SqlBinOp::Ne,
        }),
        cardinality: match broadcast_truncate {
            Some((offset, limit)) => Cardinality::Take { offset, limit },
            None => cardinality,
        },
        tag: None,
    };
    let shape = match cardinality {
        Cardinality::Many | Cardinality::Take { .. } => ResultShape::List {
            element: Box::new(element),
            child: child_ref,
        },
        Cardinality::One => ResultShape::Single {
            element: Box::new(element),
            child: child_ref,
        },
    };
    (shape, Some(corr.outer_col.clone()))
}

fn per_row_take_op(tail: &TypedExpr) -> Option<(Vec<(String, TypedExpr)>, TypedExpr)> {
    let mut suffix = Vec::new();
    let peeled = peel_cross_window(tail, &mut suffix, true);
    if suffix.is_empty() {
        return None;
    }
    let mut ops = Vec::new();
    for (method, args) in &suffix {
        let [arg] = args.as_slice() else {
            return None;
        };
        if !matches!(&arg.expr.ty, QueryType::Scalar(ScalarType::Int)) {
            return None;
        }
        if uses_outer(&arg.expr) || !bound_lang_ok(&arg.expr) {
            return None;
        }
        ops.push((method.clone(), arg.expr.clone()));
    }
    if !ops.iter().any(|(_, bound)| has_scope_ident(bound)) {
        return None;
    }
    ops.reverse();
    Some((ops, peeled))
}

fn bound_lang_ok(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::Literal(_) | TypedExprKind::TypeConstant { .. } => true,
        TypedExprKind::Ident { depth, .. } => *depth == 0,
        TypedExprKind::Field { base, .. } => bound_lang_ok(base),
        TypedExprKind::Binary { lhs, rhs, .. } => bound_lang_ok(lhs) && bound_lang_ok(rhs),
        TypedExprKind::Unary { expr, .. } | TypedExprKind::Cast { expr, .. } => bound_lang_ok(expr),
        TypedExprKind::If { arms, default } => {
            arms.iter()
                .all(|(c, t)| bound_lang_ok(c) && bound_lang_ok(t))
                && bound_lang_ok(default)
        }
        TypedExprKind::Method { .. }
        | TypedExprKind::Some { .. }
        | TypedExprKind::Tuple { .. }
        | TypedExprKind::Array { .. }
        | TypedExprKind::Struct { .. }
        | TypedExprKind::Projection { .. } => false,
    }
}

fn expand_join_stars(builder: &mut SelectBuilder, schema: &ValidatedSchema) {
    let from = builder.from.clone();
    let mut expanded = Vec::new();
    for item in std::mem::take(&mut builder.select) {
        match item {
            SelectItem::AllFrom { depth } => {
                let side = match &from {
                    Some(FromClause::Join { left, right }) => {
                        if depth == 0 {
                            right
                        } else if depth == 1 {
                            left
                        } else {
                            unreachable!("star depth under a join: lowering emits depth-0 stars")
                        }
                    }
                    _ => unreachable!("star expansion outside a join: lanes select explicitly"),
                };
                for (name, ty) in from_columns_typed(&Some((**side).clone()), schema) {
                    expanded.push(SelectItem::Column {
                        depth,
                        column: name.clone(),
                        output: name,
                        ty,
                    });
                }
            }
            other => expanded.push(other),
        }
    }
    builder.select = expanded;
}

fn cross_cartesian(
    ctx: &mut Context,
    parent: &mut Level,
    peeled: &TypedExpr,
    extra: &[(String, ScalarType)],
    extra_computed: &[(String, SqlExpr, ScalarType)],
    outer_scope: bool,
) -> (usize, Vec<(String, Option<ScalarType>)>, Level) {
    let temp = ctx.aliases.temp();
    let mut create_q = parent.builder.clone();
    if create_q.select.is_empty() {
        create_q.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    for (name, ty) in extra {
        if !available_cols(&create_q, ctx.schema).contains(name) {
            create_q.select.push(SelectItem::Column {
                depth: 0,
                column: name.clone(),
                output: name.clone(),
                ty: Some(*ty),
            });
        }
    }
    for (name, expr, ty) in extra_computed {
        create_q.select.push(SelectItem::Expr {
            expr: expr.clone(),
            output: name.clone(),
            ty: Some(*ty),
        });
    }
    create_q.select.push(SelectItem::Expr {
        expr: SqlExpr::Window {
            func: "ROW_NUMBER".to_string(),
            partition: Vec::new(),
            order: Vec::new(),
        },
        output: "$$key".to_string(),
        ty: Some(ScalarType::Int),
    });
    let create_outputs = builder_output_types(&create_q, ctx.schema);
    let temp_idx = ctx.sink(
        create_q,
        StatementShape {
            columns: shape_columns(create_outputs.clone(), |name| name == "$$key"),
            rows: RowCount::Many,
        },
        StatementKind::Temp { name: temp.clone() },
    );
    swap_parent_to_temp(parent, &temp, &create_outputs);
    let data_from = cross_root_scan(ctx, peeled);
    let mut child_builder = SelectBuilder::new();
    child_builder.from = Some(FromClause::Join {
        left: Box::new(FromClause::Temp {
            name: temp.clone(),
            columns: create_outputs
                .iter()
                .map(|(name, ty)| NamedColumn {
                    name: name.clone(),
                    ty: *ty,
                })
                .collect(),
        }),
        right: Box::new(data_from),
    });
    let child_level = Level {
        builder: child_builder,
        element: peeled.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    };
    let anchor = tail_row_struct(ctx, peeled);
    let was_child = ctx.child.replace(ChildCtx {
        base: Some(child_level),
        per_row: false,
        partition: None,
        anchor,
    });
    let was_outer = std::mem::replace(&mut ctx.no_outer, !outer_scope);
    let mut child = rows(ctx, peeled);
    ctx.child = was_child;
    ctx.no_outer = was_outer;
    if child.pending.is_some() {
        todo!("M4b nested collection inside cross child");
    }
    expand_join_stars(&mut child.builder, ctx.schema);
    if child.builder.select.is_empty() {
        if let Some(FromClause::Join { right, .. }) = &child.builder.from {
            for (name, ty) in from_columns_typed(&Some((**right).clone()), ctx.schema) {
                child.builder.select.push(SelectItem::Column {
                    depth: 0,
                    column: name.clone(),
                    output: name,
                    ty,
                });
            }
        }
    }
    if child.builder.select.is_empty() {
        todo!("M4b cross tail shape");
    }
    child.builder.select.push(SelectItem::Expr {
        expr: SqlExpr::Column {
            depth: 1,
            column: "$$key".to_string(),
        },
        output: "$$pkey".to_string(),
        ty: None,
    });
    (temp_idx, create_outputs, child)
}

fn sink_cross_child(
    ctx: &mut Context,
    child: Level,
    temp_idx: usize,
    peeled_ty: &QueryType,
    cardinality: Cardinality,
) -> (ResultShape, Option<String>) {
    let outputs = builder_outputs(&child.builder, ctx.schema);
    let is_pairing = |name: &str| name == "$$pkey";
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&child.builder, ctx.schema), is_pairing);
    let item_ty = match peeled_ty {
        QueryType::List(sub) => sub.as_ref(),
        _ => peeled_ty,
    };
    let element = child_element(
        ctx.schema,
        child.output_shapes.as_ref(),
        item_ty,
        &outputs,
        is_pairing,
    );
    let idx = ctx.sink(
        child.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    let child_ref = ChildRef {
        stmt: idx,
        parent: Some(ParentRef {
            stmt: temp_idx,
            parent_key: "$$key".to_string(),
            child_key: "$$pkey".to_string(),
            negated: false,
        }),
        cardinality,
        tag: None,
    };
    let shape = match cardinality {
        Cardinality::Many | Cardinality::Take { .. } => ResultShape::List {
            element: Box::new(element),
            child: child_ref,
        },
        Cardinality::One => ResultShape::Single {
            element: Box::new(element),
            child: child_ref,
        },
    };
    (shape, Some("$$key".to_string()))
}

fn const_proj_cross_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
    cardinality: Cardinality,
) -> Option<(ResultShape, Option<String>)> {
    let TypedExprKind::Projection { base, items } = &tail.kind else {
        return None;
    };
    if !matches!(
        &base.kind,
        TypedExprKind::Ident {
            binding: IdentBinding::Root { .. },
            ..
        }
    ) {
        return None;
    }
    if items.iter().any(|item| !item.value.ty.is_scalar()) {
        return None;
    }
    if items.iter().all(|item| !uses_outer(&item.value)) {
        return None;
    }
    let mut carries: Vec<(String, ScalarType)> = Vec::new();
    for item in items {
        let mut fids = Vec::new();
        let mut foreign = false;
        any_ident(&item.value, &mut |e| {
            if let TypedExprKind::Ident { binding, depth, .. } = &e.kind
                && *depth > 0
            {
                match binding {
                    IdentBinding::Field(fid) if *depth == 1 => fids.push(*fid),
                    _ => foreign = true,
                }
            }
            false
        });
        if foreign {
            return None;
        }
        for fid in fids {
            let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
            let col = match field {
                Field::Value { columns, .. } => match columns.as_slice() {
                    [col] => ctx.schema.column_name(*col).to_string(),
                    _ => return None,
                },
                _ => return None,
            };
            let ty = output_ty(&parent.builder, &col, ctx.schema)?;
            if !carries.iter().any(|(c, _)| c == &col) {
                carries.push((col, ty));
            }
        }
    }
    let (temp_idx, _, child) = cross_cartesian(ctx, parent, tail, &carries, &[], true);
    Some(sink_cross_child(
        ctx,
        child,
        temp_idx,
        &tail.ty,
        cardinality,
    ))
}

fn per_row_take_shape(
    ctx: &mut Context,
    parent: &mut Level,
    ops: &[(String, TypedExpr)],
    peeled: &TypedExpr,
) -> (ResultShape, Option<String>) {
    enum BoundCarry {
        Column(String),
        Computed(SqlExpr),
    }
    let tbound_name = |i: usize| {
        if i == 0 {
            "$$tbound".to_string()
        } else {
            format!("$$tbound{i}")
        }
    };
    let mut carries: Vec<(String, ScalarType)> = Vec::new();
    let mut computed: Vec<(String, SqlExpr, ScalarType)> = Vec::new();
    let mut bounds: Vec<(String, String, ScalarType)> = Vec::new();
    for (i, (_, bound)) in ops.iter().enumerate() {
        let carry = match &bound.kind {
            TypedExprKind::Ident {
                binding: IdentBinding::Field(fid),
                ..
            } => {
                let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
                match field {
                    Field::Value { columns, .. } => match columns.as_slice() {
                        [col] => BoundCarry::Column(ctx.schema.column_name(*col).to_string()),
                        _ => todo!("M4b temp-bound per-row limit"),
                    },
                    _ => todo!("M4b temp-bound per-row limit"),
                }
            }
            TypedExprKind::Ident {
                binding: IdentBinding::ProjectedField { name },
                ..
            } => BoundCarry::Column(name.clone()),
            _ => {
                let was_take = std::mem::replace(&mut ctx.in_take_arg, false);
                let lowered = scalar(ctx, bound);
                ctx.in_take_arg = was_take;
                if free_columns(&lowered, 0)
                    .iter()
                    .any(|c| output_ty(&parent.builder, c, ctx.schema).is_none())
                {
                    todo!("M4b temp-bound per-row limit");
                }
                BoundCarry::Computed(lowered)
            }
        };
        match carry {
            BoundCarry::Column(name) => {
                let ty = output_ty(&parent.builder, &name, ctx.schema).unwrap_or_else(|| {
                    todo!("M4b temp-bound per-row limit");
                });
                bounds.push((name.clone(), tbound_name(i), ty));
                if !carries.iter().any(|(c, _)| c == &name) {
                    carries.push((name, ty));
                }
            }
            BoundCarry::Computed(lowered) => {
                bounds.push((tbound_name(i), tbound_name(i), ScalarType::Int));
                computed.push((tbound_name(i), lowered, ScalarType::Int));
            }
        }
    }
    let (temp_idx, _, mut child) = cross_cartesian(ctx, parent, peeled, &carries, &computed, false);
    for (carried, tbound, ty) in &bounds {
        child.builder.select.push(SelectItem::Expr {
            expr: SqlExpr::Column {
                depth: 1,
                column: carried.clone(),
            },
            output: tbound.clone(),
            ty: Some(*ty),
        });
    }
    let order = std::mem::take(&mut child.builder.order_by);
    let mut wrapped = wrap_level(ctx, child);
    wrapped.builder.order_by = order;
    let mut child = wrapped;
    for (j, ((method, _), (_, tbound, _))) in ops.iter().zip(bounds.iter()).enumerate() {
        let bound = SqlExpr::Column {
            depth: 0,
            column: tbound.clone(),
        };
        if j == 0 {
            child = window_take_bound(ctx, child, "$$pkey", method, bound);
        } else if has_window_bound(&child.builder) {
            conjoin_window_bound(&mut child.builder, method, bound);
        } else {
            unreachable!(
                "stacked per-row take without a window: the first op windows unconditionally"
            );
        }
    }
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    sink_cross_child(ctx, child, temp_idx, &peeled.ty, Cardinality::Many)
}

struct PredLane<'a> {
    peeled: &'a TypedExpr,
    conds: &'a [CorrCond],
    carries: Vec<(String, ScalarType)>,
    broadcast: &'a [TypedExpr],
    suffix: &'a [(String, Vec<TypedMethodArg>)],
    trunc: Option<(usize, Option<usize>)>,
    cardinality: Cardinality,
}

fn predicate_join_shape(
    ctx: &mut Context,
    parent: &mut Level,
    lane: PredLane<'_>,
) -> (ResultShape, Option<String>) {
    let PredLane {
        peeled,
        conds,
        carries,
        broadcast,
        suffix,
        trunc,
        cardinality,
    } = lane;
    let (temp_idx, _, mut child) = cross_cartesian(ctx, parent, peeled, &carries, &[], true);
    let mut pred: Option<SqlExpr> = None;
    for corr in conds {
        let was_outer = std::mem::replace(&mut ctx.no_outer, true);
        let child_frag = scalar(ctx, &corr.child);
        ctx.no_outer = was_outer;
        let arm = SqlExpr::Binary {
            op: corr.op,
            lhs: Box::new(SqlExpr::Column {
                depth: 1,
                column: corr.outer_col.clone(),
            }),
            rhs: Box::new(child_frag),
        };
        pred = Some(match pred {
            Some(acc) => SqlExpr::Binary {
                op: SqlBinOp::And,
                lhs: Box::new(acc),
                rhs: Box::new(arm),
            },
            None => arm,
        });
    }
    if !broadcast.is_empty() {
        let mut locals = broadcast.iter();
        let first = scalar(ctx, locals.next().expect("broadcast nonempty"));
        let or_pred = locals.fold(first, |acc, local| SqlExpr::Binary {
            op: SqlBinOp::Or,
            lhs: Box::new(acc),
            rhs: Box::new(scalar(ctx, local)),
        });
        pred = Some(SqlExpr::Binary {
            op: SqlBinOp::And,
            lhs: Box::new(pred.expect("conjunction over no conditions")),
            rhs: Box::new(SqlExpr::Not(Box::new(or_pred))),
        });
    }
    let scan_template = if broadcast.is_empty() {
        None
    } else {
        let mut template = child.builder.clone();
        template.select.retain(|item| match item {
            SelectItem::Column { output, .. } | SelectItem::Expr { output, .. } => {
                output != "$$pkey"
            }
            SelectItem::AllFrom { .. } => true,
        });
        Some(template)
    };
    if let Some(pred) = pred {
        child.builder.where_.push(pred);
    }
    if !broadcast.is_empty() {
        child.builder.select.push(SelectItem::Expr {
            expr: SqlExpr::Param(ParamValue::Int(0)),
            output: CROSS_PAIR_TAG.to_string(),
            ty: Some(ScalarType::Int),
        });
        let mut arm = scan_template.expect("scan template for broadcast arm");
        let was_outer = std::mem::replace(&mut ctx.no_outer, true);
        let mut preds = broadcast.iter();
        let first = scalar(ctx, preds.next().expect("broadcast nonempty"));
        let or_pred = preds.fold(first, |acc, pred| SqlExpr::Binary {
            op: SqlBinOp::Or,
            lhs: Box::new(acc),
            rhs: Box::new(scalar(ctx, pred)),
        });
        ctx.no_outer = was_outer;
        arm.where_.push(or_pred);
        arm.select.push(SelectItem::Expr {
            expr: SqlExpr::Param(ParamValue::Null),
            output: "$$pkey".to_string(),
            ty: None,
        });
        arm.select.push(SelectItem::Expr {
            expr: SqlExpr::Param(ParamValue::Int(1)),
            output: CROSS_PAIR_TAG.to_string(),
            ty: Some(ScalarType::Int),
        });
        child.builder.unions.push(arm);
    }
    if trunc.is_none() {
        for (method, args) in suffix.iter().rev() {
            if has_window_bound(&child.builder) {
                let bound = lower_take_bound(ctx, method, args);
                conjoin_window_bound(&mut child.builder, method, bound);
                continue;
            }
            let order = std::mem::take(&mut child.builder.order_by);
            let mut wrapped = wrap_level(ctx, child);
            wrapped.builder.order_by = order;
            child = window_take(ctx, wrapped, "$$pkey", method, args);
        }
    }
    let cardinality = match trunc {
        Some((offset, limit)) => Cardinality::Take { offset, limit },
        None => cardinality,
    };
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    if !child.builder.order_by.is_empty() {
        let select = child.builder.select.clone();
        child.builder.order_by = std::mem::take(&mut child.builder.order_by)
            .into_iter()
            .map(|item| OrderItem {
                expr: inline_order_aliases(&select, item.expr),
                dir: item.dir,
            })
            .collect();
    }
    sink_cross_child(ctx, child, temp_idx, &peeled.ty, cardinality)
}

struct SortLane {
    peeled: TypedExpr,
    sorts: Vec<(String, Vec<TypedMethodArg>)>,
    carries: Vec<(String, ScalarType)>,
}

fn sortable_cross_tail(ctx: &Context, parent: &Level, tail: &TypedExpr) -> Option<SortLane> {
    if tail_has_limit_op(tail) {
        return None;
    }
    let mut sorts = Vec::new();
    let peeled = peel_cross_sorts(tail, &mut sorts);
    if sorts.is_empty() {
        return None;
    }
    if sorts_left_with_outer(&peeled) {
        return None;
    }
    for (_, args) in &sorts {
        for arg in args {
            if !sort_key_divertible(&arg.expr) {
                return None;
            }
        }
    }
    let mut carries: Vec<(String, ScalarType)> = Vec::new();
    for (_, args) in &sorts {
        for arg in args {
            let mut fids = Vec::new();
            any_ident(&arg.expr, &mut |e| {
                if let TypedExprKind::Ident {
                    binding: IdentBinding::Field(fid),
                    depth,
                    ..
                } = &e.kind
                    && *depth > 0
                {
                    fids.push(*fid);
                }
                false
            });
            for fid in fids {
                let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
                let col = match field {
                    Field::Value { columns, .. } => match columns.as_slice() {
                        [col] => ctx.schema.column_name(*col).to_string(),
                        _ => return None,
                    },
                    _ => return None,
                };
                let ty = output_ty(&parent.builder, &col, ctx.schema)?;
                if !carries.iter().any(|(c, _)| c == &col) {
                    carries.push((col, ty));
                }
            }
        }
    }
    Some(SortLane {
        peeled,
        sorts,
        carries,
    })
}

fn tail_uses_optional_sort(node: &TypedExpr) -> bool {
    match &node.kind {
        TypedExprKind::Method {
            base,
            name,
            optional,
            ..
        } => {
            (["sort", "sort_asc", "sort_desc"].contains(&name.value.as_str()) && *optional)
                || tail_uses_optional_sort(base)
        }
        TypedExprKind::Projection { base, .. } => tail_uses_optional_sort(base),
        _ => false,
    }
}

fn sorts_left_with_outer(node: &TypedExpr) -> bool {
    match &node.kind {
        TypedExprKind::Method {
            base, name, args, ..
        } if ["sort", "sort_asc", "sort_desc"].contains(&name.value.as_str()) => {
            args.iter().any(|a| uses_outer(&a.expr)) || sorts_left_with_outer(base)
        }
        TypedExprKind::Method { base, .. } | TypedExprKind::Projection { base, .. } => {
            sorts_left_with_outer(base)
        }
        _ => false,
    }
}

fn sorted_cross_shape(
    ctx: &mut Context,
    parent: &mut Level,
    lane: SortLane,
    cardinality: Cardinality,
) -> (ResultShape, Option<String>) {
    let (temp_idx, _, mut child) =
        cross_cartesian(ctx, parent, &lane.peeled, &lane.carries, &[], false);
    let was_outer = std::mem::replace(&mut ctx.no_outer, false);
    let mut order = Vec::new();
    for (method, args) in &lane.sorts {
        let default = if method == "sort_desc" {
            OrderDir::Desc
        } else {
            OrderDir::Asc
        };
        for arg in args {
            let dir = match &arg.direction {
                Some(dir) => match dir.value {
                    SortDir::Asc => OrderDir::Asc,
                    SortDir::Desc => OrderDir::Desc,
                },
                None => default,
            };
            order.extend(sort_keys(ctx, &child, &arg.expr, dir));
        }
    }
    ctx.no_outer = was_outer;
    let order = order
        .into_iter()
        .map(|item| OrderItem {
            expr: inline_order_aliases(&child.builder.select, item.expr),
            dir: item.dir,
        })
        .collect();
    child.builder.order_by = order;
    sink_cross_child(ctx, child, temp_idx, &lane.peeled.ty, cardinality)
}

fn inline_order_aliases(items: &[SelectItem], expr: SqlExpr) -> SqlExpr {
    fn inline(items: &[SelectItem], expr: SqlExpr, seen: &mut Vec<String>) -> SqlExpr {
        match expr {
            SqlExpr::Column { depth, column } => {
                if depth == 0
                    && !seen.iter().any(|s| s == &column)
                    && let Some(def) = items.iter().find_map(|item| match item {
                        SelectItem::Column {
                            output,
                            column: def,
                            ..
                        } if output == &column && def != &column => Some(SqlExpr::Column {
                            depth,
                            column: def.clone(),
                        }),
                        SelectItem::Expr { output, expr, .. } if output == &column => {
                            Some(expr.clone())
                        }
                        _ => None,
                    })
                {
                    seen.push(column);
                    return inline(items, def, seen);
                }
                SqlExpr::Column { depth, column }
            }
            SqlExpr::Binary { op, lhs, rhs } => SqlExpr::Binary {
                op,
                lhs: Box::new(inline(items, *lhs, seen)),
                rhs: Box::new(inline(items, *rhs, seen)),
            },
            SqlExpr::Not(x) => SqlExpr::Not(Box::new(inline(items, *x, seen))),
            SqlExpr::Func { name, args } => SqlExpr::Func {
                name,
                args: args.into_iter().map(|a| inline(items, a, seen)).collect(),
            },
            SqlExpr::Case { arms, else_ } => SqlExpr::Case {
                arms: arms
                    .into_iter()
                    .map(|(c, t)| (inline(items, c, seen), inline(items, t, seen)))
                    .collect(),
                else_: Box::new(inline(items, *else_, seen)),
            },
            SqlExpr::In { expr, items: set } => SqlExpr::In {
                expr: Box::new(inline(items, *expr, seen)),
                items: set.into_iter().map(|i| inline(items, i, seen)).collect(),
            },
            SqlExpr::Coalesce(args) => {
                SqlExpr::Coalesce(args.into_iter().map(|a| inline(items, a, seen)).collect())
            }
            SqlExpr::Cast { expr, target } => SqlExpr::Cast {
                expr: Box::new(inline(items, *expr, seen)),
                target,
            },
            SqlExpr::Window {
                func,
                partition,
                order,
            } => SqlExpr::Window {
                func,
                partition: partition
                    .into_iter()
                    .map(|p| inline(items, p, seen))
                    .collect(),
                order: order
                    .into_iter()
                    .map(|OrderItem { expr: e, dir: d }| OrderItem {
                        expr: inline(items, e, seen),
                        dir: d,
                    })
                    .collect(),
            },
            other => other,
        }
    }
    inline(items, expr, &mut Vec::new())
}

fn cross_root_scan(ctx: &mut Context, tail: &TypedExpr) -> FromClause {
    match tail_root_ident(tail) {
        Some(IdentBinding::Root { root_idx }) => {
            let root = &ctx.schema.roots[*root_idx];
            let table = ctx.schema.tables[ctx.schema.structs[root.struct_id.index()].table.index()]
                .name
                .clone();
            FromClause::Table {
                table: table.clone(),
                alias: ctx.aliases.table(&table),
            }
        }
        _ => unreachable!(
            "exotic cross tail root: cross tails bottom at Field/Root idents (single-spine tails admit one root); remaining kinds infer non-List types, contradicting the List-only callers"
        ),
    }
}

fn collapse_tuple_method(
    ctx: &mut Context,
    prefix: String,
    base: &TypedExpr,
    name: &grove_types::Spanned<String>,
    args: &[TypedMethodArg],
    tys: &[QueryType],
) -> (Vec<SelectItem>, ResultShape, Vec<String>) {
    let mut level = rows(ctx, base);
    if level.pending.is_some() {
        unreachable!(
            "tuple collapse over split-pending rows: tuple-typed collapses imply flat List(Tuple) bases, whose rows never park pendings (crossings that park are nested)"
        )
    }
    compose_limit(&mut level.builder, &name.value, args, ctx);
    if level.builder.select.is_empty() {
        level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let outputs = builder_outputs(&level.builder, ctx.schema);
    if outputs.len() != tys.len() {
        unreachable!("tuple-row width drift: tuple levels emit positions in order");
    }
    let mut select = Vec::with_capacity(outputs.len());
    let mut shapes = Vec::with_capacity(outputs.len());
    for (i, column) in outputs.iter().enumerate() {
        let output = if prefix.is_empty() {
            format!("c{i}")
        } else {
            format!("{prefix}_c{i}")
        };
        let position_ty = tys.get(i).and_then(scalar_of);
        select.push(SelectItem::Expr {
            expr: SqlExpr::Subquery {
                query: Box::new(SelectBuilder {
                    select: vec![SelectItem::Column {
                        depth: 0,
                        column: column.clone(),
                        output: column.clone(),
                        ty: position_ty,
                    }],
                    from: level.builder.from.clone(),
                    where_: level.builder.where_.clone(),
                    order_by: level.builder.order_by.clone(),
                    limit: level.builder.limit.clone(),
                    offset: level.builder.offset.clone(),
                    unions: Vec::new(),
                }),
            },
            output: output.clone(),
            ty: position_ty,
        });
        shapes.push(ResultShape::Column(output));
    }
    (select, ResultShape::Tuple(shapes), Vec::new())
}

fn if_broadcast_tail(tail: &TypedExpr) -> bool {
    matches!(tail.kind, TypedExprKind::If { .. })
        && !tail_uses_outer(tail)
        && !has_scope_ident(tail)
}

fn sink_broadcast_tail(ctx: &mut Context, tail: &TypedExpr) -> (ResultShape, Option<String>) {
    let mut root = rows(ctx, tail);
    let hops = root.pending.take().map(drain_pending).unwrap_or_default();
    if root.builder.select.is_empty() {
        root.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let outputs = builder_outputs(&root.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&root.builder, ctx.schema), |_| false);
    let root_creation = ctx.statements.len() + hops.len() + 1;
    let root_parts = root.root_parts.clone();
    let inner = if hops.is_empty() {
        let item_ty = match peel_optional(&tail.ty) {
            QueryType::List(sub) => sub.as_ref(),
            _ => &tail.ty,
        };
        child_element(
            ctx.schema,
            root.output_shapes.as_ref(),
            item_ty,
            &outputs,
            |_| false,
        )
    } else {
        let layers = sink_chain(ctx, &hops, root_creation);
        nest_layers(
            ctx.schema,
            layers,
            &tail.ty,
            false,
            root_parts.clone(),
            root_creation,
        )
    };
    let actual = ctx.sink(
        root.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    debug_assert_eq!(
        actual, root_creation,
        "broadcast root sank at an unexpected index: sink order changed"
    );
    let complete = root_parts.is_some() && !hops.is_empty();
    let shape = if complete {
        inner
    } else {
        unkeyed_attach(inner, root_creation)
    };
    (shape, None)
}

fn distribute_if_tuple(
    arms: &[(TypedExpr, TypedExpr)],
    default: &TypedExpr,
    tys: &[QueryType],
    span: grove_types::Span,
    ty: QueryType,
) -> Option<TypedExpr> {
    fn branch_positions(
        node: &TypedExpr,
        arity: usize,
        span: grove_types::Span,
        tys: &[QueryType],
    ) -> Option<Vec<TypedExpr>> {
        match &node.kind {
            TypedExprKind::Tuple { elements } if elements.len() == arity => Some(elements.clone()),
            TypedExprKind::If {
                arms: inner_arms,
                default: inner_default,
            } => distribute_if_tuple(inner_arms, inner_default, tys, span, node.ty.clone())
                .and_then(|synthetic| match synthetic.kind {
                    TypedExprKind::Tuple { elements } => Some(elements),
                    _ => None,
                }),
            _ => None,
        }
    }
    let mut branch_elements: Vec<Vec<TypedExpr>> = Vec::with_capacity(arms.len() + 1);
    for (_, body) in arms {
        branch_elements.push(branch_positions(body, tys.len(), span, tys)?);
    }
    branch_elements.push(branch_positions(default, tys.len(), span, tys)?);
    let mut elements = Vec::with_capacity(tys.len());
    for (i, ty) in tys.iter().enumerate() {
        let position_arms: Vec<(TypedExpr, TypedExpr)> = arms
            .iter()
            .zip(branch_elements.iter())
            .map(|((cond, _), elements)| (cond.clone(), elements[i].clone()))
            .collect();
        let position_default = branch_elements[arms.len()][i].clone();
        elements.push(TypedExpr {
            kind: TypedExprKind::If {
                arms: position_arms,
                default: Box::new(position_default),
            },
            ty: ty.clone(),
            span,
        });
    }
    Some(TypedExpr {
        kind: TypedExprKind::Tuple { elements },
        ty,
        span,
    })
}
fn m2m_seed_selects(schema: &ValidatedSchema, builder: &mut SelectBuilder, child_col: &str) {
    let target_from = match builder.from.as_ref() {
        Some(FromClause::Join { right, .. }) => Some((**right).clone()),
        _ => unreachable!("M2M child without a junction join"),
    };
    for (name, ty) in from_columns_typed(&target_from, schema) {
        if name == child_col {
            todo!("M4 many-to-many navigation");
        }
        builder.select.push(SelectItem::Column {
            depth: 0,
            column: name.clone(),
            output: name,
            ty,
        });
    }
    builder.select.push(SelectItem::Column {
        depth: 1,
        column: child_col.to_string(),
        output: child_col.to_string(),
        ty: None,
    });
}

fn rvalue_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
) -> (ResultShape, Option<String>) {
    let reduced_tail = beta_reduce_tail(tail);
    let tail: &TypedExpr = &reduced_tail;
    let keyed = matches!(tail_root_ident(tail), Some(IdentBinding::Field(_)));
    if ctx.nest_depth >= 1 && !nested_bulk_lane(parent, tail) {
        todo!("M4b nested list item");
    }
    if ctx.nest_depth < 1
        && keyed
        && (!parent.stmt_bound || tail_uses_outer(tail) || take_bound_ident(tail))
    {
        return rtemp_shape(ctx, parent, tail, Cardinality::Many);
    }
    if ctx.nest_depth < 1 && !keyed && tail_uses_outer(tail) {
        return rcross_shape(ctx, parent, tail, Cardinality::Many);
    }
    if array_tail_root(tail) {
        if tail_uses_outer(tail) {
            todo!("M6 correlated literal projection item");
        }
        if has_scope_ident(tail) {
            if matches!(tail.kind, TypedExprKind::Array { .. }) {
                return rtemp_array_shape(ctx, parent, tail);
            }
            if let Some((_, methods)) = peel_row_ops(tail)
                && !methods.is_empty()
            {
                return rtemp_method_shape(ctx, parent, tail);
            }
            if let TypedExprKind::Method { base, name, .. } = &tail.kind
                && matches!(base.kind, TypedExprKind::Array { .. })
                && name.value == "flatten"
            {
                return rtemp_method_shape(ctx, parent, tail);
            }
            todo!("M6 correlated literal projection item");
        }
        return sink_broadcast_tail(ctx, tail);
    }
    if if_broadcast_tail(tail) {
        return sink_broadcast_tail(ctx, tail);
    }
    if matches!(tail.kind, TypedExprKind::If { .. }) {
        todo!("M7 correlated If item");
    }
    if matches!(tail_root_ident(tail), Some(IdentBinding::Root { .. }))
        && ctx.temp_collect.is_none()
        && let Some((ops, peeled)) = per_row_take_op(tail)
    {
        return per_row_take_shape(ctx, parent, &ops, &peeled);
    }
    let (child_from, child_col, parent_key, m2m_join) = match tail_root_ident(tail) {
        Some(IdentBinding::Field(fid)) => {
            let link = resolve_tail_link(ctx, *fid);
            let _ = parent;
            (
                link.child_from,
                link.child_col,
                Some(link.parent_col),
                link.m2m_join,
            )
        }
        Some(IdentBinding::Root { root_idx }) => {
            let root = &ctx.schema.roots[*root_idx];
            let table = ctx.schema.tables[ctx.schema.structs[root.struct_id.index()].table.index()]
                .name
                .clone();
            (
                FromClause::Table {
                    table: table.clone(),
                    alias: ctx.aliases.table(&table),
                },
                String::new(),
                None,
                None,
            )
        }
        _ => todo!("M4b projected root inside child tail"),
    };
    let keyed = parent_key.is_some();
    let mut child_builder = SelectBuilder::new();
    child_builder.from = Some(child_from);
    if keyed {
        if let Some((target_pk, junction_target_col)) = &m2m_join {
            child_builder.where_ = vec![
                SqlExpr::InSubquery {
                    expr: Box::new(SqlExpr::Column {
                        depth: 1,
                        column: child_col.clone(),
                    }),
                    query: Box::new(bulk_query()),
                },
                SqlExpr::Binary {
                    op: SqlBinOp::Eq,
                    lhs: Box::new(SqlExpr::Column {
                        depth: 0,
                        column: target_pk.clone(),
                    }),
                    rhs: Box::new(SqlExpr::Column {
                        depth: 1,
                        column: junction_target_col.clone(),
                    }),
                },
            ];
        } else {
            child_builder.where_ = vec![SqlExpr::InSubquery {
                expr: Box::new(SqlExpr::Column {
                    depth: 0,
                    column: child_col.clone(),
                }),
                query: Box::new(bulk_query()),
            }];
        }
    }
    if keyed && m2m_join.is_some() && tail_has_limit_op(tail) {
        m2m_seed_selects(ctx.schema, &mut child_builder, &child_col);
        let alias = ctx.aliases.subquery();
        let mut outer = SelectBuilder::new();
        outer.from = Some(FromClause::Subquery {
            query: Box::new(child_builder),
            alias,
        });
        child_builder = outer;
    }
    let child_level = Level {
        builder: child_builder,
        element: tail.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    };
    let mut child = lower_tail(
        ctx,
        tail,
        child_level,
        keyed,
        keyed.then(|| child_col.clone()),
        true,
    );
    let mut nested: Option<ResultShape> = None;
    let mut nested_child_idx = 0;
    if let Some(pending) = child.pending.take() {
        let (inner, child_idx) = drain_pending_chain(ctx, pending, &tail.ty);
        nested = Some(inner);
        nested_child_idx = child_idx;
    }
    if child.builder.select.is_empty() && m2m_join.is_none() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    if keyed {
        if m2m_join.is_some() {
            if child.builder.select.is_empty() {
                match child.builder.from.as_ref() {
                    Some(FromClause::Join { .. }) => {
                        m2m_seed_selects(ctx.schema, &mut child.builder, &child_col);
                    }
                    _ => {
                        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
                    }
                }
            }
            let served = child.builder.select.iter().any(|item| match item {
                SelectItem::AllFrom { .. } => false,
                SelectItem::Column { output, .. } | SelectItem::Expr { output, .. } => {
                    output == &child_col
                }
            });
            if !served && matches!(child.builder.from, Some(FromClause::Join { .. })) {
                child.builder.select.push(SelectItem::Column {
                    depth: 1,
                    column: child_col.clone(),
                    output: child_col.clone(),
                    ty: None,
                });
            }
        } else {
            ensure_key(&mut child.builder, &child_col, ctx.schema);
        }
    }
    let outputs = builder_outputs(&child.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&child.builder, ctx.schema), |name| {
            keyed && name == child_col
        });
    let item_ty = match &tail.ty {
        QueryType::List(sub) => sub.as_ref(),
        _ => &tail.ty,
    };
    let mut element = match nested {
        Some(inner) => inner,
        None => child_element(
            ctx.schema,
            child.output_shapes.as_ref(),
            item_ty,
            &outputs,
            |name| keyed && name == child_col,
        ),
    };
    let idx = ctx.sink(
        child.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    if nested_child_idx != 0 {
        debug_assert_eq!(
            idx, nested_child_idx,
            "nested child sank at an unexpected index: sink order changed"
        );
    }
    patch_pending_parents(&mut element, idx);
    let shape = ResultShape::List {
        element: Box::new(element),
        child: ChildRef {
            stmt: idx,
            parent: parent_key.clone().map(|parent_key| ParentRef {
                stmt: if ctx.nest_depth >= 1 {
                    PENDING_PARENT_STMT
                } else {
                    0
                },
                parent_key,
                child_key: child_col,
                negated: false,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    (shape, parent_key)
}

enum CollapseKind<'a> {
    First,
    Nth(&'a TypedExpr),
}

enum CollapsePrefix<'a> {
    Sort(Vec<(&'a TypedExpr, OrderDir)>),
    Take(&'a TypedExpr),
    Skip(&'a TypedExpr),
}

struct PeeledCollapse<'a> {
    inner: &'a TypedExpr,
    collapse: CollapseKind<'a>,
    prefix: Option<CollapsePrefix<'a>>,
    keyed: bool,
}

fn peel_windowed_collapse(tail: &TypedExpr, strip: bool) -> Option<PeeledCollapse<'_>> {
    let (base, collapse) = match &tail.kind {
        TypedExprKind::Method { base, name, .. } if name.value == "first" => {
            if strip && (filter_chain_to_field_ident(base) || filter_chain_to_root_ident(base)) {
                return None;
            }
            (base.as_ref(), CollapseKind::First)
        }
        TypedExprKind::Method {
            base, name, args, ..
        } if name.value == "nth" => (base.as_ref(), CollapseKind::Nth(one_arg(args, name))),
        _ => return None,
    };
    let (inner, prefix) = match &base.kind {
        TypedExprKind::Method {
            base: inner,
            name,
            args,
            ..
        } if ["sort", "sort_asc", "sort_desc"].contains(&name.value.as_str()) => {
            if args.iter().any(|a| uses_outer(&a.expr)) {
                return None;
            }
            let default = match name.value.as_str() {
                "sort_desc" => OrderDir::Desc,
                _ => OrderDir::Asc,
            };
            let mut keys = Vec::with_capacity(args.len());
            for arg in args {
                let dir = match &arg.direction {
                    Some(dir) => match dir.value {
                        SortDir::Asc => OrderDir::Asc,
                        SortDir::Desc => OrderDir::Desc,
                    },
                    None => default,
                };
                keys.push((&arg.expr, dir));
            }
            (inner.as_ref(), Some(CollapsePrefix::Sort(keys)))
        }
        TypedExprKind::Method {
            base: inner,
            name,
            args,
            ..
        } if ["take", "skip"].contains(&name.value.as_str()) => {
            let arg = one_arg(args, name);
            if uses_outer(arg) {
                return None;
            }
            let prefix = if name.value == "take" {
                CollapsePrefix::Take(arg)
            } else {
                CollapsePrefix::Skip(arg)
            };
            (inner.as_ref(), Some(prefix))
        }
        _ => (base, None),
    };
    let keyed = if filter_chain_to_field_ident(inner) {
        true
    } else if filter_chain_to_root_ident(inner) {
        false
    } else {
        return None;
    };
    Some(PeeledCollapse {
        inner,
        collapse,
        prefix,
        keyed,
    })
}

fn rows_collapse(ctx: &mut Context, tail: &TypedExpr) -> (ResultShape, Option<String>) {
    let mut level = rows(ctx, tail);
    if level.pending.is_some() {
        todo!("M4b ordered per-parent single");
    }
    if level.builder.select.is_empty() {
        level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let outputs = builder_outputs(&level.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&level.builder, ctx.schema), |_| false);
    let element = child_element(
        ctx.schema,
        level.output_shapes.as_ref(),
        peel_optional(&tail.ty),
        &outputs,
        |_| false,
    );
    let idx = ctx.sink(
        level.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::ZeroOrOne,
        },
        StatementKind::Query,
    );
    let shape = ResultShape::Single {
        element: Box::new(element),
        child: ChildRef {
            stmt: idx,
            parent: None,
            cardinality: Cardinality::One,
            tag: None,
        },
    };
    (shape, None)
}

fn build_capped_level(
    ctx: &mut Context,
    peeled: PeeledCollapse<'_>,
) -> (Level, Option<String>, String) {
    let PeeledCollapse {
        inner: base,
        collapse,
        prefix,
        ..
    } = peeled;
    let (child_from, child_col, parent_key, m2m_join) = match tail_root_ident(base) {
        Some(IdentBinding::Field(fid)) => {
            let link = resolve_tail_link(ctx, *fid);
            (
                link.child_from,
                link.child_col,
                Some(link.parent_col),
                link.m2m_join,
            )
        }
        _ => unreachable!("exotic windowed collapse base: peel admits keyed field tails only"),
    };
    let child_level = Level {
        builder: {
            let mut child_builder = SelectBuilder::new();
            child_builder.from = Some(child_from);
            if let Some((target_pk, junction_target_col)) = &m2m_join {
                child_builder.where_ = vec![
                    SqlExpr::InSubquery {
                        expr: Box::new(SqlExpr::Column {
                            depth: 1,
                            column: child_col.clone(),
                        }),
                        query: Box::new(bulk_query()),
                    },
                    SqlExpr::Binary {
                        op: SqlBinOp::Eq,
                        lhs: Box::new(SqlExpr::Column {
                            depth: 0,
                            column: target_pk.clone(),
                        }),
                        rhs: Box::new(SqlExpr::Column {
                            depth: 1,
                            column: junction_target_col.clone(),
                        }),
                    },
                ];
            } else {
                child_builder.where_ = vec![SqlExpr::InSubquery {
                    expr: Box::new(SqlExpr::Column {
                        depth: 0,
                        column: child_col.clone(),
                    }),
                    query: Box::new(bulk_query()),
                }];
            }
            if m2m_join.is_some() {
                m2m_seed_selects(ctx.schema, &mut child_builder, &child_col);
                let alias = ctx.aliases.subquery();
                let mut outer = SelectBuilder::new();
                outer.from = Some(FromClause::Subquery {
                    query: Box::new(child_builder),
                    alias,
                });
                outer
            } else {
                child_builder
            }
        },
        element: base.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    };
    let mut child = lower_tail(ctx, base, child_level, true, Some(child_col.clone()), false);
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    ensure_key(&mut child.builder, &child_col, ctx.schema);
    let mut order = Vec::new();
    if let Some(CollapsePrefix::Sort(keys)) = &prefix {
        for (expr, dir) in keys {
            order.extend(sort_keys(ctx, &child, expr, *dir));
        }
    }
    let mut child =
        if has_window_bound(&child.builder) && matches!(&prefix, Some(CollapsePrefix::Sort(_))) {
            todo!("M4b stacked per-parent take");
        } else if has_window_bound(&child.builder) {
            child
        } else {
            window_wrap(ctx, child, &child_col, order)
        };
    if let Some(prefix) = &prefix {
        match prefix {
            CollapsePrefix::Sort(_) => {}
            CollapsePrefix::Take(arg) => {
                conjoin_window_bound(&mut child.builder, "take", scalar(ctx, arg));
            }
            CollapsePrefix::Skip(arg) => {
                conjoin_window_bound(&mut child.builder, "skip", scalar(ctx, arg));
            }
        }
    }
    match collapse {
        CollapseKind::First => {
            conjoin_window_bound(
                &mut child.builder,
                "first",
                SqlExpr::Param(ParamValue::Int(1)),
            );
        }
        CollapseKind::Nth(arg) => {
            conjoin_window_bound(&mut child.builder, "nth", collapse_index(ctx, arg));
        }
    }
    (child, parent_key, child_col)
}

fn windowed_collapse(
    ctx: &mut Context,
    parent: &mut Level,
    base: &TypedExpr,
    collapse: CollapseKind<'_>,
    prefix: Option<CollapsePrefix<'_>>,
) -> (ResultShape, Option<String>) {
    let _ = parent;
    let (mut child, parent_key, child_col) = build_capped_level(
        ctx,
        PeeledCollapse {
            inner: base,
            collapse,
            prefix,
            keyed: true,
        },
    );
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    ensure_key(&mut child.builder, &child_col, ctx.schema);
    let outputs = builder_outputs(&child.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&child.builder, ctx.schema), |name| {
            name == child_col
        });
    let item_ty = match &base.ty {
        QueryType::List(sub) => sub.as_ref(),
        _ => &base.ty,
    };
    let mut element = child_element(
        ctx.schema,
        child.output_shapes.as_ref(),
        item_ty,
        &outputs,
        |name| name == child_col,
    );
    let idx = ctx.sink(
        child.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    patch_pending_parents(&mut element, idx);
    let shape = ResultShape::Single {
        element: Box::new(element),
        child: ChildRef {
            stmt: idx,
            parent: parent_key.clone().map(|parent_key| ParentRef {
                stmt: if ctx.nest_depth >= 1 {
                    PENDING_PARENT_STMT
                } else {
                    0
                },
                parent_key,
                child_key: child_col,
                negated: false,
            }),
            cardinality: Cardinality::One,
            tag: None,
        },
    };
    (shape, parent_key)
}

fn collapse_index(ctx: &mut Context, arg: &TypedExpr) -> SqlExpr {
    match scalar(ctx, arg) {
        SqlExpr::Param(ParamValue::Int(n)) => SqlExpr::Param(ParamValue::Int(n.saturating_add(1))),
        guard => SqlExpr::Func {
            name: "$$add".to_string(),
            args: vec![
                non_neg_guard(guard, "nth"),
                SqlExpr::Param(ParamValue::Int(1)),
            ],
        },
    }
}

fn literal_field_col(served: &[String], fields: &[ProjectionField], name: &str) -> String {
    let positional: Vec<String> = (0..fields.len()).map(|i| format!("c{i}")).collect();
    if served == positional {
        fields
            .iter()
            .position(|f| f.name == name)
            .map(|i| format!("c{i}"))
            .unwrap_or_else(|| unreachable!("literal field {name} missing from its own layout"))
    } else {
        name.to_string()
    }
}

fn broadcast_first(ctx: &mut Context, base: &TypedExpr) -> (ResultShape, Option<String>) {
    let mut level = rows(ctx, base);
    if level.pending.is_some() {
        todo!("M6 correlated literal projection item");
    }
    let item_ty = match &base.ty {
        QueryType::List(sub) => sub.as_ref(),
        _ => &base.ty,
    };
    if let QueryType::Record(RecordSource::Projection(fields)) = peel_optional(item_ty) {
        if level.builder.select.is_empty() {
            level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
        }
        let served = builder_outputs(&level.builder, ctx.schema);
        let positional: Vec<String> = (0..fields.len()).map(|i| format!("c{i}")).collect();
        if served == positional {
            level.builder.select = fields
                .iter()
                .map(|field| SelectItem::Column {
                    depth: 0,
                    column: literal_field_col(&served, fields, &field.name),
                    output: field.name.clone(),
                    ty: scalar_of(&field.ty),
                })
                .collect();
        }
    }
    compose_limit(&mut level.builder, "first", &[], ctx);
    if level.builder.select.is_empty() {
        level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let outputs = builder_outputs(&level.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&level.builder, ctx.schema), |_| false);
    let element = child_element(
        ctx.schema,
        level.output_shapes.as_ref(),
        item_ty,
        &outputs,
        |_| false,
    );
    let idx = ctx.sink(
        level.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::ZeroOrOne,
        },
        StatementKind::Query,
    );
    let shape = ResultShape::Single {
        element: Box::new(element),
        child: ChildRef {
            stmt: idx,
            parent: None,
            cardinality: Cardinality::One,
            tag: None,
        },
    };
    (shape, None)
}

fn rscalar_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
) -> (ResultShape, Option<String>) {
    let reduced_tail = beta_reduce_tail(tail);
    let tail: &TypedExpr = &reduced_tail;
    let windowed = if parent.stmt_bound && !tail_uses_outer(tail) && !take_bound_ident(tail) {
        peel_windowed_collapse(tail, true)
    } else {
        None
    };
    let eff = match &tail.kind {
        TypedExprKind::Method { base, name, .. } if name.value == "first" => {
            if filter_chain_to_field_ident(base) || filter_chain_to_root_ident(base) {
                base.as_ref()
            } else if matches!(&base.kind, TypedExprKind::Array { .. }) {
                if has_scope_ident(base) {
                    todo!("M6 correlated literal projection item");
                }
                return broadcast_first(ctx, base);
            } else if let Some(peeled) = windowed {
                if peeled.keyed {
                    let PeeledCollapse {
                        inner,
                        collapse,
                        prefix,
                        ..
                    } = peeled;
                    return windowed_collapse(ctx, parent, inner, collapse, prefix);
                }
                return rows_collapse(ctx, tail);
            } else {
                todo!("M4b ordered per-parent single");
            }
        }
        TypedExprKind::Method { name, .. }
            if ["nth", "take", "skip"].contains(&name.value.as_str()) =>
        {
            if name.value == "nth"
                && let Some(peeled) = windowed
            {
                if peeled.keyed {
                    let PeeledCollapse {
                        inner,
                        collapse,
                        prefix,
                        ..
                    } = peeled;
                    return windowed_collapse(ctx, parent, inner, collapse, prefix);
                }
                return rows_collapse(ctx, tail);
            }
            if name.value != "nth" || !nth_temp_routable(tail) {
                todo!("M4b ordered per-parent single")
            }
            tail
        }
        _ => tail,
    };
    let keyed = matches!(tail_root_ident(eff), Some(IdentBinding::Field(_)));
    if ctx.nest_depth >= 1 && !nested_bulk_lane(parent, eff) {
        todo!("M4b nested record item");
    }
    if ctx.nest_depth < 1
        && keyed
        && (!parent.stmt_bound || tail_uses_outer(eff) || take_bound_ident(eff))
    {
        return rtemp_shape(ctx, parent, eff, Cardinality::One);
    }
    if ctx.nest_depth < 1 && !keyed && tail_uses_outer(eff) {
        return rcross_shape(ctx, parent, eff, Cardinality::One);
    }
    let (child_from, child_col, parent_key, m2m_join) = match tail_root_ident(eff) {
        Some(IdentBinding::Field(fid)) => {
            let link = resolve_tail_link(ctx, *fid);
            let _ = parent;
            (
                link.child_from,
                link.child_col,
                Some(link.parent_col),
                link.m2m_join,
            )
        }
        Some(IdentBinding::Root { root_idx }) => {
            let root = &ctx.schema.roots[*root_idx];
            let table = ctx.schema.tables[ctx.schema.structs[root.struct_id.index()].table.index()]
                .name
                .clone();
            (
                FromClause::Table {
                    table: table.clone(),
                    alias: ctx.aliases.table(&table),
                },
                String::new(),
                None,
                None,
            )
        }
        _ => unreachable!(
            "exotic single tail root: If tails are rejected by callers and Field/Root tails route above; remaining kinds infer non-record types (Some-of-optional is checker-rejected), contradicting the record-only callers"
        ),
    };
    let keyed = parent_key.is_some();
    let mut child_builder = SelectBuilder::new();
    child_builder.from = Some(child_from);
    if keyed {
        if let Some((target_pk, junction_target_col)) = &m2m_join {
            child_builder.where_ = vec![
                SqlExpr::InSubquery {
                    expr: Box::new(SqlExpr::Column {
                        depth: 1,
                        column: child_col.clone(),
                    }),
                    query: Box::new(bulk_query()),
                },
                SqlExpr::Binary {
                    op: SqlBinOp::Eq,
                    lhs: Box::new(SqlExpr::Column {
                        depth: 0,
                        column: target_pk.clone(),
                    }),
                    rhs: Box::new(SqlExpr::Column {
                        depth: 1,
                        column: junction_target_col.clone(),
                    }),
                },
            ];
            m2m_seed_selects(ctx.schema, &mut child_builder, &child_col);
            let alias = ctx.aliases.subquery();
            let mut outer = SelectBuilder::new();
            outer.from = Some(FromClause::Subquery {
                query: Box::new(child_builder),
                alias,
            });
            child_builder = outer;
        } else {
            child_builder.where_ = vec![SqlExpr::InSubquery {
                expr: Box::new(SqlExpr::Column {
                    depth: 0,
                    column: child_col.clone(),
                }),
                query: Box::new(bulk_query()),
            }];
        }
    }
    let child_level = Level {
        builder: child_builder,
        element: eff.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    };
    let mut child = lower_tail(
        ctx,
        eff,
        child_level,
        keyed,
        keyed.then(|| child_col.clone()),
        false,
    );
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    if keyed {
        ensure_key(&mut child.builder, &child_col, ctx.schema);
    }
    let outputs = builder_outputs(&child.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&child.builder, ctx.schema), |name| {
            keyed && name == child_col
        });
    let item_ty = match &eff.ty {
        QueryType::List(sub) => sub.as_ref(),
        _ => &eff.ty,
    };
    let mut element = child_element(
        ctx.schema,
        child.output_shapes.as_ref(),
        item_ty,
        &outputs,
        |name| keyed && name == child_col,
    );
    let idx = ctx.sink(
        child.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    patch_pending_parents(&mut element, idx);
    let shape = ResultShape::Single {
        element: Box::new(element),
        child: ChildRef {
            stmt: idx,
            parent: parent_key.clone().map(|parent_key| ParentRef {
                stmt: if ctx.nest_depth >= 1 {
                    PENDING_PARENT_STMT
                } else {
                    0
                },
                parent_key,
                child_key: child_col,
                negated: false,
            }),
            cardinality: Cardinality::One,
            tag: None,
        },
    };
    (shape, parent_key)
}

fn temp_key_query(temp: &str) -> SelectBuilder {
    temp_col_query(temp, "$$key", Some(ScalarType::Int))
}

fn temp_col_query(temp: &str, column: &str, ty: Option<ScalarType>) -> SelectBuilder {
    SelectBuilder {
        select: vec![SelectItem::Column {
            depth: 0,
            column: column.to_string(),
            output: column.to_string(),
            ty,
        }],
        from: Some(FromClause::Table {
            table: temp.to_string(),
            alias: temp.to_string(),
        }),
        where_: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        unions: Vec::new(),
    }
}

fn rtemp_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
    cardinality: Cardinality,
) -> (ResultShape, Option<String>) {
    if ctx.nest_depth >= 1 {
        if matches!(cardinality, Cardinality::Many) {
            todo!("M4b nested list item");
        }
        todo!("M4b nested record item");
    }
    let reduced_tail = beta_reduce_tail(tail);
    let tail: &TypedExpr = &reduced_tail;
    let temp = ctx.aliases.temp();
    let (child_from, child_col, key_col, m2m_join) = match tail_root_ident(tail) {
        Some(IdentBinding::Field(fid)) => {
            let link = resolve_tail_link(ctx, *fid);
            (
                link.child_from,
                link.child_col,
                link.parent_col,
                link.m2m_join,
            )
        }
        _ => unreachable!(
            "exotic temp tail root: temp tails bottom at Field idents (single-spine tails admit one root); remaining kinds infer non-List types, contradicting the List-only callers"
        ),
    };
    let parent_q = parent.builder.clone();
    let mut child_builder = SelectBuilder::new();
    child_builder.from = Some(child_from);
    let pair_col = if m2m_join.is_some() {
        key_col.clone()
    } else {
        "$$key".to_string()
    };
    if let Some((target_pk, junction_target_col)) = &m2m_join {
        child_builder.where_ = vec![
            SqlExpr::InSubquery {
                expr: Box::new(SqlExpr::Column {
                    depth: 1,
                    column: child_col.clone(),
                }),
                query: Box::new(temp_col_query(&temp, &key_col, None)),
            },
            SqlExpr::Binary {
                op: SqlBinOp::Eq,
                lhs: Box::new(SqlExpr::Column {
                    depth: 0,
                    column: target_pk.clone(),
                }),
                rhs: Box::new(SqlExpr::Column {
                    depth: 1,
                    column: junction_target_col.clone(),
                }),
            },
        ];
        m2m_seed_selects(ctx.schema, &mut child_builder, &child_col);
        let alias = ctx.aliases.subquery();
        let mut outer = SelectBuilder::new();
        outer.from = Some(FromClause::Subquery {
            query: Box::new(child_builder),
            alias,
        });
        child_builder = outer;
    } else {
        child_builder.where_ = vec![SqlExpr::InSubquery {
            expr: Box::new(SqlExpr::Column {
                depth: 0,
                column: child_col.clone(),
            }),
            query: Box::new(temp_key_query(&temp)),
        }];
    }
    let child_level = Level {
        builder: child_builder,
        element: tail.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    };
    ctx.temp_collect = Some(TempCollect {
        temp: temp.clone(),
        child_fk: child_col.clone(),
        refs: Vec::new(),
    });
    let mut child = lower_tail(ctx, tail, child_level, true, Some(child_col.clone()), false);
    let refs = ctx
        .temp_collect
        .take()
        .unwrap_or_else(|| unreachable!("temp collect vanished mid-lowering"))
        .refs;
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    ensure_key(&mut child.builder, &child_col, ctx.schema);
    let mut create_q = parent_q;
    if create_q.select.is_empty() {
        create_q.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    for col in &refs {
        if !available_cols(&create_q, ctx.schema).contains(col) {
            todo!("M4b key threading through subqueries");
        }
        ensure_key(&mut create_q, col, ctx.schema);
    }
    if m2m_join.is_some() {
        ensure_key(&mut create_q, &key_col, ctx.schema);
    }
    create_q.select.push(SelectItem::Column {
        depth: 0,
        column: key_col,
        output: "$$key".to_string(),
        ty: Some(ScalarType::Int),
    });
    let create_outputs = builder_output_types(&create_q, ctx.schema);
    let temp_idx = ctx.sink(
        create_q,
        StatementShape {
            columns: shape_columns(create_outputs.clone(), |name| name == "$$key"),
            rows: RowCount::Many,
        },
        StatementKind::Temp { name: temp.clone() },
    );
    swap_parent_to_temp(parent, &temp, &create_outputs);
    let outputs = builder_outputs(&child.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&child.builder, ctx.schema), |name| {
            name == child_col
        });
    let item_ty = match &tail.ty {
        QueryType::List(sub) => sub.as_ref(),
        _ => &tail.ty,
    };
    let element = child_element(
        ctx.schema,
        child.output_shapes.as_ref(),
        item_ty,
        &outputs,
        |name| name == child_col,
    );
    let idx = ctx.sink(
        child.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    let shape = match cardinality {
        Cardinality::Many | Cardinality::Take { .. } => ResultShape::List {
            element: Box::new(element),
            child: ChildRef {
                stmt: idx,
                parent: Some(ParentRef {
                    stmt: temp_idx,
                    parent_key: pair_col.clone(),
                    child_key: child_col,
                    negated: false,
                }),
                cardinality: Cardinality::Many,
                tag: None,
            },
        },
        Cardinality::One => ResultShape::Single {
            element: Box::new(element),
            child: ChildRef {
                stmt: idx,
                parent: Some(ParentRef {
                    stmt: temp_idx,
                    parent_key: pair_col.clone(),
                    child_key: child_col,
                    negated: false,
                }),
                cardinality: Cardinality::One,
                tag: None,
            },
        },
    };
    (shape, Some(pair_col))
}

fn null_cells(ty: &QueryType, out: &mut Vec<(SqlExpr, Option<ScalarType>)>) {
    match peel_optional(ty) {
        QueryType::Scalar(s) => out.push((SqlExpr::Param(ParamValue::Null), Some(*s))),
        QueryType::Tuple(tys) => tys.iter().for_each(|t| null_cells(t, out)),
        QueryType::Record(RecordSource::Projection(fields)) => {
            fields.iter().for_each(|field| null_cells(&field.ty, out))
        }
        _ => unreachable!(
            "null cells over {ty}: lists pre-excluded, schema records rejected upfront"
        ),
    }
}

fn temp_array_cell(
    ctx: &mut Context,
    ty: &QueryType,
    elem: &TypedExpr,
    out: &mut Vec<(SqlExpr, Option<ScalarType>)>,
) {
    let node = peel_some(elem);
    match peel_optional(ty) {
        QueryType::Scalar(s) => out.push((scalar(ctx, node), Some(*s))),
        QueryType::Tuple(tys) => {
            let TypedExprKind::Tuple { elements } = &node.kind else {
                todo!("M6 correlated literal projection item");
            };
            if elements.len() != tys.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            for (t, e) in tys.iter().zip(elements.iter()) {
                temp_array_cell(ctx, t, e, out);
            }
        }
        QueryType::Record(RecordSource::Projection(fields)) => {
            let TypedExprKind::Struct { fields: elements } = &node.kind else {
                todo!("M6 correlated literal projection item");
            };
            for field in fields {
                let (_, e) = elements
                    .iter()
                    .find(|(fname, _)| fname.value == field.name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {} vanished after checking: unification matches field sets",
                            field.name
                        )
                    });
                temp_array_cell(ctx, &field.ty, e, out);
            }
        }
        _ => todo!("M6 correlated literal projection item"),
    }
}

fn temp_array_cells(
    ctx: &mut Context,
    elem: &TypedExpr,
    inner: &QueryType,
    out: &mut Vec<(SqlExpr, Option<ScalarType>)>,
) {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        null_cells(inner, out);
        return;
    }
    temp_array_cell(ctx, inner, elem, out);
}

fn temp_nest_ok(ty: &QueryType) -> bool {
    match peel_optional(ty) {
        QueryType::Scalar(_) => true,
        QueryType::Tuple(tys) => tys.iter().all(temp_nest_ok),
        QueryType::Record(RecordSource::Projection(fields)) => {
            fields.iter().all(|field| temp_nest_ok(&field.ty))
        }
        QueryType::List(sub) => temp_nest_ok(sub),
        _ => false,
    }
}

enum BranchPlan {
    Value { col: String, ty: ScalarType },
    Nested { sub: QueryType },
    Tuple(Vec<BranchPlan>),
    Record(Vec<(String, BranchPlan)>),
}

fn branch_plan(ty: &QueryType, next_col: &mut usize) -> BranchPlan {
    match peel_optional(ty) {
        QueryType::Scalar(s) => {
            let col = format!("c{next_col}");
            *next_col += 1;
            BranchPlan::Value { col, ty: *s }
        }
        QueryType::List(sub) => BranchPlan::Nested {
            sub: sub.as_ref().clone(),
        },
        QueryType::Tuple(tys) => {
            BranchPlan::Tuple(tys.iter().map(|t| branch_plan(t, next_col)).collect())
        }
        QueryType::Record(RecordSource::Projection(fields)) => BranchPlan::Record(
            fields
                .iter()
                .map(|field| (field.name.clone(), branch_plan(&field.ty, next_col)))
                .collect(),
        ),
        _ => unreachable!(
            "branch layout over {ty}: validation admits scalars, tuples, records, and lists only"
        ),
    }
}

fn branch_nested_count(plan: &BranchPlan) -> usize {
    match plan {
        BranchPlan::Value { .. } => 0,
        BranchPlan::Nested { .. } => 1,
        BranchPlan::Tuple(ps) => ps.iter().map(branch_nested_count).sum(),
        BranchPlan::Record(fs) => fs.iter().map(|(_, p)| branch_nested_count(p)).sum(),
    }
}

fn branch_value_cols(plan: &BranchPlan, out: &mut Vec<(String, ScalarType)>) {
    match plan {
        BranchPlan::Value { col, ty } => out.push((col.clone(), *ty)),
        BranchPlan::Nested { .. } => {}
        BranchPlan::Tuple(ps) => ps.iter().for_each(|p| branch_value_cols(p, out)),
        BranchPlan::Record(fs) => fs.iter().for_each(|(_, p)| branch_value_cols(p, out)),
    }
}

fn temp_leaf_tys(ty: &QueryType, out: &mut Vec<ScalarType>) {
    match peel_optional(ty) {
        QueryType::Scalar(s) => out.push(*s),
        QueryType::Tuple(tys) => tys.iter().for_each(|t| temp_leaf_tys(t, out)),
        QueryType::Record(RecordSource::Projection(fields)) => fields
            .iter()
            .for_each(|field| temp_leaf_tys(&field.ty, out)),
        _ => unreachable!(
            "leaf layout over {ty}: validation admits scalars, tuples, and records only"
        ),
    }
}

fn serve_branch_cells(
    ctx: &mut Context,
    plan: &BranchPlan,
    elem: &TypedExpr,
    out: &mut Vec<(SqlExpr, Option<ScalarType>)>,
) {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        null_branch_cells(plan, out);
        return;
    }
    match (plan, &node.kind) {
        (BranchPlan::Value { ty, .. }, _) => out.push((scalar(ctx, node), Some(*ty))),
        (BranchPlan::Nested { .. }, _) => {}
        (BranchPlan::Tuple(ps), TypedExprKind::Tuple { elements }) => {
            if elements.len() != ps.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            for (p, e) in ps.iter().zip(elements.iter()) {
                serve_branch_cells(ctx, p, e, out);
            }
        }
        (BranchPlan::Record(fs), TypedExprKind::Struct { fields }) => {
            for (name, p) in fs {
                let (_, e) = fields
                    .iter()
                    .find(|(fname, _)| fname.value == *name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {name} vanished after checking: unification matches field sets"
                        )
                    });
                serve_branch_cells(ctx, p, e, out);
            }
        }
        _ => todo!("M6 correlated literal projection item"),
    }
}

fn null_branch_cells(plan: &BranchPlan, out: &mut Vec<(SqlExpr, Option<ScalarType>)>) {
    match plan {
        BranchPlan::Value { ty, .. } => out.push((SqlExpr::Param(ParamValue::Null), Some(*ty))),
        BranchPlan::Nested { .. } => {}
        BranchPlan::Tuple(ps) => ps.iter().for_each(|p| null_branch_cells(p, out)),
        BranchPlan::Record(fs) => fs.iter().for_each(|(_, p)| null_branch_cells(p, out)),
    }
}

fn collect_branch_nested<'a>(
    plan: &BranchPlan,
    elem: &'a TypedExpr,
    ord: i64,
    out: &mut Vec<(Option<i64>, &'a TypedExpr)>,
) {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        return;
    }
    match (plan, &node.kind) {
        (BranchPlan::Value { .. }, _) => {}
        (BranchPlan::Nested { .. }, TypedExprKind::Array { elements }) => {
            out.extend(elements.iter().map(|e| (Some(ord), e)));
        }
        (BranchPlan::Tuple(ps), TypedExprKind::Tuple { elements }) => {
            if elements.len() != ps.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            for (p, e) in ps.iter().zip(elements.iter()) {
                collect_branch_nested(p, e, ord, out);
            }
        }
        (BranchPlan::Record(fs), TypedExprKind::Struct { fields }) => {
            for (name, p) in fs {
                let (_, e) = fields
                    .iter()
                    .find(|(fname, _)| fname.value == *name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {name} vanished after checking: unification matches field sets"
                        )
                    });
                collect_branch_nested(p, e, ord, out);
            }
        }
        _ => todo!("M6 correlated literal projection item"),
    }
}

fn shape_branch_plan(plan: &BranchPlan, nested: Option<(&ResultShape, &ChildRef)>) -> ResultShape {
    match plan {
        BranchPlan::Value { col, .. } => ResultShape::Column(col.clone()),
        BranchPlan::Nested { .. } => {
            let (element, child) = nested.unwrap_or_else(|| {
                unreachable!("branch shape without nested sub-shape: multi-list is gated upfront")
            });
            ResultShape::List {
                element: Box::new(element.clone()),
                child: child.clone(),
            }
        }
        BranchPlan::Tuple(ps) => {
            ResultShape::Tuple(ps.iter().map(|p| shape_branch_plan(p, nested)).collect())
        }
        BranchPlan::Record(fs) => ResultShape::Record(
            fs.iter()
                .map(|(name, p)| (name.clone(), shape_branch_plan(p, nested)))
                .collect(),
        ),
    }
}

fn false_arm(columns: &[(String, Option<ScalarType>)]) -> SelectBuilder {
    SelectBuilder {
        select: columns
            .iter()
            .map(|(name, ty)| SelectItem::Expr {
                expr: SqlExpr::Param(ParamValue::Null),
                output: name.clone(),
                ty: *ty,
            })
            .collect(),
        from: None,
        where_: vec![SqlExpr::Param(ParamValue::Bool(false))],
        order_by: Vec::new(),
        limit: None,
        offset: None,
        unions: Vec::new(),
    }
}

fn union_builders(arms: Vec<SelectBuilder>) -> SelectBuilder {
    let mut arms = arms.into_iter();
    let mut first = arms.next().unwrap_or_else(|| {
        unreachable!("union without arms: empties render an unsatisfiable arm instead")
    });
    first.unions = arms.collect();
    first
}

fn sink_arm_temp(ctx: &mut Context, union: SelectBuilder) -> (String, usize) {
    let temp = ctx.aliases.temp();
    let alias = ctx.aliases.subquery();
    let mut union = union;
    let create_order = std::mem::take(&mut union.order_by);
    let mut create = SelectBuilder::new();
    create.select = vec![
        SelectItem::AllFrom { depth: 0 },
        SelectItem::Expr {
            expr: SqlExpr::Window {
                func: "ROW_NUMBER".to_string(),
                partition: Vec::new(),
                order: Vec::new(),
            },
            output: "$$key".to_string(),
            ty: Some(ScalarType::Int),
        },
    ];
    create.from = Some(FromClause::Subquery {
        query: Box::new(union),
        alias,
    });
    create.order_by = create_order;
    let outputs = builder_output_types(&create, ctx.schema);
    let idx = ctx.sink(
        create,
        StatementShape {
            columns: shape_columns(outputs.clone(), |name| {
                name == "c0" || name == "c1" || name == "$$key"
            }),
            rows: RowCount::Many,
        },
        StatementKind::Temp { name: temp.clone() },
    );
    (temp, idx)
}

fn temp_branch_arms<'a>(
    ctx: &mut Context,
    temp: &str,
    refs: &[(String, Option<ScalarType>)],
    plan: &BranchPlan,
    value_cols: &[(String, ScalarType)],
    items: &[(Option<i64>, &'a TypedExpr)],
) -> (Vec<SelectBuilder>, Vec<(Option<i64>, &'a TypedExpr)>) {
    let mut arms: Vec<SelectBuilder> = Vec::with_capacity(items.len());
    let mut sub_items: Vec<(Option<i64>, &'a TypedExpr)> = Vec::new();
    for (i, (filter, elem)) in items.iter().enumerate() {
        let own = items[..i].iter().filter(|(o, _)| *o == *filter).count() as i64;
        let mut select = vec![
            SelectItem::Column {
                depth: 0,
                column: "$$key".to_string(),
                output: "c0".to_string(),
                ty: None,
            },
            SelectItem::Expr {
                expr: SqlExpr::Param(ParamValue::Int(own)),
                output: "c1".to_string(),
                ty: Some(ScalarType::Int),
            },
        ];
        let mut cells = Vec::new();
        serve_branch_cells(ctx, plan, elem, &mut cells);
        if cells.len() != value_cols.len() {
            unreachable!("branch arm arity drift: cell walk and layout walk share the plan");
        }
        for ((expr, _), (name, ty)) in cells.into_iter().zip(value_cols.iter()) {
            select.push(SelectItem::Expr {
                expr,
                output: name.clone(),
                ty: Some(*ty),
            });
        }
        for (name, ty) in refs {
            select.push(SelectItem::Column {
                depth: 0,
                column: name.clone(),
                output: name.clone(),
                ty: *ty,
            });
        }
        let mut arm = SelectBuilder {
            select,
            from: Some(FromClause::Table {
                table: temp.to_string(),
                alias: temp.to_string(),
            }),
            where_: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            unions: Vec::new(),
        };
        if let Some(ord) = filter {
            arm.where_ = vec![SqlExpr::Binary {
                op: SqlBinOp::Eq,
                lhs: Box::new(SqlExpr::Column {
                    depth: 0,
                    column: "c1".to_string(),
                }),
                rhs: Box::new(SqlExpr::Param(ParamValue::Int(*ord))),
            }];
        }
        arms.push(arm);
        collect_branch_nested(plan, elem, own, &mut sub_items);
    }
    if arms.is_empty() {
        let mut layout = vec![
            ("c0".to_string(), None),
            ("c1".to_string(), Some(ScalarType::Int)),
        ];
        layout.extend(
            value_cols
                .iter()
                .map(|(name, ty)| (name.clone(), Some(*ty))),
        );
        layout.extend(refs.iter().map(|(name, ty)| (name.clone(), *ty)));
        arms.push(false_arm(&layout));
    }
    (arms, sub_items)
}

fn peel_row_ops<'a>(mut node: &'a TypedExpr) -> Option<(&'a TypedExpr, Vec<EntryMethod<'a>>)> {
    let mut methods = Vec::new();
    let mut sorted = false;
    loop {
        match &node.kind {
            TypedExprKind::Method {
                base, name, args, ..
            } => {
                let entry = match name.value.as_str() {
                    "take" | "skip" => {
                        if sorted {
                            return None;
                        }
                        EntryMethod::TakeSkip {
                            name: name.value.clone(),
                            args,
                        }
                    }
                    "sort" | "sort_asc" | "sort_desc" => {
                        sorted = true;
                        EntryMethod::Sort {
                            args,
                            default: if name.value == "sort_desc" {
                                OrderDir::Desc
                            } else {
                                OrderDir::Asc
                            },
                            wrap: false,
                        }
                    }
                    "filter" => {
                        sorted = true;
                        EntryMethod::Filter {
                            pred: one_arg(args, name),
                        }
                    }
                    _ => return None,
                };
                methods.push(entry);
                node = base;
            }
            TypedExprKind::Array { .. } => {
                methods.reverse();
                return Some((node, methods));
            }
            _ => return None,
        }
    }
}

enum EntryMethod<'a> {
    TakeSkip {
        name: String,
        args: &'a [TypedMethodArg],
    },
    Sort {
        args: &'a [TypedMethodArg],
        default: OrderDir,
        wrap: bool,
    },
    Filter {
        pred: &'a TypedExpr,
    },
}

fn apply_entry_method(
    ctx: &mut Context,
    method: EntryMethod,
    union: SelectBuilder,
    elem_ty: &QueryType,
) -> SelectBuilder {
    let level = Level {
        builder: union,
        element: QueryType::List(Box::new(elem_ty.clone())),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    };
    let mut builder = match method {
        EntryMethod::TakeSkip { name, args } => {
            let wrapped = wrap_level(ctx, level);
            window_take(ctx, wrapped, "c0", &name, args).builder
        }
        EntryMethod::Sort {
            args,
            default,
            wrap,
        } => {
            let mut level = level;
            if wrap {
                level = wrap_level(ctx, level);
            }
            let mut keys = Vec::new();
            for arg in args {
                let dir = match &arg.direction {
                    Some(dir) => match dir.value {
                        SortDir::Asc => OrderDir::Asc,
                        SortDir::Desc => OrderDir::Desc,
                    },
                    None => default,
                };
                keys.extend(sort_keys(ctx, &level, &arg.expr, dir));
            }
            level.builder.order_by.extend(keys);
            level.builder
        }
        EntryMethod::Filter { pred } => {
            let mut wrapped = wrap_level(ctx, level);
            wrapped.builder.where_.push(scalar(ctx, pred));
            wrapped.builder
        }
    };
    if builder.select.is_empty() {
        builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    builder
}

fn temp_array_level<'a>(
    ctx: &mut Context,
    temp: &str,
    temp_idx: usize,
    refs: &[(String, Option<ScalarType>)],
    elem_ty: &QueryType,
    items: Vec<(Option<i64>, &'a TypedExpr)>,
    methods: Vec<EntryMethod<'a>>,
) -> (ResultShape, ChildRef) {
    if !array_has_list_leaf(elem_ty) {
        return temp_leaf_level(ctx, temp, temp_idx, elem_ty, items, methods);
    }
    let mut next_col = 2usize;
    let plan = branch_plan(elem_ty, &mut next_col);
    if branch_nested_count(&plan) > 1 {
        todo!("M6 multi-list correlated {}", branch_kind(&plan));
    }
    let mut value_cols = Vec::new();
    branch_value_cols(&plan, &mut value_cols);
    let (arms, sub_items) = temp_branch_arms(ctx, temp, refs, &plan, &value_cols, &items);
    let mut union = union_builders(arms);
    for m in methods {
        union = apply_entry_method(ctx, m, union, elem_ty);
    }
    let (next_temp, next_idx) = sink_arm_temp(ctx, union);
    let sub = branch_nested_sub(&plan)
        .unwrap_or_else(|| unreachable!("branch without nested slot: multi-list is gated upfront"));
    let (sub_element, sub_child) =
        temp_array_level(ctx, &next_temp, next_idx, refs, &sub, sub_items, Vec::new());
    let element = shape_branch_plan(&plan, Some((&sub_element, &sub_child)));
    let child = ChildRef {
        stmt: next_idx,
        parent: Some(ParentRef {
            stmt: temp_idx,
            parent_key: "$$key".to_string(),
            child_key: "c0".to_string(),
            negated: false,
        }),
        cardinality: Cardinality::Many,
        tag: None,
    };
    (element, child)
}

fn temp_leaf_arms(
    ctx: &mut Context,
    temp: &str,
    elem_ty: &QueryType,
    items: &[(Option<i64>, &TypedExpr)],
    key_col: &str,
) -> Vec<SelectBuilder> {
    let mut arms: Vec<SelectBuilder> = Vec::with_capacity(items.len());
    for (filter, elem) in items {
        let mut cells = Vec::new();
        temp_array_cells(ctx, elem, elem_ty, &mut cells);
        let mut select = vec![SelectItem::Column {
            depth: 0,
            column: key_col.to_string(),
            output: "c0".to_string(),
            ty: None,
        }];
        for (i, (expr, ty)) in cells.into_iter().enumerate() {
            select.push(SelectItem::Expr {
                expr,
                output: format!("c{}", i + 1),
                ty,
            });
        }
        let mut arm = SelectBuilder {
            select,
            from: Some(FromClause::Table {
                table: temp.to_string(),
                alias: temp.to_string(),
            }),
            where_: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            unions: Vec::new(),
        };
        if let Some(ord) = filter {
            arm.where_ = vec![SqlExpr::Binary {
                op: SqlBinOp::Eq,
                lhs: Box::new(SqlExpr::Column {
                    depth: 0,
                    column: "c1".to_string(),
                }),
                rhs: Box::new(SqlExpr::Param(ParamValue::Int(*ord))),
            }];
        }
        arms.push(arm);
    }
    if arms.is_empty() {
        let mut tys = Vec::new();
        temp_leaf_tys(elem_ty, &mut tys);
        let mut layout = vec![("c0".to_string(), None)];
        layout.extend(
            tys.iter()
                .enumerate()
                .map(|(i, ty)| (format!("c{}", i + 1), Some(*ty))),
        );
        arms.push(false_arm(&layout));
    }
    arms
}

fn temp_leaf_level(
    ctx: &mut Context,
    temp: &str,
    temp_idx: usize,
    elem_ty: &QueryType,
    items: Vec<(Option<i64>, &TypedExpr)>,
    methods: Vec<EntryMethod>,
) -> (ResultShape, ChildRef) {
    let arms = temp_leaf_arms(ctx, temp, elem_ty, &items, "$$key");
    let mut union = union_builders(arms);
    for m in methods {
        union = apply_entry_method(ctx, m, union, elem_ty);
    }
    let child_builder = union;
    let outputs = builder_outputs(&child_builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&child_builder, ctx.schema), |name| {
            name == "c0"
        });
    let element = child_element(ctx.schema, None, elem_ty, &outputs, |name| name == "c0");
    let idx = ctx.sink(
        child_builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    let child = ChildRef {
        stmt: idx,
        parent: Some(ParentRef {
            stmt: temp_idx,
            parent_key: "$$key".to_string(),
            child_key: "c0".to_string(),
            negated: false,
        }),
        cardinality: Cardinality::Many,
        tag: None,
    };
    (element, child)
}

fn branch_nested_sub(plan: &BranchPlan) -> Option<QueryType> {
    match plan {
        BranchPlan::Value { .. } => None,
        BranchPlan::Nested { sub } => Some(sub.clone()),
        BranchPlan::Tuple(ps) => ps.iter().find_map(branch_nested_sub),
        BranchPlan::Record(fs) => fs.iter().find_map(|(_, p)| branch_nested_sub(p)),
    }
}

fn branch_kind(plan: &BranchPlan) -> &'static str {
    match plan {
        BranchPlan::Record(_) => "record",
        BranchPlan::Tuple(ps) if ps.iter().any(|p| branch_kind(p) == "record") => "record",
        _ => "tuple",
    }
}

fn materialize_array_parent(
    ctx: &mut Context,
    parent: &Level,
    refs: &[String],
) -> (String, usize, Vec<(String, Option<ScalarType>)>) {
    let parent_q = parent.builder.clone();
    let temp = ctx.aliases.temp();
    let mut create_q = parent_q;
    if create_q.select.is_empty() {
        create_q.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    for col in refs {
        if !available_cols(&create_q, ctx.schema).contains(col) {
            todo!("M4b key threading through subqueries");
        }
        ensure_key(&mut create_q, col, ctx.schema);
    }
    create_q.select.push(SelectItem::Expr {
        expr: SqlExpr::Window {
            func: "ROW_NUMBER".to_string(),
            partition: Vec::new(),
            order: Vec::new(),
        },
        output: "$$key".to_string(),
        ty: Some(ScalarType::Int),
    });
    let create_outputs = builder_output_types(&create_q, ctx.schema);
    let temp_idx = ctx.sink(
        create_q,
        StatementShape {
            columns: shape_columns(create_outputs.clone(), |name| name == "$$key"),
            rows: RowCount::Many,
        },
        StatementKind::Temp { name: temp.clone() },
    );
    (temp, temp_idx, create_outputs)
}

fn resolve_ref_tys(
    ctx: &Context,
    parent: &Level,
    refs: &[String],
) -> Vec<(String, Option<ScalarType>)> {
    refs.iter()
        .map(|name| {
            (
                name.clone(),
                output_ty(&parent.builder, name, ctx.schema).or(Some(ScalarType::Int)),
            )
        })
        .collect()
}

fn swap_parent_to_temp(
    parent: &mut Level,
    temp: &str,
    create_outputs: &[(String, Option<ScalarType>)],
) {
    parent.builder = SelectBuilder {
        select: Vec::new(),
        from: Some(FromClause::Temp {
            name: temp.to_string(),
            columns: create_outputs
                .iter()
                .map(|(name, ty)| NamedColumn {
                    name: name.clone(),
                    ty: *ty,
                })
                .collect(),
        }),
        where_: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        unions: Vec::new(),
    };
    parent.stmt_bound = true;
}

fn rtemp_array_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
) -> (ResultShape, Option<String>) {
    let TypedExprKind::Array { elements } = &tail.kind else {
        unreachable!("rtemp array over non-array tail: the rvalue gate admits bare arrays only")
    };
    let inner = match &tail.ty {
        QueryType::List(inner) => inner.as_ref(),
        _ => unreachable!(
            "non-list array type {}: the checker types arrays as lists",
            tail.ty
        ),
    };
    if !temp_nest_ok(inner) {
        todo!("M6 query-valued array element");
    }
    let Some(refs) = collect_temp_refs(ctx.schema, tail, &[]) else {
        todo!("M6 correlated literal projection item");
    };
    if refs.iter().any(|r| r == "c0" || r == "c1" || r == "$$key") {
        todo!("M6 correlated literal projection item");
    }
    let (temp, temp_idx, create_outputs) = materialize_array_parent(ctx, parent, &refs);
    swap_parent_to_temp(parent, &temp, &create_outputs);
    let ref_tys = resolve_ref_tys(ctx, parent, &refs);
    ctx.temp_collect = Some(TempCollect {
        temp: temp.clone(),
        child_fk: "$$key".to_string(),
        refs,
    });
    ctx.in_array_temp = true;
    let items: Vec<(Option<i64>, &TypedExpr)> = elements.iter().map(|e| (None, e)).collect();
    let (element, child) =
        temp_array_level(ctx, &temp, temp_idx, &ref_tys, inner, items, Vec::new());
    ctx.in_array_temp = false;
    ctx.temp_collect.take();
    (
        ResultShape::List {
            element: Box::new(element),
            child,
        },
        Some("$$key".to_string()),
    )
}

fn rtemp_method_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
) -> (ResultShape, Option<String>) {
    let array: &TypedExpr = match &tail.kind {
        TypedExprKind::Method { base, name, .. } if name.value == "flatten" => {
            let TypedExprKind::Array { .. } = &base.kind else {
                todo!("M6 correlated literal projection item");
            };
            base.as_ref()
        }
        _ => match peel_row_ops(tail) {
            Some((array, _)) => array,
            _ => todo!("M6 correlated literal projection item"),
        },
    };
    let TypedExprKind::Array { .. } = &array.kind else {
        unreachable!("peeled chain bottom is always the bare array")
    };
    let inner = match &array.ty {
        QueryType::List(inner) => inner.as_ref(),
        _ => unreachable!(
            "non-list array type {}: the checker types arrays as lists",
            array.ty
        ),
    };
    if !temp_nest_ok(inner) {
        todo!("M6 query-valued array element");
    }
    let fieldmap = element_field_cols(inner).unwrap_or_default();
    if fieldmap
        .iter()
        .any(|(field, _)| field == "c0" || field == "c1" || field == "$$key")
        || tail_scope_named(tail, &["c0", "c1", "$$key"])
    {
        todo!("M6 correlated literal projection item");
    }
    let mut rewritten;
    let tail: &TypedExpr = if fieldmap.is_empty() {
        tail
    } else {
        rewritten = tail.clone();
        rewrite_tail_args(&mut rewritten, &fieldmap);
        &rewritten
    };
    let (array, mut methods): (&TypedExpr, Vec<EntryMethod>) = match &tail.kind {
        TypedExprKind::Method { base, name, .. } if name.value == "flatten" => {
            let TypedExprKind::Array { .. } = &base.kind else {
                todo!("M6 correlated literal projection item");
            };
            (base.as_ref(), Vec::new())
        }
        _ => match peel_row_ops(tail) {
            Some((array, methods)) if !methods.is_empty() => (array, methods),
            _ => todo!("M6 correlated literal projection item"),
        },
    };
    let rewritten_cols: Vec<&str> = fieldmap.iter().map(|(_, col)| col.as_str()).collect();
    for m in &mut methods {
        let EntryMethod::Sort { args, wrap, .. } = m else {
            continue;
        };
        let mut rewritten = false;
        let mut scope = false;
        for arg in *args {
            any_ident(&arg.expr, &mut |e| match &e.kind {
                TypedExprKind::Ident {
                    binding: IdentBinding::ProjectedField { name },
                    depth: 0,
                    ..
                } if rewritten_cols.iter().any(|c| *c == name) => {
                    rewritten = true;
                    false
                }
                TypedExprKind::Ident {
                    binding: IdentBinding::Field(_) | IdentBinding::ProjectedField { .. },
                    depth: 0,
                    ..
                } => {
                    scope = true;
                    false
                }
                _ => false,
            });
        }
        if rewritten && scope {
            todo!("M6 correlated literal projection item");
        }
        *wrap = rewritten;
    }
    let TypedExprKind::Array { elements } = &array.kind else {
        unreachable!("peeled chain bottom is always the bare array")
    };
    let inner = match &array.ty {
        QueryType::List(inner) => inner.as_ref(),
        _ => unreachable!(
            "non-list array type {}: the checker types arrays as lists",
            array.ty
        ),
    };
    let skip: Vec<String> = fieldmap.iter().map(|(_, col)| col.clone()).collect();
    let Some(refs) = collect_temp_refs(ctx.schema, tail, &skip) else {
        todo!("M6 correlated literal projection item");
    };
    if refs.iter().any(|r| r == "c0" || r == "c1" || r == "$$key") {
        todo!("M6 correlated literal projection item");
    }
    let (temp, temp_idx, create_outputs) = materialize_array_parent(ctx, parent, &refs);
    swap_parent_to_temp(parent, &temp, &create_outputs);
    let ref_tys = resolve_ref_tys(ctx, parent, &refs);
    ctx.temp_collect = Some(TempCollect {
        temp: temp.clone(),
        child_fk: "$$key".to_string(),
        refs,
    });
    ctx.in_array_temp = true;
    let items: Vec<(Option<i64>, &TypedExpr)> = elements.iter().map(|e| (None, e)).collect();
    let (element, child) = if methods.is_empty() {
        temp_flatten_level(ctx, &temp, temp_idx, &ref_tys, inner, items)
    } else {
        temp_array_level(ctx, &temp, temp_idx, &ref_tys, inner, items, methods)
    };
    ctx.in_array_temp = false;
    ctx.temp_collect.take();
    (
        ResultShape::List {
            element: Box::new(element),
            child,
        },
        Some("$$key".to_string()),
    )
}

fn temp_flatten_level(
    ctx: &mut Context,
    temp: &str,
    temp_idx: usize,
    refs: &[(String, Option<ScalarType>)],
    elem_ty: &QueryType,
    items: Vec<(Option<i64>, &TypedExpr)>,
) -> (ResultShape, ChildRef) {
    let mut next_col = 2usize;
    let plan = branch_plan(elem_ty, &mut next_col);
    let BranchPlan::Nested { sub } = &plan else {
        unreachable!("flatten over non-nested arrays: the checker admits doubly-nested lists only")
    };
    if array_has_list_leaf(sub) {
        todo!("M6 correlated literal projection item");
    }
    let mut value_cols = Vec::new();
    branch_value_cols(&plan, &mut value_cols);
    debug_assert!(
        value_cols.is_empty(),
        "flatten over scalar-carrying branch: pure nesting serves keys only"
    );
    let (arms, sub_items) = temp_branch_arms(ctx, temp, refs, &plan, &value_cols, &items);
    let (next_temp, _) = sink_arm_temp(ctx, union_builders(arms));
    let arms = temp_leaf_arms(ctx, &next_temp, sub, &sub_items, "c0");
    let child_builder = union_builders(arms);
    let outputs = builder_outputs(&child_builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> =
        shape_columns(builder_output_types(&child_builder, ctx.schema), |name| {
            name == "c0"
        });
    let element = child_element(ctx.schema, None, sub, &outputs, |name| name == "c0");
    let idx = ctx.sink(
        child_builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    let child = ChildRef {
        stmt: idx,
        parent: Some(ParentRef {
            stmt: temp_idx,
            parent_key: "$$key".to_string(),
            child_key: "c0".to_string(),
            negated: false,
        }),
        cardinality: Cardinality::Many,
        tag: None,
    };
    (element, child)
}

fn scalar_temp_array(
    ctx: &mut Context,
    parent: &mut Level,
    prefix: &str,
    value: &TypedExpr,
) -> Option<(Vec<SelectItem>, ResultShape, Vec<String>)> {
    if ctx.child.is_some()
        || ctx.temp_collect.is_some()
        || ctx.in_take_arg
        || ctx.corr
        || ctx.in_array_temp
    {
        return None;
    }
    let TypedExprKind::Method {
        base,
        name,
        args,
        optional,
    } = &value.kind
    else {
        return None;
    };
    if *optional {
        return None;
    }
    if !matches!(
        name.value.as_str(),
        "len" | "sum" | "avg" | "max" | "min" | "contains" | "first" | "nth"
    ) {
        return None;
    }
    let (array, methods): (&TypedExpr, Vec<EntryMethod>) = match &base.kind {
        TypedExprKind::Array { .. } => (base, Vec::new()),
        _ => match peel_row_ops(base) {
            Some((array, methods)) if !methods.is_empty() => (array, methods),
            _ => return None,
        },
    };
    let TypedExprKind::Array { elements } = &array.kind else {
        unreachable!("peeled chain bottom is always the bare array")
    };
    let inner = match &array.ty {
        QueryType::List(inner) => inner.as_ref(),
        _ => return None,
    };
    if !matches!(peel_optional(inner), QueryType::Scalar(_)) {
        return None;
    }
    if tail_uses_outer(base) {
        return None;
    }
    if !has_scope_ident(base) {
        return None;
    }
    if name.value == "contains" {
        let needle = one_arg(args, name);
        if (has_scope_ident(needle) || uses_outer(needle)) && !shiftable_needle(needle) {
            return None;
        }
    } else if args
        .iter()
        .any(|arg| has_scope_ident(&arg.expr) || uses_outer(&arg.expr))
    {
        return None;
    }
    if !temp_nest_ok(inner) {
        return None;
    }
    let refs = collect_temp_refs(ctx.schema, base, &[])?;
    if refs.iter().any(|r| r == "c0" || r == "c1" || r == "$$key") {
        return None;
    }
    if name.value == "contains" && !one_arg(args, name).ty.is_scalar() {
        todo!("comparison-overhaul: composite membership");
    }
    let (temp, _temp_idx, create_outputs) = materialize_array_parent(ctx, parent, &refs);
    swap_parent_to_temp(parent, &temp, &create_outputs);
    ctx.temp_collect = Some(TempCollect {
        temp: temp.clone(),
        child_fk: "$$key".to_string(),
        refs,
    });
    ctx.in_array_temp = true;
    let items: Vec<(Option<i64>, &TypedExpr)> = elements.iter().map(|e| (None, e)).collect();
    let arms = temp_leaf_arms(ctx, &temp, inner, &items, "$$key");
    let mut union = union_builders(arms);
    for m in methods {
        union = apply_entry_method(ctx, m, union, inner);
    }
    let inner_alias = ctx.aliases.subquery();
    let inner_from = FromClause::Subquery {
        query: Box::new(union),
        alias: inner_alias,
    };
    let key_pred = || SqlExpr::Binary {
        op: SqlBinOp::Eq,
        lhs: Box::new(SqlExpr::Column {
            depth: 0,
            column: "c0".to_string(),
        }),
        rhs: Box::new(SqlExpr::Column {
            depth: 1,
            column: "$$key".to_string(),
        }),
    };
    let result_ty = scalar_of(&value.ty);
    let expr = match name.value.as_str() {
        "contains" => {
            let needle_node = one_arg(args, name);
            let needle = if has_scope_ident(needle_node) || uses_outer(needle_node) {
                if !shiftable_needle(needle_node) {
                    todo!("M6 correlated literal outside temp lane");
                }
                shift_depths(scalar(ctx, needle_node), 1)
            } else {
                scalar(ctx, needle_node)
            };
            let mut exists = SelectBuilder::new();
            exists.select = vec![SelectItem::AllFrom { depth: 0 }];
            exists.from = Some(inner_from);
            exists.where_ = vec![
                key_pred(),
                SqlExpr::Binary {
                    op: SqlBinOp::Eq,
                    lhs: Box::new(SqlExpr::Column {
                        depth: 0,
                        column: "c1".to_string(),
                    }),
                    rhs: Box::new(needle),
                },
            ];
            SqlExpr::Exists(Box::new(exists))
        }
        "first" | "nth" => {
            let mut middle = SelectBuilder::new();
            middle.select = vec![SelectItem::Column {
                depth: 0,
                column: "c1".to_string(),
                output: ctx.aliases.generic(),
                ty: result_ty,
            }];
            middle.from = Some(inner_from);
            middle.where_ = vec![key_pred()];
            compose_limit(&mut middle, &name.value, args, ctx);
            SqlExpr::Subquery {
                query: Box::new(middle),
            }
        }
        method @ ("len" | "sum" | "avg" | "max" | "min") => {
            let agg = match method {
                "len" => AggOp::Len,
                "sum" => AggOp::Bare(BareAgg::Sum, inner_scalar(inner, method)),
                "avg" => AggOp::Bare(BareAgg::Avg, inner_scalar(inner, method)),
                "max" => AggOp::Bare(BareAgg::Max, inner_scalar(inner, method)),
                _ => AggOp::Bare(BareAgg::Min, inner_scalar(inner, method)),
            };
            let mut query = SelectBuilder::new();
            query.select = vec![SelectItem::Expr {
                expr: agg_call(&agg, || SqlExpr::Column {
                    depth: 0,
                    column: "c1".to_string(),
                }),
                output: ctx.aliases.generic(),
                ty: result_ty,
            }];
            query.from = Some(inner_from);
            query.where_ = vec![key_pred()];
            SqlExpr::Subquery {
                query: Box::new(query),
            }
        }
        _ => unreachable!(
            "scalar-temp method outside the allowlist: gates admit reductions, membership, and collapses only"
        ),
    };
    ctx.in_array_temp = false;
    ctx.temp_collect.take();
    let item = SelectItem::Expr {
        expr,
        output: prefix.to_string(),
        ty: result_ty,
    };
    Some((
        vec![item],
        ResultShape::Column(prefix.to_string()),
        vec!["$$key".to_string()],
    ))
}

fn filter_chain_to_field_ident(mut node: &TypedExpr) -> bool {
    loop {
        match &node.kind {
            TypedExprKind::Ident {
                binding: IdentBinding::Field(_),
                ..
            } => return true,
            TypedExprKind::Method { base, name, .. } if name.value == "filter" => node = base,
            _ => return false,
        }
    }
}

fn filter_chain_to_root_ident(mut node: &TypedExpr) -> bool {
    loop {
        match &node.kind {
            TypedExprKind::Ident {
                binding: IdentBinding::Root { .. },
                ..
            } => return true,
            TypedExprKind::Method { base, name, .. } if name.value == "filter" => node = base,
            _ => return false,
        }
    }
}

pub struct RootPlan {
    pub builder: SelectBuilder,
    pub shape: StatementShape,
    pub result: ResultShape,
}

pub fn lower_query(ctx: &mut Context, result: &TypedExpr) -> RootPlan {
    if result.ty.is_scalar() {
        let mut builder = SelectBuilder::new();
        let output = ctx.aliases.generic();
        match scalar(ctx, result) {
            SqlExpr::Subquery { mut query }
                if query.select.len() == 1
                    && !matches!(query.select[0], SelectItem::AllFrom { .. }) =>
            {
                match &mut query.select[0] {
                    SelectItem::Column { output: o, .. } | SelectItem::Expr { output: o, .. } => {
                        *o = output.clone();
                    }
                    SelectItem::AllFrom { .. } => unreachable!("guarded above"),
                }
                builder = *query;
            }
            expr => {
                builder.select = vec![SelectItem::Expr {
                    expr,
                    output: output.clone(),
                    ty: scalar_of(&result.ty),
                }];
            }
        }
        return RootPlan {
            builder,
            shape: StatementShape {
                columns: vec![OutputColumn {
                    name: output.clone(),
                    role: ColumnRole::Value,
                    ty: scalar_of(&result.ty),
                }],
                rows: RowCount::One,
            },
            result: ResultShape::Column(output),
        };
    }
    if matches!(result.ty, QueryType::Tuple(_)) {
        return lower_tuple_result(ctx, result);
    }
    if matches!(result.kind, TypedExprKind::If { .. }) && matches!(result.ty, QueryType::Record(_))
    {
        return lower_struct_result(ctx, result);
    }
    if matches!(result.kind, TypedExprKind::Struct { .. }) {
        return lower_struct_result(ctx, result);
    }
    let level = rows(ctx, result);
    finalize_level(ctx, level)
}

fn finalize_level(ctx: &mut Context, level: Level) -> RootPlan {
    if let Some(pending) = level.pending {
        return emit_split(
            ctx,
            level.builder,
            pending,
            &level.element,
            level.root_parts,
        );
    }
    if matches!(level.element, QueryType::Tuple(_)) {
        let mut builder = level.builder;
        if builder.select.is_empty() {
            builder.select = vec![SelectItem::AllFrom { depth: 0 }];
        }
        let outputs = builder_outputs(&builder, ctx.schema);
        let shape = StatementShape {
            columns: shape_columns(builder_output_types(&builder, ctx.schema), |_| false),
            rows: RowCount::Many,
        };
        let result = ResultShape::List {
            element: Box::new(ResultShape::Tuple(
                outputs.into_iter().map(ResultShape::Column).collect(),
            )),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        };
        return RootPlan {
            builder,
            shape,
            result,
        };
    }
    if matches!(
        level.element,
        QueryType::Optional(ref inner) if matches!(inner.as_ref(), QueryType::Tuple(_))
    ) {
        unreachable!(
            "optional-tuple level in finalize: Optional(Tuple) node types never arise — wrap_optional maps tuples element-wise instead of wrapping, Some-of-tuple is rejected by the final check (QT0027), and no method signature wraps a bare tuple"
        )
    }
    let mut builder = level.builder;
    if builder.select.is_empty() {
        builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let outputs = builder_outputs(&builder, ctx.schema);
    let element = match level.output_shapes.as_ref() {
        Some(items) => ResultShape::Record(
            items
                .iter()
                .map(|(alias, shape)| {
                    (
                        alias.clone(),
                        shape
                            .clone()
                            .unwrap_or_else(|| ResultShape::Column(alias.clone())),
                    )
                })
                .collect(),
        ),
        None => {
            let item_ty = match peel_optional(&level.element) {
                QueryType::List(sub) => sub.as_ref(),
                _ => &level.element,
            };
            child_element(ctx.schema, None, item_ty, &outputs, |_| false)
        }
    };
    let single = matches!(level.element, QueryType::Optional(_));
    let shape = StatementShape {
        columns: shape_columns(builder_output_types(&builder, ctx.schema), |_| false),
        rows: if single {
            RowCount::ZeroOrOne
        } else {
            RowCount::Many
        },
    };
    let result = if single {
        ResultShape::Single {
            element: Box::new(element),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::One,
                tag: None,
            },
        }
    } else {
        ResultShape::List {
            element: Box::new(element),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        }
    };
    RootPlan {
        builder,
        shape,
        result,
    }
}

fn drain_pending(pending: PendingSplit) -> Vec<PendingSplit> {
    let mut hops: Vec<PendingSplit> = Vec::new();
    let mut cursor = Some(pending);
    while let Some(mut hop) = cursor.take() {
        cursor = hop.next.take().map(|boxed| *boxed);
        hops.push(hop);
    }
    hops
}

fn sink_chain(ctx: &mut Context, hops: &[PendingSplit], root_creation: usize) -> Vec<NestLayer> {
    let mut built: Vec<(SelectBuilder, StatementShape, Vec<String>)> =
        Vec::with_capacity(hops.len());
    for (i, hop) in hops.iter().enumerate() {
        let child = hop_builder(hop);
        let child_outputs = builder_outputs(&child, ctx.schema);
        let next_parent = hops.get(i + 1).map(|next| next.parent_col.clone());
        let child_shape = StatementShape {
            columns: shape_columns(builder_output_types(&child, ctx.schema), |name| {
                name == hop.child_col || next_parent.as_deref() == Some(name)
            }),
            rows: RowCount::Many,
        };
        built.push((child, child_shape, child_outputs));
    }
    let mut creation: Vec<usize> = vec![0; hops.len()];
    let mut outputs: Vec<Vec<String>> = vec![Vec::new(); hops.len()];
    for (i, (child, shape, child_outputs)) in built.into_iter().enumerate().rev() {
        creation[i] = ctx.sink(child, shape, StatementKind::Query);
        outputs[i] = child_outputs;
    }
    let mut layers: Vec<NestLayer> = Vec::with_capacity(hops.len());
    for (i, hop) in hops.iter().enumerate() {
        layers.push(NestLayer {
            creation: creation[i],
            pref: ParentRef {
                stmt: if i == 0 {
                    root_creation
                } else {
                    creation[i - 1]
                },
                parent_key: hop.parent_col.clone(),
                child_key: hop.child_col.clone(),
                negated: false,
            },
            outputs: std::mem::take(&mut outputs[i]),
            middle: hop.middle_parts.clone(),
        });
    }
    layers
}

struct NestLayer {
    creation: usize,
    pref: ParentRef,
    outputs: Vec<String>,
    middle: Option<Vec<MiddlePart>>,
}

fn peel_lists(mut ty: &QueryType) -> &QueryType {
    loop {
        match peel_optional(ty) {
            QueryType::List(sub) => ty = sub,
            _ => return peel_optional(ty),
        }
    }
}

fn nest_layers(
    schema: &ValidatedSchema,
    layers: Vec<NestLayer>,
    root_ty: &QueryType,
    root_dup: bool,
    root_parts: Option<Vec<MiddlePart>>,
    root_stmt: usize,
) -> ResultShape {
    if layers.is_empty() {
        unreachable!("nesting no layers: callers only nest non-empty chains");
    }
    let wrap = |element: ResultShape, layer: &NestLayer| ResultShape::List {
        element: Box::new(element),
        child: ChildRef {
            stmt: layer.creation,
            parent: Some(layer.pref.clone()),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    let innermost = layers.len() - 1;
    let has_middle = root_parts.is_some() || layers.iter().any(|layer| layer.middle.is_some());
    let mut element = if layers[innermost].middle.is_some() {
        middle_tuple(
            layers[innermost].middle.as_ref().unwrap_or_else(|| {
                unreachable!("middle hop lost its parts: markers travel with the hop")
            }),
            None,
        )
    } else if has_middle {
        let inner = &layers[innermost];
        let kept: Vec<String> = inner
            .outputs
            .iter()
            .filter(|name| name.as_str() != inner.pref.child_key)
            .cloned()
            .collect();
        debug_assert_eq!(
            kept.len(),
            1,
            "position leaf serves one scalar besides its pairing key"
        );
        match kept.as_slice() {
            [col] => ResultShape::Column(col.clone()),
            kept => ResultShape::Record(
                kept.iter()
                    .map(|name| (name.clone(), ResultShape::Column(name.clone())))
                    .collect(),
            ),
        }
    } else {
        let inner = &layers[innermost];
        child_element(schema, None, peel_lists(root_ty), &inner.outputs, |name| {
            name == inner.pref.child_key
        })
    };
    let grouped = matches!(element, ResultShape::Column(_) | ResultShape::Tuple(_))
        || layers[innermost].middle.is_some();
    let (first_creation, first_pref) = (layers[0].creation, layers[0].pref.clone());
    if grouped {
        element = wrap(element, &layers[innermost]);
    }
    let skip = usize::from(grouped);
    for layer in layers.iter().rev().skip(skip) {
        if let Some(parts) = layer.middle.as_ref() {
            element = wrap(middle_tuple(parts, Some(element)), layer);
        } else {
            element = wrap(element, layer);
        }
    }
    if root_dup && root_parts.is_none() {
        element = ResultShape::List {
            element: Box::new(element),
            child: ChildRef {
                stmt: first_creation,
                parent: Some(first_pref),
                cardinality: Cardinality::Many,
                tag: None,
            },
        };
    }
    match root_parts {
        Some(parts) => ResultShape::List {
            element: Box::new(middle_tuple(&parts, Some(element))),
            child: ChildRef {
                stmt: root_stmt,
                parent: None,
                cardinality: Cardinality::Many,
                tag: None,
            },
        },
        None => element,
    }
}

fn drain_pending_chain(
    ctx: &mut Context,
    pending: PendingSplit,
    tail_ty: &QueryType,
) -> (ResultShape, usize) {
    let hops = drain_pending(pending);
    let child_idx = ctx.statements.len() + hops.len() + 1;
    let layers = sink_chain(ctx, &hops, child_idx);
    (
        nest_layers(ctx.schema, layers, tail_ty, false, None, 0),
        child_idx,
    )
}

fn middle_tuple(parts: &[MiddlePart], sub: Option<ResultShape>) -> ResultShape {
    if parts.iter().any(|part| middle_name(part).is_some()) {
        ResultShape::Record(
            parts
                .iter()
                .map(|part| {
                    (
                        middle_name(part).cloned().unwrap_or_else(|| {
                            unreachable!(
                                "mixed named and unnamed middle positions: composite levels are uniformly tuples or records"
                            )
                        }),
                        middle_shape(part, &sub),
                    )
                })
                .collect(),
        )
    } else {
        ResultShape::Tuple(parts.iter().map(|part| middle_shape(part, &sub)).collect())
    }
}

fn middle_name(part: &MiddlePart) -> Option<&String> {
    match part {
        MiddlePart::Value { name, .. } | MiddlePart::Nested { name, .. } => name.as_ref(),
        MiddlePart::Tuple(_) => None,
        MiddlePart::Record { name, .. } => name.as_ref(),
    }
}

fn middle_shape(part: &MiddlePart, sub: &Option<ResultShape>) -> ResultShape {
    match part {
        MiddlePart::Value { col, .. } => ResultShape::Column(col.clone()),
        MiddlePart::Nested { .. } => sub.clone().unwrap_or_else(|| {
            unreachable!("composite slot without sub-shape: chained levels always accumulate one")
        }),
        MiddlePart::Tuple(inner) => middle_tuple(inner, sub.clone()),
        MiddlePart::Record { fields, .. } => middle_tuple(fields, sub.clone()),
    }
}

fn split_parent_shape(
    parent_q: &mut SelectBuilder,
    hops: &[PendingSplit],
    schema: &ValidatedSchema,
    root_parts: &Option<Vec<MiddlePart>>,
) -> StatementShape {
    if root_parts.is_some() {
        if parent_q.select.is_empty() {
            parent_q.select = vec![SelectItem::AllFrom { depth: 0 }];
        }
        let outputs = builder_output_types(parent_q, schema);
        StatementShape {
            columns: shape_columns(outputs, |name| name == hops[0].parent_col),
            rows: RowCount::Many,
        }
    } else {
        narrow_to_key(parent_q, &hops[0].parent_col, schema);
        StatementShape {
            columns: vec![OutputColumn {
                name: hops[0].parent_col.clone(),
                role: ColumnRole::Key,
                ty: None,
            }],
            rows: RowCount::Many,
        }
    }
}

fn narrow_to_key(builder: &mut SelectBuilder, col: &str, schema: &ValidatedSchema) {
    let ty = output_ty(builder, col, schema);
    builder.select = vec![SelectItem::Column {
        depth: 0,
        column: col.to_string(),
        output: col.to_string(),
        ty,
    }];
    for arm in &mut builder.unions {
        narrow_to_key(arm, col, schema);
    }
}

fn unkeyed_attach(element: ResultShape, stmt: usize) -> ResultShape {
    ResultShape::List {
        element: Box::new(element),
        child: ChildRef {
            stmt,
            parent: None,
            cardinality: Cardinality::Many,
            tag: None,
        },
    }
}

fn emit_split(
    ctx: &mut Context,
    parent_builder: SelectBuilder,
    pending: PendingSplit,
    root_ty: &QueryType,
    root_parts: Option<Vec<MiddlePart>>,
) -> RootPlan {
    let hops = drain_pending(pending);
    let mut parent_q = parent_builder;
    let parent_shape = split_parent_shape(&mut parent_q, &hops, ctx.schema, &root_parts);
    let layers = sink_chain(ctx, &hops, 0);
    let single = matches!(root_ty, QueryType::Optional(_));
    let nest = nest_layers(ctx.schema, layers, root_ty, !single, root_parts, 0);
    let result = if single {
        ResultShape::Single {
            element: Box::new(nest),
            child: ChildRef {
                stmt: 0,
                parent: None,
                cardinality: Cardinality::One,
                tag: None,
            },
        }
    } else {
        nest
    };
    RootPlan {
        builder: parent_q,
        shape: parent_shape,
        result,
    }
}

fn anchor_split(
    ctx: &mut Context,
    level_builder: SelectBuilder,
    pending: PendingSplit,
    ty: &QueryType,
    root_parts: Option<Vec<MiddlePart>>,
) -> ResultShape {
    let hops = drain_pending(pending);
    let mut parent_q = level_builder;
    let parent_shape = split_parent_shape(&mut parent_q, &hops, ctx.schema, &root_parts);
    let parent_idx = ctx.statements.len() + hops.len() + 1;
    let layers = sink_chain(ctx, &hops, parent_idx);
    let actual = ctx.sink(parent_q, parent_shape, StatementKind::Query);
    debug_assert_eq!(
        actual, parent_idx,
        "anchor parent sank at an unexpected index: sink order changed"
    );
    let inner = nest_layers(
        ctx.schema,
        layers,
        ty,
        false,
        root_parts.clone(),
        parent_idx,
    );
    if root_parts.is_some() {
        inner
    } else {
        unkeyed_attach(inner, parent_idx)
    }
}

fn builder_outputs(builder: &SelectBuilder, schema: &ValidatedSchema) -> Vec<String> {
    if builder.select.is_empty() && !builder.unions.is_empty() {
        return union_outputs(builder, schema);
    }
    builder
        .select
        .iter()
        .flat_map(|item| match item {
            SelectItem::AllFrom { .. } => from_columns(&builder.from, schema),
            SelectItem::Column { output, .. } | SelectItem::Expr { output, .. } => {
                vec![output.clone()]
            }
        })
        .collect()
}

fn union_outputs(builder: &SelectBuilder, schema: &ValidatedSchema) -> Vec<String> {
    builder
        .unions
        .first()
        .map(|first| builder_outputs(first, schema))
        .unwrap_or_else(|| {
            unreachable!("union outputs without arms: merges always push at least one arm")
        })
}

fn from_columns(from: &Option<FromClause>, schema: &ValidatedSchema) -> Vec<String> {
    match from {
        Some(FromClause::Table { table, .. }) => schema
            .tables
            .iter()
            .find(|t| &t.name == table)
            .unwrap_or_else(|| unreachable!("unknown table {table}"))
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect(),
        Some(FromClause::Subquery { query, .. }) => builder_outputs(query, schema),
        Some(FromClause::TableFunction { .. }) => vec!["value".to_string()],
        Some(FromClause::Temp { columns, .. }) => columns.iter().map(|c| c.name.clone()).collect(),
        Some(FromClause::Literal { columns, .. }) | Some(FromClause::Computed { columns, .. }) => {
            columns.iter().map(|c| c.name.clone()).collect()
        }
        Some(FromClause::Join { .. }) => {
            unreachable!("star expansion under a join: lanes select explicitly")
        }
        None => unreachable!("row pipeline without FROM"),
    }
}

fn scalar_of(ty: &QueryType) -> Option<ScalarType> {
    match ty {
        QueryType::Scalar(s) => Some(*s),
        QueryType::Optional(inner) => scalar_of(inner),
        _ => None,
    }
}

fn value_scalar(ty: &ValueType) -> Option<ScalarType> {
    match ty {
        ValueType::Scalar(s) => Some(*s),
        ValueType::Optional(inner) => value_scalar(inner),
        ValueType::Tuple(_) | ValueType::Array { .. } => None,
    }
}

fn builder_output_types(
    builder: &SelectBuilder,
    schema: &ValidatedSchema,
) -> Vec<(String, Option<ScalarType>)> {
    if builder.select.is_empty() && !builder.unions.is_empty() {
        return union_output_types(builder, schema);
    }
    builder
        .select
        .iter()
        .flat_map(|item| match item {
            SelectItem::AllFrom { .. } => from_columns_typed(&builder.from, schema),
            SelectItem::Column { output, ty, .. } | SelectItem::Expr { output, ty, .. } => {
                vec![(output.clone(), *ty)]
            }
        })
        .collect()
}

fn union_output_types(
    builder: &SelectBuilder,
    schema: &ValidatedSchema,
) -> Vec<(String, Option<ScalarType>)> {
    builder
        .unions
        .first()
        .map(|first| builder_output_types(first, schema))
        .unwrap_or_else(|| {
            unreachable!("union outputs without arms: merges always push at least one arm")
        })
}

fn from_columns_typed(
    from: &Option<FromClause>,
    schema: &ValidatedSchema,
) -> Vec<(String, Option<ScalarType>)> {
    match from {
        Some(FromClause::Table { table, .. }) => schema
            .tables
            .iter()
            .find(|t| &t.name == table)
            .unwrap_or_else(|| unreachable!("unknown table {table}"))
            .columns
            .iter()
            .map(|c| {
                (c.name.clone(), c.ty.or(Some(ScalarType::Int)))
            })
            .collect(),
        Some(FromClause::Subquery { query, .. }) => builder_output_types(query, schema),
        Some(FromClause::TableFunction { .. }) => {
            vec![("value".to_string(), Some(ScalarType::Int))]
        }
        Some(FromClause::Temp { columns, .. }) => {
            columns.iter().map(|c| (c.name.clone(), c.ty)).collect()
        }
        Some(FromClause::Literal { columns, .. }) | Some(FromClause::Computed { columns, .. }) => {
            columns.iter().map(|c| (c.name.clone(), c.ty)).collect()
        }
        Some(FromClause::Join { .. }) => {
            unreachable!("star expansion under a join: lanes select explicitly")
        }
        None => unreachable!("row pipeline without FROM"),
    }
}

fn struct_declares_column(schema: &ValidatedSchema, sid: StructId, name: &str) -> bool {
    schema.structs[sid.index()].fields.iter().any(|f| {
        matches!(f, Field::Value { columns, .. }
            if columns.iter().any(|c| schema.column_name(*c) == name))
    })
}

fn child_element(
    schema: &ValidatedSchema,
    output_shapes: Option<&Vec<(String, Option<ResultShape>)>>,
    element_ty: &QueryType,
    outputs: &[String],
    is_key: impl Fn(&str) -> bool,
) -> ResultShape {
    if let Some(fields) = output_shapes
        && !fields.is_empty()
    {
        return ResultShape::Record(
            fields
                .iter()
                .map(|(alias, shape)| {
                    (
                        alias.clone(),
                        shape
                            .clone()
                            .unwrap_or_else(|| ResultShape::Column(alias.clone())),
                    )
                })
                .collect(),
        );
    }
    let declared = match peel_optional(element_ty) {
        QueryType::Record(RecordSource::Schema(sid)) => Some(*sid),
        _ => None,
    };
    let kept: Vec<&String> = outputs
        .iter()
        .filter(|name| {
            name.as_str() != "$$rn"
                && name.as_str() != "$$abort"
                && name.as_str() != CROSS_PAIR_TAG
                && match declared {
                    Some(sid) => struct_declares_column(schema, sid, name),
                    None => !is_key(name),
                }
        })
        .collect();
    match peel_optional(element_ty) {
        QueryType::Scalar(_) if kept.len() == 1 => ResultShape::Column(kept[0].clone()),
        _ => {
            let mut cursor = kept.iter();
            let shape = mirror_shape(peel_optional(element_ty), &mut cursor);
            if shape.is_some() && cursor.next().is_none() {
                return shape.unwrap_or_else(|| {
                    unreachable!("mirror without shape: constructors always build one")
                });
            }
            debug_assert!(
                !matches!(
                    peel_optional(element_ty),
                    QueryType::Scalar(_)
                        | QueryType::Tuple(_)
                        | QueryType::Record(RecordSource::Projection(_))
                ),
                "composite element over {} kept columns: type/row mismatch",
                kept.len(),
            );
            ResultShape::Record(
                kept.into_iter()
                    .map(|name| (name.clone(), ResultShape::Column(name.clone())))
                    .collect(),
            )
        }
    }
}

fn mirror_shape<'a>(
    ty: &QueryType,
    cursor: &mut impl Iterator<Item = &'a &'a String>,
) -> Option<ResultShape> {
    match peel_optional(ty) {
        QueryType::Scalar(_) => cursor
            .next()
            .map(|name| ResultShape::Column((*name).clone())),
        QueryType::Tuple(tys) => tys
            .iter()
            .map(|ty| mirror_shape(ty, cursor))
            .collect::<Option<Vec<_>>>()
            .map(ResultShape::Tuple),
        QueryType::Record(RecordSource::Projection(fields)) => fields
            .iter()
            .map(|field| mirror_shape(&field.ty, cursor).map(|shape| (field.name.clone(), shape)))
            .collect::<Option<Vec<_>>>()
            .map(ResultShape::Record),
        _ => None,
    }
}

fn shape_columns(
    outputs: Vec<(String, Option<ScalarType>)>,
    is_key: impl Fn(&str) -> bool,
) -> Vec<OutputColumn> {
    outputs
        .into_iter()
        .map(|(name, ty)| {
            let key = is_key(&name);
            debug_assert!(
                key || ty.is_some(),
                "typeless value column {name}: lowering must thread its ScalarType"
            );
            OutputColumn {
                name,
                role: if key {
                    ColumnRole::Key
                } else {
                    ColumnRole::Value
                },
                ty: if key { None } else { ty },
            }
        })
        .collect()
}

enum ChainRoot<'a> {
    Enclosing,
    Global(Box<Level>),
    Collapsed(&'a TypedExpr),
}

fn scalar_field(
    ctx: &mut Context,
    base: &TypedExpr,
    name: &grove_types::Spanned<String>,
) -> SqlExpr {
    let mut hops: Vec<(StructId, FieldId)> = Vec::new();
    let mut cursor = base;
    let root = loop {
        match &cursor.kind {
            TypedExprKind::Field {
                base: next,
                name: hop,
                ..
            } => {
                if let Some(value) = struct_field_value(next, &hop.value) {
                    cursor = value;
                    continue;
                }
                let sid = match &cursor.ty {
                    QueryType::List(_) => record_struct(&next.ty),
                    _ => record_struct(&cursor.ty),
                };
                let (fid, field) = ctx.schema.struct_field(sid, &hop.value).unwrap_or_else(|| {
                    unreachable!("resolved field {} vanished from the schema", hop.value)
                });
                match field {
                    Field::Ref { .. } => {}
                    _ => unreachable!("value-typed hop {}: scalar results don't chain", hop.value),
                }
                hops.push((sid, fid));
                cursor = next;
            }
            TypedExprKind::Ident {
                binding: IdentBinding::Field(fid),
                ..
            } => {
                let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
                match field {
                    Field::Ref { .. } => {}
                    _ => unreachable!("value-typed chain root: fields of scalars don't exist"),
                }
                hops.push((fid.struct_id(), *fid));
                break ChainRoot::Enclosing;
            }
            TypedExprKind::Ident {
                binding: IdentBinding::ProjectedField { .. },
                ..
            } => todo!("M4 navigation over projected records"),
            TypedExprKind::Ident {
                binding: IdentBinding::Root { root_idx },
                ..
            } => {
                break ChainRoot::Global(Box::new(fresh_root_level(ctx, *root_idx, &cursor.ty)));
            }
            TypedExprKind::Ident { .. } => {
                unreachable!("non-field chain root in scalar demand")
            }
            _ => break ChainRoot::Collapsed(cursor),
        }
    };
    hops.reverse();

    let global_parent = match root {
        ChainRoot::Collapsed(base) if hops.is_empty() => {
            return single_hop_over_collapsed(ctx, base, name);
        }
        ChainRoot::Collapsed(base) => {
            let derived = single_row_query(ctx, base, vec![SelectItem::AllFrom { depth: 0 }]);
            Some(Box::new(Level {
                builder: SelectBuilder {
                    select: vec![SelectItem::AllFrom { depth: 0 }],
                    from: Some(FromClause::Subquery {
                        query: Box::new(derived),
                        alias: ctx.aliases.subquery(),
                    }),
                    where_: Vec::new(),
                    order_by: Vec::new(),
                    limit: None,
                    offset: None,
                    unions: Vec::new(),
                },
                element: base.ty.clone(),
                parent: ParentLink::Root { root_idx: 0 },
                pending: None,
                stmt_bound: false,
                output_shapes: None,
                root_parts: None,
            }))
        }
        ChainRoot::Enclosing => {
            if hops.is_empty() {
                unreachable!("ident chain root always contributes a hop");
            }
            None
        }
        ChainRoot::Global(parent) => {
            if hops.is_empty() {
                unreachable!("global chain root always contributes a hop");
            }
            Some(parent)
        }
    };
    let mut source = hops[0].0;
    let (_, first_fid) = hops[0];
    let (next_source, first_shell) = match global_parent {
        Some(parent) => global_first_hop(ctx, *parent, source, first_fid),
        None if ctx.temp_collect.is_some() && !ctx.in_array_temp => {
            temp_first_hop(ctx, source, first_fid)
        }
        None => hop_shell(ctx, source, first_fid),
    };
    source = next_source;
    let mut shells = vec![first_shell];
    for (_, fid) in &hops[1..] {
        let (next, shell) = hop_shell(ctx, source, *fid);
        shells.push(shell);
        source = next;
    }
    let (_, field) = ctx
        .schema
        .struct_field(source, &name.value)
        .unwrap_or_else(|| unreachable!("resolved field {} vanished from the schema", name.value));
    let final_col = match field {
        Field::Value { columns, ty, .. } => match columns.as_slice() {
            [column] => (
                ctx.schema.column_name(*column).to_string(),
                value_scalar(ty),
            ),
            _ => unreachable!(
                "tuple-valued navigation endpoint: scalar-demand chains ending at tuples contradict scalar result types (tuple field access is checker-rejected)"
            ),
        },
        Field::Ref { .. } => unreachable!(
            "single record in scalar value slot: scalar-demand chains ending at collections contradict scalar result types (list-typed links are checker-rejected)"
        ),
        Field::Array { .. } => unreachable!(
            "array in scalar value slot: scalar-demand chains ending at collections contradict scalar result types (list-typed links are checker-rejected)"
        ),
    };
    let (final_col, final_ty) = final_col;
    let mut inner = shells
        .pop()
        .unwrap_or_else(|| unreachable!("chain with no hops"));
    inner.select = vec![SelectItem::Column {
        depth: 0,
        column: final_col.clone(),
        output: final_col,
        ty: final_ty,
    }];
    while let Some(mut outer) = shells.pop() {
        let output = ctx.aliases.generic();
        outer.select = vec![SelectItem::Expr {
            expr: SqlExpr::Subquery {
                query: Box::new(inner),
            },
            output: output.clone(),
            ty: final_ty,
        }];
        inner = outer;
    }
    SqlExpr::Subquery {
        query: Box::new(inner),
    }
}

fn single_hop_over_collapsed(
    ctx: &mut Context,
    base: &TypedExpr,
    name: &grove_types::Spanned<String>,
) -> SqlExpr {
    if let TypedExprKind::Method {
        base: inner,
        name: m,
        ..
    } = &base.kind
        && m.value == "first"
        && (matches!(&inner.kind, TypedExprKind::Ident {
            binding: IdentBinding::Field(fid),
            ..
        } if is_collection_field(ctx, *fid))
            || (matches!(&inner.kind, TypedExprKind::Field { .. })
                && matches!(&inner.ty, QueryType::List(_))))
    {
        if ctx.in_take_arg {
            todo!("M4b temp-bound per-row limit");
        }
        return scalar_field(ctx, inner, name);
    }
    if let TypedExprKind::Method {
        base: inner,
        name: m,
        ..
    } = &base.kind
        && m.value == "first"
        && matches!(&inner.kind, TypedExprKind::Array { .. })
        && !has_scope_ident(inner)
        && let QueryType::Optional(o) = &base.ty
        && let QueryType::Record(RecordSource::Projection(fields)) = peel_optional(o)
    {
        let mut level = rows(ctx, inner);
        if level.pending.is_some() {
            todo!("M6 correlated literal projection item");
        }
        let served = from_columns(&level.builder.from, ctx.schema);
        let column = literal_field_col(&served, fields, &name.value);
        let column_ty = record_column_ty(ctx, &base.ty, &name.value);
        level.builder.select = vec![SelectItem::Column {
            depth: 0,
            column: column.clone(),
            output: column,
            ty: column_ty,
        }];
        compose_limit(&mut level.builder, "first", &[], ctx);
        return SqlExpr::Subquery {
            query: Box::new(level.builder),
        };
    }
    let column = record_column(ctx, &base.ty, &name.value);
    let column_ty = record_column_ty(ctx, &base.ty, &name.value);
    SqlExpr::Subquery {
        query: Box::new(single_row_query(
            ctx,
            base,
            vec![SelectItem::Column {
                depth: 0,
                column: column.clone(),
                output: column,
                ty: column_ty,
            }],
        )),
    }
}

fn is_collection_field(ctx: &Context, fid: FieldId) -> bool {
    match &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()] {
        Field::Ref { is_list, .. } => *is_list,
        Field::Array { .. } => true,
        Field::Value { .. } => false,
    }
}

fn has_row_setting_ident(expr: &TypedExpr) -> bool {
    any_ident(expr, &mut |e| {
        if let TypedExprKind::Ident {
            binding: IdentBinding::Field(_),
            ..
        } = &e.kind
        {
            !e.ty.is_scalar()
        } else {
            false
        }
    })
}

fn take_bound_ident(tail: &TypedExpr) -> bool {
    if let TypedExprKind::Method {
        base: _,
        args,
        name,
        ..
    } = &tail.kind
        && ["take", "skip", "nth"].contains(&name.value.as_str())
        && args.iter().any(|a| has_scope_ident(&a.expr))
    {
        return true;
    }
    match &tail.kind {
        TypedExprKind::Field { base, .. } => take_bound_ident(base),
        TypedExprKind::Method { base, args, .. } => {
            take_bound_ident(base) || args.iter().any(|a| take_bound_ident(&a.expr))
        }
        TypedExprKind::Projection { base, items } => {
            take_bound_ident(base) || items.iter().any(|i| take_bound_ident(&i.value))
        }
        TypedExprKind::Binary { lhs, rhs, .. } => take_bound_ident(lhs) || take_bound_ident(rhs),
        TypedExprKind::Unary { expr, .. } | TypedExprKind::Cast { expr, .. } => {
            take_bound_ident(expr)
        }
        TypedExprKind::If { arms, default } => {
            arms.iter()
                .any(|(c, t)| take_bound_ident(c) || take_bound_ident(t))
                || take_bound_ident(default)
        }
        TypedExprKind::Some { value } => take_bound_ident(value),
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            elements.iter().any(take_bound_ident)
        }
        TypedExprKind::Struct { fields } => fields.iter().any(|(_, e)| take_bound_ident(e)),
        TypedExprKind::Ident { .. }
        | TypedExprKind::Literal(_)
        | TypedExprKind::TypeConstant { .. } => false,
    }
}

fn nth_temp_routable(tail: &TypedExpr) -> bool {
    let TypedExprKind::Method {
        base, name, args, ..
    } = &tail.kind
    else {
        return false;
    };
    if name.value != "nth" {
        return false;
    }
    if !matches!(
        &base.kind,
        TypedExprKind::Ident {
            binding: IdentBinding::Field(_),
            ..
        }
    ) {
        return false;
    }
    let [arg] = args.as_slice() else {
        return false;
    };
    if !matches!(&arg.expr.ty, QueryType::Scalar(ScalarType::Int)) {
        return false;
    }
    !any_ident(
        &arg.expr,
        &mut |e| matches!(&e.kind, TypedExprKind::Ident { depth, .. } if *depth > 1),
    )
}

fn has_scope_ident(expr: &TypedExpr) -> bool {
    any_ident(expr, &mut |e| {
        matches!(
            &e.kind,
            TypedExprKind::Ident {
                binding: IdentBinding::Field(_) | IdentBinding::ProjectedField { .. },
                ..
            }
        )
    })
}

fn uses_outer(expr: &TypedExpr) -> bool {
    any_ident(
        expr,
        &mut |e| matches!(&e.kind, TypedExprKind::Ident { depth, .. } if *depth > 0),
    )
}

fn tail_uses_projected_outer(tail: &TypedExpr) -> bool {
    any_ident(tail, &mut |e| {
        matches!(
            &e.kind,
            TypedExprKind::Ident {
                binding: IdentBinding::ProjectedField { .. },
                depth,
                ..
            } if *depth > 0
        )
    })
}

fn tail_uses_outer(tail: &TypedExpr) -> bool {
    let mut bottom: Option<*const TypedExpr> = None;
    let mut node = tail;
    loop {
        match &node.kind {
            TypedExprKind::Field { base, .. }
            | TypedExprKind::Method { base, .. }
            | TypedExprKind::Projection { base, .. } => node = base,
            TypedExprKind::Ident { .. } => {
                bottom = Some(node as *const TypedExpr);
                break;
            }
            _ => break,
        }
    }
    any_ident(tail, &mut |e| {
        let is_bottom = bottom.is_some_and(|b| std::ptr::eq(b, e as *const TypedExpr));
        !is_bottom && matches!(&e.kind, TypedExprKind::Ident { depth, .. } if *depth > 0)
    })
}

fn any_ident(expr: &TypedExpr, pred: &mut impl FnMut(&TypedExpr) -> bool) -> bool {
    match &expr.kind {
        TypedExprKind::Ident { .. } => pred(expr),
        TypedExprKind::Literal(_) | TypedExprKind::TypeConstant { .. } => false,
        TypedExprKind::Field { base, .. } => any_ident(base, pred),
        TypedExprKind::Method { base, args, .. } => {
            any_ident(base, pred) || args.iter().any(|a| any_ident(&a.expr, pred))
        }
        TypedExprKind::Binary { lhs, rhs, .. } => any_ident(lhs, pred) || any_ident(rhs, pred),
        TypedExprKind::Unary { expr, .. } | TypedExprKind::Cast { expr, .. } => {
            any_ident(expr, pred)
        }
        TypedExprKind::If { arms, default } => {
            arms.iter()
                .any(|(c, t)| any_ident(c, pred) || any_ident(t, pred))
                || any_ident(default, pred)
        }
        TypedExprKind::Some { value } => any_ident(value, pred),
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            elements.iter().any(|e| any_ident(e, pred))
        }
        TypedExprKind::Struct { fields } => fields.iter().any(|(_, e)| any_ident(e, pred)),
        TypedExprKind::Projection { base, items } => {
            any_ident(base, pred) || items.iter().any(|i| any_ident(&i.value, pred))
        }
    }
}

fn single_row_query(ctx: &mut Context, base: &TypedExpr, select: Vec<SelectItem>) -> SelectBuilder {
    let corr = has_row_setting_ident(base) || tail_uses_outer(base);
    if corr && ctx.in_take_arg {
        todo!("M4b temp-bound per-row limit");
    }
    let was_corr = std::mem::replace(&mut ctx.corr, corr);
    let was_outer = std::mem::replace(&mut ctx.no_outer, !corr);
    let mut level = if corr {
        match reanchored_level(ctx, base) {
            Some(level) => level,
            None => rows(ctx, base),
        }
    } else {
        rows(ctx, base)
    };
    ctx.corr = was_corr;
    ctx.no_outer = was_outer;
    level.stmt_bound = false;
    while let Some(pending) = level.pending.take() {
        let PendingSplit {
            child_from,
            child_col,
            parent_col,
            next,
            ..
        } = pending;
        let floor = nest_floor(&level.builder.from);
        level.builder = shift_nest_builder(level.builder, floor);
        level = merge_navigate(ctx, child_from, child_col, level, parent_col);
        level.pending = next.map(|boxed| *boxed);
    }
    if !level.builder.select.is_empty() || !level.builder.unions.is_empty() {
        level = wrap_level(ctx, level);
    }
    level.builder.select = select;
    level.builder
}

fn record_column(ctx: &Context, ty: &QueryType, name: &str) -> String {
    match ty {
        QueryType::Record(RecordSource::Schema(sid)) => {
            let (_, field) = ctx
                .schema
                .struct_field(*sid, name)
                .unwrap_or_else(|| unreachable!("resolved field {name} vanished from the schema"));
            match field {
                Field::Value { columns, .. } => match columns.as_slice() {
                    [column] => ctx.schema.column_name(*column).to_string(),
                    _ => unreachable!(
                        "tuple-valued field access: scalar-demand chains ending at tuples contradict scalar result types (tuple field access is checker-rejected)"
                    ),
                },
                Field::Ref { .. } => unreachable!(
                    "single record in scalar value slot: scalar-demand chains ending at collections contradict scalar result types (list-typed links are checker-rejected)"
                ),
                Field::Array { .. } => unreachable!(
                    "array in scalar value slot: scalar-demand chains ending at collections contradict scalar result types (list-typed links are checker-rejected)"
                ),
            }
        }
        QueryType::Record(RecordSource::Projection(_)) => name.to_string(),
        QueryType::Optional(inner) => record_column(ctx, inner, name),
        other => unreachable!("field access over {other}: the checker only admits records"),
    }
}

fn record_column_ty(ctx: &Context, ty: &QueryType, name: &str) -> Option<ScalarType> {
    match ty {
        QueryType::Record(RecordSource::Schema(sid)) => {
            let (_, field) = ctx
                .schema
                .struct_field(*sid, name)
                .unwrap_or_else(|| unreachable!("resolved field {name} vanished from the schema"));
            match field {
                Field::Value { ty, .. } => value_scalar(ty),
                Field::Ref { .. } | Field::Array { .. } => None,
            }
        }
        QueryType::Record(RecordSource::Projection(fields)) => fields
            .iter()
            .find(|f| f.name == name)
            .and_then(|f| scalar_of(&f.ty)),
        QueryType::Optional(inner) => record_column_ty(ctx, inner, name),
        _ => None,
    }
}

fn record_struct(ty: &QueryType) -> StructId {
    match ty {
        QueryType::Record(RecordSource::Schema(sid)) => *sid,
        QueryType::Optional(inner) => record_struct(inner),
        QueryType::Record(RecordSource::Projection(_)) => {
            todo!("M4 navigation over projected records")
        }
        other => unreachable!("chain link over {other}: the checker only admits records"),
    }
}

enum LinkCols {
    ParentFk { parent_fk: String, child_pk: String },
    TargetFk { child_fk: String, parent_pk: String },
    ManyToMany,
}

fn link_cols(schema: &ValidatedSchema, fid: FieldId) -> LinkCols {
    let relation = &schema.relations[schema
        .relation_of_field(fid)
        .unwrap_or_else(|| unreachable!("reference field without relation"))
        .index()];
    let name = |c| schema.column_name(c).to_string();
    match relation {
        Relation::OneToOne {
            child_ref,
            parent_ref,
            fk,
            pk,
        }
        | Relation::ManyToOne {
            child_ref,
            parent_ref,
            fk,
            pk,
        } => {
            if fid == *child_ref {
                LinkCols::ParentFk {
                    parent_fk: name(*fk),
                    child_pk: name(*pk),
                }
            } else if fid == *parent_ref {
                LinkCols::TargetFk {
                    child_fk: name(*fk),
                    parent_pk: name(*pk),
                }
            } else {
                unreachable!("hop field is not an endpoint of its relation")
            }
        }
        Relation::OneToMany {
            a_ref,
            b_ref,
            fk,
            pk,
        } => {
            if fid == *b_ref {
                LinkCols::ParentFk {
                    parent_fk: name(*fk),
                    child_pk: name(*pk),
                }
            } else if fid == *a_ref {
                LinkCols::TargetFk {
                    child_fk: name(*fk),
                    parent_pk: name(*pk),
                }
            } else {
                unreachable!("hop field is not an endpoint of its relation")
            }
        }
        Relation::ManyToMany { .. } => LinkCols::ManyToMany,
    }
}

fn hop_shell(ctx: &mut Context, source: StructId, fid: FieldId) -> (StructId, SelectBuilder) {
    let field = &ctx.schema.structs[source.index()].fields[fid.local()];
    let target = match field {
        Field::Ref { target, .. } => *target,
        _ => unreachable!("hop over non-reference field"),
    };
    let (child_col, parent_col) = hop_link_cols(ctx.schema, fid);
    let parent_rhs = SqlExpr::Column {
        depth: 1,
        column: parent_col,
    };
    (target, hop_shell_on(child_col, parent_rhs, ctx, target))
}

fn temp_first_hop(ctx: &mut Context, source: StructId, fid: FieldId) -> (StructId, SelectBuilder) {
    let field = &ctx.schema.structs[source.index()].fields[fid.local()];
    let target = match field {
        Field::Ref { target, .. } => *target,
        _ => unreachable!("hop over non-reference field"),
    };
    let (temp, child_fk) = ctx
        .temp_collect
        .as_ref()
        .map(|c| (c.temp.clone(), c.child_fk.clone()))
        .unwrap_or_else(|| unreachable!("temp first hop outside temp mode"));
    let (child_col, parent_col) = hop_link_cols(ctx.schema, fid);
    record_temp_ref(ctx, &parent_col);
    let parent_rhs = temp_lookup_at(ctx, &temp, &parent_col, &child_fk, 2);
    (target, hop_shell_on(child_col, parent_rhs, ctx, target))
}

fn hop_link_cols(schema: &ValidatedSchema, fid: FieldId) -> (String, String) {
    match link_cols(schema, fid) {
        LinkCols::ParentFk {
            parent_fk,
            child_pk,
        } => (child_pk, parent_fk),
        LinkCols::TargetFk {
            child_fk,
            parent_pk,
        } => (child_fk, parent_pk),
        LinkCols::ManyToMany => {
            todo!("M4 many-to-many navigation");
        }
    }
}
fn hop_shell_on(
    child_col: String,
    parent_rhs: SqlExpr,
    ctx: &mut Context,
    target: StructId,
) -> SelectBuilder {
    let table = ctx.schema.tables[ctx.schema.structs[target.index()].table.index()]
        .name
        .clone();
    let mut shell = SelectBuilder::new();
    shell.from = Some(FromClause::Table {
        table: table.clone(),
        alias: ctx.aliases.table(&table),
    });
    shell.where_ = vec![SqlExpr::Binary {
        op: SqlBinOp::Eq,
        lhs: Box::new(SqlExpr::Column {
            depth: 0,
            column: child_col,
        }),
        rhs: Box::new(parent_rhs),
    }];
    shell
}

fn scalar_literal(lit: &Literal) -> SqlExpr {
    match lit {
        Literal::Int(n) => SqlExpr::Param(ParamValue::Int(*n)),
        Literal::Float(f) => SqlExpr::Param(ParamValue::Float(*f)),
        Literal::Dec(d) => SqlExpr::Param(ParamValue::Dec(*d)),
        Literal::String(s) => SqlExpr::Param(ParamValue::String(s.clone())),
        Literal::Bool(b) => SqlExpr::Param(ParamValue::Bool(*b)),
        Literal::Instant(t) => SqlExpr::Param(ParamValue::Instant(*t)),
        Literal::Duration(d) => SqlExpr::Param(ParamValue::Duration(*d)),
        Literal::Now => SqlExpr::Func {
            name: "$$now".to_string(),
            args: vec![],
        },
        Literal::Today(None) => SqlExpr::Func {
            name: "$$today".to_string(),
            args: vec![SqlExpr::Param(ParamValue::Null)],
        },
        Literal::Today(Some(time)) => {
            let midnight =
                chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("midnight is a valid time");
            SqlExpr::Func {
                name: "$$today".to_string(),
                args: vec![SqlExpr::Param(ParamValue::Duration(
                    time.signed_duration_since(midnight),
                ))],
            }
        }
        Literal::None => SqlExpr::Param(ParamValue::Null),
    }
}

fn scalar_constant(ty: TypeName, name: ConstantName) -> SqlExpr {
    SqlExpr::Param(const_value(ty, name))
}

fn scalar_ident(ctx: &mut Context, binding: &IdentBinding, depth: usize) -> SqlExpr {
    if ctx.in_take_arg
        && matches!(
            binding,
            IdentBinding::Field(_) | IdentBinding::ProjectedField { .. }
        )
    {
        if ctx.temp_collect.is_some() {
            return scalar_ident_temp(ctx, binding);
        }
        todo!("M4b temp-bound per-row limit");
    }
    if depth == 0 && ctx.in_array_temp {
        match binding {
            IdentBinding::Field(fid) => {
                let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
                match field {
                    Field::Value { columns, .. } => match columns.as_slice() {
                        [col] => {
                            let column = ctx.schema.column_name(*col).to_string();
                            record_temp_ref(ctx, &column);
                            return SqlExpr::Column { depth: 0, column };
                        }
                        _ => unreachable!(
                            "tuple field ident in temp array: readability gates tuple-typed idents"
                        ),
                    },
                    _ => unreachable!(
                        "non-value field ident in temp array: readability gates non-scalar idents"
                    ),
                }
            }
            IdentBinding::ProjectedField { name } => {
                record_temp_ref(ctx, name);
                return SqlExpr::Column {
                    depth: 0,
                    column: name.clone(),
                };
            }
            _ => {}
        }
    }
    if depth > 0 {
        if ctx.temp_collect.is_some() {
            return scalar_ident_temp(ctx, binding);
        }
        if ctx.no_outer {
            todo!("M4b correlated reference inside keyed child");
        }
    }
    match binding {
        IdentBinding::Root { .. } => {
            unreachable!("root idents are List-typed: scalar() only receives scalar inputs")
        }
        IdentBinding::Field(fid) => {
            let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
            match field {
                Field::Value { columns, .. } => {
                    let [column] = columns.as_slice() else {
                        unreachable!(
                            "tuple-valued field ident in scalar demand: callers dispatch tuple nodes by type"
                        )
                    };
                    SqlExpr::Column {
                        depth,
                        column: ctx.schema.column_name(*column).to_string(),
                    }
                }
                _ => {
                    unreachable!("reference/array field idents are never scalar-typed")
                }
            }
        }
        IdentBinding::ProjectedField { name } => SqlExpr::Column {
            depth,
            column: name.clone(),
        },
        IdentBinding::Prev { .. } => todo!("mutations era: prev"),
    }
}

fn collapse_parks_split(mut node: &TypedExpr) -> bool {
    loop {
        if matches!(
            peel_optional(&node.ty),
            QueryType::List(inner) if matches!(peel_optional(inner), QueryType::List(_))
        ) {
            return true;
        }
        match &node.kind {
            TypedExprKind::Method { base, name, .. }
                if [
                    "first",
                    "nth",
                    "take",
                    "skip",
                    "sort",
                    "sort_asc",
                    "sort_desc",
                    "filter",
                    "flatten",
                ]
                .contains(&name.value.as_str()) =>
            {
                node = base;
            }
            TypedExprKind::Field { base, .. } => {
                node = base;
            }
            _ => return false,
        }
    }
}

enum AggOp {
    Len,
    Bare(BareAgg, ScalarType),
    Over(OverAgg, ScalarType, SqlExpr),
}

enum BareAgg {
    Sum,
    Avg,
    Max,
    Min,
}

enum OverAgg {
    Sum,
    Avg,
}

fn inner_scalar(inner: &QueryType, method: &str) -> ScalarType {
    match inner {
        QueryType::Scalar(s) => *s,
        _ => unreachable!("{method} over non-scalar elements: the checker restricts inners"),
    }
}

fn over_arg(
    ctx: &mut Context,
    args: &[TypedMethodArg],
    name: &grove_types::Spanned<String>,
) -> (ScalarType, SqlExpr) {
    let arg = one_arg(args, name);
    let ty = match &arg.ty {
        QueryType::Scalar(s) => *s,
        _ => unreachable!("summable scoped arg is scalar: the checker enforces it"),
    };
    if has_field_node(arg) {
        if uses_outer(arg) {
            todo!("M4b scoped navigation");
        }
    }
    (ty, scalar(ctx, arg))
}

fn has_field_node(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::Field { .. } => true,
        TypedExprKind::Method { base, args, .. } => {
            has_field_node(base) || args.iter().any(|a| has_field_node(&a.expr))
        }
        TypedExprKind::Binary { lhs, rhs, .. } => has_field_node(lhs) || has_field_node(rhs),
        TypedExprKind::Unary { expr, .. } | TypedExprKind::Cast { expr, .. } => {
            has_field_node(expr)
        }
        TypedExprKind::If { arms, default } => {
            arms.iter()
                .any(|(c, t)| has_field_node(c) || has_field_node(t))
                || has_field_node(default)
        }
        TypedExprKind::Some { value } => has_field_node(value),
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            elements.iter().any(has_field_node)
        }
        TypedExprKind::Struct { fields } => fields.iter().any(|(_, e)| has_field_node(e)),
        TypedExprKind::Projection { base, items } => {
            has_field_node(base) || items.iter().any(|i| has_field_node(&i.value))
        }
        _ => false,
    }
}

fn scalar_list_col(ctx: &mut Context, base: &TypedExpr) -> String {
    match &base.kind {
        TypedExprKind::Ident {
            binding: IdentBinding::Field(fid),
            ..
        } => {
            let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
            match field {
                Field::Value { columns, .. } => match columns.as_slice() {
                    [col] => ctx.schema.column_name(*col).to_string(),
                    _ => unreachable!("tuple field in scalar aggregation: tuples aren't summable"),
                },
                Field::Array {
                    element, storage, ..
                } => match element {
                    grove_schema::validated::ValueType::Scalar(_) => match storage.value.as_slice()
                    {
                        [col] => ctx.schema.column_name(*col).to_string(),
                        _ => unreachable!(
                            "multi-column via array in aggregation: via clauses carry exactly key+value columns"
                        ),
                    },
                    _ => unreachable!(
                        "nested via array in aggregation: reductions require summable scalar inners, which nested inners never satisfy"
                    ),
                },
                _ => unreachable!("reference ident in scalar aggregation: records aren't summable"),
            }
        }
        TypedExprKind::Field { base, name, .. } => match &base.ty {
            QueryType::List(inner) => match inner.as_ref() {
                QueryType::Record(RecordSource::Schema(struct_id)) => {
                    match ctx.schema.struct_field(*struct_id, &name.value) {
                        Some((_, Field::Value { columns, .. }))
                            if matches!(columns.as_slice(), [_]) =>
                        {
                            name.value.clone()
                        }
                        _ => todo!("M4b scalar aggregation over derived rows"),
                    }
                }
                QueryType::Record(RecordSource::Projection(fields)) => {
                    match fields.iter().find(|f| f.name == name.value) {
                        Some(field) if field.ty.is_scalar() => name.value.clone(),
                        _ => todo!("M4b scalar aggregation over derived rows"),
                    }
                }
                _ => todo!("M4b scalar aggregation over derived rows"),
            },
            _ => todo!("M4b scalar aggregation over derived rows"),
        },
        TypedExprKind::Method { base, name, .. }
            if ["take", "skip", "sort", "sort_asc", "sort_desc"].contains(&name.value.as_str()) =>
        {
            scalar_list_col(ctx, base)
        }
        TypedExprKind::Method { base, name, .. } if name.value == "flatten" => match &base.kind {
            TypedExprKind::Array { .. } | TypedExprKind::Method { .. } => "c0".to_string(),
            _ => unreachable!(
                "flatten over non-mergeable rows in aggregation: only array-rooted pipelines flatten to scalar lists"
            ),
        },
        TypedExprKind::Array { .. } => match &base.ty {
            QueryType::List(inner) if matches!(peel_optional(inner), QueryType::Scalar(_)) => {
                "c0".to_string()
            }
            _ => unreachable!(
                "reduction over non-scalar literal {}: the checker restricts sum/avg/max/min to scalar inners",
                base.ty
            ),
        },
        TypedExprKind::If { .. } => "c0".to_string(),
        _ => todo!("M4b scalar aggregation over derived rows"),
    }
}

fn agg_call(agg: &AggOp, col: impl FnOnce() -> SqlExpr) -> SqlExpr {
    match agg {
        AggOp::Len => SqlExpr::Count,
        AggOp::Bare(BareAgg::Sum, ScalarType::Int)
        | AggOp::Bare(BareAgg::Sum, ScalarType::Float) => SqlExpr::Func {
            name: "SUM".to_string(),
            args: vec![col()],
        },
        AggOp::Bare(BareAgg::Sum, ScalarType::Dec) => SqlExpr::Func {
            name: "$$sum_dec".to_string(),
            args: vec![col()],
        },
        AggOp::Bare(BareAgg::Sum, ScalarType::Duration) => SqlExpr::Func {
            name: "$$sum_dur".to_string(),
            args: vec![col()],
        },
        AggOp::Bare(BareAgg::Avg, ScalarType::Int)
        | AggOp::Bare(BareAgg::Avg, ScalarType::Float) => SqlExpr::Func {
            name: "AVG".to_string(),
            args: vec![col()],
        },
        AggOp::Bare(BareAgg::Avg, ScalarType::Dec) => SqlExpr::Func {
            name: "$$avg_dec".to_string(),
            args: vec![col()],
        },
        AggOp::Bare(BareAgg::Avg, ScalarType::Duration) => SqlExpr::Func {
            name: "$$avg_dur".to_string(),
            args: vec![col()],
        },
        AggOp::Bare(BareAgg::Avg, ScalarType::Instant) => SqlExpr::Func {
            name: "$$avg_instant".to_string(),
            args: vec![col()],
        },
        AggOp::Bare(BareAgg::Max, _) | AggOp::Bare(BareAgg::Min, _) => {
            let func = match agg {
                AggOp::Bare(BareAgg::Max, _) => "MAX",
                _ => "MIN",
            };
            SqlExpr::Func {
                name: func.to_string(),
                args: vec![col()],
            }
        }
        AggOp::Over(OverAgg::Sum, ScalarType::Int, arg)
        | AggOp::Over(OverAgg::Sum, ScalarType::Float, arg) => SqlExpr::Func {
            name: "SUM".to_string(),
            args: vec![arg.clone()],
        },
        AggOp::Over(OverAgg::Sum, ScalarType::Dec, arg) => SqlExpr::Func {
            name: "$$sum_dec".to_string(),
            args: vec![arg.clone()],
        },
        AggOp::Over(OverAgg::Sum, ScalarType::Duration, arg) => SqlExpr::Func {
            name: "$$sum_dur".to_string(),
            args: vec![arg.clone()],
        },
        AggOp::Over(OverAgg::Avg, ScalarType::Int, arg)
        | AggOp::Over(OverAgg::Avg, ScalarType::Float, arg) => SqlExpr::Func {
            name: "AVG".to_string(),
            args: vec![arg.clone()],
        },
        AggOp::Over(OverAgg::Avg, ScalarType::Dec, arg) => SqlExpr::Func {
            name: "$$avg_dec".to_string(),
            args: vec![arg.clone()],
        },
        AggOp::Over(OverAgg::Avg, ScalarType::Duration, arg) => SqlExpr::Func {
            name: "$$avg_dur".to_string(),
            args: vec![arg.clone()],
        },
        AggOp::Over(OverAgg::Avg, ScalarType::Instant, arg) => SqlExpr::Func {
            name: "$$avg_instant".to_string(),
            args: vec![arg.clone()],
        },
        _ => unreachable!(
            "rejected input reaches aggregation: the checker restricts bases and inners"
        ),
    }
}

fn reanchored_level(ctx: &mut Context, base: &TypedExpr) -> Option<Level> {
    let tail = ctx.child.as_ref()?;
    if tail.base.is_some() {
        return None;
    }
    let anchor = tail.anchor?;
    let cfid = match &base.kind {
        TypedExprKind::Ident {
            binding: IdentBinding::Field(fid),
            depth,
            ..
        } if *depth > 0 => *fid,
        _ => return None,
    };
    let outer = row_struct(&base.ty)?;
    let rel_id = ctx.schema.relation_of_field(cfid)?;
    let relation = &ctx.schema.relations[rel_id.index()];
    let (c_table, c_col, pk) = match relation {
        Relation::OneToOne { fk, pk, .. }
        | Relation::ManyToOne { fk, pk, .. }
        | Relation::OneToMany { fk, pk, .. } => (
            ctx.schema.tables[fk.table_id().index()].name.clone(),
            ctx.schema.column_name(*fk).to_string(),
            *pk,
        ),
        Relation::ManyToMany { .. } => return None,
    };
    let parent_col = if anchor == outer {
        ctx.schema.column_name(pk).to_string()
    } else {
        let mut refs = ctx.schema.structs[anchor.index()]
            .fields
            .iter()
            .enumerate()
            .filter_map(|(local, f)| match f {
                Field::Ref {
                    target, is_list, ..
                } if *target == outer && !is_list => Some(FieldId::new(anchor, local)),
                _ => None,
            });
        let unique = match (refs.next(), refs.next()) {
            (Some(fid), None) => fid,
            _ => return None,
        };
        match link_cols(ctx.schema, unique) {
            LinkCols::ParentFk { parent_fk, .. } => parent_fk,
            _ => return None,
        }
    };
    let data_alias = ctx.aliases.table(&c_table);
    let mut builder = SelectBuilder::new();
    builder.from = Some(FromClause::Table {
        table: c_table,
        alias: data_alias,
    });
    builder.where_ = vec![SqlExpr::Binary {
        op: SqlBinOp::Eq,
        lhs: Box::new(SqlExpr::Column {
            depth: 0,
            column: c_col,
        }),
        rhs: Box::new(SqlExpr::Column {
            depth: 1,
            column: parent_col,
        }),
    }];
    Some(Level {
        builder,
        element: base.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
        root_parts: None,
    })
}

fn scalar_aggregate(ctx: &mut Context, base: &TypedExpr, agg: AggOp) -> SqlExpr {
    let corr = has_row_setting_ident(base) || tail_uses_outer(base);
    if corr && ctx.in_take_arg {
        todo!("M4b temp-bound per-row limit");
    }
    let was_corr = std::mem::replace(&mut ctx.corr, corr);
    let was_outer = std::mem::replace(&mut ctx.no_outer, !corr);
    let mut level = if corr {
        match reanchored_level(ctx, base) {
            Some(level) => level,
            None => rows(ctx, base),
        }
    } else {
        let was_child = ctx.child.take();
        let level = rows(ctx, base);
        ctx.child = was_child;
        level
    };
    ctx.corr = was_corr;
    ctx.no_outer = was_outer;
    if let Some(pending) = level.pending {
        let _ = pending;
        match agg {
            AggOp::Len => {}
            _ => {
                unreachable!("reduction over nested lists is rejected upstream (len() excepted)")
            }
        }
        return SqlExpr::Subquery {
            query: Box::new(SelectBuilder {
                select: vec![SelectItem::Expr {
                    expr: SqlExpr::Count,
                    output: ctx.aliases.generic(),
                    ty: Some(ScalarType::Int),
                }],
                from: level.builder.from,
                where_: level.builder.where_,
                order_by: level.builder.order_by,
                limit: level.builder.limit,
                offset: level.builder.offset,
                unions: Vec::new(),
            }),
        };
    }
    if !level.builder.select.is_empty() || !level.builder.unions.is_empty() {
        level = wrap_level(ctx, level);
    }
    let output = ctx.aliases.generic();
    let result_ty: Option<ScalarType> = Some(match &agg {
        AggOp::Len => ScalarType::Int,
        AggOp::Bare(op, s) => match op {
            BareAgg::Sum => *s,
            BareAgg::Avg => match s {
                ScalarType::Int | ScalarType::Float => ScalarType::Float,
                _ => *s,
            },
            BareAgg::Max | BareAgg::Min => *s,
        },
        AggOp::Over(op, s, _) => match op {
            OverAgg::Sum => *s,
            OverAgg::Avg => match s {
                ScalarType::Int | ScalarType::Float => ScalarType::Float,
                _ => *s,
            },
        },
    });
    let select = |expr| SelectItem::Expr {
        expr,
        output: output.clone(),
        ty: result_ty,
    };
    let expr = agg_call(&agg, || SqlExpr::Column {
        depth: 0,
        column: scalar_list_col(ctx, base),
    });
    SqlExpr::Subquery {
        query: Box::new(SelectBuilder {
            select: vec![select(expr)],
            from: level.builder.from,
            where_: level.builder.where_,
            order_by: level.builder.order_by,
            limit: level.builder.limit,
            offset: level.builder.offset,
            unions: Vec::new(),
        }),
    }
}

fn scalar_method(
    ctx: &mut Context,
    base: &TypedExpr,
    name: &grove_types::Spanned<String>,
    args: &[TypedMethodArg],
    optional: bool,
) -> SqlExpr {
    let _ = optional;
    match &base.ty {
        QueryType::Scalar(s) => {
            let frag = scalar(ctx, base);
            apply_scalar_method(ctx, frag, *s, name, args)
        }
        QueryType::Optional(inner) => match inner.as_ref() {
            QueryType::Scalar(s) => {
                let frag = scalar(ctx, base);
                match name.value.as_str() {
                    "is_none" => SqlExpr::Binary {
                        op: SqlBinOp::Is,
                        lhs: Box::new(frag),
                        rhs: Box::new(SqlExpr::Param(ParamValue::Null)),
                    },
                    "is_some" => SqlExpr::Binary {
                        op: SqlBinOp::IsNot,
                        lhs: Box::new(frag),
                        rhs: Box::new(SqlExpr::Param(ParamValue::Null)),
                    },
                    "unwrap_or" => SqlExpr::Coalesce(vec![frag, scalar(ctx, one_arg(args, name))]),
                    "unwrap" => SqlExpr::Coalesce(vec![
                        frag,
                        SqlExpr::Func {
                            name: "$$panic".to_string(),
                            args: vec![SqlExpr::Param(ParamValue::String(
                                "unwrap failed: value was none".to_string(),
                            ))],
                        },
                    ]),
                    _ => apply_scalar_method(ctx, frag, *s, name, args),
                }
            }
            QueryType::Record(_) => {
                match name.value.as_str() {
                    "is_some" => SqlExpr::Exists(Box::new(single_row_query(
                        ctx,
                        base,
                        vec![SelectItem::AllFrom { depth: 0 }],
                    ))),
                    "is_none" => SqlExpr::Not(Box::new(SqlExpr::Exists(Box::new(
                        single_row_query(ctx, base, vec![SelectItem::AllFrom { depth: 0 }]),
                    )))),
                    "unwrap" | "unwrap_or" => {
                        todo!("M4 presence-directed unwrap over records")
                    }
                    _ => unreachable!("no other methods on ?Record: the checker rejects them"),
                }
            }
            QueryType::List(inner) => match name.value.as_str() {
                "contains" if matches!(inner.as_ref(), QueryType::List(_)) => {
                    todo!("H1 QT0028: membership over nested lists")
                }
                "is_some" => {
                    if collapse_parks_split(base) {
                        todo!("M4 list-optional mechanics");
                    }
                    SqlExpr::Exists(Box::new(single_row_query(
                        ctx,
                        base,
                        vec![SelectItem::AllFrom { depth: 0 }],
                    )))
                }
                "is_none" => {
                    if collapse_parks_split(base) {
                        todo!("M4 list-optional mechanics");
                    }
                    SqlExpr::Not(Box::new(SqlExpr::Exists(Box::new(single_row_query(
                        ctx,
                        base,
                        vec![SelectItem::AllFrom { depth: 0 }],
                    )))))
                }
                _ => todo!("M4 list-optional mechanics"),
            },
            _ => unreachable!(
                "method on optional tuple: Optional(Tuple) node types never arise — wrap_optional maps tuples element-wise, Some-of-tuple is rejected by the final check (QT0027)"
            ),
        },
        QueryType::List(inner) => match name.value.as_str() {
            "len" => scalar_aggregate(ctx, base, AggOp::Len),
            "sum" => scalar_aggregate(
                ctx,
                base,
                AggOp::Bare(BareAgg::Sum, inner_scalar(inner, "sum")),
            ),
            "avg" => scalar_aggregate(
                ctx,
                base,
                AggOp::Bare(BareAgg::Avg, inner_scalar(inner, "avg")),
            ),
            "max" => scalar_aggregate(
                ctx,
                base,
                AggOp::Bare(BareAgg::Max, inner_scalar(inner, "max")),
            ),
            "min" => scalar_aggregate(
                ctx,
                base,
                AggOp::Bare(BareAgg::Min, inner_scalar(inner, "min")),
            ),
            "sum_over" => {
                let (ty, arg) = over_arg(ctx, args, name);
                scalar_aggregate(ctx, base, AggOp::Over(OverAgg::Sum, ty, arg))
            }
            "avg_over" => {
                let (ty, arg) = over_arg(ctx, args, name);
                scalar_aggregate(ctx, base, AggOp::Over(OverAgg::Avg, ty, arg))
            }
            "max_by" | "min_by" => {
                unreachable!("max_by/min_by return records: callers never enter scalar demand")
            }
            "first" | "nth" => {
                let mut level = rows(ctx, base);
                if level.pending.is_some() {
                    unreachable!(
                        "collapse over split-pending rows in scalar demand: scalar results imply List(Scalar) bases, whose rows never park pendings (crossings that park are nested)"
                    )
                }
                compose_limit(&mut level.builder, &name.value, args, ctx);
                if !level.builder.unions.is_empty() {
                    level = wrap_level(ctx, level);
                }
                if level.builder.select.is_empty() {
                    level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
                }
                match level.builder.select.as_slice() {
                    [_] => SqlExpr::Subquery {
                        query: Box::new(SelectBuilder {
                            select: std::mem::take(&mut level.builder.select),
                            from: level.builder.from,
                            where_: level.builder.where_,
                            order_by: level.builder.order_by,
                            limit: level.builder.limit,
                            offset: level.builder.offset,
                            unions: Vec::new(),
                        }),
                    },
                    _ => todo!("M4b first over multi-col flat list"),
                }
            }
            "contains" if matches!(inner.as_ref(), QueryType::List(_)) => {
                todo!("H1 QT0028: membership over nested lists")
            }
            "contains" => contains_exists(ctx, base, one_arg(args, name)),
            other => {
                unreachable!(
                    "row-pipeline method {other} over List in scalar demand: caller dispatches by node type"
                )
            }
        },
        QueryType::Record(_) => {
            unreachable!("no methods take bare record bases: the checker rejects them")
        }
        QueryType::Tuple(_) => unreachable!(
            "unknown tuple method {}: the checker rejects it",
            name.value.as_str()
        ),
        QueryType::Void | QueryType::Unknown => {
            unreachable!("checker never emits Void/Unknown method bases")
        }
    }
}

fn one_arg<'a>(args: &'a [TypedMethodArg], name: &grove_types::Spanned<String>) -> &'a TypedExpr {
    match args {
        [arg] => &arg.expr,
        _ => unreachable!(
            "wrong arity for {}: the checker enforces Fixed arg counts",
            name.value
        ),
    }
}

fn derived_contains_exists(ctx: &mut Context, base: &TypedExpr, needle: &TypedExpr) -> SqlExpr {
    let value_col = scalar_list_col(ctx, base);
    let mut level = rows(ctx, base);
    if level.pending.is_some() {
        todo!("M4b contains over derived collection");
    }
    if !level.builder.select.is_empty() || !level.builder.unions.is_empty() {
        level = wrap_level(ctx, level);
    }
    if level.builder.select.is_empty() {
        level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    level.builder.where_.push(SqlExpr::Binary {
        op: SqlBinOp::Eq,
        lhs: Box::new(SqlExpr::Column {
            depth: 0,
            column: value_col,
        }),
        rhs: Box::new(scalar(ctx, needle)),
    });
    SqlExpr::Exists(Box::new(level.builder))
}

fn contains_exists(ctx: &mut Context, base: &TypedExpr, needle: &TypedExpr) -> SqlExpr {
    if !needle.ty.is_scalar() {
        todo!("comparison-overhaul: composite membership");
    }
    if let TypedExprKind::Array { elements } = &base.kind {
        let inner = match &base.ty {
            QueryType::List(inner) => inner.as_ref(),
            _ => unreachable!(
                "non-list array type {}: the checker types arrays as lists",
                base.ty
            ),
        };
        match peel_optional(inner) {
            QueryType::Scalar(_) => {
                let items: Vec<(&TypedExpr, Option<i64>)> =
                    elements.iter().map(|e| (e, None)).collect();
                let chain = register_literal_level(ctx, inner, items);
                let [this] = chain.try_into().ok().unwrap_or_else(|| {
                    unreachable!(
                        "scalar-literal contains over a branch: the H1 arm owns nested membership"
                    )
                });
                let mut query = SelectBuilder::new();
                query.from = Some(this.to_from_clause());
                query.select = vec![SelectItem::AllFrom { depth: 0 }];
                query.where_ = vec![SqlExpr::Binary {
                    op: SqlBinOp::Eq,
                    lhs: Box::new(SqlExpr::Column {
                        depth: 0,
                        column: "c0".to_string(),
                    }),
                    rhs: Box::new(scalar(ctx, needle)),
                }];
                return SqlExpr::Exists(Box::new(query));
            }
            QueryType::Tuple(_) => unreachable!(
                "tuple-typed literal base with scalar needle: the contains signature gates needles to the element type, so scalar needles never meet tuple inners"
            ),
            QueryType::Record(_) => unreachable!(
                "record-typed literal base with scalar needle: the contains signature gates needles to the element type, so scalar needles never meet record inners"
            ),
            QueryType::List(_) => {
                unreachable!("nested literal contains: the H1 nested-membership arm fires first")
            }
            QueryType::Unknown => {
                unreachable!(
                    "un-unified literal contains: the checker rejects it with AmbiguousType"
                )
            }
            QueryType::Optional(_) | QueryType::Void => {
                unreachable!("impossible contains base {inner}: no checker rule produces it")
            }
        }
    }
    let fid = match &base.kind {
        TypedExprKind::Method { name, .. }
            if ["take", "skip", "sort", "sort_asc", "sort_desc", "flatten"]
                .contains(&name.value.as_str()) =>
        {
            return derived_contains_exists(ctx, base, needle);
        }
        TypedExprKind::Field { .. } => {
            return derived_contains_exists(ctx, base, needle);
        }
        TypedExprKind::Ident {
            binding: IdentBinding::Field(fid),
            ..
        } => *fid,
        TypedExprKind::Method {
            base: inner, name, ..
        } if name.value == "flatten" => match &inner.kind {
            TypedExprKind::Ident {
                binding: IdentBinding::Field(fid),
                ..
            } => *fid,
            _ => todo!("M4b contains over derived collection"),
        },
        _ => todo!("M4b contains over derived collection"),
    };
    let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
    let (storage, link) = match field {
        Field::Array { storage, link, .. } => (storage, link),
        _ => unreachable!(
            "contains over non-array field with scalar needle: the contains signature gates needles to the element type, so scalar needles never meet reference-typed inners"
        ),
    };
    let table = ctx.schema.tables[storage.table.index()].name.clone();
    let alias = ctx.aliases.table(&table);
    let mut query = SelectBuilder::new();
    query.from = Some(FromClause::Table { table, alias });
    let key_col = ctx.schema.column_name(storage.key).to_string();
    query.select = vec![SelectItem::Column {
        depth: 0,
        column: key_col.clone(),
        output: key_col.clone(),
        ty: Some(ScalarType::Int),
    }];
    let value_col = match storage.value.as_slice() {
        [col] => ctx.schema.column_name(*col).to_string(),
        _ => unreachable!(
            "multi-column via array in contains: via clauses carry exactly key+value columns"
        ),
    };
    query.where_ = vec![
        SqlExpr::Binary {
            op: SqlBinOp::Eq,
            lhs: Box::new(SqlExpr::Column {
                depth: 0,
                column: key_col,
            }),
            rhs: Box::new(SqlExpr::Column {
                depth: 1,
                column: ctx.schema.column_name(*link).to_string(),
            }),
        },
        SqlExpr::Binary {
            op: SqlBinOp::Eq,
            lhs: Box::new(SqlExpr::Column {
                depth: 0,
                column: value_col,
            }),
            rhs: Box::new(scalar(ctx, needle)),
        },
    ];
    SqlExpr::Exists(Box::new(query))
}

fn apply_scalar_method(
    ctx: &mut Context,
    base: SqlExpr,
    scalar_ty: ScalarType,
    name: &grove_types::Spanned<String>,
    args: &[TypedMethodArg],
) -> SqlExpr {
    let method = name.value.as_str();
    match (scalar_ty, method) {
        (ScalarType::String, "contains") => SqlExpr::Binary {
            op: SqlBinOp::Gt,
            lhs: Box::new(SqlExpr::Func {
                name: "instr".to_string(),
                args: vec![base, scalar(ctx, one_arg(args, name))],
            }),
            rhs: Box::new(SqlExpr::Param(ParamValue::Int(0))),
        },
        (ScalarType::String, "starts_with") => SqlExpr::Binary {
            op: SqlBinOp::Eq,
            lhs: Box::new(SqlExpr::Func {
                name: "instr".to_string(),
                args: vec![base, scalar(ctx, one_arg(args, name))],
            }),
            rhs: Box::new(SqlExpr::Param(ParamValue::Int(1))),
        },
        (ScalarType::String, "ends_with") => {
            let sought = scalar(ctx, one_arg(args, name));
            SqlExpr::Binary {
                op: SqlBinOp::Eq,
                lhs: Box::new(SqlExpr::Func {
                    name: "substr".to_string(),
                    args: vec![
                        base.clone(),
                        SqlExpr::Func {
                            name: "$$add".to_string(),
                            args: vec![
                                SqlExpr::Func {
                                    name: "$$sub".to_string(),
                                    args: vec![
                                        SqlExpr::Func {
                                            name: "length".to_string(),
                                            args: vec![base],
                                        },
                                        SqlExpr::Func {
                                            name: "length".to_string(),
                                            args: vec![sought.clone()],
                                        },
                                    ],
                                },
                                SqlExpr::Param(ParamValue::Int(1)),
                            ],
                        },
                    ],
                }),
                rhs: Box::new(sought),
            }
        }
        (ScalarType::String, "to_upper") => SqlExpr::Func {
            name: "$$upper".to_string(),
            args: vec![base],
        },
        (ScalarType::String, "to_lower") => SqlExpr::Func {
            name: "$$lower".to_string(),
            args: vec![base],
        },
        (ScalarType::String, "len") => SqlExpr::Func {
            name: "length".to_string(),
            args: vec![base],
        },
        (ScalarType::Int | ScalarType::Float | ScalarType::Bool, "to_string") => SqlExpr::Cast {
            expr: Box::new(base),
            target: "TEXT",
        },
        (ScalarType::Dec, "to_string") => SqlExpr::Func {
            name: "$$dec_to_string".to_string(),
            args: vec![base],
        },
        (
            ScalarType::Instant,
            "year" | "month" | "day" | "hour" | "minute" | "second" | "weekday" | "epoch",
        ) => SqlExpr::Func {
            name: format!("$$instant_{method}"),
            args: vec![base],
        },
        (ScalarType::Instant, "to_rfc3339") => SqlExpr::Func {
            name: "$$instant_to_rfc3339".to_string(),
            args: vec![base],
        },
        (ScalarType::Duration, "as_seconds" | "as_minutes" | "as_hours" | "as_days") => {
            SqlExpr::Func {
                name: format!("$$duration_{method}"),
                args: vec![base],
            }
        }
        (ScalarType::Dec, "round_dp") => SqlExpr::Func {
            name: "$$dec_round_dp".to_string(),
            args: vec![base, scalar(ctx, one_arg(args, name))],
        },
        (ScalarType::Float, "round") => SqlExpr::Func {
            name: "$$float_round".to_string(),
            args: vec![base],
        },
        (ScalarType::Float, "floor") => SqlExpr::Func {
            name: "$$float_floor".to_string(),
            args: vec![base],
        },
        (ScalarType::Float, "ceil") => SqlExpr::Func {
            name: "$$float_ceil".to_string(),
            args: vec![base],
        },
        _ => {
            unreachable!("unknown scalar method {method} on {scalar_ty:?}: the checker rejects it")
        }
    }
}

fn scalar_binary(
    ctx: &mut Context,
    op: &grove_types::Spanned<BinaryOp>,
    lhs: &TypedExpr,
    rhs: &TypedExpr,
) -> SqlExpr {
    match op.value {
        BinaryOp::Add
        | BinaryOp::Sub
        | BinaryOp::Mul
        | BinaryOp::Div
        | BinaryOp::Rem
        | BinaryOp::Mod => {
            let name = match op.value {
                BinaryOp::Add => "$$add",
                BinaryOp::Sub => "$$sub",
                BinaryOp::Mul => "$$mul",
                BinaryOp::Div => "$$div",
                BinaryOp::Rem => "$$rem",
                _ => "$$mod",
            };
            let (left, right) = (scalar(ctx, lhs), scalar(ctx, rhs));
            match (operand_scalar(&lhs.ty), operand_scalar(&rhs.ty)) {
                (Some(l), Some(r)) if is_temporal(l) || is_temporal(r) => SqlExpr::Func {
                    name: name.to_string(),
                    args: vec![left, right, descriptor(l), descriptor(r)],
                },
                (Some(_), Some(_)) => SqlExpr::Func {
                    name: name.to_string(),
                    args: vec![left, right],
                },
                _ => unreachable!(
                    "non-scalar arithmetic operand: the checker only admits scalar pairs"
                ),
            }
        }
        BinaryOp::Eq | BinaryOp::Ne => {
            if !lhs.ty.is_scalar() || !rhs.ty.is_scalar() {
                todo!(
                    "comparison-overhaul: deep equality over {} vs {}",
                    lhs.ty,
                    rhs.ty
                );
            }
            let op = match op.value {
                BinaryOp::Eq if lhs.ty.is_optional() || rhs.ty.is_optional() => SqlBinOp::Is,
                BinaryOp::Ne if lhs.ty.is_optional() || rhs.ty.is_optional() => SqlBinOp::IsNot,
                BinaryOp::Eq => SqlBinOp::Eq,
                _ => SqlBinOp::Ne,
            };
            SqlExpr::Binary {
                op,
                lhs: Box::new(scalar(ctx, lhs)),
                rhs: Box::new(scalar(ctx, rhs)),
            }
        }
        BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            if matches!(lhs.ty, QueryType::Tuple(_)) || matches!(rhs.ty, QueryType::Tuple(_)) {
                todo!(
                    "comparison-overhaul: tuple ordering over {} vs {}",
                    lhs.ty,
                    rhs.ty
                );
            }
            if !lhs.ty.is_scalar() || !rhs.ty.is_scalar() {
                unreachable!(
                    "non-scalar unordered comparison operand: the checker requires ordered operands"
                );
            }
            SqlExpr::Binary {
                op: match op.value {
                    BinaryOp::Lt => SqlBinOp::Lt,
                    BinaryOp::Gt => SqlBinOp::Gt,
                    BinaryOp::Le => SqlBinOp::Le,
                    _ => SqlBinOp::Ge,
                },
                lhs: Box::new(scalar(ctx, lhs)),
                rhs: Box::new(scalar(ctx, rhs)),
            }
        }
        BinaryOp::And | BinaryOp::Or => {
            if !lhs.ty.is_bool() || !rhs.ty.is_bool() {
                unreachable!("non-Bool logic operand: the checker requires Bool on both sides");
            }
            SqlExpr::Binary {
                op: match op.value {
                    BinaryOp::And => SqlBinOp::And,
                    _ => SqlBinOp::Or,
                },
                lhs: Box::new(scalar(ctx, lhs)),
                rhs: Box::new(scalar(ctx, rhs)),
            }
        }
        BinaryOp::In => match &rhs.kind {
            TypedExprKind::Tuple { elements } => {
                if !lhs.ty.is_scalar() || elements.iter().any(|e| !e.ty.is_scalar()) {
                    todo!("comparison-overhaul: deep `in` over {}", lhs.ty);
                }
                SqlExpr::In {
                    expr: Box::new(scalar(ctx, lhs)),
                    items: elements.iter().map(|e| scalar(ctx, e)).collect(),
                }
            }
            _ => unreachable!("non-tuple `in` RHS: the checker requires a tuple"),
        },
    }
}

fn operand_scalar(ty: &QueryType) -> Option<ScalarType> {
    match ty {
        QueryType::Scalar(s) => Some(*s),
        _ => None,
    }
}

fn is_temporal(s: ScalarType) -> bool {
    matches!(s, ScalarType::Instant | ScalarType::Duration)
}

fn descriptor(s: ScalarType) -> SqlExpr {
    match s {
        ScalarType::Instant => SqlExpr::Param(ParamValue::String("instant".to_string())),
        ScalarType::Duration => SqlExpr::Param(ParamValue::String("duration".to_string())),
        _ => SqlExpr::Param(ParamValue::Null),
    }
}

fn scalar_unary(op: &grove_types::Spanned<UnaryOp>, operand: SqlExpr, ty: &QueryType) -> SqlExpr {
    match op.value {
        UnaryOp::Not => {
            if !ty.is_bool() {
                unreachable!("non-Bool `!` operand: the checker requires Bool");
            }
            SqlExpr::Not(Box::new(operand))
        }
        UnaryOp::Neg => match operand_scalar(ty) {
            Some(ScalarType::Duration) => SqlExpr::Func {
                name: "$$neg".to_string(),
                args: vec![
                    operand,
                    SqlExpr::Param(ParamValue::String("duration".to_string())),
                ],
            },
            Some(_) => SqlExpr::Func {
                name: "$$neg".to_string(),
                args: vec![operand],
            },
            None => {
                unreachable!(
                    "non-scalar unary `-` operand: the checker admits numerics and Duration only"
                )
            }
        },
    }
}

fn scalar_cast(operand: SqlExpr, from: &QueryType, target: TypeName) -> SqlExpr {
    if matches!(from, QueryType::Scalar(ScalarType::Bool)) {
        return operand;
    }
    if !from.is_numeric() {
        unreachable!("non-numeric cast operand: the checker admits numerics and Bool→Int only");
    }
    SqlExpr::Func {
        name: "$$cast".to_string(),
        args: vec![
            operand,
            SqlExpr::Param(ParamValue::String(
                match target {
                    TypeName::Int => "int",
                    TypeName::Float => "float",
                    TypeName::Dec => "dec",
                }
                .to_string(),
            )),
        ],
    }
}


struct LitLevel {
    source: LitSource,
    columns: Vec<NamedColumn>,
    middle_parts: Option<Vec<MiddlePart>>,
    own_key: Option<String>,
}

enum LitSource {
    Grid { module: String, id: String },
    Computed {
        query: Box<SelectBuilder>,
        alias: String,
    },
}

impl LitLevel {
    fn to_from_clause(&self) -> FromClause {
        match &self.source {
            LitSource::Grid { module, id } => FromClause::Literal {
                module: module.clone(),
                id: id.clone(),
                columns: self.columns.clone(),
                middle_parts: self.middle_parts.clone(),
            },
            LitSource::Computed { query, alias } => FromClause::Computed {
                query: query.clone(),
                alias: alias.clone(),
                columns: self.columns.clone(),
                middle_parts: self.middle_parts.clone(),
            },
        }
    }
}

fn sig_letter(s: ScalarType) -> char {
    match s {
        ScalarType::Int | ScalarType::Bool => 'I',
        ScalarType::Float => 'F',
        ScalarType::String => 'T',
        ScalarType::Dec | ScalarType::Instant | ScalarType::Duration => 'B',
    }
}

fn peel_optional(ty: &QueryType) -> &QueryType {
    match ty {
        QueryType::Optional(inner) => inner.as_ref(),
        other => other,
    }
}

fn peel_some(elem: &TypedExpr) -> &TypedExpr {
    match &elem.kind {
        TypedExprKind::Some { value } => value.as_ref(),
        _ => elem,
    }
}

fn literal_depth(inner: &QueryType) -> usize {
    match peel_optional(inner) {
        QueryType::Scalar(_) | QueryType::Tuple(_) => 1,
        QueryType::List(sub) => 1 + literal_depth(sub),
        QueryType::Record(RecordSource::Projection(fields)) => {
            for field in fields {
                literal_depth(&field.ty);
            }
            1
        }
        QueryType::Record(RecordSource::Schema(_)) => todo!("M6 query-valued array element"),
        QueryType::Unknown => {
            unreachable!("un-unified literal element: the checker rejects it with AmbiguousType")
        }
        QueryType::Optional(_) | QueryType::Void => {
            unreachable!("impossible literal element type {inner}: no checker rule produces it")
        }
    }
}

fn leaf_value_tys(inner: &QueryType) -> Vec<ScalarType> {
    match peel_optional(inner) {
        QueryType::Scalar(s) => vec![*s],
        QueryType::Tuple(tys) => tys
            .iter()
            .map(|t| match peel_optional(t) {
                QueryType::Scalar(s) => *s,
                QueryType::List(_) => unreachable!(
                    "tuple with list element in leaf lowering: register routes composites via register_composite_middle"
                ),
                QueryType::Record(_) => unreachable!(
                    "tuple with record element in leaf lowering: register routes composites via register_composite_middle"
                ),
                QueryType::Unknown => unreachable!(
                    "un-unified tuple element: the checker rejects it with AmbiguousType"
                ),
                QueryType::Tuple(_) | QueryType::Optional(_) | QueryType::Void => {
                    unreachable!("impossible tuple element type {t}: no checker rule produces it")
                }
            })
            .collect(),
        _ => {
            unreachable!("leaf_value_tys on non-leaf {inner}: callers dispatch on the peeled type")
        }
    }
}

fn try_fold_static(elem: &TypedExpr) -> Option<ParamValue> {
    let node = peel_some(elem);
    match &node.kind {
        TypedExprKind::Literal(lit) => match &lit.value {
            Literal::Int(n) => Some(ParamValue::Int(*n)),
            Literal::Float(f) => Some(ParamValue::Float(*f)),
            Literal::Dec(d) => Some(ParamValue::Dec(*d)),
            Literal::String(s) => Some(ParamValue::String(s.clone())),
            Literal::Bool(b) => Some(ParamValue::Bool(*b)),
            Literal::Instant(t) => Some(ParamValue::Instant(*t)),
            Literal::Duration(d) => Some(ParamValue::Duration(*d)),
            Literal::None => Some(ParamValue::Null),
            Literal::Now | Literal::Today(_) => None,
        },
        TypedExprKind::TypeConstant { ty, name } => Some(const_value(ty.value, name.value)),
        _ => None,
    }
}

fn const_value(ty: TypeName, name: ConstantName) -> ParamValue {
    match (ty, name) {
        (TypeName::Int, ConstantName::Min) => ParamValue::Int(i64::MIN),
        (TypeName::Int, ConstantName::Max) => ParamValue::Int(i64::MAX),
        (TypeName::Float, ConstantName::Min) => ParamValue::Float(f64::MIN),
        (TypeName::Float, ConstantName::Max) => ParamValue::Float(f64::MAX),
        (TypeName::Dec, ConstantName::Min) => ParamValue::Dec(rust_decimal::Decimal::MIN),
        (TypeName::Dec, ConstantName::Max) => ParamValue::Dec(rust_decimal::Decimal::MAX),
    }
}

fn cells_fold(plan: &PlanNode, elem: &TypedExpr) -> bool {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        return true;
    }
    match (plan, &node.kind) {
        (PlanNode::Scalar(_), _) => try_fold_static(node).is_some(),
        (PlanNode::List(_), _) => true,
        (PlanNode::Tuple(ps), TypedExprKind::Tuple { elements }) => {
            if elements.len() != ps.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            ps.iter()
                .zip(elements.iter())
                .all(|(p, e)| cells_fold(p, e))
        }
        (PlanNode::Record(fs), TypedExprKind::Struct { fields }) => {
            fs.iter().all(|(name, p)| {
                let (_, e) = fields
                    .iter()
                    .find(|(fname, _)| fname.value == *name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {name} vanished after checking: unification matches field sets"
                        )
                    });
                cells_fold(p, e)
            })
        }
        _ => false,
    }
}

fn middle_folds(plans: &[(Option<String>, PlanNode)], elem: &TypedExpr) -> bool {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        return true;
    }
    match &node.kind {
        TypedExprKind::Tuple { elements } => {
            if elements.len() != plans.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            plans
                .iter()
                .zip(elements.iter())
                .all(|((_, plan), e)| cells_fold(plan, e))
        }
        TypedExprKind::Struct { fields } => {
            if fields.len() != plans.len() {
                unreachable!("record arity drift: checking unifies struct field sets");
            }
            plans.iter().all(|(name, plan)| {
                let name = name.as_ref().unwrap_or_else(|| {
                    unreachable!(
                        "record plan without field names: planning names every record field"
                    )
                });
                let (_, e) = fields
                    .iter()
                    .find(|(fname, _)| fname.value == *name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {name} vanished after checking: unification matches field sets"
                        )
                    });
                cells_fold(plan, e)
            })
        }
        _ => false,
    }
}

fn leaf_folds(elem: &TypedExpr, inner: &QueryType) -> bool {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        return true;
    }
    match peel_optional(inner) {
        QueryType::Tuple(_) => match &node.kind {
            TypedExprKind::Tuple { elements } => {
                elements.iter().all(|e| try_fold_static(e).is_some())
            }
            _ => false,
        },
        _ => try_fold_static(node).is_some(),
    }
}

enum PlanNode {
    Scalar(ScalarType),
    List(QueryType),
    Tuple(Vec<PlanNode>),
    Record(Vec<(String, PlanNode)>),
}

fn plan_node(ty: &QueryType) -> PlanNode {
    let mut ty = ty;
    while let QueryType::Optional(inner) = ty {
        ty = inner;
    }
    match ty {
        QueryType::Scalar(s) => PlanNode::Scalar(*s),
        QueryType::List(_) => PlanNode::List(ty.clone()),
        QueryType::Tuple(tys) => PlanNode::Tuple(tys.iter().map(plan_node).collect()),
        QueryType::Record(RecordSource::Projection(fields)) => PlanNode::Record(
            fields
                .iter()
                .map(|field| (field.name.clone(), plan_node(&field.ty)))
                .collect(),
        ),
        QueryType::Record(RecordSource::Schema(_)) => todo!("M6 query-valued array element"),
        QueryType::Unknown => {
            unreachable!("un-unified composite position: the checker rejects it with AmbiguousType")
        }
        QueryType::Void => {
            unreachable!("void composite position: the checker excludes it")
        }
        QueryType::Optional(_) => {
            unreachable!("optional composite position: peeling owns optionals")
        }
    }
}

fn plan_lists(plan: &PlanNode) -> usize {
    match plan {
        PlanNode::Scalar(_) => 0,
        PlanNode::List(_) => 1,
        PlanNode::Tuple(ps) => ps.iter().map(plan_lists).sum(),
        PlanNode::Record(fs) => fs.iter().map(|(_, p)| plan_lists(p)).sum(),
    }
}

fn plan_leaves(plan: &PlanNode) -> usize {
    match plan {
        PlanNode::Scalar(_) => 1,
        PlanNode::List(_) => 0,
        PlanNode::Tuple(ps) => ps.iter().map(plan_leaves).sum(),
        PlanNode::Record(fs) => fs.iter().map(|(_, p)| plan_leaves(p)).sum(),
    }
}

fn register_composite_middle<'a>(
    ctx: &mut Context,
    sub: &QueryType,
    items: Vec<(&'a TypedExpr, Option<i64>)>,
    keyed: bool,
) -> Vec<LitLevel> {
    let top: Vec<(Option<String>, QueryType)> = match peel_optional(sub) {
        QueryType::Tuple(tys) => tys.iter().map(|ty| (None, ty.clone())).collect(),
        QueryType::Record(RecordSource::Projection(fields)) => fields
            .iter()
            .map(|field| (Some(field.name.clone()), field.ty.clone()))
            .collect(),
        _ => unreachable!(
            "composite middle over {sub}: callers route tuples and projection records only"
        ),
    };
    let kind = if top.iter().any(|(name, _)| name.is_some()) {
        "record"
    } else {
        "tuple"
    };
    let plans: Vec<(Option<String>, PlanNode)> = top
        .iter()
        .map(|(name, ty)| (name.clone(), plan_node(ty)))
        .collect();
    if plans
        .iter()
        .map(|(_, plan)| plan_lists(plan))
        .sum::<usize>()
        > 1
    {
        todo!("M7 multi-list {kind} literal");
    }
    let chained = plans.iter().any(|(_, plan)| plan_lists(plan) > 0);
    let mut columns: Vec<NamedColumn> = Vec::new();
    if keyed {
        columns.push(NamedColumn {
            name: "c0".to_string(),
            ty: Some(ScalarType::Int),
        });
    }
    if chained {
        columns.push(NamedColumn {
            name: format!("c{}", columns.len()),
            ty: Some(ScalarType::Int),
        });
    }
    let mut parts: Vec<MiddlePart> = Vec::with_capacity(plans.len());
    for (name, plan) in &plans {
        parts.push(lower_plan(plan, name.clone(), &mut columns));
    }
    let mut sig = String::with_capacity(columns.len());
    if keyed {
        sig.push('I');
    }
    if chained {
        sig.push('I');
    }
    push_sig(&plans, &mut sig);
    let own_key = chained.then(|| format!("c{}", usize::from(keyed)));
    let source = if items.iter().all(|(elem, _)| middle_folds(&plans, elem)) {
        let module = format!("$$lit_{}_{sig}", columns.len());
        let id = ctx.aliases.literal();
        let mut grid: Vec<Vec<ParamValue>> = Vec::with_capacity(items.len());
        for (i, (elem, pkey)) in items.iter().enumerate() {
            let mut row = Vec::with_capacity(columns.len());
            if let Some(p) = pkey {
                row.push(ParamValue::Int(*p));
            }
            if chained {
                row.push(ParamValue::Int(i as i64));
            }
            push_row_cells(&plans, elem, &mut row);
            grid.push(row);
        }
        ctx.literals.insert(id.clone(), grid);
        LitSource::Grid { module, id }
    } else {
        let mut arms: Vec<SelectBuilder> = Vec::with_capacity(items.len());
        for (i, (elem, pkey)) in items.iter().enumerate() {
            let mut exprs = Vec::with_capacity(columns.len());
            if let Some(p) = pkey {
                exprs.push(SqlExpr::Param(ParamValue::Int(*p)));
            }
            if chained {
                exprs.push(SqlExpr::Param(ParamValue::Int(i as i64)));
            }
            let mut cells = Vec::new();
            push_computed_row_cells(ctx, &plans, elem, &mut cells);
            exprs.extend(cells.into_iter().map(|(expr, _)| expr));
            arms.push(computed_arm(&columns, exprs));
        }
        let mut arms = arms.into_iter();
        let mut query = arms.next().unwrap_or_else(|| {
            unreachable!("computed level without arms: empty item sets stay grids")
        });
        query.unions = arms.collect();
        LitSource::Computed {
            query: Box::new(query),
            alias: ctx.aliases.literal(),
        }
    };
    let mut chain = vec![LitLevel {
        source,
        columns,
        middle_parts: Some(parts),
        own_key,
    }];
    if chained {
        let mut chain_ty: Option<QueryType> = None;
        let mut child_items: Vec<(&'a TypedExpr, Option<i64>)> = Vec::new();
        for (i, (elem, _)) in items.iter().enumerate() {
            if let Some(ty) = collect_row(&plans, elem, i as i64, &mut child_items) {
                chain_ty = Some(ty);
            }
        }
        let Some(list_ty) = chain_ty else {
            unreachable!("chained middle without list site: the plan counts exactly one")
        };
        let sub = match peel_optional(&list_ty) {
            QueryType::List(sub) => sub,
            _ => unreachable!("list site without list type: planning owns positions"),
        };
        chain.extend(register_literal_level(ctx, sub, child_items));
    }
    chain
}

fn lower_plan(plan: &PlanNode, name: Option<String>, columns: &mut Vec<NamedColumn>) -> MiddlePart {
    match plan {
        PlanNode::Scalar(s) => {
            let col = format!("c{}", columns.len());
            columns.push(NamedColumn {
                name: col.clone(),
                ty: Some(*s),
            });
            MiddlePart::Value { name, col }
        }
        PlanNode::List(_) => MiddlePart::Nested { name },
        PlanNode::Tuple(ps) => {
            MiddlePart::Tuple(ps.iter().map(|p| lower_plan(p, None, columns)).collect())
        }
        PlanNode::Record(fs) => MiddlePart::Record {
            name,
            fields: fs
                .iter()
                .map(|(fname, p)| lower_plan(p, Some(fname.clone()), columns))
                .collect(),
        },
    }
}

fn push_sig(plans: &[(Option<String>, PlanNode)], sig: &mut String) {
    for (_, plan) in plans {
        push_sig_node(plan, sig);
    }
}

fn push_sig_node(plan: &PlanNode, sig: &mut String) {
    match plan {
        PlanNode::Scalar(s) => sig.push(sig_letter(*s)),
        PlanNode::List(_) => {}
        PlanNode::Tuple(ps) => ps.iter().for_each(|p| push_sig_node(p, sig)),
        PlanNode::Record(fs) => fs.iter().for_each(|(_, p)| push_sig_node(p, sig)),
    }
}

fn push_row_cells(
    plans: &[(Option<String>, PlanNode)],
    elem: &TypedExpr,
    row: &mut Vec<ParamValue>,
) {
    let leaves: usize = plans.iter().map(|(_, plan)| plan_leaves(plan)).sum();
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        row.extend(std::iter::repeat_n(ParamValue::Null, leaves));
        return;
    }
    match &node.kind {
        TypedExprKind::Tuple { elements } => {
            if elements.len() != plans.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            for ((_, plan), e) in plans.iter().zip(elements.iter()) {
                push_cells(plan, e, row);
            }
        }
        TypedExprKind::Struct { fields } => {
            if fields.len() != plans.len() {
                unreachable!("record arity drift: checking unifies struct field sets");
            }
            for ((name, plan), _) in plans.iter().zip(fields.iter()) {
                let name = name.as_ref().unwrap_or_else(|| {
                    unreachable!(
                        "record plan without field names: planning names every record field"
                    )
                });
                let (_, e) = fields
                    .iter()
                    .find(|(fname, _)| fname.value == *name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {name} vanished after checking: unification matches field sets"
                        )
                    });
                push_cells(plan, e, row);
            }
        }
        _ => todo!("M6 query-valued array element"),
    }
}

fn push_cells(plan: &PlanNode, elem: &TypedExpr, out: &mut Vec<ParamValue>) {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        out.extend(std::iter::repeat_n(ParamValue::Null, plan_leaves(plan)));
        return;
    }
    match (plan, &node.kind) {
        (PlanNode::Scalar(_), _) => {
            out.push(try_fold_static(node).unwrap_or_else(|| {
                unreachable!("grid cell refused to fold after eligibility passed")
            }))
        }
        (PlanNode::List(_), _) => {}
        (PlanNode::Tuple(ps), TypedExprKind::Tuple { elements }) => {
            if elements.len() != ps.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            for (p, e) in ps.iter().zip(elements.iter()) {
                push_cells(p, e, out);
            }
        }
        (PlanNode::Record(fs), TypedExprKind::Struct { fields }) => {
            for (name, p) in fs {
                let (_, e) = fields
                    .iter()
                    .find(|(fname, _)| fname.value == *name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {name} vanished after checking: unification matches field sets"
                        )
                    });
                push_cells(p, e, out);
            }
        }
        _ => todo!("M6 query-valued array element"),
    }
}

fn push_computed_row_cells(
    ctx: &mut Context,
    plans: &[(Option<String>, PlanNode)],
    elem: &TypedExpr,
    out: &mut Vec<(SqlExpr, Option<ScalarType>)>,
) {
    let leaves: usize = plans.iter().map(|(_, plan)| plan_leaves(plan)).sum();
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        out.extend(std::iter::repeat_n(
            (SqlExpr::Param(ParamValue::Null), None),
            leaves,
        ));
        return;
    }
    match &node.kind {
        TypedExprKind::Tuple { elements } => {
            if elements.len() != plans.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            for ((_, plan), e) in plans.iter().zip(elements.iter()) {
                push_computed_cells(ctx, plan, e, out);
            }
        }
        TypedExprKind::Struct { fields } => {
            if fields.len() != plans.len() {
                unreachable!("record arity drift: checking unifies struct field sets");
            }
            for (name, plan) in plans.iter() {
                let name = name.as_ref().unwrap_or_else(|| {
                    unreachable!(
                        "record plan without field names: planning names every record field"
                    )
                });
                let (_, e) = fields
                    .iter()
                    .find(|(fname, _)| fname.value == *name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {name} vanished after checking: unification matches field sets"
                        )
                    });
                push_computed_cells(ctx, plan, e, out);
            }
        }
        _ => todo!("M6 query-valued array element"),
    }
}

fn push_computed_cells(
    ctx: &mut Context,
    plan: &PlanNode,
    elem: &TypedExpr,
    out: &mut Vec<(SqlExpr, Option<ScalarType>)>,
) {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        out.extend(std::iter::repeat_n(
            (SqlExpr::Param(ParamValue::Null), None),
            plan_leaves(plan),
        ));
        return;
    }
    match (plan, &node.kind) {
        (PlanNode::Scalar(s), _) => out.push((scalar(ctx, node), Some(*s))),
        (PlanNode::List(_), _) => {}
        (PlanNode::Tuple(ps), TypedExprKind::Tuple { elements }) => {
            if elements.len() != ps.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            for (p, e) in ps.iter().zip(elements.iter()) {
                push_computed_cells(ctx, p, e, out);
            }
        }
        (PlanNode::Record(fs), TypedExprKind::Struct { fields }) => {
            for (name, p) in fs {
                let (_, e) = fields
                    .iter()
                    .find(|(fname, _)| fname.value == *name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {name} vanished after checking: unification matches field sets"
                        )
                    });
                push_computed_cells(ctx, p, e, out);
            }
        }
        _ => todo!("M6 query-valued array element"),
    }
}

fn leaf_computed_cells(
    ctx: &mut Context,
    elem: &TypedExpr,
    inner: &QueryType,
) -> Vec<(SqlExpr, Option<ScalarType>)> {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        let width = leaf_value_tys(inner).len();
        return vec![(SqlExpr::Param(ParamValue::Null), None); width];
    }
    match peel_optional(inner) {
        QueryType::Tuple(tys) => match &node.kind {
            TypedExprKind::Tuple { elements } => elements
                .iter()
                .zip(tys.iter())
                .map(|(e, ty)| (scalar(ctx, e), scalar_of(ty)))
                .collect(),
            _ => todo!("M6 computed array elements"),
        },
        _ => vec![(scalar(ctx, node), scalar_of(inner))],
    }
}

fn computed_arm(columns: &[NamedColumn], exprs: Vec<SqlExpr>) -> SelectBuilder {
    if exprs.len() != columns.len() {
        unreachable!(
            "computed arm arity drift: {} expressions for {} served columns",
            exprs.len(),
            columns.len()
        );
    }
    SelectBuilder {
        select: exprs
            .into_iter()
            .zip(columns.iter())
            .map(|(expr, column)| SelectItem::Expr {
                expr,
                output: column.name.clone(),
                ty: column.ty,
            })
            .collect(),
        from: None,
        where_: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        unions: Vec::new(),
    }
}

fn collect_row<'a>(
    plans: &[(Option<String>, PlanNode)],
    elem: &'a TypedExpr,
    own: i64,
    out: &mut Vec<(&'a TypedExpr, Option<i64>)>,
) -> Option<QueryType> {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        return None;
    }
    match &node.kind {
        TypedExprKind::Tuple { elements } => {
            if elements.len() != plans.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            let mut found = None;
            for ((_, plan), e) in plans.iter().zip(elements.iter()) {
                if let Some(ty) = collect_chain(plan, e, own, out)
                    && found.replace(ty).is_some()
                {
                    unreachable!("second list site: planning counts exactly one per middle")
                }
            }
            found
        }
        TypedExprKind::Struct { fields } => {
            if fields.len() != plans.len() {
                unreachable!("record arity drift: checking unifies struct field sets");
            }
            let mut found = None;
            for ((name, plan), _) in plans.iter().zip(fields.iter()) {
                let name = name.as_ref().unwrap_or_else(|| {
                    unreachable!(
                        "record plan without field names: planning names every record field"
                    )
                });
                let (_, e) = fields
                    .iter()
                    .find(|(fname, _)| fname.value == *name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {name} vanished after checking: unification matches field sets"
                        )
                    });
                if let Some(ty) = collect_chain(plan, e, own, out)
                    && found.replace(ty).is_some()
                {
                    unreachable!("second list site: planning counts exactly one per middle")
                }
            }
            found
        }
        _ => todo!("M6 query-valued array element"),
    }
}

fn collect_chain<'a>(
    plan: &PlanNode,
    elem: &'a TypedExpr,
    own: i64,
    out: &mut Vec<(&'a TypedExpr, Option<i64>)>,
) -> Option<QueryType> {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        return None;
    }
    match (plan, &node.kind) {
        (PlanNode::Scalar(_), _) => None,
        (PlanNode::List(ty), TypedExprKind::Array { elements }) => {
            out.extend(elements.iter().map(|e| (e, Some(own))));
            Some(ty.clone())
        }
        (PlanNode::Tuple(ps), TypedExprKind::Tuple { elements }) => {
            if elements.len() != ps.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            let mut found = None;
            for (p, e) in ps.iter().zip(elements.iter()) {
                if let Some(ty) = collect_chain(p, e, own, out)
                    && found.replace(ty).is_some()
                {
                    unreachable!("second list site: planning counts exactly one per middle")
                }
            }
            found
        }
        (PlanNode::Record(fs), TypedExprKind::Struct { fields }) => {
            let mut found = None;
            for (name, p) in fs {
                let (_, e) = fields
                    .iter()
                    .find(|(fname, _)| fname.value == *name)
                    .unwrap_or_else(|| {
                        unreachable!(
                            "record field {name} vanished after checking: unification matches field sets"
                        )
                    });
                if let Some(ty) = collect_chain(p, e, own, out)
                    && found.replace(ty).is_some()
                {
                    unreachable!("second list site: planning counts exactly one per middle")
                }
            }
            found
        }
        _ => todo!("M6 query-valued array element"),
    }
}

fn leaf_cells(elem: &TypedExpr, inner: &QueryType, width: usize) -> Vec<ParamValue> {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        return vec![ParamValue::Null; width];
    }
    let fold = |e: &TypedExpr| {
        try_fold_static(e)
            .unwrap_or_else(|| unreachable!("grid cell refused to fold after eligibility passed"))
    };
    match peel_optional(inner) {
        QueryType::Tuple(_) => match &node.kind {
            TypedExprKind::Tuple { elements } => elements.iter().map(fold).collect(),
            _ => todo!("M6 computed array elements"),
        },
        _ => vec![fold(node)],
    }
}

fn register_literal_level<'a>(
    ctx: &mut Context,
    inner: &QueryType,
    items: Vec<(&'a TypedExpr, Option<i64>)>,
) -> Vec<LitLevel> {
    if ctx.temp_collect.is_none()
        && items
            .iter()
            .any(|(elem, _)| has_scope_ident(elem) || uses_outer(elem))
    {
        todo!("M6 correlated literal outside temp lane");
    }
    match peel_optional(inner) {
        QueryType::List(sub) => {
            let sub = peel_optional(sub);
            if matches!(
                sub,
                QueryType::Tuple(_) | QueryType::Record(RecordSource::Projection(_))
            ) {
                let rooted = items.first().is_some_and(|(_, p)| p.is_some());
                let mut composite_items: Vec<(&'a TypedExpr, Option<i64>)> = Vec::new();
                let mut root_grid: Vec<Vec<ParamValue>> = Vec::new();
                for (i, (elem, pkey)) in items.iter().enumerate() {
                    let parent = if rooted { *pkey } else { Some(i as i64) };
                    match &peel_some(elem).kind {
                        TypedExprKind::Array { elements } => {
                            composite_items.extend(elements.iter().map(|e| (e, parent)));
                            if !rooted {
                                root_grid.push(vec![ParamValue::Int(i as i64)]);
                            }
                        }
                        TypedExprKind::Tuple { .. } | TypedExprKind::Struct { .. } => {
                            composite_items.push((elem, parent));
                            if !rooted {
                                root_grid.push(vec![ParamValue::Int(i as i64)]);
                            }
                        }
                        TypedExprKind::Literal(lit) if matches!(&lit.value, Literal::None) => {
                            if !rooted {
                                root_grid.push(vec![ParamValue::Int(i as i64)]);
                            }
                        }
                        _ => todo!("M6 query-valued array element"),
                    }
                }
                let mut chain = Vec::new();
                if !rooted {
                    let root_id = ctx.aliases.literal();
                    ctx.literals.insert(root_id.clone(), root_grid);
                    chain.push(LitLevel {
                        source: LitSource::Grid {
                            module: "$$lit_1_I".to_string(),
                            id: root_id,
                        },
                        columns: vec![NamedColumn {
                            name: "c0".to_string(),
                            ty: Some(ScalarType::Int),
                        }],
                        middle_parts: None,
                        own_key: None,
                    });
                }
                chain.extend(register_composite_middle(ctx, sub, composite_items, true));
                return chain;
            }
            let keyed = items.is_empty() || items.first().is_some_and(|(_, p)| p.is_some());
            let columns: Vec<NamedColumn> = if keyed {
                vec![
                    NamedColumn {
                        name: "c0".to_string(),
                        ty: Some(ScalarType::Int),
                    },
                    NamedColumn {
                        name: "c1".to_string(),
                        ty: Some(ScalarType::Int),
                    },
                ]
            } else {
                vec![NamedColumn {
                    name: "c0".to_string(),
                    ty: Some(ScalarType::Int),
                }]
            };
            let module = if keyed {
                "$$lit_2_II".to_string()
            } else {
                "$$lit_1_I".to_string()
            };
            let id = ctx.aliases.literal();
            let mut child_items: Vec<(&'a TypedExpr, Option<i64>)> = Vec::new();
            let mut grid: Vec<Vec<ParamValue>> = Vec::with_capacity(items.len());
            for (i, (elem, pkey)) in items.iter().enumerate() {
                let key = i as i64;
                match &peel_some(elem).kind {
                    TypedExprKind::Array { elements } => {
                        child_items.extend(elements.iter().map(|e| (e, Some(key))));
                    }
                    TypedExprKind::Literal(lit) if matches!(&lit.value, Literal::None) => {}
                    _ => todo!("M6 query-valued array element"),
                }
                let mut row = Vec::with_capacity(2);
                if let Some(p) = pkey {
                    row.push(ParamValue::Int(*p));
                }
                row.push(ParamValue::Int(key));
                grid.push(row);
            }
            ctx.literals.insert(id.clone(), grid);
            let mut chain = vec![LitLevel {
                source: LitSource::Grid { module, id },
                columns,
                middle_parts: None,
                own_key: None,
            }];
            chain.extend(register_literal_level(ctx, sub, child_items));
            chain
        }
        _ => {
            let composite = match peel_optional(inner) {
                QueryType::Tuple(tys)
                    if tys.iter().any(|t| {
                        matches!(peel_optional(t), QueryType::List(_) | QueryType::Record(_))
                    }) =>
                {
                    true
                }
                QueryType::Record(RecordSource::Projection(_)) => true,
                _ => false,
            };
            if composite {
                return register_composite_middle(ctx, inner, items, false);
            }
            let value_tys = leaf_value_tys(inner);
            if value_tys.is_empty() {
                todo!("M6 empty tuple literal");
            }
            let keyed = items.first().is_some_and(|(_, p)| p.is_some());
            let width = value_tys.len() + usize::from(keyed);
            let mut columns: Vec<NamedColumn> = Vec::with_capacity(width);
            if keyed {
                columns.push(NamedColumn {
                    name: "c0".to_string(),
                    ty: Some(ScalarType::Int),
                });
            }
            columns.extend(value_tys.iter().enumerate().map(|(i, ty)| NamedColumn {
                name: format!("c{}", i + usize::from(keyed)),
                ty: Some(*ty),
            }));
            let mut sig = String::with_capacity(width);
            if keyed {
                sig.push('I');
            }
            for ty in &value_tys {
                sig.push(sig_letter(*ty));
            }
            let source = if items.iter().all(|(elem, _)| leaf_folds(elem, inner)) {
                let module = format!("$$lit_{width}_{sig}");
                let id = ctx.aliases.literal();
                let grid: Vec<Vec<ParamValue>> = items
                    .iter()
                    .map(|(elem, pkey)| {
                        let mut row = Vec::with_capacity(width);
                        if let Some(p) = pkey {
                            row.push(ParamValue::Int(*p));
                        }
                        row.extend(leaf_cells(elem, inner, value_tys.len()));
                        row
                    })
                    .collect();
                ctx.literals.insert(id.clone(), grid);
                LitSource::Grid { module, id }
            } else {
                let mut arms: Vec<SelectBuilder> = Vec::with_capacity(items.len());
                for (elem, pkey) in items.iter() {
                    let mut exprs = Vec::with_capacity(width);
                    if let Some(p) = pkey {
                        exprs.push(SqlExpr::Param(ParamValue::Int(*p)));
                    }
                    exprs.extend(
                        leaf_computed_cells(ctx, elem, inner)
                            .into_iter()
                            .map(|(expr, _)| expr),
                    );
                    arms.push(computed_arm(&columns, exprs));
                }
                let mut arms = arms.into_iter();
                let mut query = arms.next().unwrap_or_else(|| {
                    unreachable!("computed level without arms: empty item sets stay grids")
                });
                query.unions = arms.collect();
                LitSource::Computed {
                    query: Box::new(query),
                    alias: ctx.aliases.literal(),
                }
            };
            vec![LitLevel {
                source,
                columns,
                middle_parts: None,
                own_key: None,
            }]
        }
    }
}

fn rows_array(ctx: &mut Context, expr: &TypedExpr) -> Level {
    let TypedExprKind::Array { elements } = &expr.kind else {
        unreachable!("rows_array on non-array node: callers dispatch on node kind")
    };
    let inner = match &expr.ty {
        QueryType::List(inner) => inner.as_ref(),
        QueryType::Optional(_) => todo!("M4b presence: optional list in row demand"),
        _ => unreachable!(
            "non-list array type {}: the checker types arrays as lists",
            expr.ty
        ),
    };
    literal_depth(inner);
    let items: Vec<(&TypedExpr, Option<i64>)> = elements.iter().map(|e| (e, None)).collect();
    let mut chain = register_literal_level(ctx, inner, items);
    let this = chain.remove(0);
    let mut builder = SelectBuilder::new();
    builder.from = Some(this.to_from_clause());
    let root_parts = this.middle_parts.clone();
    let root_key = this.own_key.unwrap_or_else(|| "c0".to_string());
    let mut pending: Option<PendingSplit> = None;
    for (i, level) in chain.into_iter().enumerate().rev() {
        pending = Some(PendingSplit {
            child_from: level.to_from_clause(),
            child_col: "c0".to_string(),
            parent_col: if i == 0 {
                root_key.clone()
            } else {
                "c1".to_string()
            },
            multi_parent: true,
            next: pending.map(Box::new),
            middle_parts: level.middle_parts,
        });
    }
    Level {
        builder,
        element: expr.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending,
        stmt_bound: true,
        output_shapes: None,
        root_parts,
    }
}

fn array_tail_root(mut tail: &TypedExpr) -> bool {
    loop {
        match &tail.kind {
            TypedExprKind::Field { base, .. }
            | TypedExprKind::Method { base, .. }
            | TypedExprKind::Projection { base, .. } => tail = base,
            TypedExprKind::Array { .. } => return true,
            _ => return false,
        }
    }
}

fn array_has_list_leaf(ty: &QueryType) -> bool {
    match peel_optional(ty) {
        QueryType::List(_) => true,
        QueryType::Tuple(tys) => tys.iter().any(array_has_list_leaf),
        QueryType::Record(RecordSource::Projection(fields)) => {
            fields.iter().any(|field| array_has_list_leaf(&field.ty))
        }
        _ => false,
    }
}

fn link_parent_col(schema: &ValidatedSchema, fid: FieldId) -> Option<String> {
    match link_cols(schema, fid) {
        LinkCols::ParentFk { parent_fk, .. } => Some(parent_fk),
        LinkCols::TargetFk { parent_pk, .. } => Some(parent_pk),
        LinkCols::ManyToMany => None,
    }
}

fn nav_root_fk(schema: &ValidatedSchema, elem: &TypedExpr) -> Option<String> {
    scalar_of(&elem.ty)?;
    let mut cursor = elem;
    let mut first = true;
    loop {
        match &cursor.kind {
            TypedExprKind::Field {
                base, name: hop, ..
            } => {
                if let Some(value) = struct_field_value(base, &hop.value) {
                    cursor = value;
                    continue;
                }
                if !first {
                    let sid = match peel_optional(&base.ty) {
                        QueryType::Record(RecordSource::Schema(sid)) => *sid,
                        _ => return None,
                    };
                    let (_, field) = schema.struct_field(sid, &hop.value)?;
                    if !matches!(field, Field::Ref { .. }) {
                        return None;
                    }
                }
                first = false;
                cursor = base;
            }
            TypedExprKind::Ident {
                binding: IdentBinding::Field(fid),
                depth,
                ..
            } => {
                if *depth != 0 {
                    return None;
                }
                let field = &schema.structs[fid.struct_id().index()].fields[fid.local()];
                if !matches!(field, Field::Ref { .. }) {
                    return None;
                }
                return link_parent_col(schema, *fid);
            }
            _ => return None,
        }
    }
}

fn element_field_cols(inner: &QueryType) -> Option<Vec<(String, String)>> {
    fn leaves(ty: &QueryType, col: &mut usize, out: &mut Vec<(String, String)>) -> bool {
        match peel_optional(ty) {
            QueryType::Scalar(_) => {
                *col += 1;
                true
            }
            QueryType::Tuple(tys) => tys.iter().all(|t| leaves(t, col, out)),
            QueryType::Record(RecordSource::Projection(fields)) => fields.iter().all(|field| {
                if scalar_of(&field.ty).is_some() {
                    out.push((field.name.clone(), format!("c{col}")));
                    *col += 1;
                    true
                } else {
                    let len = out.len();
                    let ok = leaves(&field.ty, col, out);
                    out.truncate(len);
                    ok
                }
            }),
            _ => false,
        }
    }
    match peel_optional(inner) {
        QueryType::Record(RecordSource::Projection(_)) => {
            let mut out = Vec::new();
            let mut col = 1usize;
            leaves(inner, &mut col, &mut out).then_some(out)
        }
        _ => Some(Vec::new()),
    }
}
fn rewrite_element_fields(node: &mut TypedExpr, map: &[(String, String)]) {
    match &mut node.kind {
        TypedExprKind::Ident { binding, depth, .. } => {
            if *depth == 0
                && let IdentBinding::ProjectedField { name } = binding
                && let Some((_, col)) = map.iter().find(|(field, _)| field == name)
            {
                *binding = IdentBinding::ProjectedField { name: col.clone() };
            }
        }
        TypedExprKind::Field { .. }
        | TypedExprKind::Method { .. }
        | TypedExprKind::Projection { .. } => {}
        TypedExprKind::Binary { lhs, rhs, .. } => {
            rewrite_element_fields(lhs, map);
            rewrite_element_fields(rhs, map);
        }
        TypedExprKind::Unary { expr, .. }
        | TypedExprKind::Cast { expr, .. }
        | TypedExprKind::Some { value: expr } => rewrite_element_fields(expr, map),
        TypedExprKind::If { arms, default } => {
            for (cond, then) in arms {
                rewrite_element_fields(cond, map);
                rewrite_element_fields(then, map);
            }
            rewrite_element_fields(default, map);
        }
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            for e in elements {
                rewrite_element_fields(e, map);
            }
        }
        TypedExprKind::Struct { fields } => {
            for (_, e) in fields {
                rewrite_element_fields(e, map);
            }
        }
        TypedExprKind::Literal(_) | TypedExprKind::TypeConstant { .. } => {}
    }
}

fn tail_scope_named(tail: &TypedExpr, names: &[&str]) -> bool {
    let mut hit = false;
    any_ident(tail, &mut |e| {
        let is_hit = matches!(
            &e.kind,
            TypedExprKind::Ident {
                name,
                binding: IdentBinding::Field(_) | IdentBinding::ProjectedField { .. },
                ..
            } if names.contains(&name.value.as_str())
        );
        hit = hit || is_hit;
        is_hit
    });
    hit
}

fn rewrite_tail_args(tail: &mut TypedExpr, map: &[(String, String)]) {
    if let TypedExprKind::Method {
        base, name, args, ..
    } = &mut tail.kind
    {
        rewrite_tail_args(base, map);
        if matches!(
            name.value.as_str(),
            "sort" | "sort_asc" | "sort_desc" | "filter"
        ) {
            for arg in args {
                rewrite_element_fields(&mut arg.expr, map);
            }
        }
    }
}

fn shiftable_needle(elem: &TypedExpr) -> bool {
    match &elem.kind {
        TypedExprKind::Ident {
            binding: IdentBinding::Field(_) | IdentBinding::ProjectedField { .. },
            depth,
            ..
        } => *depth == 0 && scalar_of(&elem.ty).is_some(),
        TypedExprKind::Literal(_) | TypedExprKind::TypeConstant { .. } => true,
        TypedExprKind::Binary { lhs, rhs, .. } => shiftable_needle(lhs) && shiftable_needle(rhs),
        TypedExprKind::Unary { expr, .. }
        | TypedExprKind::Cast { expr, .. }
        | TypedExprKind::Some { value: expr } => shiftable_needle(expr),
        TypedExprKind::If { arms, default } => {
            arms.iter()
                .all(|(cond, then)| shiftable_needle(cond) && shiftable_needle(then))
                && shiftable_needle(default)
        }
        _ => false,
    }
}

fn collect_temp_refs(
    schema: &ValidatedSchema,
    tail: &TypedExpr,
    skip: &[String],
) -> Option<Vec<String>> {
    fn go(
        schema: &ValidatedSchema,
        elem: &TypedExpr,
        under_row: bool,
        out: &mut Vec<String>,
        skip: &[String],
    ) -> bool {
        match &elem.kind {
            TypedExprKind::Ident { binding, depth, .. } => match binding {
                IdentBinding::Root { .. } => true,
                IdentBinding::Field(fid) => {
                    if under_row || *depth != 0 || scalar_of(&elem.ty).is_none() {
                        return false;
                    }
                    let field = &schema.structs[fid.struct_id().index()].fields[fid.local()];
                    match field {
                        Field::Value { columns, .. } => match columns.as_slice() {
                            [col] => {
                                let name = schema.column_name(*col).to_string();
                                if !out.contains(&name) {
                                    out.push(name);
                                }
                                true
                            }
                            _ => false,
                        },
                        _ => false,
                    }
                }
                IdentBinding::ProjectedField { name } => {
                    if under_row || *depth != 0 || scalar_of(&elem.ty).is_none() {
                        return false;
                    }
                    if skip.contains(name) {
                        return true;
                    }
                    if !out.contains(name) {
                        out.push(name.clone());
                    }
                    true
                }
                IdentBinding::Prev { .. } => false,
            },
            TypedExprKind::Field { base, name, .. } => {
                if !under_row {
                    if let Some(field) = struct_field_value(base, &name.value) {
                        return go(schema, field, under_row, out, skip);
                    }
                    if let Some(fk) = nav_root_fk(schema, elem) {
                        if !out.contains(&fk) {
                            out.push(fk);
                        }
                        return true;
                    }
                }
                go(schema, base, true, out, skip)
            }
            TypedExprKind::Method {
                base, name, args, ..
            } => {
                let flat_args = matches!(
                    name.value.as_str(),
                    "sort" | "sort_asc" | "sort_desc" | "filter"
                );
                go(schema, base, under_row, out, skip)
                    && args.iter().all(|arg| {
                        if flat_args {
                            go_arg(schema, &arg.expr, out, skip)
                        } else {
                            go(schema, &arg.expr, true, out, skip)
                        }
                    })
            }
            TypedExprKind::Projection { base, items } => {
                go(schema, base, true, out, skip)
                    && items
                        .iter()
                        .all(|item| go(schema, &item.value, true, out, skip))
            }
            TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
                elements.iter().all(|e| go(schema, e, under_row, out, skip))
            }
            TypedExprKind::Struct { fields } => fields
                .iter()
                .all(|(_, e)| go(schema, e, under_row, out, skip)),
            TypedExprKind::Binary { lhs, rhs, .. } => {
                go(schema, lhs, under_row, out, skip) && go(schema, rhs, under_row, out, skip)
            }
            TypedExprKind::Unary { expr, .. }
            | TypedExprKind::Cast { expr, .. }
            | TypedExprKind::Some { value: expr } => go(schema, expr, under_row, out, skip),
            TypedExprKind::If { arms, default } => {
                arms.iter().all(|(cond, then)| {
                    go(schema, cond, under_row, out, skip) && go(schema, then, under_row, out, skip)
                }) && go(schema, default, under_row, out, skip)
            }
            TypedExprKind::Literal(_) | TypedExprKind::TypeConstant { .. } => true,
        }
    }
    let mut out = Vec::new();
    go(schema, tail, false, &mut out, skip).then_some(out)
}

fn go_arg(
    schema: &ValidatedSchema,
    elem: &TypedExpr,
    out: &mut Vec<String>,
    skip: &[String],
) -> bool {
    match &elem.kind {
        TypedExprKind::Ident { binding, depth, .. } => match binding {
            IdentBinding::Root { .. } => true,
            IdentBinding::Field(fid) => {
                if *depth != 0 || scalar_of(&elem.ty).is_none() {
                    return false;
                }
                let field = &schema.structs[fid.struct_id().index()].fields[fid.local()];
                match field {
                    Field::Value { columns, .. } => match columns.as_slice() {
                        [col] => {
                            let name = schema.column_name(*col).to_string();
                            if !out.contains(&name) {
                                out.push(name);
                            }
                            true
                        }
                        _ => false,
                    },
                    _ => false,
                }
            }
            IdentBinding::ProjectedField { name } => {
                if *depth != 0 || scalar_of(&elem.ty).is_none() {
                    return false;
                }
                if skip.contains(name) {
                    return true;
                }
                if !out.contains(name) {
                    out.push(name.clone());
                }
                true
            }
            IdentBinding::Prev { .. } => false,
        },
        TypedExprKind::Field { .. }
        | TypedExprKind::Method { .. }
        | TypedExprKind::Projection { .. } => false,
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            elements.iter().all(|e| go_arg(schema, e, out, skip))
        }
        TypedExprKind::Struct { fields } => {
            fields.iter().all(|(_, e)| go_arg(schema, e, out, skip))
        }
        TypedExprKind::Binary { lhs, rhs, .. } => {
            go_arg(schema, lhs, out, skip) && go_arg(schema, rhs, out, skip)
        }
        TypedExprKind::Unary { expr, .. }
        | TypedExprKind::Cast { expr, .. }
        | TypedExprKind::Some { value: expr } => go_arg(schema, expr, out, skip),
        TypedExprKind::If { arms, default } => {
            arms.iter().all(|(cond, then)| {
                go_arg(schema, cond, out, skip) && go_arg(schema, then, out, skip)
            }) && go_arg(schema, default, out, skip)
        }
        TypedExprKind::Literal(_) | TypedExprKind::TypeConstant { .. } => true,
    }
}

struct IfArm {
    level: Level,
    guard: Vec<SqlExpr>,
    pending: Option<PendingSplit>,
}
fn rows_if(ctx: &mut Context, expr: &TypedExpr) -> Level {
    let TypedExprKind::If { arms, default } = &expr.kind else {
        unreachable!("rows_if on non-if node: callers dispatch on node kind")
    };
    match &expr.ty {
        QueryType::List(_) => {}
        QueryType::Record(_) | QueryType::Optional(_) => todo!("M7 If over records"),
        _ => unreachable!("if over scalars/tuples in row demand: callers dispatch on node type"),
    }
    let mut conds: Vec<SqlExpr> = Vec::with_capacity(arms.len());
    for (cond, _) in arms {
        conds.push(scalar(ctx, cond));
    }
    let sunk_before = ctx.statements.len();
    let mut lowered: Vec<IfArm> = Vec::with_capacity(arms.len() + 1);
    let mut denied: Vec<SqlExpr> = Vec::new();
    for ((_, body), cond) in arms.iter().zip(conds.iter()) {
        let mut guard = denied.clone();
        guard.push(cond.clone());
        let mut level = rows(ctx, body);
        let pending = level.pending.take();
        if ctx.statements.len() != sunk_before {
            todo!("M7 multi-statement If branches");
        }
        if level.builder.select.is_empty() {
            level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
        }
        for cond in guard.iter().rev() {
            level.builder.where_.insert(0, cond.clone());
        }
        denied.push(SqlExpr::Not(Box::new(cond.clone())));
        lowered.push(IfArm {
            level,
            guard,
            pending,
        });
    }
    {
        let guard = denied.clone();
        let mut level = rows(ctx, default);
        let pending = level.pending.take();
        if ctx.statements.len() != sunk_before {
            todo!("M7 multi-statement If branches");
        }
        if level.builder.select.is_empty() {
            level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
        }
        for cond in guard.iter().rev() {
            level.builder.where_.insert(0, cond.clone());
        }
        lowered.push(IfArm {
            level,
            guard,
            pending,
        });
    }
    if lowered.iter().all(|arm| arm.pending.is_none()) {
        let mut branches: Vec<SelectBuilder> =
            lowered.into_iter().map(|arm| arm.level.builder).collect();
        let first_outputs = builder_outputs(&branches[0], ctx.schema);
        for branch in branches.iter().skip(1) {
            if builder_outputs(branch, ctx.schema) != first_outputs {
                todo!("M7 mismatched If branch outputs");
            }
        }
        let mut first = branches.remove(0);
        first.unions = branches;
        return Level {
            builder: first,
            element: expr.ty.clone(),
            parent: ParentLink::Root { root_idx: 0 },
            pending: None,
            stmt_bound: true,
            output_shapes: None,
            root_parts: None,
        };
    }
    union_branches(ctx, lowered, expr.ty.clone())
}

fn union_branches(ctx: &mut Context, arms: Vec<IfArm>, ty: QueryType) -> Level {
    let mut builders: Vec<SelectBuilder> = Vec::with_capacity(arms.len());
    let mut guards: Vec<Vec<SqlExpr>> = Vec::with_capacity(arms.len());
    let mut chains: Vec<Vec<PendingSplit>> = Vec::with_capacity(arms.len());
    let mut roots: Vec<Option<Vec<MiddlePart>>> = Vec::with_capacity(arms.len());
    for arm in arms {
        let Some(pending) = arm.pending else {
            todo!("M7 multi-statement If branches");
        };
        builders.push(arm.level.builder);
        guards.push(arm.guard);
        chains.push(drain_pending(pending));
        roots.push(arm.level.root_parts);
    }
    let mut roots = roots.into_iter();
    let root_parts = roots.next().unwrap_or(None);
    if roots.any(|parts| parts != root_parts) {
        todo!("M7 mismatched If branch outputs");
    }
    let depth = chains[0].len();
    if chains.iter().any(|chain| chain.len() != depth) {
        todo!("M7 multi-statement If branches");
    }
    for level in 0..depth {
        let first = &chains[0][level];
        let first_outputs = pending_outputs(&first.child_from, ctx.schema);
        for chain in chains.iter().skip(1) {
            let hop = &chain[level];
            if pending_outputs(&hop.child_from, ctx.schema) != first_outputs
                || hop.child_col != first.child_col
                || hop.parent_col != first.parent_col
                || hop.middle_parts != first.middle_parts
            {
                todo!("M7 mismatched If branch outputs");
            }
        }
    }
    let first_outputs = builder_outputs(&builders[0], ctx.schema);
    for branch in builders.iter().skip(1) {
        if builder_outputs(branch, ctx.schema) != first_outputs {
            todo!("M7 mismatched If branch outputs");
        }
    }
    let mut first = builders.remove(0);
    first.unions = builders;
    let mut hop_unions: Vec<PendingSplit> = Vec::with_capacity(depth);
    for level in 0..depth {
        let mut hop_builders: Vec<SelectBuilder> = Vec::with_capacity(chains.len());
        for (chain, guard) in chains.iter().zip(guards.iter()) {
            let mut hop = hop_builder(&chain[level]);
            for cond in guard {
                hop.where_.push(cond.clone());
            }
            hop_builders.push(hop);
        }
        let first_outputs = builder_outputs(&hop_builders[0], ctx.schema);
        for hop in hop_builders.iter().skip(1) {
            if builder_outputs(hop, ctx.schema) != first_outputs {
                todo!("M7 mismatched If branch outputs");
            }
        }
        let mut first_hop = hop_builders.remove(0);
        first_hop.unions = hop_builders;
        let first = &chains[0][level];
        hop_unions.push(PendingSplit {
            child_from: FromClause::Subquery {
                query: Box::new(first_hop),
                alias: ctx.aliases.subquery(),
            },
            child_col: first.child_col.clone(),
            parent_col: first.parent_col.clone(),
            multi_parent: chains.iter().any(|chain| chain[level].multi_parent),
            next: None,
            middle_parts: first.middle_parts.clone(),
        });
    }
    let mut pending: Option<PendingSplit> = None;
    for hop in hop_unions.into_iter().rev() {
        pending = Some(PendingSplit {
            next: pending.map(Box::new),
            ..hop
        });
    }
    Level {
        builder: first,
        element: ty,
        parent: ParentLink::Root { root_idx: 0 },
        pending,
        stmt_bound: true,
        output_shapes: None,
        root_parts,
    }
}

fn pending_outputs(from: &FromClause, schema: &ValidatedSchema) -> Vec<String> {
    from_columns(&Some(from.clone()), schema)
}

fn hop_builder(hop: &PendingSplit) -> SelectBuilder {
    let mut child = SelectBuilder::new();
    child.from = Some(hop.child_from.clone());
    child.where_ = vec![SqlExpr::InSubquery {
        expr: Box::new(SqlExpr::Column {
            depth: 0,
            column: hop.child_col.clone(),
        }),
        query: Box::new(bulk_query()),
    }];
    child.select = vec![SelectItem::AllFrom { depth: 0 }];
    child
}

fn lower_tuple_result(ctx: &mut Context, result: &TypedExpr) -> RootPlan {
    let QueryType::Tuple(tys) = &result.ty else {
        unreachable!(
            "tuple node with non-tuple type {}: the checker types tuples as tuples",
            result.ty
        )
    };
    if let TypedExprKind::Method {
        base, name, args, ..
    } = &result.kind
        && ["first", "nth"].contains(&name.value.as_str())
    {
        let (select, shape, _) = collapse_tuple_method(ctx, String::new(), base, name, args, tys);
        let ResultShape::Tuple(shapes) = &shape else {
            unreachable!("collapse yields a tuple shape by construction");
        };
        let shape_cols = shapes
            .iter()
            .zip(tys.iter())
            .map(|(s, ty)| match s {
                ResultShape::Column(output) => OutputColumn {
                    name: output.clone(),
                    role: ColumnRole::Value,
                    ty: scalar_of(ty),
                },
                _ => unreachable!("collapse yields flat columns by construction"),
            })
            .collect();
        return RootPlan {
            builder: SelectBuilder {
                select,
                from: None,
                where_: Vec::new(),
                order_by: Vec::new(),
                limit: None,
                offset: None,
                unions: Vec::new(),
            },
            shape: StatementShape {
                columns: shape_cols,
                rows: RowCount::One,
            },
            result: shape,
        };
    }
    let TypedExprKind::Tuple { elements } = &result.kind else {
        if let TypedExprKind::If { arms, default } = &result.kind {
            match distribute_if_tuple(arms, default, tys, result.span, result.ty.clone()) {
                Some(synthetic) => return lower_tuple_result(ctx, &synthetic),
                None => unreachable!(
                    "undistributable root tuple-`If`: file roots bind nothing, so non-Tuple branches (fields, idents, methods) fail checking upstream"
                ),
            }
        }
        unreachable!(
            "remaining tuple-typed result shapes: Tuple/Method/If nodes route above; bare tuple-typed fields and idents are unbound at the root (QT0001) and no other node kind infers Tuple"
        )
    };
    if elements.len() != tys.len() {
        unreachable!("tuple arity drift: the checker unifies tuple arity");
    }
    let mut select = Vec::with_capacity(elements.len());
    let mut shape_cols = Vec::with_capacity(elements.len());
    let mut tuple = Vec::with_capacity(elements.len());
    for (i, (elem, ty)) in elements.iter().zip(tys.iter()).enumerate() {
        let prefix = format!("c{i}");
        let (mut sub_select, sub_shape) =
            root_composite_element(ctx, prefix, elem, ty, "tuple element");
        for item in &sub_select {
            match item {
                SelectItem::Column { output, ty, .. } | SelectItem::Expr { output, ty, .. } => {
                    shape_cols.push(OutputColumn {
                        name: output.clone(),
                        role: ColumnRole::Value,
                        ty: *ty,
                    })
                }
                SelectItem::AllFrom { .. } => {
                    unreachable!("tuple element selects are always named")
                }
            }
        }
        select.append(&mut sub_select);
        tuple.push(sub_shape);
    }
    if select.is_empty() {
        let (anchor, anchor_col) = unit_anchor(ctx);
        select.push(anchor);
        shape_cols.push(anchor_col);
    }
    RootPlan {
        builder: SelectBuilder {
            select,
            from: None,
            where_: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            unions: Vec::new(),
        },
        shape: StatementShape {
            columns: shape_cols,
            rows: RowCount::One,
        },
        result: ResultShape::Tuple(tuple),
    }
}

fn root_composite_element(
    ctx: &mut Context,
    output: String,
    elem: &TypedExpr,
    ty: &QueryType,
    kind: &str,
) -> (Vec<SelectItem>, ResultShape) {
    if let TypedExprKind::Struct { fields } = &elem.kind {
        let QueryType::Record(RecordSource::Projection(_)) = &elem.ty else {
            unreachable!(
                "struct literal with non-projection type {}: the checker infers anonymous records",
                elem.ty
            )
        };
        let mut select = Vec::new();
        let mut record = Vec::with_capacity(fields.len());
        for (name, field) in fields {
            let field_output = if output.is_empty() {
                name.value.clone()
            } else {
                format!("{output}_{}", name.value)
            };
            let (mut sub_select, sub_shape) =
                root_composite_element(ctx, field_output, field, &field.ty, "struct field");
            select.append(&mut sub_select);
            record.push((name.value.clone(), sub_shape));
        }
        return (select, ResultShape::Record(record));
    }
    if elem.ty.is_scalar() && ty.is_scalar() {
        return (
            vec![SelectItem::Expr {
                expr: scalar(ctx, elem),
                output: output.clone(),
                ty: scalar_of(&elem.ty),
            }],
            ResultShape::Column(output),
        );
    }
    match ty {
        QueryType::List(_) => {
            if matches!(tail_root_ident(elem), Some(IdentBinding::Field(_)))
                || tail_uses_outer(elem)
                || take_bound_ident(elem)
            {
                todo!("M7 per-row {kind}");
            }
            let mut level = rows(ctx, elem);
            if let Some(pending) = level.pending.take() {
                let shape = anchor_split(ctx, level.builder, pending, ty, level.root_parts);
                return (Vec::new(), shape);
            }
            if level.builder.select.is_empty() {
                level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
            }
            let outputs = builder_outputs(&level.builder, ctx.schema);
            let shape_cols: Vec<OutputColumn> =
                shape_columns(builder_output_types(&level.builder, ctx.schema), |_| false);
            let item_ty = match ty {
                QueryType::List(sub) => sub.as_ref(),
                _ => ty,
            };
            let element = child_element(
                ctx.schema,
                level.output_shapes.as_ref(),
                item_ty,
                &outputs,
                |_| false,
            );
            let idx = ctx.sink(
                level.builder,
                StatementShape {
                    columns: shape_cols,
                    rows: RowCount::Many,
                },
                StatementKind::Query,
            );
            (
                Vec::new(),
                ResultShape::List {
                    element: Box::new(element),
                    child: ChildRef {
                        stmt: idx,
                        parent: None,
                        cardinality: Cardinality::Many,
                        tag: None,
                    },
                },
            )
        }
        QueryType::Optional(inner) if matches!(inner.as_ref(), QueryType::Record(_)) => {
            if matches!(elem.kind, TypedExprKind::If { .. }) {
                todo!("M7 If over records");
            }
            if tail_uses_outer(elem) {
                todo!("M7 per-row {kind}");
            }
            let mut level = rows(ctx, elem);
            if let Some(pending) = level.pending.take() {
                if matches!(
                    pending.child_from,
                    FromClause::Literal { .. } | FromClause::Computed { .. }
                ) {
                    todo!("M7 nested {kind}");
                }
                todo!("M7 per-row {kind}");
            }
            if level.builder.select.is_empty() {
                level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
            }
            let outputs = builder_outputs(&level.builder, ctx.schema);
            let shape_cols: Vec<OutputColumn> =
                shape_columns(builder_output_types(&level.builder, ctx.schema), |_| false);
            let element = child_element(
                ctx.schema,
                level.output_shapes.as_ref(),
                ty,
                &outputs,
                |_| false,
            );
            let idx = ctx.sink(
                level.builder,
                StatementShape {
                    columns: shape_cols,
                    rows: RowCount::ZeroOrOne,
                },
                StatementKind::Query,
            );
            (
                Vec::new(),
                ResultShape::Single {
                    element: Box::new(element),
                    child: ChildRef {
                        stmt: idx,
                        parent: None,
                        cardinality: Cardinality::One,
                        tag: None,
                    },
                },
            )
        }
        QueryType::Tuple(tys) => {
            let TypedExprKind::Tuple { elements } = &elem.kind else {
                todo!("M7 non-literal tuple value")
            };
            if elements.len() != tys.len() {
                unreachable!("tuple arity drift: the checker unifies tuple arity");
            }
            let mut shapes = Vec::with_capacity(elements.len());
            let mut select = Vec::new();
            for (i, (sub, sub_ty)) in elements.iter().zip(tys.iter()).enumerate() {
                let (mut sub_select, sub_shape) = root_composite_element(
                    ctx,
                    format!("{output}_c{i}"),
                    sub,
                    sub_ty,
                    "tuple element",
                );
                select.append(&mut sub_select);
                shapes.push(sub_shape);
            }
            (select, ResultShape::Tuple(shapes))
        }
        QueryType::Optional(inner) if matches!(inner.as_ref(), QueryType::List(_)) => {
            if let TypedExprKind::Method { base, name, .. } = &elem.kind
                && (name.value == "first" || name.value == "nth")
                && matches!(&base.kind, TypedExprKind::Array { .. })
                && !has_scope_ident(base)
            {
                let (_, shape, _) = b1_literal(ctx, elem);
                return (Vec::new(), shape);
            }
            todo!("M4b presence: optional list tuple element")
        }
        _ => todo!("M7 exotic {kind}"),
    }
}

fn unit_anchor(ctx: &mut Context) -> (SelectItem, OutputColumn) {
    let output = ctx.aliases.generic();
    (
        SelectItem::Expr {
            expr: SqlExpr::Param(ParamValue::Int(1)),
            output: output.clone(),
            ty: Some(ScalarType::Int),
        },
        OutputColumn {
            name: output,
            role: ColumnRole::Value,
            ty: Some(ScalarType::Int),
        },
    )
}

fn lower_struct_result(ctx: &mut Context, result: &TypedExpr) -> RootPlan {
    if let TypedExprKind::If { arms, default } = &result.kind {
        let distributable = matches!(&result.ty, QueryType::Record(RecordSource::Projection(_)))
            && arms
                .iter()
                .all(|(_, body)| matches!(body.kind, TypedExprKind::Struct { .. }))
            && matches!(default.kind, TypedExprKind::Struct { .. });
        if !distributable {
            todo!("M7 If over records");
        }
        let QueryType::Record(RecordSource::Projection(unified)) = &result.ty else {
            unreachable!(
                "if over non-projection record {}: struct literals infer anonymous records",
                result.ty
            )
        };
        let field_in = |node: &TypedExpr, name: &str| match &node.kind {
            TypedExprKind::Struct { fields } => fields
                .iter()
                .find(|(field_name, _)| field_name.value == name)
                .map(|(_, expr)| expr.clone()),
            _ => None,
        };
        let mut fields = Vec::with_capacity(unified.len());
        for unified_field in unified {
            let mut field_arms = Vec::with_capacity(arms.len());
            for (cond, body) in arms {
                let Some(body_field) = field_in(body, &unified_field.name) else {
                    todo!("M7 mismatched If branches")
                };
                field_arms.push((cond.clone(), body_field));
            }
            let Some(default_field) = field_in(default, &unified_field.name) else {
                todo!("M7 mismatched If branches")
            };
            fields.push((
                grove_types::Spanned {
                    value: unified_field.name.clone(),
                    span: result.span,
                },
                TypedExpr {
                    kind: TypedExprKind::If {
                        arms: field_arms,
                        default: Box::new(default_field),
                    },
                    ty: unified_field.ty.clone(),
                    span: result.span,
                },
            ));
        }
        let synthetic = TypedExpr {
            kind: TypedExprKind::Struct { fields },
            ty: result.ty.clone(),
            span: result.span,
        };
        return lower_struct_result(ctx, &synthetic);
    }
    let TypedExprKind::Struct { fields } = &result.kind else {
        unreachable!(
            "remaining record-typed result shapes: Struct/If nodes route above (If falls through only when undistributable, which todos); bare record-typed fields and idents are unbound at the root (QT0001), single-record projections are rejected (QT0017), and no other node kind infers a bare record"
        )
    };
    let mut select = Vec::new();
    let mut shape_cols = Vec::new();
    let mut record = Vec::with_capacity(fields.len());
    for (name, field) in fields {
        let (mut sub_select, sub_shape) =
            root_composite_element(ctx, name.value.clone(), field, &field.ty, "struct field");
        for item in &sub_select {
            match item {
                SelectItem::Column { output, ty, .. } | SelectItem::Expr { output, ty, .. } => {
                    shape_cols.push(OutputColumn {
                        name: output.clone(),
                        role: ColumnRole::Value,
                        ty: *ty,
                    })
                }
                SelectItem::AllFrom { .. } => {
                    unreachable!("struct field selects are always named")
                }
            }
        }
        select.append(&mut sub_select);
        record.push((name.value.clone(), sub_shape));
    }
    if select.is_empty() {
        let (anchor, anchor_col) = unit_anchor(ctx);
        select.push(anchor);
        shape_cols.push(anchor_col);
    }
    RootPlan {
        builder: SelectBuilder {
            select,
            from: None,
            where_: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            unions: Vec::new(),
        },
        shape: StatementShape {
            columns: shape_cols,
            rows: RowCount::One,
        },
        result: ResultShape::Record(record),
    }
}
