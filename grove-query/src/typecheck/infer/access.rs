use crate::typecheck::env::{TypeEnv, record_field_by_name, record_field_provenance};
use crate::typecheck::error::TypeError;
use crate::typecheck::infer::method::row_ref_container_leaf;
use crate::typecheck::types::{QueryType, TypedExpr, TypedExprKind};

use grove_types::Spanned;

pub(crate) fn infer_field(
    base: TypedExpr,
    name: &Spanned<String>,
    opt_depth: usize,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let schema = env.schema();
    let lookup_ty = base.ty.unwrap_optional_depth(opt_depth).ok_or_else(|| {
        TypeError::OptionalDepthMismatch {
            depth: opt_depth,
            ty: base.ty.to_string(),
            span: name.span,
        }
    })?;

    let (inner, was_list) = match lookup_ty {
        QueryType::List(inner) => (inner.as_ref(), true),
        other => (other, false),
    };

    if row_ref_container_leaf(inner) {
        return Err(TypeError::RowRefOperationNotAllowed {
            operation: format!("field access `.{}`", name.value),
            span: name.span,
        });
    }

    let mut field_ty =
        record_field_by_name(inner, name, schema).ok_or_else(|| TypeError::UnknownField {
            field: name.value.clone(),
            base_ty: base.ty.to_string(),
            span: name.span,
        })?;
    let provenance = record_field_provenance(inner, name, schema);

    if was_list {
        field_ty = QueryType::List(Box::new(field_ty));
    }
    let field_ty = field_ty.wrap_optional_depth(opt_depth);

    let span = base.span.merge(name.span);
    Ok(env.typed_with_provenance(
        TypedExprKind::Field {
            base: Box::new(base),
            name: name.clone(),
            opt_depth,
        },
        field_ty,
        provenance,
        span,
    ))
}

pub(crate) fn infer_tuple_access(
    base: TypedExpr,
    index: &Spanned<i64>,
    opt_depth: usize,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let lookup_ty = base.ty.unwrap_optional_depth(opt_depth).ok_or_else(|| {
        TypeError::OptionalDepthMismatch {
            depth: opt_depth,
            ty: base.ty.to_string(),
            span: index.span,
        }
    })?;
    let QueryType::Tuple(elems) = lookup_ty else {
        return Err(TypeError::TupleAccessOnNonTuple {
            base_ty: base.ty.to_string(),
            span: index.span,
        });
    };
    let pos = index.value as usize;
    if pos >= elems.len() {
        return Err(TypeError::TupleIndexOutOfRange {
            index: pos,
            len: elems.len(),
            span: index.span,
        });
    }
    let elem_ty = elems[pos].clone().wrap_optional_depth(opt_depth);
    let span = base.span.merge(index.span);
    Ok(env.typed(
        TypedExprKind::TupleAccess {
            base: Box::new(base),
            index: pos,
            opt_depth,
        },
        elem_ty,
        span,
    ))
}

pub(crate) fn element_ty(ty: &QueryType) -> QueryType {
    match ty {
        QueryType::List(inner) | QueryType::Optional(inner) => inner.as_ref().clone(),
        _ => QueryType::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::infer::infer;
    use crate::typecheck::tests::{enum_schema, infer_ok, test_schema};
    use grove_schema::validated::ScalarType;

    #[test]
    fn field_access_struct_list() {
        assert_eq!(
            infer_ok("users.name", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
        );
    }

    #[test]
    fn field_access_unknown_field() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.unknown");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn field_access_optional_value() {
        assert!(
            matches!(infer_ok("users.profile", &test_schema()).ty, QueryType::List(inner) if matches!(*inner, QueryType::Optional(_)))
        );
    }

    #[test]
    fn field_access_enum_value() {
        let schema = enum_schema();
        let priority_id = schema.enum_by_name("Priority").unwrap().0;
        assert_eq!(
            infer_ok("tickets.first().unwrap().priority", &schema).ty,
            QueryType::Enum(priority_id)
        );
    }

    #[test]
    fn optional_field_access() {
        assert_eq!(
            infer_ok("users[age % 5 == 0].first()?.age", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
    }

    #[test]
    fn deep_optional_tuple_access() {
        assert_eq!(
            infer_ok("some(some((1, 2)))??.1", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
                .wrap_optional()
                .wrap_optional()
        );
    }

    #[test]
    fn deep_optional_field_and_method() {
        assert_eq!(
            infer_ok(
                r#"some(some({name = "Ada"}))??.name??.to_upper()"#,
                &test_schema()
            )
            .ty,
            QueryType::Scalar(ScalarType::String)
                .wrap_optional()
                .wrap_optional()
        );
    }

    #[test]
    fn tuple_access() {
        let schema = test_schema();
        assert_eq!(
            infer_ok("(\"a\", 1).0", &schema).ty,
            QueryType::Scalar(ScalarType::String)
        );
        assert_eq!(
            infer_ok("(\"a\", 1).1", &schema).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn tuple_access_chained() {
        assert_eq!(
            infer_ok("((\"a\", 1), 2).0.1", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn tuple_access_optional() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("(\"a\", 1)?.0");
        assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
    }

    #[test]
    fn tuple_access_out_of_range() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        for query in ["(\"a\", 1).2", "(\"a\", 1).1000"] {
            let (file, _diags) = crate::parse_query(query);
            assert!(
                infer(file.unwrap().output().unwrap(), &mut env).is_err(),
                "{query}"
            );
        }
    }

    #[test]
    fn tuple_access_on_non_tuple() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        for query in ["\"a\".0", "users.0", "[1, 2].0"] {
            let (file, _diags) = crate::parse_query(query);
            assert!(
                infer(file.unwrap().output().unwrap(), &mut env).is_err(),
                "{query}"
            );
        }
    }

    #[test]
    fn tuple_access_optional_tuple() {
        assert_eq!(
            infer_ok("[(\"a\", 1)].first()?.0", &test_schema()).ty,
            QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::String)))
        );
    }

    #[test]
    fn tuple_access_optional_schema_field() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        assert!(matches!(
            infer_ok("users { x = maybe_pos?.0 }", &schema).ty,
            QueryType::List(_)
        ));
        let (file, _diags) = crate::parse_query("users { x = maybe_pos.0 }");
        assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
    }

    #[test]
    fn tuple_access_over_scope() {
        assert!(matches!(
            infer_ok("users { x = (\"a\", age).1 }", &test_schema()).ty,
            QueryType::List(_)
        ));
    }
}
