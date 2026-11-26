use super::infer;
use crate::ast::Expr;
use crate::typecheck::env::TypeEnv;
use crate::typecheck::error::TypeError;
use crate::typecheck::types::{
    QueryType, RecordSource, TargetProvenance, TypedExpr, TypedExprKind,
};
use grove_types::Span;

pub(crate) fn infer_row_ref(
    expr: &Expr,
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    if !env.allow_row_refs {
        return Err(TypeError::RowRefNotAllowed { span });
    }
    let typed = infer(expr, env)?;
    let ty = row_ref_shape(&typed.ty, typed.provenance).ok_or_else(|| {
        TypeError::RowRefSourceNotTraceable {
            got: typed.ty.to_string(),
            span,
        }
    })?;
    Ok(env.typed(
        TypedExprKind::RowRef {
            expr: Box::new(typed),
        },
        ty,
        span,
    ))
}

/// Map traceable schema records to RowRefs through optional and list layers
/// (apply `&`-operation)
pub(crate) fn row_ref_shape(ty: &QueryType, provenance: TargetProvenance) -> Option<QueryType> {
    match ty {
        QueryType::Record(RecordSource::Schema(struct_id)) if provenance.rows => {
            Some(QueryType::RowRef(*struct_id))
        }
        QueryType::Optional(inner) => {
            row_ref_shape(inner, provenance).map(|inner| QueryType::Optional(Box::new(inner)))
        }
        QueryType::List(inner) => {
            row_ref_shape(inner, provenance).map(|inner| QueryType::List(Box::new(inner)))
        }
        _ => None,
    }
}

pub(crate) fn infer_mutation_arg(expr: &Expr, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    let previous = env.allow_row_refs;
    env.allow_row_refs = true;
    let result = infer(expr, env);
    env.allow_row_refs = previous;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::tests::{
        error_codes, infer_err_with_row_ref, infer_ok_with_row_ref, nested_schema, nested_struct,
        test_schema,
    };

    #[test]
    fn row_ref_type_equality_display() {
        let schema = test_schema();
        let user_id = schema.roots[0].struct_id;
        let ty = QueryType::RowRef(user_id);
        assert_eq!(ty.to_string(), format!("&Struct#{}", user_id.index()));
        assert!(!ty.is_unknown());
        assert!(ty.eq(&QueryType::RowRef(user_id), &schema));
        assert!(!ty.eq(&QueryType::Record(RecordSource::Schema(user_id)), &schema));
    }

    #[test]
    fn row_ref_only_inside_mutation_args() {
        let schema = test_schema();
        assert_eq!(error_codes("&users", &schema), vec!["QT0040"]);
        assert_eq!(error_codes("let r = &users;", &schema), vec!["QT0040"]);
        assert_eq!(
            error_codes("users.filter(&users).len()", &schema),
            vec!["QT0040"]
        );
        assert_eq!(
            error_codes(
                "users.update({ active = &users.first().unwrap() });",
                &schema
            ),
            ["QT0024"]
        );
    }

    #[test]
    fn row_ref_shapes() {
        let schema = nested_schema();
        let user = nested_struct(&schema, "User");
        let order = nested_struct(&schema, "Order");

        assert_eq!(
            infer_ok_with_row_ref("&users", &schema, true).ty,
            QueryType::List(Box::new(QueryType::RowRef(user)))
        );
        assert_eq!(
            infer_ok_with_row_ref("&users.first()", &schema, true).ty,
            QueryType::Optional(Box::new(QueryType::RowRef(user)))
        );
        assert_eq!(
            infer_ok_with_row_ref("(&users).first()", &schema, true).ty,
            QueryType::Optional(Box::new(QueryType::RowRef(user)))
        );
        assert_eq!(
            infer_ok_with_row_ref("&users.first().unwrap()", &schema, true).ty,
            QueryType::RowRef(user)
        );
        assert_eq!(
            infer_ok_with_row_ref("&users.subordinates", &schema, true).ty,
            QueryType::List(Box::new(QueryType::List(Box::new(QueryType::RowRef(user)))))
        );
        assert_eq!(
            infer_ok_with_row_ref("&users.orders.flatten()", &schema, true).ty,
            QueryType::List(Box::new(QueryType::RowRef(order)))
        );
        assert_eq!(
            infer_ok_with_row_ref("(&users.orders).flatten()", &schema, true).ty,
            QueryType::List(Box::new(QueryType::RowRef(order)))
        );
        assert!(matches!(
            infer_ok_with_row_ref("&users", &schema, true).kind,
            TypedExprKind::RowRef { .. }
        ));
    }

    #[test]
    fn row_ref_opaque() {
        let schema = nested_schema();
        for source in [
            "(&users.first().unwrap()).name",
            "(&users).unique()",
            "(&users).contains(&users.first().unwrap())",
            "(&users).filter(true)",
            "(&users).map(true)",
            "(&users.first()).map(true)",
            "(&users).subordinates",
            "&users == &users",
        ] {
            assert_eq!(
                infer_err_with_row_ref(source, &schema, true),
                ["QT0042"],
                "{source}"
            );
        }
    }

    #[test]
    fn row_ref_shape_preserving_operations_allowed() {
        let schema = nested_schema();
        for source in [
            "(&users).take(2)",
            "(&users).skip(1)",
            "(&users).take_back(1)",
            "(&users).skip_back(1)",
            "(&users).reverse()",
            "(&users).first()",
            "(&users).nth(0)",
            "(&users).last()",
            "(&users).len()",
            "(&users).extend(&users.take(1))",
            "&users.take(2)",
            "(&users.first()).unwrap()",
            "(&users.first()).is_some()",
            "(&users.first()).is_none()",
            "(&users.first()).unwrap_or(&users.first().unwrap())",
        ] {
            infer_ok_with_row_ref(source, &schema, true);
        }
    }

    #[test]
    fn row_ref_untraceable_source_error() {
        let schema = nested_schema();
        for source in [
            r#"users.update({ active = &users { name } });"#,
            r#"let u = users; users.update({ active = &u });"#,
            r#"users.update({ active = & &users });"#,
            r#"users.update({ active = &1 });"#,
        ] {
            assert_eq!(error_codes(source, &schema), ["QT0041"], "{source}");
        }
    }
}
