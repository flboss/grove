pub mod bindings;
pub(crate) mod error;
pub mod types;

mod compat;
mod env;
mod mutation;
mod validate;

pub use bindings::TypeBindings;
pub use types::{
    CollectionOriginKind, IdentBinding, ProjectionField, QueryType, RecordSource, TargetProvenance,
    TypedExpr, TypedExprKind, TypedLetStmt, TypedMethodArg, TypedMutationStmt, TypedProjectionItem,
    TypedQueryFile, TypedStatement,
};

pub(crate) use env::TypeEnv;

use std::collections::{HashMap, HashSet};

use crate::ast::{
    Arg, BinaryOp, Expr, Literal, MutationKind, MutationStmt, ProjectionItem, QueryFile, Statement,
    TypeExpr, TypeName, UnaryOp,
};
use crate::casts::{self, CastMode};
use crate::methods::{self, ArgCount, ArgSpec, MethodId, ScopeSpec};
use crate::typecheck::compat::{check, types_compatible};
use crate::typecheck::env::{record_field_by_name, record_field_provenance};
use crate::typecheck::error::TypeError;
use crate::typecheck::mutation::{InsertTargetSource, check_insert_base, check_mutation_base};
use crate::typecheck::validate::{
    validate_insert_arg_non_owning, validate_insert_arg_owning, validate_update_arg,
};
use grove_schema::validated::{ScalarType, ValidatedSchema};
use grove_types::{Diagnostic, Severity, Span, Spanned};

fn infer_literal(literal: &Literal) -> QueryType {
    match literal {
        Literal::Int(_) => QueryType::Scalar(ScalarType::Int),
        Literal::Float(_) => QueryType::Scalar(ScalarType::Float),
        Literal::Dec(_) => QueryType::Scalar(ScalarType::Dec),
        Literal::String(_) => QueryType::Scalar(ScalarType::String),
        Literal::Bytes(_) => QueryType::Scalar(ScalarType::Bytes),
        Literal::Bool(_) => QueryType::Scalar(ScalarType::Bool),
        Literal::Instant(_) | Literal::Now | Literal::Today(_) => {
            QueryType::Scalar(ScalarType::Instant)
        }
        Literal::Duration(_) => QueryType::Scalar(ScalarType::Duration),
        Literal::None => QueryType::Unknown.wrap_optional(),
    }
}

fn infer(expr: &Expr, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    match expr {
        Expr::Literal(lit) => {
            let ty = infer_literal(lit);
            Ok(env.typed(TypedExprKind::Literal(lit.clone()), ty, lit.span))
        }
        Expr::Ident(name) => {
            let (entry, depth) = env
                .resolve(name)
                .ok_or_else(|| TypeError::UnknownIdentifier {
                    name: name.to_string(),
                    span: name.span,
                })?;
            let binding = entry.binding.clone();
            let ty = entry.ty.clone();
            let provenance = entry.provenance;
            Ok(env.typed_with_provenance(
                TypedExprKind::Ident {
                    name: name.clone(),
                    binding,
                    depth,
                },
                ty,
                provenance,
                name.span,
            ))
        }
        Expr::EnumVariant {
            enum_name,
            variant,
            span,
        } => {
            let Some((enum_id, enum_def)) = env.schema().enum_by_name(enum_name) else {
                return Err(TypeError::UnknownEnum {
                    name: enum_name.value.clone(),
                    span: enum_name.span,
                });
            };
            let Some(variant) = enum_def.variant_index(variant) else {
                return Err(TypeError::UnknownEnumVariant {
                    enum_name: enum_name.value.clone(),
                    variant: variant.value.clone(),
                    span: variant.span,
                });
            };
            Ok(env.typed(
                TypedExprKind::EnumVariant { enum_id, variant },
                QueryType::Enum(enum_id),
                *span,
            ))
        }
        Expr::Field {
            base,
            name,
            opt_depth,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_field(typed_base, name, *opt_depth, env)
        }
        Expr::TupleAccess {
            base,
            index,
            opt_depth,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_tuple_access(typed_base, index, *opt_depth, env)
        }
        Expr::Method {
            base,
            name,
            args,
            opt_depth,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_method(typed_base, name, args, *opt_depth, env)
        }
        Expr::Binary { op, lhs, rhs, .. } => infer_binary(op, lhs, rhs, env),
        Expr::Unary { op, expr, .. } => infer_unary(op, expr, env),
        Expr::RowRef { expr, span } => infer_row_ref(expr, *span, env),
        Expr::Tuple { elements, .. } => {
            let mut typed_elems = Vec::new();
            for elem in elements {
                typed_elems.push(infer(elem, env)?);
            }
            let ty = QueryType::Tuple(typed_elems.iter().map(|e| e.ty.clone()).collect());
            Ok(env.typed(
                TypedExprKind::Tuple {
                    elements: typed_elems,
                },
                ty,
                expr.span(),
            ))
        }
        Expr::Array { elements, span } => infer_array(elements, *span, env),
        Expr::Some { value, .. } => {
            let typed_value = infer(value, env)?;
            let ty = typed_value.ty.clone().wrap_optional();
            let provenance = typed_value.provenance;
            Ok(env.typed_with_provenance(
                TypedExprKind::Some {
                    value: Box::new(typed_value),
                },
                ty,
                provenance,
                expr.span(),
            ))
        }
        Expr::If {
            arms,
            default,
            span,
        } => infer_if(arms, default, *span, env),
        Expr::Cast {
            expr, ty, fallible, ..
        } => infer_cast(expr, ty, *fallible, env),
        Expr::Projection {
            base,
            items,
            spread,
            span,
        } => infer_projection(base, items, *spread, *span, env),
        Expr::TypeConstant { ty, name } => infer_type_constant(ty, name, env),
        Expr::Struct { fields, span } => infer_struct_literal(fields, *span, env),
    }
}

fn infer_array(elements: &[Expr], span: Span, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    let mut typed_elems = Vec::with_capacity(elements.len());
    for elem in elements {
        typed_elems.push(infer(elem, env)?);
    }

    let mut ty = QueryType::Unknown;
    for elem in &mut typed_elems {
        if !types_compatible(&mut ty, &mut elem.ty, env.schema()) {
            return Err(TypeError::ArrayElementTypeMismatch {
                expected: ty.to_string(),
                got: elem.ty.to_string(),
                span: elem.span,
            });
        }
    }

    if !matches!(ty, QueryType::Unknown) {
        for elem in &mut typed_elems {
            types_compatible(&mut ty, &mut elem.ty, env.schema());
        }
    }

    let ty = QueryType::List(Box::new(ty));

    Ok(env.typed(
        TypedExprKind::Array {
            elements: typed_elems,
        },
        ty,
        span,
    ))
}

fn infer_if(
    arms: &[(Expr, Expr)],
    default: &Expr,
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_arms = Vec::with_capacity(arms.len());
    let mut typed_default = infer(default, env)?;

    for (cond, body) in arms {
        let typed_cond = infer(cond, env)?;
        if !typed_cond.ty.is_bool() {
            return Err(TypeError::IfConditionNotBool {
                span: typed_cond.span,
            });
        }
        let typed_body = infer(body, env)?;
        typed_arms.push((typed_cond, typed_body));
    }

    for (_, body) in &mut typed_arms {
        if !types_compatible(&mut typed_default.ty, &mut body.ty, env.schema()) {
            return Err(TypeError::IfBranchTypeMismatch {
                expected: typed_default.ty.to_string(),
                got: body.ty.to_string(),
                span: body.span,
            });
        }
    }

    for (_, body) in &mut typed_arms {
        types_compatible(&mut typed_default.ty, &mut body.ty, env.schema());
    }

    let ty = typed_default.ty.clone();
    let mut provenance = typed_default.provenance;
    for (_, body) in &typed_arms {
        provenance = provenance.merge_branches(body.provenance);
    }

    Ok(env.typed_with_provenance(
        TypedExprKind::If {
            arms: typed_arms,
            default: Box::new(typed_default),
        },
        ty,
        provenance,
        span,
    ))
}

fn resolve_type_expr(
    ty: &Spanned<TypeExpr>,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    match &ty.value {
        &TypeExpr::Scalar(scalar) => Ok(scalar.into()),
        TypeExpr::Named(name) => {
            if let Some((enum_id, _)) = schema.enum_by_name(name) {
                return Ok(QueryType::Enum(enum_id));
            }
            if let Some((struct_id, _)) = schema.struct_by_name(name) {
                return Ok(QueryType::Record(RecordSource::Schema(struct_id)));
            }
            Err(TypeError::UnknownTypeName {
                name: name.clone(),
                span: ty.span,
            })
        }
        TypeExpr::Optional { inner, .. } => Ok(resolve_type_expr(inner, schema)?.wrap_optional()),
        TypeExpr::List { element, .. } => Ok(QueryType::List(Box::new(resolve_type_expr(
            element, schema,
        )?))),
        TypeExpr::Tuple { elements, .. } => elements
            .iter()
            .map(|element| resolve_type_expr(element, schema))
            .collect::<Result<Vec<_>, _>>()
            .map(QueryType::Tuple),
        TypeExpr::Record { fields, .. } => {
            let mut projection = Vec::with_capacity(fields.len());
            for (name, field_ty) in fields {
                projection.push(ProjectionField {
                    name: name.value.clone(),
                    ty: resolve_type_expr(field_ty, schema)?,
                });
            }
            Ok(QueryType::Record(RecordSource::Projection(projection)))
        }
    }
}

fn infer_cast(
    expr: &Expr,
    target: &Spanned<TypeExpr>,
    fallible: bool,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_expr = infer(expr, env)?;
    let span = typed_expr.span.merge(target.span);
    let mut target_ty = resolve_type_expr(target, env.schema())?;
    // target_ty can never contain QueryType::Unknown, so it won't get
    // mutated in types_compatible() or check(); no clone necessary
    debug_assert!(!target_ty.is_unknown());

    if fallible {
        if typed_expr.ty.is_unknown() {
            return Err(TypeError::AmbiguousCast {
                from: typed_expr.ty.to_string(),
                to: target_ty.to_string(),
                span,
            });
        }
        let kind = casts::lookup(&typed_expr.ty, &target_ty, CastMode::Fallible, env.schema())
            .ok_or_else(|| TypeError::InvalidCast {
                from: typed_expr.ty.to_string(),
                to: target_ty.to_string(),
                span,
            })?;
        return Ok(env.typed(
            TypedExprKind::Cast {
                expr: Box::new(typed_expr),
                kind,
            },
            kind.target().wrap_optional(),
            span,
        ));
    }

    if typed_expr.ty.is_unknown() {
        let mut refined = typed_expr.ty.clone();
        if !types_compatible(&mut refined, &mut target_ty, env.schema()) {
            return Err(TypeError::AmbiguousCast {
                from: typed_expr.ty.to_string(),
                to: target_ty.to_string(),
                span,
            });
        }
        typed_expr.ty = refined;
        check(&mut typed_expr, &mut target_ty, env)?;
        if typed_expr.ty.eq(&target_ty, env.schema()) {
            return Ok(typed_expr);
        }
        return Err(TypeError::AmbiguousCast {
            from: typed_expr.ty.to_string(),
            to: target_ty.to_string(),
            span,
        });
    }

    if typed_expr.ty.eq(&target_ty, env.schema()) {
        return Ok(typed_expr);
    }

    let kind = casts::lookup(
        &typed_expr.ty,
        &target_ty,
        CastMode::Infallible,
        env.schema(),
    )
    .ok_or_else(|| TypeError::InvalidCast {
        from: typed_expr.ty.to_string(),
        to: target_ty.to_string(),
        span,
    })?;

    Ok(env.typed(
        TypedExprKind::Cast {
            expr: Box::new(typed_expr),
            kind,
        },
        kind.target(),
        span,
    ))
}

fn infer_type_constant(
    type_name: &Spanned<TypeName>,
    name: &Spanned<crate::ast::ConstantName>,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let scalar_ty = match type_name.value {
        TypeName::Int => ScalarType::Int,
        TypeName::Float => ScalarType::Float,
        TypeName::Dec => ScalarType::Dec,
        TypeName::String => unreachable!("String has no type constants"),
        TypeName::Bytes => unreachable!("Bytes has no type constants"),
        TypeName::Bool => unreachable!("Bool has no type constants"),
        TypeName::Instant => unreachable!("Instant has no type constants"),
        TypeName::Duration => unreachable!("Duration has no type constants"),
    };

    let result_ty = QueryType::Scalar(scalar_ty);
    let span = type_name.span.merge(name.span);

    Ok(env.typed(
        TypedExprKind::TypeConstant {
            ty: type_name.clone(),
            name: name.clone(),
        },
        result_ty,
        span,
    ))
}

fn infer_struct_literal(
    fields: &[(Spanned<String>, Expr)],
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_fields = Vec::with_capacity(fields.len());
    let mut proj_fields = Vec::with_capacity(fields.len());
    let mut seen_names = HashSet::with_capacity(fields.len());

    for (name, value) in fields {
        if !seen_names.insert(name.as_str()) {
            return Err(TypeError::DuplicateStructField {
                name: name.value.clone(),
                span: name.span,
            });
        }
        let typed_value = infer(value, env)?;
        proj_fields.push(ProjectionField {
            name: name.value.clone(),
            ty: typed_value.ty.clone(),
        });
        typed_fields.push((name.clone(), typed_value));
    }

    let result_ty = QueryType::Record(RecordSource::Projection(proj_fields));
    Ok(env.typed(
        TypedExprKind::Struct {
            fields: typed_fields,
        },
        result_ty,
        span,
    ))
}

fn infer_projection(
    base: &Expr,
    items: &[ProjectionItem],
    spread: Option<Span>,
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    if items.is_empty() && spread.is_none() {
        return Err(TypeError::EmptyProjection { span });
    }

    let typed_base = infer(base, env)?;

    match &typed_base.ty {
        QueryType::Record(_) => {}
        QueryType::List(inner) if matches!(**inner, QueryType::Record(_)) => {}
        _ => {
            return Err(TypeError::ProjectionBaseNotRecord {
                got: typed_base.ty.to_string(),
                span: typed_base.span,
            });
        }
    }

    let pushed_scope = env.push_record_fields(&typed_base.ty);

    let mut typed_items = Vec::with_capacity(items.len());
    let mut seen_names: HashMap<&str, Span> = HashMap::new();

    for item in items {
        let (name, typed_value) = match (&item.alias, &item.value) {
            (Some(name), value)
            | (None, value @ Expr::Ident(name))
            | (None, value @ Expr::Field { name, .. }) => {
                let typed_value = infer(value, env)?;
                (name, typed_value)
            }
            (None, _) => {
                return Err(TypeError::UnnamedComputedField {
                    span: item.value.span(),
                });
            }
        };

        if let Some(&previous) = seen_names.get(name.as_str()) {
            return Err(TypeError::DuplicateProjectionField {
                name: name.to_string(),
                span: item.value.span(),
                previous,
            });
        }
        seen_names.insert(name.as_str(), name.span);

        typed_items.push(TypedProjectionItem {
            alias: Spanned::new(item.value.span(), name.to_string()),
            value: typed_value,
        });
    }

    if let Some(spread_span) = spread {
        for field_name in projection_base_fields(&typed_base.ty, env.schema()) {
            if seen_names.contains_key(field_name.as_str()) {
                continue;
            }
            let alias = Spanned::new(spread_span, field_name);
            let value = Expr::Ident(alias);
            let typed_value = infer(&value, env)?;
            let Expr::Ident(alias) = value else {
                unreachable!();
            };
            typed_items.push(TypedProjectionItem {
                alias,
                value: typed_value,
            });
        }
    }

    let is_list = matches!(&typed_base.ty, QueryType::List(_));
    let proj_source = RecordSource::Projection(
        typed_items
            .iter()
            .map(|t| ProjectionField {
                name: t.alias.to_string(),
                ty: t.value.ty.clone(),
            })
            .collect(),
    );
    let result_ty = if is_list {
        QueryType::List(Box::new(QueryType::Record(proj_source)))
    } else {
        QueryType::Record(proj_source)
    };

    if pushed_scope {
        env.pop_scope();
    }

    Ok(env.typed(
        TypedExprKind::Projection {
            base: Box::new(typed_base),
            items: typed_items,
        },
        result_ty,
        span,
    ))
}

fn projection_base_fields(ty: &QueryType, schema: &ValidatedSchema) -> Vec<String> {
    match ty {
        QueryType::Record(RecordSource::Schema(struct_id)) => schema.structs[struct_id.index()]
            .fields
            .iter()
            .map(|field| field.name().to_string())
            .collect(),
        QueryType::Record(RecordSource::Projection(fields)) => {
            fields.iter().map(|field| field.name.clone()).collect()
        }
        QueryType::List(inner) => projection_base_fields(inner, schema),
        _ => Vec::new(),
    }
}

fn infer_field(
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

fn infer_tuple_access(
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

fn element_ty(ty: &QueryType) -> QueryType {
    match ty {
        QueryType::List(inner) | QueryType::Optional(inner) => inner.as_ref().clone(),
        _ => QueryType::Unknown,
    }
}

fn infer_method(
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

fn method_provenance(
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
fn row_ref_container_leaf(ty: &QueryType) -> bool {
    match ty {
        QueryType::RowRef(_) => true,
        QueryType::Optional(inner) | QueryType::List(inner) => row_ref_container_leaf(inner),
        _ => false,
    }
}

/// methods must preserve row-reference opacity
fn row_ref_method_allowed(method: MethodId) -> bool {
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

fn infer_binary(
    op: &Spanned<BinaryOp>,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_lhs = infer(lhs, env)?;
    let mut typed_rhs = infer(rhs, env)?;
    let span = typed_lhs.span.merge(typed_rhs.span);

    if typed_lhs.ty.contains_row_ref() || typed_rhs.ty.contains_row_ref() {
        return Err(TypeError::RowRefOperationNotAllowed {
            operation: op.value.to_string(),
            span,
        });
    }

    let result_ty = match op.value {
        BinaryOp::In => infer_binary_in(&mut typed_lhs.ty, &mut typed_rhs.ty, span, env.schema())?,
        BinaryOp::And | BinaryOp::Or => {
            if !typed_lhs.ty.is_bool() || !typed_rhs.ty.is_bool() {
                return Err(TypeError::BinaryOpTypeMismatch {
                    left: typed_lhs.ty.to_string(),
                    op: op.value.to_string(),
                    right: typed_rhs.ty.to_string(),
                    span,
                });
            }
            QueryType::Scalar(ScalarType::Bool)
        }
        BinaryOp::Eq | BinaryOp::Ne => {
            infer_equality_op(&mut typed_lhs.ty, op, &mut typed_rhs.ty, span, env.schema())?
        }
        BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            infer_comparison_op(&mut typed_lhs.ty, op, &mut typed_rhs.ty, span, env.schema())?
        }
        _ => infer_arithmetic_op(&mut typed_lhs.ty, op, &mut typed_rhs.ty, span)?,
    };

    Ok(env.typed(
        TypedExprKind::Binary {
            op: op.clone(),
            lhs: Box::new(typed_lhs),
            rhs: Box::new(typed_rhs),
        },
        result_ty,
        span,
    ))
}

fn infer_binary_in(
    lhs: &mut QueryType,
    rhs: &mut QueryType,
    span: Span,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    let rhs_inner = match rhs {
        QueryType::Tuple(elems) if !elems.is_empty() => elems,
        _ => return Err(TypeError::InRequiresTuple { span }),
    };
    for elem in rhs_inner {
        if types_compatible(lhs, elem, schema) {
            return Ok(QueryType::Scalar(ScalarType::Bool));
        }
    }
    Err(TypeError::BinaryOpTypeMismatch {
        left: lhs.to_string(),
        op: "in".into(),
        right: rhs.to_string(),
        span,
    })
}

fn infer_equality_op(
    lhs: &mut QueryType,
    op: &BinaryOp,
    rhs: &mut QueryType,
    span: Span,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    if !types_compatible(lhs, rhs, schema) {
        return Err(TypeError::BinaryOpTypeMismatch {
            left: lhs.to_string(),
            op: op.to_string(),
            right: rhs.to_string(),
            span,
        });
    }
    Ok(QueryType::Scalar(ScalarType::Bool))
}

fn infer_comparison_op(
    lhs: &mut QueryType,
    op: &BinaryOp,
    rhs: &mut QueryType,
    span: Span,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    if !types_compatible(lhs, rhs, schema) || !lhs.has_defined_order() {
        return Err(TypeError::BinaryOpTypeMismatch {
            left: lhs.to_string(),
            op: op.to_string(),
            right: rhs.to_string(),
            span,
        });
    }
    Ok(QueryType::Scalar(ScalarType::Bool))
}

fn infer_arithmetic_op(
    lhs: &mut QueryType,
    op: &BinaryOp,
    rhs: &mut QueryType,
    span: Span,
) -> Result<QueryType, TypeError> {
    use ScalarType::*;

    match (lhs, rhs) {
        (QueryType::Scalar(Int), QueryType::Scalar(Int)) => Ok(QueryType::Scalar(Int)),
        (QueryType::Scalar(Float), QueryType::Scalar(Float)) => Ok(QueryType::Scalar(Float)),
        (QueryType::Scalar(Dec), QueryType::Scalar(Dec)) => Ok(QueryType::Scalar(Dec)),
        (lhs @ QueryType::Scalar(Instant), rhs @ QueryType::Scalar(Duration)) => {
            if matches!(op, BinaryOp::Add | BinaryOp::Sub) {
                Ok(QueryType::Scalar(Instant))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs @ QueryType::Scalar(Instant), rhs @ QueryType::Scalar(Instant)) => {
            if matches!(op, BinaryOp::Sub) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs @ QueryType::Scalar(Duration), rhs @ QueryType::Scalar(Duration)) => {
            if matches!(op, BinaryOp::Add | BinaryOp::Sub) {
                Ok(QueryType::Scalar(Duration))
            } else if matches!(op, BinaryOp::Div) {
                Ok(QueryType::Scalar(Dec))
            } else if matches!(op, BinaryOp::Rem | BinaryOp::Mod) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs, rhs @ QueryType::Scalar(Duration)) if lhs.is_numeric() => {
            if matches!(op, BinaryOp::Mul) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs @ QueryType::Scalar(Duration), rhs) if rhs.is_numeric() => {
            if matches!(op, BinaryOp::Mul | BinaryOp::Div) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs, rhs) => Err(TypeError::BinaryOpTypeMismatch {
            left: lhs.to_string(),
            op: op.to_string(),
            right: rhs.to_string(),
            span,
        }),
    }
}

fn infer_unary(
    op: &Spanned<UnaryOp>,
    expr: &Expr,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let typed_expr = infer(expr, env)?;
    let span = op.span.merge(typed_expr.span);

    let result_ty = match op.value {
        UnaryOp::Neg => match &typed_expr.ty {
            QueryType::Scalar(ScalarType::Int) => QueryType::Scalar(ScalarType::Int),
            QueryType::Scalar(ScalarType::Float) => QueryType::Scalar(ScalarType::Float),
            QueryType::Scalar(ScalarType::Dec) => QueryType::Scalar(ScalarType::Dec),
            QueryType::Scalar(ScalarType::Duration) => QueryType::Scalar(ScalarType::Duration),
            _ => {
                return Err(TypeError::UnaryOpTypeMismatch {
                    op: op.to_string(),
                    operand: typed_expr.ty.to_string(),
                    span,
                });
            }
        },
        UnaryOp::Not => {
            if !typed_expr.ty.is_bool() {
                return Err(TypeError::UnaryOpTypeMismatch {
                    op: op.to_string(),
                    operand: typed_expr.ty.to_string(),
                    span,
                });
            }
            QueryType::Scalar(ScalarType::Bool)
        }
    };

    Ok(env.typed(
        TypedExprKind::Unary {
            op: op.clone(),
            expr: Box::new(typed_expr),
        },
        result_ty,
        span,
    ))
}

/// Build a RowRef from a traceable mutation source
fn infer_row_ref(expr: &Expr, span: Span, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
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
fn row_ref_shape(ty: &QueryType, provenance: TargetProvenance) -> Option<QueryType> {
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

fn infer_mutation_arg(expr: &Expr, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    let previous = env.allow_row_refs;
    env.allow_row_refs = true;
    let result = infer(expr, env);
    env.allow_row_refs = previous;
    result
}

fn typecheck_stmt(stmt: &Statement, env: &mut TypeEnv) -> Result<TypedStatement, TypeError> {
    match stmt {
        Statement::Expr(expr) => {
            let typed = infer(expr, env)?;
            Ok(TypedStatement::Expr(typed))
        }
        Statement::Mutation(mutation) => typecheck_mutation(mutation, env),
        Statement::Let(let_stmt) => {
            let value = infer(&let_stmt.value, env)?;
            env.define(
                let_stmt.name.value.clone(),
                value.ty.clone(),
                IdentBinding::Let,
            );
            Ok(TypedStatement::Let(TypedLetStmt {
                name: let_stmt.name.clone(),
                span: let_stmt.value.span(),
                value,
            }))
        }
    }
}

fn typecheck_mutation(
    mutation: &MutationStmt,
    env: &mut TypeEnv,
) -> Result<TypedStatement, TypeError> {
    let kind = mutation.kind.value;

    if kind == MutationKind::Delete && !mutation.args.is_empty() {
        return Err(TypeError::WrongArgCount {
            method: kind.to_string(),
            expected: 0,
            got: mutation.args.len(),
            span: mutation.kind.span,
        });
    }

    if matches!(kind, MutationKind::Insert | MutationKind::Update) && mutation.args.len() != 1 {
        return Err(TypeError::WrongArgCount {
            method: kind.to_string(),
            expected: 1,
            got: mutation.args.len(),
            span: mutation.kind.span,
        });
    }

    match kind {
        MutationKind::Delete => {
            let typed_base = infer(&mutation.base, env)?;
            check_mutation_base(&typed_base, mutation.opt_depth)?;
            Ok(TypedStatement::Mutation(TypedMutationStmt::Delete {
                span: mutation.kind.span,
                base: typed_base,
                opt_depth: mutation.opt_depth,
            }))
        }
        MutationKind::Insert => {
            let typed_base = infer(&mutation.base, env)?;
            let (struct_id, source) =
                check_insert_base(&typed_base, mutation.opt_depth, env.schema())?;
            let mut typed_arg = infer_mutation_arg(&mutation.args[0], env)?;
            match source {
                InsertTargetSource::Root | InsertTargetSource::OwningRef => {
                    validate_insert_arg_owning(struct_id, &mut typed_arg, env.schema())?;
                }
                InsertTargetSource::NonOwningRef { field } => {
                    validate_insert_arg_non_owning(field, struct_id, &mut typed_arg, env.schema())?;
                }
            }
            Ok(TypedStatement::Mutation(TypedMutationStmt::Insert {
                span: mutation.kind.span,
                base: typed_base,
                arg: typed_arg,
                opt_depth: mutation.opt_depth,
            }))
        }
        MutationKind::Update => {
            let typed_base = infer(&mutation.base, env)?;
            let struct_id = check_mutation_base(&typed_base, mutation.opt_depth)?;
            env.push_scope();
            env.define(
                "prev".to_string(),
                QueryType::Record(RecordSource::Schema(struct_id)),
                IdentBinding::Prev { struct_id },
            );
            let mut typed_arg = infer_mutation_arg(&mutation.args[0], env)?;
            env.pop_scope();
            validate_update_arg(struct_id, &mut typed_arg, env.schema())?;
            Ok(TypedStatement::Mutation(TypedMutationStmt::Update {
                span: mutation.kind.span,
                base: typed_base,
                arg: typed_arg,
                opt_depth: mutation.opt_depth,
            }))
        }
    }
}

pub fn typecheck(
    file: QueryFile,
    schema: &ValidatedSchema,
) -> (Option<TypedQueryFile>, Vec<Diagnostic>) {
    typecheck_with_bindings(file, schema, &TypeBindings::new())
}

pub fn typecheck_with_bindings(
    file: QueryFile,
    schema: &ValidatedSchema,
    bindings: &TypeBindings,
) -> (Option<TypedQueryFile>, Vec<Diagnostic>) {
    let (statements_src, result_src) = file.into_statements_and_output();
    let mut env = TypeEnv::new(schema);
    for (name, ty) in bindings.iter() {
        env.define(name.to_owned(), ty.clone(), IdentBinding::Let);
    }
    // repl turn scope always pushed, executor matching
    env.push_scope();

    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut statements = Vec::new();

    for stmt in &statements_src {
        match typecheck_stmt(stmt, &mut env) {
            Ok(typed) => statements.push(typed),
            Err(err) => diags.push(err.into()),
        }
    }

    let mut result = None;
    if let Some(expr) = result_src {
        match infer(&expr, &mut env) {
            Ok(mut typed) => {
                let mut ty = typed.ty.clone();
                if let Err(err) = check(&mut typed, &mut ty, &mut env) {
                    diags.push(err.into());
                }
                result = Some(typed);
            }
            Err(err) => {
                diags.push(err.into());
            }
        }
    }

    if diags.iter().any(|d| d.severity == Severity::Error) {
        (None, diags)
    } else {
        (Some(TypedQueryFile { statements, result }), diags)
    }
}

#[cfg(test)]
mod tests;
