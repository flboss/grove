
use std::collections::HashMap;

use super::builder::{
    AliasGen, FromClause, OrderDir, OrderItem, SelectBuilder, SelectItem, SqlBinOp, SqlExpr,
};
use super::model::{
    Cardinality, ChildRef, ColumnRole, OutputColumn, ParamValue, ParentRef, ResultShape, RowCount,
    StatementShape, SunkStatement,
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
    pub child: Option<ChildCtx>,
}

pub struct ChildCtx {
    pub base: Option<Level>,
    pub per_row: bool,
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
            child: None,
        }
    }

    pub fn sink(&mut self, builder: SelectBuilder, shape: StatementShape) -> usize {
        let idx = self.statements.len() + 1;
        self.statements.push(SunkStatement { builder, shape });
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
        TypedExprKind::Array { .. } => todo!("M6 array literals"),
        TypedExprKind::Tuple { .. } => todo!("M7 tuples"),
        TypedExprKind::Struct { .. } => todo!("M7 record literals"),
        TypedExprKind::If { .. } => todo!("M7 if-unions"),
        TypedExprKind::Some { .. } => todo!("M3 optional presence"),
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
        _ => unreachable!(
            "non-root ident in row demand: scope idents are scalar, projection outputs route through items"
        ),
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
                        todo!("M7 tuple-valued field in row demand")
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
    level: Level,
    multi_parent: bool,
    fid: FieldId,
    target: StructId,
    is_list: bool,
    field_optional: bool,
) -> Level {
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
    level: Level,
    element: &grove_schema::validated::ValueType,
    link: grove_schema::validated::ColumnId,
    storage: &grove_schema::validated::StorageTable,
) -> Level {
    use grove_schema::validated::ValueType;
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
            if matches!(&ctx.child, Some(tail) if tail.per_row) {
                todo!("M4b per-parent take inside child tail");
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
                    ..
                } = pending;
                merge_navigate(ctx, child_from, child_col, level, parent_col)
            }
            None => {
                unreachable!("flatten over flat list is rejected upstream (strict sig)")
            }
        },
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
        match &item.value.ty {
            ty if ty.is_scalar() => {
                match scalar(ctx, &item.value) {
                    SqlExpr::Column { depth, column } => select.push(SelectItem::Column {
                        depth,
                        column,
                        output: alias.clone(),
                    }),
                    expr => select.push(SelectItem::Expr {
                        expr,
                        output: alias.clone(),
                    }),
                }
                fields.push((alias, None));
            }
            QueryType::List(_) => {
                if ctx.child.is_some() {
                    todo!("M4b nested list item");
                }
                if !level.stmt_bound {
                    todo!("M4b R-value under subquery (temp lane)");
                }
                let (shape, parent_key) = rvalue_shape(ctx, &mut level, &item.value);
                fields.push((alias, Some(shape)));
                if let Some(key) = parent_key {
                    parent_keys.push(key);
                }
            }
            QueryType::Record(_) => {
                if ctx.child.is_some() {
                    todo!("M4b nested record item");
                }
                if !level.stmt_bound {
                    todo!("M4b R-scalar under subquery (temp lane)");
                }
                let (shape, parent_key) = rscalar_shape(ctx, &mut level, &item.value);
                fields.push((alias, Some(shape)));
                if let Some(key) = parent_key {
                    parent_keys.push(key);
                }
            }
            QueryType::Optional(inner) => match inner.as_ref() {
                QueryType::Record(_) => {
                    if ctx.child.is_some() {
                        todo!("M4b nested record item");
                    }
                    if !level.stmt_bound {
                        todo!("M4b R-scalar under subquery (temp lane)");
                    }
                    let (shape, parent_key) = rscalar_shape(ctx, &mut level, &item.value);
                    fields.push((alias, Some(shape)));
                    if let Some(key) = parent_key {
                        parent_keys.push(key);
                    }
                }
                QueryType::List(_) => todo!("M4b presence: optional list item"),
                _ => todo!("M4b exotic optional item"),
            },
            QueryType::Tuple(_) => todo!("M7 tuples"),
            QueryType::Void | QueryType::Unknown => {
                unreachable!("void/unknown projection item: the checker excludes them")
            }
            _ => unreachable!("scalar items match the first arm"),
        }
    }
    if !level.builder.select.is_empty() {
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
    }
}

fn rvalue_shape(
    ctx: &mut Context,
    parent: &mut Level,
    tail: &TypedExpr,
) -> (ResultShape, Option<String>) {
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
    let was_child = ctx.child.replace(ChildCtx {
        base: Some(child_level),
        per_row: keyed,
    });
    let was_outer = std::mem::replace(&mut ctx.no_outer, true);
    let mut child = rows(ctx, tail);
    ctx.child = was_child;
    ctx.no_outer = was_outer;
    if child.pending.is_some() {
        todo!("M4b nested collection inside child");
    }
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
            if filter_chain_to_field_ident(base) {
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
    let was_child = ctx.child.replace(ChildCtx {
        base: Some(child_level),
        per_row: keyed,
    });
    let was_outer = std::mem::replace(&mut ctx.no_outer, true);
    let mut child = rows(ctx, eff);
    ctx.child = was_child;
    ctx.no_outer = was_outer;
    if child.pending.is_some() {
        todo!("M4b nested collection inside child");
    }
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

pub struct RootPlan {
    pub builder: SelectBuilder,
    pub shape: StatementShape,
    pub result: ResultShape,
}

pub fn lower_query(ctx: &mut Context, result: &TypedExpr) -> RootPlan {
    if result.ty.is_scalar() {
        let mut builder = SelectBuilder::new();
        let output = ctx.aliases.generic();
        builder.select = vec![SelectItem::Expr {
            expr: scalar(ctx, result),
            output: output.clone(),
        }];
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
    let level = rows(ctx, result);
    finalize_level(ctx, level)
}

fn finalize_level(ctx: &mut Context, level: Level) -> RootPlan {
    if let Some(pending) = level.pending {
        return emit_split(ctx, level.builder, pending);
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

fn emit_split(ctx: &mut Context, parent_builder: SelectBuilder, pending: PendingSplit) -> RootPlan {
    let PendingSplit {
        child_from,
        child_col,
        parent_col,
        ..
    } = pending;
    let mut parent_q = parent_builder;
    parent_q.select = vec![SelectItem::Column {
        depth: 0,
        column: parent_col.clone(),
        output: parent_col.clone(),
    }];
    let parent_shape = StatementShape {
        columns: vec![OutputColumn {
            name: parent_col.clone(),
            role: ColumnRole::Key,
            ty: None,
        }],
        rows: RowCount::Many,
    };
    let mut child = SelectBuilder::new();
    child.from = Some(child_from);
    child.where_ = vec![SqlExpr::InSubquery {
        expr: Box::new(SqlExpr::Column {
            depth: 0,
            column: child_col.clone(),
        }),
        query: Box::new(bulk_query()),
    }];
    child.select = vec![SelectItem::AllFrom { depth: 0 }];
    let child_outputs = builder_outputs(&child, ctx.schema);
    let child_shape = StatementShape {
        columns: child_outputs
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
            .collect(),
        rows: RowCount::Many,
    };
    let element = ResultShape::Record(
        child_outputs
            .iter()
            .map(|name| (name.clone(), ResultShape::Column(name.clone())))
            .collect(),
    );
    let idx = ctx.sink(child, child_shape);
    RootPlan {
        builder: parent_q,
        shape: parent_shape,
        result: ResultShape::List {
            element: Box::new(element),
            child: ChildRef {
                stmt: idx,
                parent: Some(ParentRef {
                    stmt: 0,
                    parent_key: parent_col,
                    child_key: child_col,
                }),
                cardinality: Cardinality::Many,
                tag: None,
            },
        },
    }
}

fn builder_outputs(builder: &SelectBuilder, schema: &ValidatedSchema) -> Vec<String> {
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
    if has_row_setting_ident(base) {
        todo!("M4b per-parent collapse with pipeline");
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
    match &expr.kind {
        TypedExprKind::Ident {
            binding: IdentBinding::Field(_),
            ..
        } => !expr.ty.is_scalar(),
        TypedExprKind::Ident { .. } => false,
        TypedExprKind::Literal(_) | TypedExprKind::TypeConstant { .. } => false,
        TypedExprKind::Field { base, .. } => has_row_setting_ident(base),
        TypedExprKind::Method { base, args, .. } => {
            has_row_setting_ident(base) || args.iter().any(|a| has_row_setting_ident(&a.expr))
        }
        TypedExprKind::Binary { lhs, rhs, .. } => {
            has_row_setting_ident(lhs) || has_row_setting_ident(rhs)
        }
        TypedExprKind::Unary { expr, .. } | TypedExprKind::Cast { expr, .. } => {
            has_row_setting_ident(expr)
        }
        TypedExprKind::If { arms, default } => {
            arms.iter()
                .any(|(c, t)| has_row_setting_ident(c) || has_row_setting_ident(t))
                || has_row_setting_ident(default)
        }
        TypedExprKind::Some { value } => has_row_setting_ident(value),
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            elements.iter().any(has_row_setting_ident)
        }
        TypedExprKind::Struct { fields } => fields.iter().any(|(_, e)| has_row_setting_ident(e)),
        TypedExprKind::Projection { base, items } => {
            has_row_setting_ident(base) || items.iter().any(|i| has_row_setting_ident(&i.value))
        }
    }
}

fn single_row_query(ctx: &mut Context, base: &TypedExpr, select: Vec<SelectItem>) -> SelectBuilder {
    let was = std::mem::replace(&mut ctx.no_outer, true);
    let mut level = rows(ctx, base);
    ctx.no_outer = was;
    level.stmt_bound = false;
    if level.pending.is_some() {
        todo!("M4 single-row query over split-pending base");
    }
    if !level.builder.select.is_empty() {
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
    let value = match (ty, name) {
        (TypeName::Int, ConstantName::Min) => ParamValue::Int(i64::MIN),
        (TypeName::Int, ConstantName::Max) => ParamValue::Int(i64::MAX),
        (TypeName::Float, ConstantName::Min) => ParamValue::Float(f64::MIN),
        (TypeName::Float, ConstantName::Max) => ParamValue::Float(f64::MAX),
        (TypeName::Dec, ConstantName::Min) => ParamValue::Dec(rust_decimal::Decimal::MIN),
        (TypeName::Dec, ConstantName::Max) => ParamValue::Dec(rust_decimal::Decimal::MAX),
    };
    SqlExpr::Param(value)
}

fn scalar_ident(ctx: &mut Context, binding: &IdentBinding, depth: usize) -> SqlExpr {
    if ctx.no_outer && depth > 0 {
        todo!("M4b correlated reference inside keyed child");
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
            "len" | "sum" | "avg" | "max" | "min" | "sum_over" | "avg_over" | "max_by"
            | "min_by" => todo!("M5 aggregations: {}", name.value),
            "first" | "nth" => todo!("M3 single collapse: {}", name.value),
            "contains" if matches!(inner.as_ref(), QueryType::List(_)) => {
                todo!("H1 QT0028: membership over nested lists")
            }
            "contains" => todo!("M3 flipped EXISTS over element rows"),
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
