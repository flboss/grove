use super::builder::{
    AliasGen, FromClause, OrderDir, OrderItem, SelectBuilder, SelectItem, SqlBinOp, SqlExpr,
};
use super::model::ParamValue;
use crate::ast::{BinaryOp, Literal, SortDir, UnaryOp};
use crate::typecheck::types::{
    IdentBinding, TypedExpr, TypedExprKind, TypedMethodArg, TypedProjectionItem,
};
use grove_schema::validated::{Field, ValidatedSchema};

pub struct Context<'s> {
    pub schema: &'s ValidatedSchema,
    pub aliases: AliasGen,
}

impl<'s> Context<'s> {
    pub fn new(schema: &'s ValidatedSchema) -> Self {
        Context {
            schema,
            aliases: AliasGen::default(),
        }
    }
}

pub fn lower_query(ctx: &mut Context, result: &TypedExpr) -> (SelectBuilder, Vec<String>) {
    let mut builder = SelectBuilder::new();
    if result.ty.is_scalar() {
        let expr = lower_value(ctx, result);
        let output = ctx.aliases.generic();
        builder.select = vec![SelectItem::Expr {
            expr,
            output: output.clone(),
        }];
        return (builder, vec![output]);
    }
    match &result.kind {
        TypedExprKind::Tuple { .. } => todo!("tuple result"),
        TypedExprKind::Array { .. } => todo!("array result"),
        TypedExprKind::Struct { .. } => todo!("struct literal result"),
        _ => {}
    }
    lower_expr(ctx, result, &mut builder);
    default_select(&mut builder);
    let outputs = builder_outputs(&builder, ctx.schema);
    (builder, outputs)
}

fn lower_expr(ctx: &mut Context, expr: &TypedExpr, builder: &mut SelectBuilder) {
    match &expr.kind {
        TypedExprKind::Projection { base, items } => {
            if !builder.select.is_empty()
                || !builder.where_.is_empty()
                || !builder.order_by.is_empty()
            {
                let sub = ctx.aliases.subquery();
                let mut child = SelectBuilder::new();
                set_select(ctx, &mut child, items);
                lower_expr(ctx, base, &mut child);
                default_select(&mut child);
                attach_child(builder, child, sub);
            } else {
                set_select(ctx, builder, items);
                lower_expr(ctx, base, builder);
            }
        }
        TypedExprKind::Method {
            base,
            name,
            args,
            optional,
        } => {
            if *optional {
                todo!("optional-chained method");
            }
            match name.as_str() {
                "filter" => {
                    builder.where_.push(lower_value(ctx, &args[0].expr));
                    lower_expr(ctx, base, builder);
                }
                "sort" | "sort_asc" | "sort_desc" => {
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
                        builder
                            .order_by
                            .extend(lower_order_key(ctx, &arg.expr, dir));
                    }
                    lower_expr(ctx, base, builder);
                }
                // TODO: prove depth parity with typechecker
                "take" | "skip" | "first" | "nth" => {
                    if !builder.where_.is_empty() || !builder.order_by.is_empty() {
                        let sub = ctx.aliases.subquery();
                        let mut child = SelectBuilder::new();
                        compose_limit(ctx, &mut child, &name.value, args);
                        lower_expr(ctx, base, &mut child);
                        default_select(&mut child);
                        attach_child(builder, child, sub);
                    } else {
                        compose_limit(ctx, builder, &name.value, args);
                        lower_expr(ctx, base, builder);
                    }
                }
                _ => todo!("method `{}`", name.value),
            }
        }
        TypedExprKind::Ident { binding, .. } => match binding {
            IdentBinding::Root { root_idx } => {
                let schema = ctx.schema;
                let root = &schema.roots[*root_idx];
                let struct_ = &schema.structs[root.struct_id.index()];
                let table = &schema.tables[struct_.table.index()];
                let alias = ctx.aliases.table(&table.name);
                debug_assert!(builder.from.is_none());
                builder.from = Some(FromClause::Table {
                    table: table.name.clone(),
                    alias,
                });
            }
            _ => todo!("bare non-root identifier as query"),
        },
        _ => todo!("{} as query", kind_noun(expr)),
    }
}

fn default_select(builder: &mut SelectBuilder) {
    if builder.select.is_empty() {
        builder.select = vec![SelectItem::AllFrom { depth: 0 }];
    }
}

fn attach_child(builder: &mut SelectBuilder, child: SelectBuilder, sub: String) {
    debug_assert!(builder.from.is_none());
    builder.from = Some(FromClause::Subquery {
        query: Box::new(child),
        alias: sub,
    });
}

fn builder_outputs(builder: &SelectBuilder, schema: &ValidatedSchema) -> Vec<String> {
    let explicit = !builder
        .select
        .iter()
        .any(|item| matches!(item, SelectItem::AllFrom { .. }));
    if explicit {
        return builder
            .select
            .iter()
            .filter_map(|item| match item {
                SelectItem::Column { output, .. } | SelectItem::Expr { output, .. } => {
                    Some(output.clone())
                }
                SelectItem::AllFrom { .. } => None,
            })
            .collect();
    }
    match &builder.from {
        Some(FromClause::Table { table, .. }) => schema
            .tables
            .iter()
            .find(|t| &t.name == table)
            .expect("lowered from a known table")
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect(),
        Some(FromClause::Subquery { query, .. }) => builder_outputs(query, schema),
        None => unreachable!("select without row source"),
    }
}

fn compose_limit(
    ctx: &mut Context,
    builder: &mut SelectBuilder,
    method: &str,
    args: &[TypedMethodArg],
) {
    match method {
        "take" => {
            let n = non_negative_guard_sql(ctx, &args[0].expr, method);
            take_r(&mut builder.limit, &builder.offset, n);
        }
        "skip" => {
            let n = non_negative_guard_sql(ctx, &args[0].expr, method);
            skip_r(&mut builder.offset, n);
        }
        "first" => {
            take_r(
                &mut builder.limit,
                &builder.offset,
                SqlExpr::Param(ParamValue::Int(1)),
            );
        }
        "nth" => {
            let n = non_negative_guard_sql(ctx, &args[0].expr, method);
            take_r(
                &mut builder.limit,
                &builder.offset,
                SqlExpr::Param(ParamValue::Int(1)),
            );
            skip_r(&mut builder.offset, n);
        }
        _ => unreachable!("compose_limit called for a non-limit method"),
    }
}

fn take_r(limit: &mut Option<SqlExpr>, offset: &Option<SqlExpr>, n: SqlExpr) {
    let capped = match offset {
        Some(o) => max_sql(sub_sql(n, o.clone()), SqlExpr::Param(ParamValue::Int(0))),
        None => n,
    };
    *limit = Some(match limit.take() {
        Some(l) => min_sql(l, capped),
        None => capped,
    });
}

fn skip_r(offset: &mut Option<SqlExpr>, n: SqlExpr) {
    *offset = Some(match offset.take() {
        Some(o) => add_sql(o, n),
        None => n,
    });
}

fn sub_sql(a: SqlExpr, b: SqlExpr) -> SqlExpr {
    SqlExpr::Func {
        name: "$$sub".to_string(),
        args: vec![a, b],
    }
}

fn add_sql(a: SqlExpr, b: SqlExpr) -> SqlExpr {
    SqlExpr::Func {
        name: "$$add".to_string(),
        args: vec![a, b],
    }
}

fn max_sql(a: SqlExpr, b: SqlExpr) -> SqlExpr {
    SqlExpr::Func {
        name: "max".to_string(),
        args: vec![a, b],
    }
}

fn min_sql(a: SqlExpr, b: SqlExpr) -> SqlExpr {
    SqlExpr::Func {
        name: "min".to_string(),
        args: vec![a, b],
    }
}

fn non_negative_guard_sql(ctx: &mut Context, arg: &TypedExpr, method: &str) -> SqlExpr {
    let value = lower_value(ctx, arg);
    SqlExpr::Case {
        arms: vec![(
            SqlExpr::Binary {
                op: SqlBinOp::Ge,
                lhs: Box::new(value.clone()),
                rhs: Box::new(SqlExpr::Param(ParamValue::Int(0))),
            },
            value,
        )],
        else_: Box::new(SqlExpr::Func {
            name: "$$panic".to_string(),
            args: vec![SqlExpr::Param(ParamValue::String(format!(
                "{method} requires a non-negative integer"
            )))],
        }),
    }
}

fn lower_order_key(ctx: &mut Context, expr: &TypedExpr, dir: OrderDir) -> Vec<OrderItem> {
    match &expr.kind {
        TypedExprKind::Tuple { elements, .. } => elements
            .iter()
            .flat_map(|element| lower_order_key(ctx, element, dir))
            .collect(),
        TypedExprKind::Ident { binding, depth, .. } => bound_column_names(ctx.schema, binding)
            .into_iter()
            .map(|column| OrderItem {
                expr: SqlExpr::Column {
                    depth: *depth,
                    column,
                },
                dir,
            })
            .collect(),
        _ => vec![OrderItem {
            expr: lower_value(ctx, expr),
            dir,
        }],
    }
}

fn lower_value(ctx: &mut Context, expr: &TypedExpr) -> SqlExpr {
    match &expr.kind {
        TypedExprKind::Binary { op, lhs, rhs } => {
            let lhs_sql = lower_value(ctx, lhs);
            let rhs_sql = lower_value(ctx, rhs);
            match op.value {
                BinaryOp::Eq | BinaryOp::Ne => {
                    if lhs.ty.is_optional() || rhs.ty.is_optional() {
                        return SqlExpr::Binary {
                            op: if op.value == BinaryOp::Eq {
                                SqlBinOp::Is
                            } else {
                                SqlBinOp::IsNot
                            },
                            lhs: Box::new(lhs_sql),
                            rhs: Box::new(rhs_sql),
                        };
                    }
                    SqlExpr::Binary {
                        op: bin_op(op.value),
                        lhs: Box::new(lhs_sql),
                        rhs: Box::new(rhs_sql),
                    }
                }
                BinaryOp::Lt
                | BinaryOp::Gt
                | BinaryOp::Le
                | BinaryOp::Ge
                | BinaryOp::And
                | BinaryOp::Or => SqlExpr::Binary {
                    op: bin_op(op.value),
                    lhs: Box::new(lhs_sql),
                    rhs: Box::new(rhs_sql),
                },
                BinaryOp::In => todo!("`in` membership test"),
                BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::Rem
                | BinaryOp::Mod => todo!("arithmetic"),
            }
        }
        TypedExprKind::Unary { op, expr: operand } => match op.value {
            UnaryOp::Not => SqlExpr::Not(Box::new(lower_value(ctx, operand))),
            UnaryOp::Neg => todo!("negation"),
        },
        TypedExprKind::Ident { binding, depth, .. } => bound_column(ctx.schema, *depth, binding),
        TypedExprKind::Literal(lit) => SqlExpr::Param(literal_param(&lit.value)),
        _ => todo!("{}", kind_noun(expr)),
    }
}

fn bin_op(op: BinaryOp) -> SqlBinOp {
    match op {
        BinaryOp::Eq => SqlBinOp::Eq,
        BinaryOp::Ne => SqlBinOp::Ne,
        BinaryOp::Lt => SqlBinOp::Lt,
        BinaryOp::Gt => SqlBinOp::Gt,
        BinaryOp::Le => SqlBinOp::Le,
        BinaryOp::Ge => SqlBinOp::Ge,
        BinaryOp::And => SqlBinOp::And,
        BinaryOp::Or => SqlBinOp::Or,
        BinaryOp::In
        | BinaryOp::Add
        | BinaryOp::Sub
        | BinaryOp::Mul
        | BinaryOp::Div
        | BinaryOp::Rem
        | BinaryOp::Mod => unreachable!(),
    }
}

fn literal_param(lit: &Literal) -> ParamValue {
    match lit {
        Literal::Int(n) => ParamValue::Int(*n),
        Literal::Float(x) => ParamValue::Float(*x),
        Literal::Dec(d) => ParamValue::Dec(*d),
        Literal::String(s) => ParamValue::String(s.clone()),
        Literal::Bool(b) => ParamValue::Bool(*b),
        Literal::Instant(dt) => ParamValue::Instant(*dt),
        Literal::Duration(td) => ParamValue::Duration(*td),
        Literal::None => ParamValue::Null,
        Literal::Now | Literal::Today(_) => todo!("wall-clock instant"),
    }
}

fn bound_column_names(schema: &ValidatedSchema, binding: &IdentBinding) -> Vec<String> {
    match binding {
        IdentBinding::Field(field_id) => {
            let field = &schema.structs[field_id.struct_id().index()].fields[field_id.local()];
            match field {
                Field::Value { columns, .. } => columns
                    .iter()
                    .map(|column| {
                        schema.tables[column.table_id().index()].columns[column.local()]
                            .name
                            .clone()
                    })
                    .collect(),
                Field::Ref { .. } => todo!("reference field"),
                Field::Array { .. } => todo!("array field"),
            }
        }
        IdentBinding::ProjectedField { name } => vec![name.clone()],
        _ => todo!("non-field identifier"),
    }
}

fn bound_column(schema: &ValidatedSchema, depth: usize, binding: &IdentBinding) -> SqlExpr {
    let mut columns = bound_column_names(schema, binding);
    if columns.len() == 1 {
        SqlExpr::Column {
            depth,
            column: columns.pop().unwrap(),
        }
    } else {
        todo!("multi-column field")
    }
}

fn set_select(ctx: &mut Context, builder: &mut SelectBuilder, items: &[TypedProjectionItem]) {
    for item in items {
        let TypedExprKind::Ident { binding, depth, .. } = &item.value.kind else {
            todo!("{} in projection", kind_noun(&item.value));
        };
        let SqlExpr::Column { column, .. } = bound_column(ctx.schema, *depth, binding) else {
            unreachable!("bound_column only returns columns");
        };
        builder.select.push(SelectItem::Column {
            depth: *depth,
            column,
            output: item.alias.to_string(),
        });
    }
}

fn kind_noun(expr: &TypedExpr) -> &'static str {
    match &expr.kind {
        TypedExprKind::Literal(_) | TypedExprKind::Ident { .. } => "value",
        TypedExprKind::Field { .. } => "field navigation",
        TypedExprKind::Method { .. } => "method call",
        TypedExprKind::Binary { .. } => "binary expression",
        TypedExprKind::Unary { .. } => "unary expression",
        TypedExprKind::Cast { .. } => "cast",
        TypedExprKind::If { .. } => "if expression",
        TypedExprKind::Some { .. } => "some(...)",
        TypedExprKind::Tuple { .. } => "tuple",
        TypedExprKind::Array { .. } => "array",
        TypedExprKind::Struct { .. } => "struct literal",
        TypedExprKind::Projection { .. } => "nested projection",
        TypedExprKind::TypeConstant { .. } => "type constant",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::model::{CompiledQuery, Param};
    use crate::typecheck::typecheck;

    const SCHEMA_SRC: &str = r#"
        root users: User;
        struct User {
            name: String,
            email: String@address,
            age: Int,
            active: Bool,
            nickname: ?String,
            pos: Tuple<Float, Float>@(lat, lng),
        }
    "#;

    fn compile(query: &str) -> CompiledQuery {
        let schema = grove_schema::validate(
            grove_schema::parse_schema(SCHEMA_SRC)
                .0
                .expect("invalid test schema"),
        )
        .0
        .expect("invalid test schema");
        let (file, parse_diags) = crate::parse_query(query);
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors, got {parse_diags:?}"
        );
        let (typed, type_diags) = typecheck(file.unwrap(), &schema);
        assert!(
            type_diags.is_empty(),
            "expected no type errors, got {type_diags:?}"
        );
        crate::codegen::codegen(&typed.unwrap(), &schema)
    }

    fn sql(query: &str) -> (String, Vec<Param>) {
        let mut compiled = compile(query);
        assert_eq!(compiled.statements.len(), 1);
        let stmt = compiled.statements.pop().unwrap();
        (stmt.sql, stmt.params)
    }

    fn int_param(n: i64) -> Param {
        Param::Value(ParamValue::Int(n))
    }

    fn limit_msg(method: &str) -> Param {
        Param::Value(ParamValue::String(format!(
            "{method} requires a non-negative integer"
        )))
    }

    fn case_pos_guard(n: i64, method: &str) -> (String, Vec<Param>) {
        (
            "CASE WHEN ? >= ? THEN ? ELSE \"$$panic\"(?) END".to_string(),
            vec![int_param(n), int_param(0), int_param(n), limit_msg(method)],
        )
    }

    #[test]
    fn flat_projection() {
        let (sql, params) = sql("users { name, age }");
        assert_eq!(
            sql,
            r#"SELECT "$$users0"."name" AS "name", "$$users0"."age" AS "age" FROM "users" AS "$$users0""#
        );
        assert!(params.is_empty());
    }

    #[test]
    fn renamed_column_alias() {
        let (sql, params) = sql("users { email }");
        assert_eq!(
            sql,
            r#"SELECT "$$users0"."address" AS "email" FROM "users" AS "$$users0""#
        );
        assert!(params.is_empty());
    }

    #[test]
    fn filter_arg_compose() {
        let (sql, params) = sql("users[active][age >= 18] { name }");
        assert_eq!(
            sql,
            r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" WHERE ("$$users0"."age" >= ?) AND "$$users0"."active""#
        );
        assert_eq!(params, vec![int_param(18)]);
    }

    #[test]
    fn equality_non_optional() {
        let (sql, params) = sql("users[age == 18] { name }");
        assert_eq!(
            sql,
            r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" WHERE ("$$users0"."age" = ?)"#
        );
        assert_eq!(params, vec![int_param(18)]);
    }

    #[test]
    fn equality_optional_null_tolerant() {
        let (sql_eq, params) = sql("users[nickname == none] { name }");
        assert_eq!(
            sql_eq,
            r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" WHERE ("$$users0"."nickname" IS ?)"#
        );
        assert_eq!(params, vec![Param::Value(ParamValue::Null)]);

        let (sql_ne, _) = sql("users[nickname != nickname] { name }");
        assert_eq!(
            sql_ne,
            r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" WHERE ("$$users0"."nickname" IS NOT "$$users0"."nickname")"#
        );
    }

    #[test]
    fn bool_operations_precedence() {
        let (sql, params) = sql("users[!active || age < 30] { name }");
        assert_eq!(
            sql,
            r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" WHERE (NOT "$$users0"."active" OR ("$$users0"."age" < ?))"#
        );
        assert_eq!(params, vec![int_param(30)]);
    }

    #[test]
    fn sort_take() {
        let (limit_sql, limit_params) = case_pos_guard(10, "take");
        let (sql, params) = sql("users.sort_desc(age).take(10) { name }");
        assert_eq!(
            sql,
            format!(
                r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" ORDER BY "$$users0"."age" DESC LIMIT ({limit_sql})"#
            )
        );
        assert_eq!(params, limit_params);
    }

    #[test]
    fn sort_per_arg_direction() {
        let (sql, params) = sql("users.sort_desc(age, asc name) { name }");
        assert_eq!(
            sql,
            r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" ORDER BY "$$users0"."age" DESC, "$$users0"."name" ASC"#
        );
        assert!(params.is_empty());
    }

    #[test]
    fn sort_tuple_fields() {
        let (sql, params) = sql("users.sort_asc(pos) { name }");
        assert_eq!(
            sql,
            r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" ORDER BY "$$users0"."lat" ASC, "$$users0"."lng" ASC"#
        );
        assert!(params.is_empty());
    }

    #[test]
    fn skip_take_compose() {
        let (take_sql, mut expected) = case_pos_guard(20, "take");
        let (skip_sql, offset_params) = case_pos_guard(10, "skip");
        expected.extend(offset_params);
        let (sql, params) = sql("users.skip(10).take(20) { name }");
        assert_eq!(
            sql,
            format!(
                r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" LIMIT ({take_sql}) OFFSET ({skip_sql})"#
            )
        );
        assert_eq!(params, expected);
    }

    #[test]
    fn take_skip_compose() {
        let (take_sql, mut expected) = case_pos_guard(5, "take");
        let (skip_sql, skip_params) = case_pos_guard(2, "skip");
        expected.extend(skip_params.clone());
        expected.push(int_param(0));
        expected.extend(skip_params);
        let (sql, params) = sql("users.take(5).skip(2) { name }");
        assert_eq!(
            sql,
            format!(
                r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" LIMIT "max"("$$sub"({take_sql}, {skip_sql}), ?) OFFSET ({skip_sql})"#
            )
        );
        assert_eq!(params, expected);
    }

    #[test]
    fn skip_nth_compose() {
        let (nth_limit, mut expected) = (String::from("?"), vec![int_param(1)]);
        let (nth_offset, nth_params) = case_pos_guard(4, "nth");
        expected.extend(nth_params);
        let (skip_offset, skip_params) = case_pos_guard(3, "skip");
        expected.extend(skip_params);
        let (sql, params) = sql("users.skip(3).nth(4)");
        assert_eq!(
            sql,
            format!(
                r#"SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT {nth_limit} OFFSET "$$add"({nth_offset}, {skip_offset})"#
            )
        );
        assert_eq!(params, expected);
    }

    #[test]
    fn take_nth_compose() {
        let mut expected = vec![int_param(1)];
        let (take_sql, take_params) = case_pos_guard(2, "take");
        expected.extend(take_params);
        let (nth_sql, nth_params) = case_pos_guard(4, "nth");
        expected.extend(nth_params.clone());
        expected.push(int_param(0));
        expected.extend(nth_params);

        let (sql, params) = sql("users.take(2).nth(4)");
        assert_eq!(
            sql,
            format!(
                r#"SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT "min"(?, "max"("$$sub"({take_sql}, {nth_sql}), ?)) OFFSET ({nth_sql})"#
            )
        );
        assert_eq!(params, expected);
    }

    #[test]
    fn take_skip_compose_empty() {
        let (take_sql, mut expected) = case_pos_guard(1, "take");
        let (skip_sql, skip_params) = case_pos_guard(2, "skip");
        expected.extend(skip_params.clone());
        expected.push(int_param(0));
        expected.extend(skip_params);
        let (sql, params) = sql("users.take(1).skip(2) { name }");
        assert_eq!(
            sql,
            format!(
                r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" LIMIT "max"("$$sub"({take_sql}, {skip_sql}), ?) OFFSET ({skip_sql})"#
            )
        );
        assert_eq!(params, expected);
    }

    #[test]
    fn take_take_compose() {
        let (first, mut expected) = case_pos_guard(5, "take");
        let (second, second_params) = case_pos_guard(30, "take");
        expected.extend(second_params);
        let (sql, params) = sql("users.take(30).take(5) { name }");
        assert_eq!(
            sql,
            format!(
                r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" LIMIT "min"({first}, {second})"#
            )
        );
        assert_eq!(params, expected);
    }

    #[test]
    fn skip_skip_compose() {
        let (first, mut expected) = case_pos_guard(20, "skip");
        let (second, second_params) = case_pos_guard(10, "skip");
        expected.extend(second_params);
        let (sql, params) = sql("users.skip(10).skip(20) { name }");
        assert_eq!(
            sql,
            format!(
                r#"SELECT "$$users0"."name" AS "name" FROM "users" AS "$$users0" LIMIT -1 OFFSET "$$add"({first}, {second})"#
            )
        );
        assert_eq!(params, expected);
    }

    #[test]
    fn take_first_compose() {
        let (take_sql, mut expected) = case_pos_guard(3, "take");
        expected.insert(0, int_param(1));
        let (sql, params) = sql("users.take(3).first()");
        assert_eq!(
            sql,
            format!(r#"SELECT "$$users0".* FROM "users" AS "$$users0" LIMIT "min"(?, {take_sql})"#)
        );
        assert_eq!(params, expected);
    }

    #[test]
    fn take_sort_subquery_boundary() {
        let (inner_limit, mut expected) = case_pos_guard(20, "take");
        let (outer_limit, outer_params) = case_pos_guard(10, "take");
        expected.extend(outer_params);
        let (sql, params) = sql("users.sort_desc(age).take(20).sort_asc(name).take(10)");
        assert_eq!(
            sql,
            format!(
                r#"SELECT "$$sub0".* FROM (SELECT "$$users1".* FROM "users" AS "$$users1" ORDER BY "$$users1"."age" DESC LIMIT ({inner_limit})) AS "$$sub0" ORDER BY "$$sub0"."name" ASC LIMIT ({outer_limit})"#
            )
        );
        assert_eq!(params, expected);
    }

    #[test]
    fn filter_projection_subquery_boundary() {
        let (sql, params) = sql(r#"users { name }[name == "x"]"#);
        assert_eq!(
            sql,
            r#"SELECT "$$sub0".* FROM (SELECT "$$users1"."name" AS "name" FROM "users" AS "$$users1") AS "$$sub0" WHERE ("$$sub0"."name" = ?)"#
        );
        assert_eq!(
            params,
            vec![Param::Value(ParamValue::String("x".to_string()))]
        );
    }

    #[test]
    fn chained_projection_subquery_boundary() {
        let (sql, params) = sql("users { name } { name }");
        assert_eq!(
            sql,
            r#"SELECT "$$sub0"."name" AS "name" FROM (SELECT "$$users1"."name" AS "name" FROM "users" AS "$$users1") AS "$$sub0""#
        );
        assert!(params.is_empty());
    }

    #[test]
    fn bare_literal_result() {
        let (sql, params) = sql("0");
        assert_eq!(sql, r#"SELECT ? AS "$$val0""#);
        assert_eq!(params, vec![int_param(0)]);
    }

    #[test]
    fn result_shape_output_names() {
        let compiled = compile("users[active].sort_asc(name).take(5) { name, age }");
        assert_eq!(
            compiled.result,
            crate::codegen::model::ResultShape::Rows {
                columns: vec!["name".to_string(), "age".to_string()],
            }
        );
        assert_eq!(compiled.statements[0].shape.columns.len(), 2);
    }
}
