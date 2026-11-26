use super::infer;
use crate::ast::Arg;
use crate::methods::{self, ArgCount, ArgSpec, MethodId, ScopeSpec};
use crate::typecheck::compat::types_compatible;
use crate::typecheck::env::TypeEnv;
use crate::typecheck::error::TypeError;
use crate::typecheck::infer::access::element_ty;
use crate::typecheck::types::{
    QueryType, TargetProvenance, TypedExpr, TypedExprKind, TypedMethodArg,
};
use grove_types::Spanned;

pub(crate) fn infer_method(
    base: TypedExpr,
    name: &Spanned<String>,
    args: &[Arg],
    opt_depth: usize,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let resolve_ty = base.ty.unwrap_optional_depth(opt_depth).ok_or_else(|| {
        TypeError::OptionalDepthMismatch {
            depth: opt_depth,
            ty: base.ty.to_string(),
            span: name.span,
        }
    })?;

    let def = methods::resolve(resolve_ty, name).ok_or_else(|| TypeError::UnknownMethod {
        method: name.value.clone(),
        base_ty: base.ty.to_string(),
        span: name.span,
    })?;

    if resolve_ty.contains_row_ref() && !row_ref_method_allowed(def.id) {
        return Err(TypeError::RowRefOperationNotAllowed {
            operation: name.value.clone(),
            span: name.span,
        });
    }

    let mut fixed_args = def.fixed_arg_types(resolve_ty);

    match &def.args {
        ArgSpec::Fixed(_) => {
            if args.len() != fixed_args.len() {
                return Err(TypeError::WrongArgCount {
                    method: name.value.clone(),
                    expected: fixed_args.len(),
                    got: args.len(),
                    span: name.span,
                });
            }
        }
        ArgSpec::Scoped { count, .. } => match count {
            ArgCount::Exact(n) => {
                if args.len() != *n {
                    return Err(TypeError::WrongArgCount {
                        method: name.value.clone(),
                        expected: *n,
                        got: args.len(),
                        span: name.span,
                    });
                }
            }
            ArgCount::AtLeast(min) => {
                if args.len() < *min {
                    return Err(TypeError::WrongArgCount {
                        method: name.value.clone(),
                        expected: *min,
                        got: args.len(),
                        span: name.span,
                    });
                }
            }
        },
    }

    let (pushed_scope, pushed_element) = match def.scope {
        ScopeSpec::None => (false, false),
        ScopeSpec::ElementOfReceiver => {
            env.push_scope();
            env.push_element(element_ty(resolve_ty));
            (true, true)
        }
        ScopeSpec::FieldsOrElement => {
            if env.push_record_fields(resolve_ty) {
                (true, false)
            } else {
                env.push_scope();
                env.push_element(element_ty(resolve_ty));
                (true, true)
            }
        }
    };

    let mut typed_args = Vec::new();
    let mut arg_error = None;
    for (i, arg) in args.iter().enumerate() {
        let mut typed = match infer(&arg.expr, env) {
            Ok(typed) => typed,
            Err(err) => {
                arg_error = Some(err);
                break;
            }
        };
        match &def.args {
            ArgSpec::Fixed(_) => {
                if let Some(exp_ty) = fixed_args.get_mut(i)
                    && !types_compatible(&mut typed.ty, exp_ty, env.schema())
                {
                    arg_error = Some(TypeError::ArgTypeMismatch {
                        method: name.value.clone(),
                        expected: exp_ty.to_string(),
                        got: typed.ty.to_string(),
                        span: arg.expr.span(),
                    });
                    break;
                }
            }
            ArgSpec::Scoped { constraint, .. } => {
                if !constraint(&typed.ty) {
                    arg_error = Some(TypeError::ArgTypeMismatch {
                        method: name.value.clone(),
                        expected: "valid argument type".into(),
                        got: typed.ty.to_string(),
                        span: arg.expr.span(),
                    });
                    break;
                }
            }
        }
        typed_args.push(TypedMethodArg {
            direction: arg.direction.clone(),
            expr: typed,
        });
    }

    if pushed_scope {
        env.pop_scope();
    }
    if pushed_element {
        env.element_depth -= 1;
    }
    if let Some(err) = arg_error {
        return Err(err);
    }

    let return_ty = def
        .return_type(resolve_ty, typed_args.first().map(|arg| &arg.expr.ty))
        .wrap_optional_depth(opt_depth);
    let provenance = method_provenance(def.id, base.provenance, &typed_args);

    let span = base.span.merge(name.span);
    Ok(env.typed_with_provenance(
        TypedExprKind::Method {
            method: def.id,
            base: Box::new(base),
            name: name.clone(),
            args: typed_args,
            opt_depth,
        },
        return_ty,
        provenance,
        span,
    ))
}

pub(crate) fn method_provenance(
    method: MethodId,
    base: TargetProvenance,
    args: &[TypedMethodArg],
) -> TargetProvenance {
    match method {
        MethodId::ListFilter
        | MethodId::ListSortAsc
        | MethodId::ListSortDesc
        | MethodId::ListSortAscOver
        | MethodId::ListSortDescOver
        | MethodId::ListTake
        | MethodId::ListSkip
        | MethodId::ListTakeBack
        | MethodId::ListSkipBack
        | MethodId::ListReverse
        | MethodId::ListUnique
        | MethodId::ListUniqueOver
        | MethodId::ListCompact
        | MethodId::ListTranspose
        | MethodId::OptionalUnwrap
        | MethodId::OptionalFlatten => base,
        MethodId::ListFirst
        | MethodId::ListNth
        | MethodId::ListLast
        | MethodId::ListFind
        | MethodId::ListMax
        | MethodId::ListMin
        | MethodId::ListMaxBy
        | MethodId::ListMinBy
        | MethodId::ListFlatten
        | MethodId::ListExtend => base.rows_only(),
        MethodId::OptionalUnwrapOr => args.first().map_or(TargetProvenance::NONE, |arg| {
            base.merge_branches(arg.expr.provenance)
        }),
        _ => TargetProvenance::NONE,
    }
}

/// Returns whether a list/optional chain ends in a row reference
pub(crate) fn row_ref_container_leaf(ty: &QueryType) -> bool {
    match ty {
        QueryType::RowRef(_) => true,
        QueryType::Optional(inner) | QueryType::List(inner) => row_ref_container_leaf(inner),
        _ => false,
    }
}

/// methods must preserve row-reference opacity
pub(crate) fn row_ref_method_allowed(method: MethodId) -> bool {
    matches!(
        method,
        MethodId::OptionalIsSome
            | MethodId::OptionalIsNone
            | MethodId::OptionalUnwrap
            | MethodId::OptionalUnwrapOr
            | MethodId::ListTake
            | MethodId::ListSkip
            | MethodId::ListTakeBack
            | MethodId::ListSkipBack
            | MethodId::ListReverse
            | MethodId::ListFirst
            | MethodId::ListNth
            | MethodId::ListLast
            | MethodId::ListLen
            | MethodId::ListExtend
            | MethodId::ListFlatten
            | MethodId::ListCompact
            | MethodId::ListTranspose
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::tests::{infer_ok, nested_schema, nested_struct, test_schema};
    use crate::typecheck::{IdentBinding, RecordSource, typecheck};
    use grove_schema::validated::ScalarType;

    #[test]
    fn bytes_len_method() {
        for source in [r#"hex"deadbeef".len()"#, r#"b64"3q2+7w==".len()"#] {
            assert_eq!(
                infer_ok(source, &test_schema()).ty,
                QueryType::Scalar(ScalarType::Int),
                "source: {source}"
            );
        }
    }

    #[test]
    fn deep_optional_list_method() {
        assert_eq!(
            infer_ok("some(some([1, 2]))??.map($ + 1)", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
                .wrap_optional()
                .wrap_optional()
        );
    }

    #[test]
    fn optional_map_types() {
        assert_eq!(
            infer_ok("some(1).map($ + 1)", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
        assert_eq!(
            infer_ok("some(some(1))?.map($ + 1)", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
                .wrap_optional()
                .wrap_optional()
        );
    }

    #[test]
    fn optional_flatten_types() {
        assert_eq!(
            infer_ok("some(some(1)).flatten()", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
        assert_eq!(
            infer_ok("some(some(some(1))).flatten()", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
                .wrap_optional()
                .wrap_optional()
        );
    }

    #[test]
    fn optional_list_methods() {
        assert_eq!(
            infer_ok("[some(1), none].compact()", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
        );
        assert_eq!(
            infer_ok("[some(1), none].transpose()", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int))).wrap_optional()
        );
    }

    #[test]
    fn list_boundary_methods() {
        assert_eq!(
            infer_ok("[1, 2].last()", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
        assert_eq!(
            infer_ok("[1, 2].take_back(1)", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
        );
    }

    #[test]
    fn list_predicate_methods() {
        assert_eq!(
            infer_ok("[1, 2].any($ > 0)", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
        assert_eq!(
            infer_ok("[1, 2].find($ > 0)", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
    }

    #[test]
    fn optional_depth_mismatch() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query("1?.abs()");
        let (typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(typed.is_none());
        assert_eq!(diags[0].code.as_ref(), "QT0032");
        let (file, _diags) = crate::parse_query("some(some(2))???.abs()");
        let (typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(typed.is_none());
        assert_eq!(diags[0].code.as_ref(), "QT0032");
        let (file, _diags) = crate::parse_query("some(some(3))?.abs()");
        let (typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(typed.is_none());
        assert_eq!(diags[0].code.as_ref(), "QT0003");
    }

    #[test]
    fn optional_method_inner_base() {
        assert_eq!(
            infer_ok("some(some(some(1)))??.unwrap()", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
                .wrap_optional()
                .wrap_optional()
        );
    }

    #[test]
    fn scalar_method() {
        assert_eq!(
            infer_ok(r#""xyz".starts_with("abc")"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn list_method_len() {
        assert_eq!(
            infer_ok("users.len()", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn list_method_nth() {
        assert!(
            matches!(infer_ok("users.nth(0)", &test_schema()).ty, QueryType::Optional(inner)
                if matches!(*inner, QueryType::Record(_))
            )
        );
    }

    #[test]
    fn string_method_split() {
        assert_eq!(
            infer_ok(r#""a,b".split(",")"#, &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
        );
    }

    #[test]
    fn method_unwrap_access() {
        assert_eq!(
            infer_ok("users.first().unwrap().name", &test_schema()).ty,
            QueryType::Scalar(ScalarType::String)
        );
    }

    #[test]
    fn optional_method_is_some() {
        assert_eq!(
            infer_ok("users.first().is_some()", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn method_unknown() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.name.nonexistent()");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn flatten_nested_collection() {
        let schema = nested_schema();
        assert_eq!(
            infer_ok("users.orders.flatten()", &schema).ty,
            QueryType::List(Box::new(QueryType::Record(RecordSource::Schema(
                nested_struct(&schema, "Order")
            ))))
        );
    }

    #[test]
    fn flatten_compose_first() {
        let schema = nested_schema();
        assert_eq!(
            infer_ok("users.orders.flatten().first()", &schema).ty,
            QueryType::Record(RecordSource::Schema(nested_struct(&schema, "Order")))
                .wrap_optional()
        );
    }

    #[test]
    fn flatten_compose_optional() {
        let schema = nested_schema();
        assert_eq!(
            infer_ok("users.orders.first()?.items?.flatten()", &schema).ty,
            QueryType::List(Box::new(QueryType::Record(RecordSource::Schema(
                nested_struct(&schema, "Item")
            ))))
            .wrap_optional()
        );
    }

    #[test]
    fn flatten_flat_rejected() {
        let schema = nested_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.name.flatten()");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn flatten_scalar_rejected() {
        let schema = nested_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("1.flatten()");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn method_wrong_arg_count() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.name.contains(\"a\", \"b\")");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn method_arg_constraint_valid() {
        infer_ok("users.sum_over(age)", &test_schema());
    }

    #[test]
    fn method_arg_constraint_invalid() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.sort_asc_over(profile)");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn sort_no_arg_over_ordered() {
        assert_eq!(
            infer_ok("[3, 1, 2].sort()", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
        );
    }

    #[test]
    fn sort_no_arg_over_ordered_tuple() {
        infer_ok("[(\"b\", 1), (\"a\", 2)].sort()", &test_schema());
    }

    #[test]
    fn sort_no_arg_over_records_rejected() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.sort()");
        assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
    }

    #[test]
    fn sort_over_single_and_multi_key() {
        let schema = test_schema();
        assert!(matches!(
            infer_ok("users.sort_over(age)", &schema).ty,
            QueryType::List(_)
        ));
        infer_ok("users.sort_desc_over(age, name)", &schema);
    }

    #[test]
    fn dollar_sum_over() {
        assert_eq!(
            infer_ok("[1, 2, 3].sum_over($ + 1)", &test_schema()).ty,
            QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int)))
        );
    }

    #[test]
    fn dollar_filter() {
        assert_eq!(
            infer_ok("[-1, -2, 2, 3][$ > 0]", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
        );
    }

    #[test]
    fn dollar_map_transformation() {
        let schema = test_schema();
        assert_eq!(
            infer_ok("[1, 2, 3].map($ - 1)", &schema).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
        );
        assert!(
            matches!(infer_ok("[1, 2].map(($, $))", &schema).ty, QueryType::List(inner)
                if matches!(inner.as_ref(), QueryType::Tuple(_)))
        );
    }

    #[test]
    fn dollar_over_family() {
        for (query, is_list) in [
            ("[1, 2, 3].avg_over($ + 3)", false),
            ("[3, 1, 2].max_by($ + 2)", false),
            ("[3, 1, 2].sort_over($ + 1, -$)", true),
        ] {
            let result = infer_ok(query, &test_schema());
            if is_list {
                assert!(matches!(result.ty, QueryType::List(_)));
            } else {
                assert_eq!(
                    result.ty,
                    QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int)))
                );
            }
        }
    }

    #[test]
    fn dollar_binds_element() {
        let schema = test_schema();
        let result = infer_ok("[1, 2].map($)", &schema);
        let TypedExprKind::Method { args, .. } = &result.kind else {
            panic!("expected method");
        };
        let TypedExprKind::Ident { name, binding, .. } = &args[0].expr.kind else {
            panic!("expected ident");
        };
        assert_eq!(name.value, "$");
        assert_eq!(*binding, IdentBinding::ListElement);
        assert_eq!(
            infer_ok("[[1, 2], [3]].map($.map($$ + 1))", &schema).ty,
            QueryType::List(Box::new(QueryType::List(Box::new(QueryType::Scalar(
                ScalarType::Int
            )))))
        );
    }

    #[test]
    fn dollar_outer_access() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        assert_eq!(
            infer_ok("[[1, 2], [3]].map($.map($$ + $.len()))", &schema).ty,
            QueryType::List(Box::new(QueryType::List(Box::new(QueryType::Scalar(
                ScalarType::Int
            )))))
        );
        assert!(matches!(
            infer_ok(
                "[[[1]], [[2, 3]]].map($.map($$.map($$$ + $$.len() + $.len())))",
                &schema,
            )
            .ty,
            QueryType::List(_)
        ));
        let (file, _diags) = crate::parse_query("[[1, 2], [3]].map($ + $$.len())");
        assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
    }

    #[test]
    fn dollar_through_record_scope() {
        assert!(matches!(
            infer_ok("[1, 2].map(users.filter(age > $))", &test_schema()).ty,
            QueryType::List(_)
        ));
    }

    #[test]
    fn dollar_not_in_record_scope() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.filter($)");
        assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
        infer_ok("users.filter(age > 0)", &schema);
        let (file, _diags) = crate::parse_query("users.map(name)");
        assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
        infer_ok("users.map($)", &schema);
    }

    #[test]
    fn dollar_outside_scope_rejected() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("$ + 1");
        assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
        let (file, _diags) = crate::parse_query("[].map($)");
        let (typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(typed.is_none() && !diags.is_empty());
    }
}
