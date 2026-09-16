use super::builder::{FromClause, OrderDir, SelectBuilder, SelectItem, SqlBinOp, SqlExpr};
use super::model::Param;

pub fn render_select(
    builder: &SelectBuilder,
    stack: &mut Vec<String>,
    sql: &mut String,
    params: &mut Vec<Param>,
) {
    let pushed = if let Some(from) = &builder.from {
        stack.push(from.alias().to_string());
        true
    } else {
        false
    };
    sql.push_str("SELECT ");
    for (i, item) in builder.select.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        match item {
            SelectItem::AllFrom { depth } => {
                sql.push_str(&quote(resolve(stack, *depth)));
                sql.push_str(".*");
            }
            SelectItem::Column {
                depth,
                column,
                output,
            } => {
                sql.push_str(&quote(resolve(stack, *depth)));
                sql.push('.');
                sql.push_str(&quote(column));
                if output != column {
                    sql.push_str(" AS ");
                    sql.push_str(&quote(output));
                }
            }
            SelectItem::Expr { expr, output } => {
                render_expr(expr, stack, true, sql, params);
                sql.push_str(" AS ");
                sql.push_str(&quote(output));
            }
        }
    }
    if let Some(from) = &builder.from {
        sql.push_str(" FROM ");
        render_from(from, stack, sql, params);
    }
    if !builder.where_.is_empty() {
        sql.push_str(" WHERE ");
        for (i, cond) in builder.where_.iter().enumerate() {
            if i > 0 {
                sql.push_str(" AND ");
            }
            render_expr(cond, stack, true, sql, params);
        }
    }
    if !builder.order_by.is_empty() {
        sql.push_str(" ORDER BY ");
        for (i, item) in builder.order_by.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            render_expr(&item.expr, stack, false, sql, params);
            match item.dir {
                OrderDir::Asc => sql.push_str(" ASC"),
                OrderDir::Desc => sql.push_str(" DESC"),
            }
        }
    }
    match (&builder.limit, &builder.offset) {
        (Some(limit), offset) => {
            sql.push_str(" LIMIT ");
            render_expr(limit, stack, true, sql, params);
            if let Some(offset) = offset {
                sql.push_str(" OFFSET ");
                render_expr(offset, stack, true, sql, params);
            }
        }
        (None, Some(offset)) => {
            sql.push_str(" LIMIT -1 OFFSET ");
            render_expr(offset, stack, true, sql, params);
        }
        (None, None) => {}
    }
    if pushed {
        let _ = stack.pop();
    }
}

fn resolve(stack: &[String], depth: usize) -> &str {
    &stack[stack.len() - 1 - depth]
}

fn render_from(
    from: &FromClause,
    stack: &mut Vec<String>,
    sql: &mut String,
    params: &mut Vec<Param>,
) {
    match from {
        FromClause::Table { table, alias } => {
            sql.push_str(&quote(table));
            sql.push_str(" AS ");
            sql.push_str(&quote(alias));
        }
        FromClause::Subquery { query, alias } => {
            sql.push('(');
            render_select(query, stack, sql, params);
            sql.push_str(") AS ");
            sql.push_str(&quote(alias));
        }
    }
}

fn render_expr(
    expr: &SqlExpr,
    stack: &mut Vec<String>,
    nested: bool,
    sql: &mut String,
    params: &mut Vec<Param>,
) {
    match expr {
        SqlExpr::Column { depth, column } => {
            sql.push_str(&quote(resolve(stack, *depth)));
            sql.push('.');
            sql.push_str(&quote(column));
        }
        SqlExpr::Param(value) => {
            sql.push('?');
            params.push(Param::Value(value.clone()));
        }
        SqlExpr::Binary { op, lhs, rhs } => {
            if nested {
                sql.push('(');
            }
            render_expr(lhs, stack, true, sql, params);
            sql.push_str(match op {
                SqlBinOp::Eq => " = ",
                SqlBinOp::Ne => " != ",
                SqlBinOp::Lt => " < ",
                SqlBinOp::Gt => " > ",
                SqlBinOp::Le => " <= ",
                SqlBinOp::Ge => " >= ",
                SqlBinOp::And => " AND ",
                SqlBinOp::Or => " OR ",
                SqlBinOp::Is => " IS ",
                SqlBinOp::IsNot => " IS NOT ",
            });
            render_expr(rhs, stack, true, sql, params);
            if nested {
                sql.push(')');
            }
        }
        SqlExpr::Not(operand) => {
            sql.push_str("NOT ");
            render_expr(operand, stack, true, sql, params);
        }
        SqlExpr::Func { name, args } => {
            sql.push_str(&quote(name));
            sql.push('(');
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                render_expr(arg, stack, false, sql, params);
            }
            sql.push(')');
        }
        SqlExpr::Case { arms, else_ } => {
            if nested {
                sql.push('(');
            }
            sql.push_str("CASE");
            for (cond, then) in arms {
                sql.push_str(" WHEN ");
                render_expr(cond, stack, false, sql, params);
                sql.push_str(" THEN ");
                render_expr(then, stack, false, sql, params);
            }
            sql.push_str(" ELSE ");
            render_expr(else_, stack, false, sql, params);
            sql.push_str(" END");
            if nested {
                sql.push(')');
            }
        }
        SqlExpr::In { expr, items } => {
            if nested {
                sql.push('(');
            }
            render_expr(expr, stack, true, sql, params);
            sql.push_str(" IN (");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                render_expr(item, stack, false, sql, params);
            }
            sql.push(')');
            if nested {
                sql.push(')');
            }
        }
        SqlExpr::Cast { expr, target } => {
            sql.push_str("CAST(");
            render_expr(expr, stack, false, sql, params);
            sql.push_str(" AS ");
            sql.push_str(target);
            sql.push(')');
        }
    }
}

pub fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::super::builder::{OrderItem, SelectBuilder};
    use super::super::model::ParamValue;
    use super::*;

    fn test_stack() -> Vec<String> {
        vec!["$$users0".to_string()]
    }

    fn simple_builder() -> SelectBuilder {
        SelectBuilder {
            from: Some(FromClause::Table {
                table: "users".to_string(),
                alias: "$$users0".to_string(),
            }),
            select: vec![SelectItem::Column {
                depth: 0,
                column: "name".to_string(),
                output: "name".to_string(),
            }],
            where_: vec![SqlExpr::Binary {
                op: SqlBinOp::Gt,
                lhs: Box::new(SqlExpr::Column {
                    depth: 0,
                    column: "age".to_string(),
                }),
                rhs: Box::new(SqlExpr::Param(ParamValue::Int(18))),
            }],
            order_by: vec![OrderItem {
                expr: SqlExpr::Column {
                    depth: 0,
                    column: "name".to_string(),
                },
                dir: OrderDir::Asc,
            }],
            limit: Some(SqlExpr::Param(ParamValue::Int(10))),
            offset: None,
        }
    }

    #[test]
    fn flat_select() {
        let mut sql = String::new();
        let mut params = Vec::new();
        render_select(&simple_builder(), &mut test_stack(), &mut sql, &mut params);
        assert_eq!(
            sql,
            r#"SELECT "$$users0"."name" FROM "users" AS "$$users0" WHERE ("$$users0"."age" > ?) ORDER BY "$$users0"."name" ASC LIMIT ?"#
        );
        assert_eq!(
            params,
            vec![
                Param::Value(ParamValue::Int(18)),
                Param::Value(ParamValue::Int(10)),
            ]
        );
    }

    #[test]
    fn nested_binary_gets_parens() {
        let expr = SqlExpr::Binary {
            op: SqlBinOp::Or,
            lhs: Box::new(SqlExpr::Binary {
                op: SqlBinOp::And,
                lhs: Box::new(SqlExpr::Param(ParamValue::Bool(true))),
                rhs: Box::new(SqlExpr::Param(ParamValue::Bool(false))),
            }),
            rhs: Box::new(SqlExpr::Not(Box::new(SqlExpr::Param(ParamValue::Bool(
                true,
            ))))),
        };
        let mut sql = String::new();
        let mut params = Vec::new();
        render_expr(&expr, &mut Vec::new(), false, &mut sql, &mut params);
        assert_eq!(sql, "(? AND ?) OR NOT ?");
    }

    #[test]
    fn stack_depth_resolves_outer_alias() {
        let mut stack = vec!["$$users0".to_string(), "$$users1".to_string()];
        let expr = SqlExpr::Column {
            depth: 1,
            column: "manager_id".to_string(),
        };
        let mut sql = String::new();
        let mut params = Vec::new();
        render_expr(&expr, &mut stack, false, &mut sql, &mut params);
        assert_eq!(sql, r#""$$users0"."manager_id""#);
    }

    #[test]
    fn multi_arm_case() {
        let expr = SqlExpr::Case {
            arms: vec![
                (
                    SqlExpr::Param(ParamValue::Bool(true)),
                    SqlExpr::Param(ParamValue::Int(1)),
                ),
                (
                    SqlExpr::Param(ParamValue::Bool(false)),
                    SqlExpr::Param(ParamValue::Int(2)),
                ),
            ],
            else_: Box::new(SqlExpr::Param(ParamValue::Int(3))),
        };
        let mut sql = String::new();
        let mut params = Vec::new();
        render_expr(&expr, &mut Vec::new(), false, &mut sql, &mut params);
        assert_eq!(sql, "CASE WHEN ? THEN ? WHEN ? THEN ? ELSE ? END");
    }

    #[test]
    fn limit_default_minus_one() {
        let mut builder = simple_builder();
        builder.limit = None;
        builder.offset = Some(SqlExpr::Param(ParamValue::Int(5)));
        let mut sql = String::new();
        let mut params = Vec::new();
        render_select(&builder, &mut test_stack(), &mut sql, &mut params);
        assert!(sql.ends_with("LIMIT -1 OFFSET ?"));
    }
}
