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
