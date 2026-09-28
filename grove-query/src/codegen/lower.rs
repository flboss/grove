
use std::collections::HashMap;

use super::builder::{
    AliasGen, FromClause, OrderDir, OrderItem, SelectBuilder, SelectItem, SqlBinOp, SqlExpr,
};
use super::model::{
    Cardinality, ChildRef, ColumnRole, OutputColumn, ParamValue, ParentRef, ResultShape, RowCount,
    StatementKind, StatementShape, SunkStatement,
};
use crate::ast::{BinaryOp, ConstantName, Literal, SortDir, TypeName, UnaryOp};
use crate::typecheck::types::{
    IdentBinding, QueryType, RecordSource, TypedExpr, TypedExprKind, TypedMethodArg,
    TypedProjectionItem,
};
use grove_schema::validated::{Field, FieldId, Relation, ScalarType, StructId, ValidatedSchema};

pub struct Context<'s> {
    pub schema: &'s ValidatedSchema,
    pub aliases: AliasGen,
    pub statements: Vec<SunkStatement>,
    pub literals: HashMap<String, Vec<Vec<ParamValue>>>,
    pub no_outer: bool,
    pub in_take_arg: bool,
    pub corr: bool,
    pub child: Option<ChildCtx>,
    pub temp_collect: Option<TempCollect>,
}

pub struct ChildCtx {
    pub base: Option<Level>,
    pub per_row: bool,
    pub partition: Option<String>,
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
            temp_collect: None,
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
}

pub struct Level {
    pub builder: SelectBuilder,
    pub element: QueryType,
    pub parent: ParentLink,
    pub pending: Option<PendingSplit>,
    pub stmt_bound: bool,
    pub output_shapes: Option<Vec<(String, Option<ResultShape>)>>,
}

pub fn scalar(ctx: &mut Context, expr: &TypedExpr) -> SqlExpr {
    match &expr.kind {
        TypedExprKind::Literal(lit) => scalar_literal(&lit.value),
        TypedExprKind::TypeConstant { ty, name } => scalar_constant(ty.value, name.value),
        TypedExprKind::Ident { binding, depth, .. } => scalar_ident(ctx, binding, *depth),
        TypedExprKind::Field { base, name, .. } => scalar_field(ctx, base, name),
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
        TypedExprKind::Tuple { .. } => todo!("M7 tuples"),
        TypedExprKind::Struct { .. } => todo!("M7 record literals"),
        TypedExprKind::Some { .. } => todo!("M4b presence: some() in row demand"),
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
        _ => unreachable!(
            "non-root ident in row demand: scope idents are scalar, projection outputs route through items"
        ),
    }
}

fn corr_base_level(ctx: &mut Context, fid: FieldId, ty: &QueryType) -> Level {
    let link = resolve_tail_link(ctx, fid);
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
    }
}

fn rows_field(
    ctx: &mut Context,
    base: &TypedExpr,
    name: &grove_types::Spanned<String>,
    optional: bool,
) -> Level {
    let _ = optional;
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
                Field::Value { columns, .. } => {
                    let [column] = columns.as_slice() else {
                        if level.pending.is_some() {
                            todo!("M7 tuple field over nested rows");
                        }
                        for column in columns {
                            level.builder.select.push(SelectItem::Column {
                                depth: 0,
                                column: ctx.schema.column_name(*column).to_string(),
                                output: ctx.schema.column_name(*column).to_string(),
                            });
                        }
                        return level;
                    };
                    level.builder.select.push(SelectItem::Column {
                        depth: 0,
                        column: ctx.schema.column_name(*column).to_string(),
                        output: name.value.clone(),
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
    let (child_col, parent_col) = match link_cols(ctx, fid) {
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
            }),
            stmt_bound,
            output_shapes,
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
                }),
                stmt_bound,
                output_shapes,
            }
        }
        _ => merge_navigate(ctx, child_from, child_col, level, parent_col),
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
    } = level;
    if pending.is_some() {
        unreachable!("merge over a split-pending level");
    }
    Level {
        builder: merge_builder(child_from, child_col, builder, parent_col),
        element,
        parent,
        pending: None,
        stmt_bound,
        output_shapes,
    }
}

fn merge_builder(
    child_from: FromClause,
    child_col: String,
    mut parent_q: SelectBuilder,
    parent_col: String,
) -> SelectBuilder {
    parent_q.select = vec![SelectItem::Column {
        depth: 0,
        column: parent_col.clone(),
        output: parent_col,
    }];
    let mut child = SelectBuilder::new();
    if let FromClause::Literal { columns, .. } = &child_from {
        child.select = columns
            .iter()
            .filter(|c| *c != &child_col)
            .enumerate()
            .map(|(i, column)| SelectItem::Column {
                depth: 0,
                column: column.clone(),
                output: format!("c{i}"),
            })
            .collect();
        if child.select.is_empty() {
            unreachable!(
                "merge over a key-only literal level: the depth guard keeps merges on leaves"
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
    let (child_col, parent_col) = match link_cols(ctx, fid) {
        LinkCols::ParentFk {
            parent_fk,
            child_pk,
        } => (child_pk, parent_fk),
        LinkCols::TargetFk {
            child_fk,
            parent_pk,
        } => (child_fk, parent_pk),
        LinkCols::ManyToMany => {
            unreachable!("many-to-many hops are list-typed: scalar chains never cross them")
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
        merge_builder(child_from, child_col, parent.builder, parent_col),
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
                todo!("M4 filter over split-pending level");
            }
            if needs_wrap(&level) {
                level = wrap_level(ctx, level);
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
                level = wrap_level(ctx, level);
            }
            let default = if name.value == "sort_desc" {
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
                level
                    .builder
                    .order_by
                    .extend(sort_keys(ctx, &arg.expr, dir));
            }
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
                let mut level = merge_navigate(ctx, child_from, child_col, level, parent_col);
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
                todo!("M4b max_by over split-pending base");
            }
            if needs_wrap(&level) {
                level = wrap_level(ctx, level);
            }
            level.builder.order_by = vec![OrderItem {
                expr: over_arg(ctx, args, name).1,
                dir,
            }];
            compose_limit(&mut level.builder, "first", &[], ctx);
            level
        }
        _ => unreachable!(
            "method {} in row demand: value methods never return row types",
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

fn wrap_level(ctx: &mut Context, level: Level) -> Level {
    let Level {
        mut builder,
        element,
        parent,
        pending,
        output_shapes,
        ..
    } = level;
    if builder.select.is_empty() {
        builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
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
    }
}

fn sort_keys(ctx: &mut Context, expr: &TypedExpr, dir: OrderDir) -> Vec<OrderItem> {
    if !matches!(expr.ty, QueryType::Tuple(_)) {
        return vec![OrderItem {
            expr: scalar(ctx, expr),
            dir,
        }];
    }
    match &expr.kind {
        TypedExprKind::Tuple { elements } => elements
            .iter()
            .flat_map(|e| sort_keys(ctx, e, dir))
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
        _ => todo!("M7 sort-key tuple expansion"),
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
    mut level: Level,
    fk: &str,
    method: &str,
    args: &[TypedMethodArg],
) -> Level {
    let arg = match args {
        [arg] => {
            let was = std::mem::replace(&mut ctx.in_take_arg, true);
            let lowered = scalar(ctx, &arg.expr);
            ctx.in_take_arg = was;
            lowered
        }
        _ => unreachable!("wrong arity for {method}: the checker enforces Fixed arg counts"),
    };
    let guard = non_neg_guard(arg, method);
    let order = std::mem::take(&mut level.builder.order_by);
    let mut level = window_wrap(ctx, level, fk, order);
    let bound = SqlExpr::Binary {
        op: match method {
            "take" => SqlBinOp::Le,
            _ => SqlBinOp::Gt,
        },
        lhs: Box::new(SqlExpr::Column {
            depth: 0,
            column: "$$rn".to_string(),
        }),
        rhs: Box::new(guard),
    };
    level.builder.where_.push(bound);
    level
}
fn window_wrap(ctx: &mut Context, level: Level, fk: &str, order: Vec<OrderItem>) -> Level {
    let Level {
        mut builder,
        element,
        parent,
        pending,
        stmt_bound,
        output_shapes,
    } = level;
    if pending.is_some() {
        todo!("M4b window over split-pending child");
    }
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
    ensure_key(&mut builder, fk);
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
    });
    let level = Level {
        builder,
        element,
        parent,
        pending: None,
        stmt_bound,
        output_shapes,
    };
    wrap_level(ctx, level)
}

fn lower_value(
    ctx: &mut Context,
    parent: &mut Level,
    prefix: String,
    value: &TypedExpr,
) -> (Vec<SelectItem>, ResultShape, Vec<String>) {
    if value.ty.is_scalar() {
        let item = match scalar(ctx, value) {
            SqlExpr::Column { depth, column } => SelectItem::Column {
                depth,
                column,
                output: prefix.clone(),
            },
            expr => SelectItem::Expr {
                expr,
                output: prefix.clone(),
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
                todo!("M4b nested list item");
            }
            let (shape, parent_key) = rvalue_shape(ctx, parent, value);
            (Vec::new(), shape, parent_key.into_iter().collect())
        }
        QueryType::Record(_) => {
            if ctx.child.is_some() {
                todo!("M4b nested record item");
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
                    todo!("M4b nested record item");
                }
                if matches!(value.kind, TypedExprKind::If { .. }) {
                    todo!("M7 If over records");
                }
                let (shape, parent_key) = rscalar_shape(ctx, parent, value);
                (Vec::new(), shape, parent_key.into_iter().collect())
            }
            QueryType::List(_) => todo!("M4b presence: optional list item"),
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
                            });
                            shapes.push(ResultShape::Column(output));
                        }
                        return (select, ResultShape::Tuple(shapes), Vec::new());
                    }
                    _ => unreachable!("tuple-typed non-value field: schema tuples are value-typed"),
                }
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
                todo!("M7 non-literal tuple value")
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
        ensure_key(&mut level.builder, &key);
    }
    level.output_shapes = Some(fields);
    level
}

fn ensure_key(builder: &mut SelectBuilder, col: &str) {
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
    builder.select.push(SelectItem::Column {
        depth: 0,
        column: col.to_string(),
        output: col.to_string(),
    });
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
}

fn resolve_tail_link(ctx: &mut Context, fid: FieldId) -> ChildLink {
    let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
    match field {
        Field::Ref { target, .. } => {
            let table = ctx.schema.tables[ctx.schema.structs[target.index()].table.index()]
                .name
                .clone();
            let (child_col, parent_col) = match link_cols(ctx, fid) {
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
            ChildLink {
                child_from: FromClause::Table {
                    table: table.clone(),
                    alias: ctx.aliases.table(&table),
                },
                child_col,
                parent_col,
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
                    _ => todo!("M4b temp ref to tuple field"),
                },
                _ => todo!("M4b temp ref to non-value field"),
            }
        }
        IdentBinding::ProjectedField { name } => name.clone(),
        IdentBinding::Root { .. } => {
            unreachable!("root idents are List-typed: scalar() only receives scalar inputs")
        }
        IdentBinding::Prev { .. } => todo!("mutations era: prev"),
    };
    let collect = ctx
        .temp_collect
        .as_mut()
        .unwrap_or_else(|| unreachable!("temp collect vanished mid-lowering"));
    if !collect.refs.contains(&column) {
        collect.refs.push(column.clone());
    }
    temp_lookup(&temp, &column, &child_fk)
}

fn temp_lookup(temp: &str, column: &str, child_fk: &str) -> SqlExpr {
    let mut query = SelectBuilder::new();
    query.select = vec![SelectItem::Column {
        depth: 0,
        column: column.to_string(),
        output: column.to_string(),
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
            depth: 1,
            column: child_fk.to_string(),
        }),
    }];
    SqlExpr::Subquery {
        query: Box::new(query),
    }
}

fn lower_tail(
    ctx: &mut Context,
    tail: &TypedExpr,
    child_level: Level,
    per_row: bool,
    partition: Option<String>,
) -> Level {
    let was_child = ctx.child.replace(ChildCtx {
        base: Some(child_level),
        per_row,
        partition,
    });
    let was_outer = std::mem::replace(&mut ctx.no_outer, true);
    let child = rows(ctx, tail);
    ctx.child = was_child;
    ctx.no_outer = was_outer;
    if child.pending.is_some() {
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
            } => SelectItem::Column {
                depth: depth + by,
                column,
                output,
            },
            SelectItem::Expr { expr, output } => SelectItem::Expr {
                expr: shift_depths(expr, by),
                output,
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
        builder.from = Some(match from {
            FromClause::Table { .. } | FromClause::TableFunction { .. } => from,
            FromClause::Temp { .. } => from,
            FromClause::Literal { .. } => from,
            FromClause::Subquery { query, alias } => FromClause::Subquery {
                query: Box::new(shift_builder(*query, by)),
                alias,
            },
        });
    }
    builder
}
fn bulk_query() -> SelectBuilder {
    SelectBuilder {
        select: vec![SelectItem::Column {
            depth: 0,
            column: "value".to_string(),
            output: "value".to_string(),
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
        return match &builder.from {
            Some(FromClause::Table { table, .. }) => schema
                .tables
                .iter()
                .find(|t| &t.name == table)
                .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
                .unwrap_or_default(),
            Some(FromClause::Subquery { query, .. }) => available_cols(query, schema),
            Some(FromClause::Temp { columns, .. }) => columns.clone(),
            Some(FromClause::Literal { columns, .. }) => columns.clone(),
            Some(FromClause::TableFunction { .. }) => vec!["value".to_string()],
            None => Vec::new(),
        };
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

struct CorrCond {
    outer_col: String,
    outer_struct: StructId,
    op: SqlBinOp,
    child: TypedExpr,
}

enum OuterRef {
    Field(FieldId),
    Projected(String),
}

fn split_cross_tail(
    ctx: &mut Context,
    tail: &TypedExpr,
    temp_form: bool,
) -> (TypedExpr, Vec<CorrCond>) {
    let mut correlated: Vec<CorrCond> = Vec::new();
    let pruned = prune_cross_filters(ctx, tail, &mut correlated, temp_form);
    (pruned, correlated)
}

fn prune_cross_filters(
    ctx: &mut Context,
    node: &TypedExpr,
    correlated: &mut Vec<CorrCond>,
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
                    correlated.push(check_corr(ctx, &conjunct, temp_form));
                } else {
                    locals.push(conjunct);
                }
            }
            let pruned_base = prune_cross_filters(ctx, base, correlated, temp_form);
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
            let pruned_base = prune_cross_filters(ctx, base, correlated, temp_form);
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
            let pruned_base = prune_cross_filters(ctx, base, correlated, temp_form);
            rebuild_same(node, pruned_base)
        }
        TypedExprKind::Ident { .. } => node.clone(),
        _ => todo!("M4b cross tail shape"),
    }
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

fn check_corr(ctx: &mut Context, conjunct: &TypedExpr, temp_form: bool) -> CorrCond {
    match &conjunct.kind {
        TypedExprKind::Binary { op, lhs, rhs } => {
            let sql_op = match op.value {
                BinaryOp::Eq => SqlBinOp::Eq,
                BinaryOp::Lt => SqlBinOp::Lt,
                BinaryOp::Gt => SqlBinOp::Gt,
                BinaryOp::Le => SqlBinOp::Le,
                BinaryOp::Ge => SqlBinOp::Ge,
                _ => todo!("M4b complex cross-root correlation"),
            };
            let (outer_side, child_side) = match (uses_outer(lhs), uses_outer(rhs)) {
                (true, false) => (lhs.as_ref(), rhs.as_ref()),
                (false, true) => (rhs.as_ref(), lhs.as_ref()),
                _ => todo!("M4b complex cross-root correlation"),
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
                _ => todo!("M4b complex cross-root correlation"),
            };
            let (outer_struct, outer_col) = match &outer_ref {
                OuterRef::Field(fid) => {
                    let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
                    match field {
                        Field::Value { columns, .. } => match columns.as_slice() {
                            [col] => (fid.struct_id(), ctx.schema.column_name(*col).to_string()),
                            _ => todo!("M4b complex cross-root correlation"),
                        },
                        _ => todo!("M4b complex cross-root correlation"),
                    }
                }
                OuterRef::Projected(name) => {
                    (StructId::new(0), name.clone())
                }
            };
            CorrCond {
                outer_struct,
                outer_col,
                op: sql_op,
                child: child_side.clone(),
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

fn rcross_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
    cardinality: Cardinality,
) -> (ResultShape, Option<String>) {
    let bare = match &parent.builder.from {
        Some(FromClause::Table { .. }) => {
            parent.builder.where_.is_empty()
                && parent.builder.order_by.is_empty()
                && parent.builder.limit.is_none()
                && parent.builder.offset.is_none()
        }
        _ => false,
    };
    let (local_tail, correlated) = split_cross_tail(ctx, tail, !bare);
    if correlated.len() != 1 {
        todo!("M4b composite cross-root correlation");
    }
    let corr = &correlated[0];
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
        if !available_cols(&create_q, ctx.schema).contains(&corr.outer_col) {
            todo!("M4b key threading through subqueries");
        }
        let create_outputs = builder_outputs(&create_q, ctx.schema);
        let idx = ctx.sink(
            create_q,
            StatementShape {
                columns: create_outputs
                    .iter()
                    .map(|name| OutputColumn {
                        name: name.clone(),
                        role: if *name == corr.outer_col {
                            ColumnRole::Key
                        } else {
                            ColumnRole::Value
                        },
                        ty: None,
                    })
                    .collect(),
                rows: RowCount::Many,
            },
            StatementKind::Temp { name: temp.clone() },
        );
        let readback_cols = {
            let probe = SelectBuilder {
                select: vec![SelectItem::AllFrom { depth: 0 }],
                from: Some(FromClause::Temp {
                    name: temp.clone(),
                    columns: create_outputs,
                }),
                where_: Vec::new(),
                order_by: Vec::new(),
                limit: None,
                offset: None,
                unions: Vec::new(),
            };
            builder_outputs(&probe, ctx.schema)
        };
        parent.builder = SelectBuilder {
            select: Vec::new(),
            from: Some(FromClause::Temp {
                name: temp.clone(),
                columns: readback_cols,
            }),
            where_: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            unions: Vec::new(),
        };
        parent.stmt_bound = true;
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
    };
    let was_child = ctx.child.replace(ChildCtx {
        base: Some(child_level),
        per_row: false,
        partition: None,
    });
    let was_outer = std::mem::replace(&mut ctx.no_outer, true);
    let mut child = rows(ctx, &local_tail);
    ctx.child = was_child;
    ctx.no_outer = was_outer;
    if child.pending.is_some() {
        todo!("M4b nested collection inside cross child");
    }
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let was_outer2 = std::mem::replace(&mut ctx.no_outer, true);
    let child_frag = shift_depths(scalar(ctx, &corr.child), 1);
    ctx.no_outer = was_outer2;
    let mut pkey_q = SelectBuilder::new();
    pkey_q.select = vec![SelectItem::Column {
        depth: 0,
        column: corr.outer_col.clone(),
        output: corr.outer_col.clone(),
    }];
    let pkey_alias = if temp_idx.is_some() {
        source_name.clone()
    } else {
        ctx.aliases.table(&source_name)
    };
    pkey_q.from = Some(FromClause::Table {
        table: source_name,
        alias: pkey_alias,
    });
    pkey_q.where_ = vec![SqlExpr::Binary {
        op: corr.op,
        lhs: Box::new(SqlExpr::Column {
            depth: 0,
            column: corr.outer_col.clone(),
        }),
        rhs: Box::new(child_frag),
    }];
    child.builder.select.push(SelectItem::Expr {
        expr: SqlExpr::Subquery {
            query: Box::new(pkey_q),
        },
        output: "$$pkey".to_string(),
    });
    let outputs = builder_outputs(&child.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> = outputs
        .iter()
        .map(|name| OutputColumn {
            name: name.clone(),
            role: if *name == "$$pkey" {
                ColumnRole::Key
            } else {
                ColumnRole::Value
            },
            ty: None,
        })
        .collect();
    let element = ResultShape::Record(
        outputs
            .iter()
            .map(|name| (name.clone(), ResultShape::Column(name.clone())))
            .collect(),
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
        }),
        cardinality,
        tag: None,
    };
    let shape = match cardinality {
        Cardinality::Many => ResultShape::List {
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
        _ => todo!("M4b exotic cross tail root"),
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
        todo!("M4b nested collapse in scalar slot");
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
        select.push(SelectItem::Expr {
            expr: SqlExpr::Subquery {
                query: Box::new(SelectBuilder {
                    select: vec![SelectItem::Column {
                        depth: 0,
                        column: column.clone(),
                        output: column.clone(),
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
    let shape_cols: Vec<OutputColumn> = outputs
        .iter()
        .map(|name| OutputColumn {
            name: name.clone(),
            role: ColumnRole::Value,
            ty: None,
        })
        .collect();
    let root_creation = ctx.statements.len() + hops.len() + 1;
    let inner = if hops.is_empty() {
        ResultShape::Record(
            outputs
                .iter()
                .map(|name| (name.clone(), ResultShape::Column(name.clone())))
                .collect(),
        )
    } else {
        nest_layers(sink_chain(ctx, &hops, root_creation))
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
    let shape = ResultShape::List {
        element: Box::new(inner),
        child: ChildRef {
            stmt: root_creation,
            parent: None,
            cardinality: Cardinality::Many,
            tag: None,
        },
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
    let mut branch_elements: Vec<Vec<TypedExpr>> = Vec::with_capacity(arms.len() + 1);
    for (_, body) in arms {
        match &body.kind {
            TypedExprKind::Tuple { elements } if elements.len() == tys.len() => {
                branch_elements.push(elements.clone());
            }
            _ => return None,
        }
    }
    match &default.kind {
        TypedExprKind::Tuple { elements } if elements.len() == tys.len() => {
            branch_elements.push(elements.clone());
        }
        _ => return None,
    }
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
fn rvalue_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
) -> (ResultShape, Option<String>) {
    let keyed = matches!(tail_root_ident(tail), Some(IdentBinding::Field(_)));
    if keyed && (!parent.stmt_bound || tail_uses_outer(tail) || take_bound_ident(tail)) {
        return rtemp_shape(ctx, parent, tail, Cardinality::Many);
    }
    if !keyed && tail_uses_outer(tail) {
        return rcross_shape(ctx, parent, tail, Cardinality::Many);
    }
    if array_tail_root(tail) {
        if tail_uses_outer(tail) || has_scope_ident(tail) {
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
    let (child_from, child_col, parent_key) = match tail_root_ident(tail) {
        Some(IdentBinding::Field(fid)) => {
            let link = resolve_tail_link(ctx, *fid);
            let _ = parent;
            (link.child_from, link.child_col, Some(link.parent_col))
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
            )
        }
        _ => todo!("M4b exotic tail root"),
    };
    let keyed = parent_key.is_some();
    let mut child_builder = SelectBuilder::new();
    child_builder.from = Some(child_from);
    if keyed {
        child_builder.where_ = vec![SqlExpr::InSubquery {
            expr: Box::new(SqlExpr::Column {
                depth: 0,
                column: child_col.clone(),
            }),
            query: Box::new(bulk_query()),
        }];
    }
    let child_level = Level {
        builder: child_builder,
        element: tail.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
    };
    let mut child = lower_tail(
        ctx,
        tail,
        child_level,
        keyed,
        keyed.then(|| child_col.clone()),
    );
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    if keyed {
        ensure_key(&mut child.builder, &child_col);
    }
    let outputs = builder_outputs(&child.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> = outputs
        .iter()
        .map(|name| OutputColumn {
            name: name.clone(),
            role: if keyed && *name == child_col {
                ColumnRole::Key
            } else {
                ColumnRole::Value
            },
            ty: None,
        })
        .collect();
    let element = ResultShape::Record(
        outputs
            .iter()
            .map(|name| (name.clone(), ResultShape::Column(name.clone())))
            .collect(),
    );
    let idx = ctx.sink(
        child.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    let shape = ResultShape::List {
        element: Box::new(element),
        child: ChildRef {
            stmt: idx,
            parent: parent_key.clone().map(|parent_key| ParentRef {
                stmt: 0,
                parent_key,
                child_key: child_col,
            }),
            cardinality: Cardinality::Many,
            tag: None,
        },
    };
    (shape, parent_key)
}

fn rscalar_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
) -> (ResultShape, Option<String>) {
    let eff = match &tail.kind {
        TypedExprKind::Method { base, name, .. } if name.value == "first" => {
            if filter_chain_to_field_ident(base) || filter_chain_to_root_ident(base) {
                base.as_ref()
            } else {
                todo!("M4b ordered per-parent single");
            }
        }
        TypedExprKind::Method { name, .. }
            if ["nth", "take", "skip"].contains(&name.value.as_str()) =>
        {
            todo!("M4b ordered per-parent single")
        }
        _ => tail,
    };
    let keyed = matches!(tail_root_ident(eff), Some(IdentBinding::Field(_)));
    if keyed && (!parent.stmt_bound || tail_uses_outer(eff) || take_bound_ident(eff)) {
        return rtemp_shape(ctx, parent, eff, Cardinality::One);
    }
    if !keyed && tail_uses_outer(eff) {
        return rcross_shape(ctx, parent, eff, Cardinality::One);
    }
    let (child_from, child_col, parent_key) = match tail_root_ident(eff) {
        Some(IdentBinding::Field(fid)) => {
            let link = resolve_tail_link(ctx, *fid);
            let _ = parent;
            (link.child_from, link.child_col, Some(link.parent_col))
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
            )
        }
        _ => todo!("M4b exotic tail root"),
    };
    let keyed = parent_key.is_some();
    let mut child_builder = SelectBuilder::new();
    child_builder.from = Some(child_from);
    if keyed {
        child_builder.where_ = vec![SqlExpr::InSubquery {
            expr: Box::new(SqlExpr::Column {
                depth: 0,
                column: child_col.clone(),
            }),
            query: Box::new(bulk_query()),
        }];
    }
    let child_level = Level {
        builder: child_builder,
        element: eff.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
    };
    let mut child = lower_tail(
        ctx,
        eff,
        child_level,
        keyed,
        keyed.then(|| child_col.clone()),
    );
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    if keyed {
        ensure_key(&mut child.builder, &child_col);
    }
    let outputs = builder_outputs(&child.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> = outputs
        .iter()
        .map(|name| OutputColumn {
            name: name.clone(),
            role: if keyed && *name == child_col {
                ColumnRole::Key
            } else {
                ColumnRole::Value
            },
            ty: None,
        })
        .collect();
    let element = ResultShape::Record(
        outputs
            .iter()
            .map(|name| (name.clone(), ResultShape::Column(name.clone())))
            .collect(),
    );
    let idx = ctx.sink(
        child.builder,
        StatementShape {
            columns: shape_cols,
            rows: RowCount::Many,
        },
        StatementKind::Query,
    );
    let shape = ResultShape::Single {
        element: Box::new(element),
        child: ChildRef {
            stmt: idx,
            parent: parent_key.clone().map(|parent_key| ParentRef {
                stmt: 0,
                parent_key,
                child_key: child_col,
            }),
            cardinality: Cardinality::One,
            tag: None,
        },
    };
    (shape, parent_key)
}

fn temp_key_query(temp: &str) -> SelectBuilder {
    SelectBuilder {
        select: vec![SelectItem::Column {
            depth: 0,
            column: "$$key".to_string(),
            output: "$$key".to_string(),
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
    let temp = ctx.aliases.temp();
    let (child_from, child_col, key_col) = match tail_root_ident(tail) {
        Some(IdentBinding::Field(fid)) => {
            let link = resolve_tail_link(ctx, *fid);
            (link.child_from, link.child_col, link.parent_col)
        }
        _ => todo!("M4b exotic temp tail root"),
    };
    let parent_q = parent.builder.clone();
    let mut child_builder = SelectBuilder::new();
    child_builder.from = Some(child_from);
    child_builder.where_ = vec![SqlExpr::InSubquery {
        expr: Box::new(SqlExpr::Column {
            depth: 0,
            column: child_col.clone(),
        }),
        query: Box::new(temp_key_query(&temp)),
    }];
    let child_level = Level {
        builder: child_builder,
        element: tail.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
    };
    ctx.temp_collect = Some(TempCollect {
        temp: temp.clone(),
        child_fk: child_col.clone(),
        refs: Vec::new(),
    });
    let mut child = lower_tail(ctx, tail, child_level, true, Some(child_col.clone()));
    let refs = ctx
        .temp_collect
        .take()
        .unwrap_or_else(|| unreachable!("temp collect vanished mid-lowering"))
        .refs;
    if child.builder.select.is_empty() {
        child.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    ensure_key(&mut child.builder, &child_col);
    let mut create_q = parent_q;
    if create_q.select.is_empty() {
        create_q.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    for col in &refs {
        if !available_cols(&create_q, ctx.schema).contains(col) {
            todo!("M4b key threading through subqueries");
        }
        ensure_key(&mut create_q, col);
    }
    create_q.select.push(SelectItem::Column {
        depth: 0,
        column: key_col,
        output: "$$key".to_string(),
    });
    let create_outputs = builder_outputs(&create_q, ctx.schema);
    let temp_idx = ctx.sink(
        create_q,
        StatementShape {
            columns: create_outputs
                .iter()
                .map(|name| OutputColumn {
                    name: name.clone(),
                    role: if *name == "$$key" {
                        ColumnRole::Key
                    } else {
                        ColumnRole::Value
                    },
                    ty: None,
                })
                .collect(),
            rows: RowCount::Many,
        },
        StatementKind::Temp { name: temp.clone() },
    );
    parent.builder = SelectBuilder {
        select: Vec::new(),
        from: Some(FromClause::Temp {
            name: temp.clone(),
            columns: create_outputs,
        }),
        where_: Vec::new(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        unions: Vec::new(),
    };
    parent.stmt_bound = true;
    let outputs = builder_outputs(&child.builder, ctx.schema);
    let shape_cols: Vec<OutputColumn> = outputs
        .iter()
        .map(|name| OutputColumn {
            name: name.clone(),
            role: if *name == child_col {
                ColumnRole::Key
            } else {
                ColumnRole::Value
            },
            ty: None,
        })
        .collect();
    let element = ResultShape::Record(
        outputs
            .iter()
            .map(|name| (name.clone(), ResultShape::Column(name.clone())))
            .collect(),
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
        Cardinality::Many => ResultShape::List {
            element: Box::new(element),
            child: ChildRef {
                stmt: idx,
                parent: Some(ParentRef {
                    stmt: temp_idx,
                    parent_key: "$$key".to_string(),
                    child_key: child_col,
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
                    parent_key: "$$key".to_string(),
                    child_key: child_col,
                }),
                cardinality: Cardinality::One,
                tag: None,
            },
        },
    };
    (shape, None)
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
                }];
            }
        }
        return RootPlan {
            builder,
            shape: StatementShape {
                columns: vec![OutputColumn {
                    name: output.clone(),
                    role: ColumnRole::Value,
                    ty: None,
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
        return emit_split(ctx, level.builder, pending);
    }
    if matches!(level.element, QueryType::Tuple(_)) {
        let mut builder = level.builder;
        if builder.select.is_empty() {
            builder.select = vec![SelectItem::AllFrom { depth: 0 }];
        }
        let outputs = builder_outputs(&builder, ctx.schema);
        let shape = StatementShape {
            columns: outputs
                .iter()
                .map(|name| OutputColumn {
                    name: name.clone(),
                    role: ColumnRole::Value,
                    ty: None,
                })
                .collect(),
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
        todo!("M7 optional tuple result");
    }
    let mut builder = level.builder;
    if builder.select.is_empty() {
        builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    let outputs = builder_outputs(&builder, ctx.schema);
    let fields: Vec<(String, ResultShape)> = match level.output_shapes {
        Some(items) => items
            .into_iter()
            .map(|(alias, shape)| {
                let shape = shape.unwrap_or_else(|| ResultShape::Column(alias.clone()));
                (alias, shape)
            })
            .collect(),
        None => outputs
            .iter()
            .map(|name| (name.clone(), ResultShape::Column(name.clone())))
            .collect(),
    };
    let shape = StatementShape {
        columns: outputs
            .iter()
            .map(|name| OutputColumn {
                name: name.clone(),
                role: ColumnRole::Value,
                ty: None,
            })
            .collect(),
        rows: RowCount::Many,
    };
    let result = ResultShape::List {
        element: Box::new(ResultShape::Record(fields)),
        child: ChildRef {
            stmt: 0,
            parent: None,
            cardinality: Cardinality::Many,
            tag: None,
        },
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

fn sink_chain(
    ctx: &mut Context,
    hops: &[PendingSplit],
    root_creation: usize,
) -> Vec<(usize, ParentRef, Vec<String>)> {
    let mut built: Vec<(SelectBuilder, StatementShape, Vec<String>)> =
        Vec::with_capacity(hops.len());
    for (i, hop) in hops.iter().enumerate() {
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
        let child_outputs = builder_outputs(&child, ctx.schema);
        let next_parent = hops.get(i + 1).map(|next| next.parent_col.clone());
        let child_shape = StatementShape {
            columns: child_outputs
                .iter()
                .map(|name| OutputColumn {
                    name: name.clone(),
                    role: if *name == hop.child_col || next_parent.as_ref() == Some(name) {
                        ColumnRole::Key
                    } else {
                        ColumnRole::Value
                    },
                    ty: None,
                })
                .collect(),
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
    let mut layers: Vec<(usize, ParentRef, Vec<String>)> = Vec::with_capacity(hops.len());
    for (i, hop) in hops.iter().enumerate() {
        layers.push((
            creation[i],
            ParentRef {
                stmt: if i == 0 {
                    root_creation
                } else {
                    creation[i - 1]
                },
                parent_key: hop.parent_col.clone(),
                child_key: hop.child_col.clone(),
            },
            std::mem::take(&mut outputs[i]),
        ));
    }
    layers
}

fn nest_layers(layers: Vec<(usize, ParentRef, Vec<String>)>) -> ResultShape {
    let mut element = None;
    for (idx, pref, outputs) in layers.into_iter().rev() {
        let inner = match element.take() {
            Some(inner) => inner,
            None => ResultShape::Record(
                outputs
                    .iter()
                    .map(|name| (name.clone(), ResultShape::Column(name.clone())))
                    .collect(),
            ),
        };
        element = Some(ResultShape::List {
            element: Box::new(inner),
            child: ChildRef {
                stmt: idx,
                parent: Some(pref),
                cardinality: Cardinality::Many,
                tag: None,
            },
        });
    }
    element.unwrap_or_else(|| unreachable!("nesting no layers: callers only nest non-empty chains"))
}

fn emit_split(ctx: &mut Context, parent_builder: SelectBuilder, pending: PendingSplit) -> RootPlan {
    let hops = drain_pending(pending);
    let mut parent_q = parent_builder;
    parent_q.select = vec![SelectItem::Column {
        depth: 0,
        column: hops[0].parent_col.clone(),
        output: hops[0].parent_col.clone(),
    }];
    let parent_shape = StatementShape {
        columns: vec![OutputColumn {
            name: hops[0].parent_col.clone(),
            role: ColumnRole::Key,
            ty: None,
        }],
        rows: RowCount::Many,
    };
    let result = nest_layers(sink_chain(ctx, &hops, 0));
    RootPlan {
        builder: parent_q,
        shape: parent_shape,
        result,
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
        Some(FromClause::Temp { columns, .. }) => columns.clone(),
        Some(FromClause::Literal { columns, .. }) => columns.clone(),
        None => unreachable!("row pipeline without FROM"),
    }
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
                let sid = record_struct(&cursor.ty);
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
        ChainRoot::Collapsed(_) => {
            todo!("M3 navigation chained over collapsed records");
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
        Field::Value { columns, .. } => match columns.as_slice() {
            [column] => ctx.schema.column_name(*column).to_string(),
            _ => todo!("M7 tuple-valued navigation endpoint"),
        },
        Field::Ref { .. } => todo!("M4 R-scalar: single record in value slot"),
        Field::Array { .. } => todo!("M4 array in value slot"),
    };
    let mut inner = shells
        .pop()
        .unwrap_or_else(|| unreachable!("chain with no hops"));
    inner.select = vec![SelectItem::Column {
        depth: 0,
        column: final_col.clone(),
        output: final_col,
    }];
    while let Some(mut outer) = shells.pop() {
        let output = ctx.aliases.generic();
        outer.select = vec![SelectItem::Expr {
            expr: SqlExpr::Subquery {
                query: Box::new(inner),
            },
            output: output.clone(),
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
        && let TypedExprKind::Ident {
            binding: IdentBinding::Field(fid),
            ..
        } = &inner.kind
        && is_collection_field(ctx, *fid)
    {
        if ctx.in_take_arg {
            todo!("M4b temp-bound per-row limit");
        }
        return scalar_field(ctx, inner, name);
    }
    let column = record_column(ctx, &base.ty, &name.value);
    SqlExpr::Subquery {
        query: Box::new(single_row_query(
            ctx,
            base,
            vec![SelectItem::Column {
                depth: 0,
                column: column.clone(),
                output: column,
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
    let mut level = rows(ctx, base);
    ctx.corr = was_corr;
    ctx.no_outer = was_outer;
    level.stmt_bound = false;
    if level.pending.is_some() {
        todo!("M4 single-row query over split-pending base");
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
                    _ => todo!("M7 tuple-valued field access"),
                },
                Field::Ref { .. } => todo!("M4 R-scalar: single record in value slot"),
                Field::Array { .. } => todo!("M4 array in value slot"),
            }
        }
        QueryType::Record(RecordSource::Projection(_)) => name.to_string(),
        QueryType::Optional(inner) => record_column(ctx, inner, name),
        other => unreachable!("field access over {other}: the checker only admits records"),
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

fn link_cols(ctx: &Context, fid: FieldId) -> LinkCols {
    let relation = &ctx.schema.relations[ctx
        .schema
        .relation_of_field(fid)
        .unwrap_or_else(|| unreachable!("reference field without relation"))
        .index()];
    let name = |c| ctx.schema.column_name(c).to_string();
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
    let (child_col, parent_col) = match link_cols(ctx, fid) {
        LinkCols::ParentFk {
            parent_fk,
            child_pk,
        } => (child_pk, parent_fk),
        LinkCols::TargetFk {
            child_fk,
            parent_pk,
        } => (child_fk, parent_pk),
        LinkCols::ManyToMany => {
            unreachable!("many-to-many hops are list-typed: scalar chains never cross them")
        }
    };
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
        rhs: Box::new(SqlExpr::Column {
            depth: 1,
            column: parent_col,
        }),
    }];
    (target, shell)
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
        todo!("M4b scoped navigation");
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
                        _ => todo!("M4b multi-col array aggregation"),
                    },
                    _ => todo!("M4b nested array aggregation"),
                },
                _ => unreachable!("reference ident in scalar aggregation: records aren't summable"),
            }
        }
        TypedExprKind::Method { base, name, .. }
            if ["take", "skip"].contains(&name.value.as_str()) =>
        {
            scalar_list_col(ctx, base)
        }
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

fn scalar_aggregate(ctx: &mut Context, base: &TypedExpr, agg: AggOp) -> SqlExpr {
    let corr = has_row_setting_ident(base) || tail_uses_outer(base);
    if corr && ctx.in_take_arg {
        todo!("M4b temp-bound per-row limit");
    }
    let was_corr = std::mem::replace(&mut ctx.corr, corr);
    let was_outer = std::mem::replace(&mut ctx.no_outer, !corr);
    let mut level = rows(ctx, base);
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
    let select = |expr| SelectItem::Expr {
        expr,
        output: output.clone(),
    };
    let col = |column: String| SqlExpr::Column { depth: 0, column };
    let expr = match agg {
        AggOp::Len => SqlExpr::Count,
        AggOp::Bare(BareAgg::Sum, ScalarType::Int)
        | AggOp::Bare(BareAgg::Sum, ScalarType::Float) => SqlExpr::Func {
            name: "SUM".to_string(),
            args: vec![col(scalar_list_col(ctx, base))],
        },
        AggOp::Bare(BareAgg::Sum, ScalarType::Dec) => SqlExpr::Func {
            name: "$$sum_dec".to_string(),
            args: vec![col(scalar_list_col(ctx, base))],
        },
        AggOp::Bare(BareAgg::Sum, ScalarType::Duration) => SqlExpr::Func {
            name: "$$sum_dur".to_string(),
            args: vec![col(scalar_list_col(ctx, base))],
        },
        AggOp::Bare(BareAgg::Avg, ScalarType::Int)
        | AggOp::Bare(BareAgg::Avg, ScalarType::Float) => SqlExpr::Func {
            name: "AVG".to_string(),
            args: vec![col(scalar_list_col(ctx, base))],
        },
        AggOp::Bare(BareAgg::Avg, ScalarType::Dec) => SqlExpr::Func {
            name: "$$avg_dec".to_string(),
            args: vec![col(scalar_list_col(ctx, base))],
        },
        AggOp::Bare(BareAgg::Avg, ScalarType::Duration) => SqlExpr::Func {
            name: "$$avg_dur".to_string(),
            args: vec![col(scalar_list_col(ctx, base))],
        },
        AggOp::Bare(BareAgg::Avg, ScalarType::Instant) => SqlExpr::Func {
            name: "$$avg_instant".to_string(),
            args: vec![col(scalar_list_col(ctx, base))],
        },
        AggOp::Bare(BareAgg::Max, _) | AggOp::Bare(BareAgg::Min, _) => {
            let func = match agg {
                AggOp::Bare(BareAgg::Max, _) => "MAX",
                _ => "MIN",
            };
            SqlExpr::Func {
                name: func.to_string(),
                args: vec![col(scalar_list_col(ctx, base))],
            }
        }
        AggOp::Over(OverAgg::Sum, ScalarType::Int, arg)
        | AggOp::Over(OverAgg::Sum, ScalarType::Float, arg) => SqlExpr::Func {
            name: "SUM".to_string(),
            args: vec![arg],
        },
        AggOp::Over(OverAgg::Sum, ScalarType::Dec, arg) => SqlExpr::Func {
            name: "$$sum_dec".to_string(),
            args: vec![arg],
        },
        AggOp::Over(OverAgg::Sum, ScalarType::Duration, arg) => SqlExpr::Func {
            name: "$$sum_dur".to_string(),
            args: vec![arg],
        },
        AggOp::Over(OverAgg::Avg, ScalarType::Int, arg)
        | AggOp::Over(OverAgg::Avg, ScalarType::Float, arg) => SqlExpr::Func {
            name: "AVG".to_string(),
            args: vec![arg],
        },
        AggOp::Over(OverAgg::Avg, ScalarType::Dec, arg) => SqlExpr::Func {
            name: "$$avg_dec".to_string(),
            args: vec![arg],
        },
        AggOp::Over(OverAgg::Avg, ScalarType::Duration, arg) => SqlExpr::Func {
            name: "$$avg_dur".to_string(),
            args: vec![arg],
        },
        AggOp::Over(OverAgg::Avg, ScalarType::Instant, arg) => SqlExpr::Func {
            name: "$$avg_instant".to_string(),
            args: vec![arg],
        },
        _ => unreachable!(
            "rejected input reaches aggregation: the checker restricts bases and inners"
        ),
    };
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
                _ => todo!("M4 list-optional mechanics"),
            },
            _ => todo!("M7 optional tuples"),
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
                    todo!("M4b nested collapse in scalar slot");
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
        QueryType::Tuple(_) => match name.value.as_str() {
            "contains" => {
                let against = scalar(ctx, one_arg(args, name));
                SqlExpr::In {
                    expr: Box::new(against),
                    items: tuple_elements(ctx, base),
                }
            }
            other => unreachable!("unknown tuple method {other}: the checker rejects it"),
        },
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

fn contains_exists(ctx: &mut Context, base: &TypedExpr, needle: &TypedExpr) -> SqlExpr {
    if !needle.ty.is_scalar() {
        todo!("comparison-overhaul: tuple membership");
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
            QueryType::Tuple(_) => todo!("comparison-overhaul: tuple membership"),
            QueryType::Record(_) => todo!("comparison-overhaul: record membership"),
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
        _ => todo!("comparison-overhaul: record membership"),
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
    }];
    let value_col = match storage.value.as_slice() {
        [col] => ctx.schema.column_name(*col).to_string(),
        _ => todo!("comparison-overhaul: tuple membership"),
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

fn tuple_elements(ctx: &mut Context, base: &TypedExpr) -> Vec<SqlExpr> {
    match &base.kind {
        TypedExprKind::Tuple { elements } => elements.iter().map(|e| scalar(ctx, e)).collect(),
        _ => unreachable!(
            "tuple method on non-tuple base: the checker resolves methods on inner types"
        ),
    }
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
    id: String,
    module: String,
    columns: Vec<String>,
}

impl LitLevel {
    fn to_from_clause(&self) -> FromClause {
        FromClause::Literal {
            module: self.module.clone(),
            id: self.id.clone(),
            columns: self.columns.clone(),
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
        QueryType::Record(_) => todo!("M7 record literal element"),
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
                QueryType::List(_) => todo!("M7 tuple with list element"),
                QueryType::Record(_) => todo!("M7 tuple with record element"),
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

fn fold_static(elem: &TypedExpr) -> ParamValue {
    match &elem.kind {
        TypedExprKind::Literal(lit) => match &lit.value {
            Literal::Int(n) => ParamValue::Int(*n),
            Literal::Float(f) => ParamValue::Float(*f),
            Literal::Dec(d) => ParamValue::Dec(*d),
            Literal::String(s) => ParamValue::String(s.clone()),
            Literal::Bool(b) => ParamValue::Bool(*b),
            Literal::Instant(t) => ParamValue::Instant(*t),
            Literal::Duration(d) => ParamValue::Duration(*d),
            Literal::None => ParamValue::Null,
            Literal::Now | Literal::Today(_) => todo!("M6 computed array elements"),
        },
        TypedExprKind::TypeConstant { ty, name } => const_value(ty.value, name.value),
        TypedExprKind::Unary { op, expr } => match op.value {
            UnaryOp::Neg => match fold_static(expr) {
                ParamValue::Int(n) => ParamValue::Int(
                    n.checked_neg()
                        .unwrap_or_else(|| todo!("M6 negated literal overflow")),
                ),
                ParamValue::Float(f) => ParamValue::Float(-f),
                ParamValue::Dec(d) => ParamValue::Dec(-d),
                ParamValue::Duration(d) => ParamValue::Duration(
                    d.checked_mul(-1)
                        .unwrap_or_else(|| todo!("M6 negated literal overflow")),
                ),
                _ => todo!("M6 computed array elements"),
            },
            UnaryOp::Not => match fold_static(expr) {
                ParamValue::Bool(b) => ParamValue::Bool(!b),
                _ => todo!("M6 computed array elements"),
            },
        },
        TypedExprKind::Some { value } => fold_static(value),
        _ => todo!("M6 computed array elements"),
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

fn leaf_cells(elem: &TypedExpr, inner: &QueryType, width: usize) -> Vec<ParamValue> {
    let node = peel_some(elem);
    if let TypedExprKind::Literal(lit) = &node.kind
        && matches!(&lit.value, Literal::None)
    {
        return vec![ParamValue::Null; width];
    }
    match peel_optional(inner) {
        QueryType::Tuple(_) => match &node.kind {
            TypedExprKind::Tuple { elements } => elements.iter().map(fold_static).collect(),
            _ => todo!("M6 computed array elements"),
        },
        _ => vec![fold_static(node)],
    }
}

fn register_literal_level<'a>(
    ctx: &mut Context,
    inner: &QueryType,
    items: Vec<(&'a TypedExpr, Option<i64>)>,
) -> Vec<LitLevel> {
    match peel_optional(inner) {
        QueryType::List(sub) => {
            let keyed = items.first().is_some_and(|(_, p)| p.is_some());
            let columns: Vec<String> = if keyed {
                vec!["c0".to_string(), "c1".to_string()]
            } else {
                vec!["c0".to_string()]
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
                id,
                module,
                columns,
            }];
            chain.extend(register_literal_level(ctx, sub, child_items));
            chain
        }
        _ => {
            let value_tys = leaf_value_tys(inner);
            if value_tys.is_empty() {
                todo!("M6 empty tuple literal");
            }
            let keyed = items.first().is_some_and(|(_, p)| p.is_some());
            let width = value_tys.len() + usize::from(keyed);
            let columns: Vec<String> = (0..width).map(|i| format!("c{i}")).collect();
            let mut sig = String::with_capacity(width);
            if keyed {
                sig.push('I');
            }
            for ty in &value_tys {
                sig.push(sig_letter(*ty));
            }
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
            vec![LitLevel {
                id,
                module,
                columns,
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
    let mut pending: Option<PendingSplit> = None;
    for (i, level) in chain.into_iter().enumerate().rev() {
        pending = Some(PendingSplit {
            child_from: level.to_from_clause(),
            child_col: "c0".to_string(),
            parent_col: if i == 0 { "c0" } else { "c1" }.to_string(),
            multi_parent: true,
            next: pending.map(Box::new),
        });
    }
    Level {
        builder,
        element: expr.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending,
        stmt_bound: true,
        output_shapes: None,
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
    let mut branches: Vec<SelectBuilder> = Vec::with_capacity(arms.len() + 1);
    let mut denied: Vec<SqlExpr> = Vec::new();
    for ((_, body), cond) in arms.iter().zip(conds.iter()) {
        let mut guard = denied.clone();
        guard.push(cond.clone());
        branches.push(guarded_branch(ctx, body, guard, sunk_before));
        denied.push(SqlExpr::Not(Box::new(cond.clone())));
    }
    branches.push(guarded_branch(ctx, default, denied, sunk_before));
    let first_outputs = builder_outputs(&branches[0], ctx.schema);
    for branch in branches.iter().skip(1) {
        if builder_outputs(branch, ctx.schema) != first_outputs {
            todo!("M7 mismatched If branch outputs");
        }
    }
    let mut first = branches.remove(0);
    first.unions = branches;
    Level {
        builder: first,
        element: expr.ty.clone(),
        parent: ParentLink::Root { root_idx: 0 },
        pending: None,
        stmt_bound: true,
        output_shapes: None,
    }
}

fn guarded_branch(
    ctx: &mut Context,
    body: &TypedExpr,
    guard: Vec<SqlExpr>,
    sunk_before: usize,
) -> SelectBuilder {
    let mut level = rows(ctx, body);
    if level.pending.take().is_some() || ctx.statements.len() != sunk_before {
        todo!("M7 multi-statement If branches");
    }
    if level.builder.select.is_empty() {
        level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
    for cond in guard.into_iter().rev() {
        level.builder.where_.insert(0, cond);
    }
    level.builder
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
            .map(|s| match s {
                ResultShape::Column(output) => OutputColumn {
                    name: output.clone(),
                    role: ColumnRole::Value,
                    ty: None,
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
                None => todo!("M7 non-literal tuple result"),
            }
        }
        todo!("M7 non-literal tuple result")
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
                SelectItem::Column { output, .. } | SelectItem::Expr { output, .. } => shape_cols
                    .push(OutputColumn {
                        name: output.clone(),
                        role: ColumnRole::Value,
                        ty: None,
                    }),
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
                if matches!(pending.child_from, FromClause::Literal { .. }) {
                    todo!("M7 nested {kind}");
                }
                todo!("M7 per-row {kind}");
            }
            if level.builder.select.is_empty() {
                level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
            }
            let outputs = builder_outputs(&level.builder, ctx.schema);
            let shape_cols: Vec<OutputColumn> = outputs
                .iter()
                .map(|name| OutputColumn {
                    name: name.clone(),
                    role: ColumnRole::Value,
                    ty: None,
                })
                .collect();
            let element = ResultShape::Record(
                outputs
                    .iter()
                    .map(|name| (name.clone(), ResultShape::Column(name.clone())))
                    .collect(),
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
                if matches!(pending.child_from, FromClause::Literal { .. }) {
                    todo!("M7 nested {kind}");
                }
                todo!("M7 per-row {kind}");
            }
            if level.builder.select.is_empty() {
                level.builder.select = vec![SelectItem::AllFrom { depth: 0 }];
            }
            let outputs = builder_outputs(&level.builder, ctx.schema);
            let shape_cols: Vec<OutputColumn> = outputs
                .iter()
                .map(|name| OutputColumn {
                    name: name.clone(),
                    role: ColumnRole::Value,
                    ty: None,
                })
                .collect();
            let element = ResultShape::Record(
                outputs
                    .iter()
                    .map(|name| (name.clone(), ResultShape::Column(name.clone())))
                    .collect(),
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
        },
        OutputColumn {
            name: output,
            role: ColumnRole::Value,
            ty: None,
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
        todo!("M7 non-literal record result")
    };
    let mut select = Vec::new();
    let mut shape_cols = Vec::new();
    let mut record = Vec::with_capacity(fields.len());
    for (name, field) in fields {
        let (mut sub_select, sub_shape) =
            root_composite_element(ctx, name.value.clone(), field, &field.ty, "struct field");
        for item in &sub_select {
            match item {
                SelectItem::Column { output, .. } | SelectItem::Expr { output, .. } => shape_cols
                    .push(OutputColumn {
                        name: output.clone(),
                        role: ColumnRole::Value,
                        ty: None,
                    }),
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
