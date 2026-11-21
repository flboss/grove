pub mod bindings;
pub mod error;
pub mod types;

pub use bindings::TypeBindings;
pub use types::TypedQueryFile;

use std::collections::{HashMap, HashSet};

use crate::ast::{
    Arg, BinaryOp, Expr, Literal, MutationKind, MutationStmt, ProjectionItem, QueryFile, Statement,
    TypeExpr, TypeName, UnaryOp,
};
use crate::casts::{self, CastMode};
use crate::methods::{self, ArgCount, ArgSpec, MethodId, ScopeSpec};
use crate::typecheck::error::TypeError;
use crate::typecheck::types::*;
use grove_schema::validated::{Field, FieldId, ScalarType, StructId, ValidatedSchema, ValueType};
use grove_types::{Diagnostic, NodeId, Severity, Span, Spanned};

#[derive(Debug, Clone, PartialEq)]
struct ScopeEntry {
    ty: QueryType,
    binding: IdentBinding,
    provenance: TargetProvenance,
}

struct TypeEnv<'s> {
    scopes: Vec<HashMap<String, ScopeEntry>>,
    schema: &'s ValidatedSchema,
    element_depth: usize,
    next_id: u64,
}

impl<'s> TypeEnv<'s> {
    fn new(schema: &'s ValidatedSchema) -> Self {
        let mut env = TypeEnv {
            scopes: vec![HashMap::new()],
            schema,
            element_depth: 0,
            next_id: 0,
        };

        for (root_idx, root) in schema.roots.iter().enumerate() {
            let record_ty = QueryType::Record(RecordSource::Schema(root.struct_id));
            let list_ty = QueryType::List(Box::new(record_ty));
            env.define_with_provenance(
                root.name.clone(),
                list_ty,
                IdentBinding::Root { root_idx },
                TargetProvenance::root(root_idx),
            );
        }

        env
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn push_element(&mut self, ty: QueryType) {
        self.element_depth += 1;
        let name = "$".repeat(self.element_depth);
        self.define(name, ty, IdentBinding::ListElement);
    }

    fn define(&mut self, name: String, ty: QueryType, binding: IdentBinding) {
        self.define_with_provenance(name, ty, binding, TargetProvenance::NONE);
    }

    fn define_with_provenance(
        &mut self,
        name: String,
        ty: QueryType,
        binding: IdentBinding,
        provenance: TargetProvenance,
    ) {
        self.scopes.last_mut().unwrap().insert(
            name,
            ScopeEntry {
                ty,
                binding,
                provenance,
            },
        );
    }

    fn resolve(&self, name: &str) -> Option<(&ScopeEntry, usize)> {
        for (skipped, scope) in self.scopes.iter().rev().enumerate() {
            if let Some(entry) = scope.get(name) {
                return Some((entry, skipped));
            }
        }
        None
    }

    fn schema(&self) -> &'s ValidatedSchema {
        self.schema
    }

    fn typed(&mut self, kind: TypedExprKind, ty: QueryType, span: Span) -> TypedExpr {
        self.typed_with_provenance(kind, ty, TargetProvenance::NONE, span)
    }

    fn typed_with_provenance(
        &mut self,
        kind: TypedExprKind,
        ty: QueryType,
        provenance: TargetProvenance,
        span: Span,
    ) -> TypedExpr {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        TypedExpr {
            kind,
            ty,
            provenance,
            span,
            id,
        }
    }

    fn push_record_fields(&mut self, ty: &QueryType) -> bool {
        match ty {
            QueryType::Record(RecordSource::Schema(struct_id)) => {
                self.push_scope();
                self.push_schema_struct_fields(*struct_id);
                true
            }
            QueryType::Record(RecordSource::Projection(fields)) => {
                self.push_scope();
                for field in fields {
                    self.define(
                        field.name.clone(),
                        field.ty.clone(),
                        IdentBinding::ProjectedField {
                            name: field.name.clone(),
                        },
                    );
                }
                true
            }
            QueryType::List(inner) => self.push_record_fields(inner),
            _ => false,
        }
    }

    fn push_schema_struct_fields(&mut self, struct_id: StructId) {
        let struct_ = &self.schema.structs[struct_id.index()];
        for (local_idx, field) in struct_.fields.iter().enumerate() {
            let field_id = FieldId::new(struct_id, local_idx);
            let binding = IdentBinding::Field(field_id);
            let provenance = match field {
                Field::Ref { .. } => TargetProvenance::ref_collection(field_id),
                _ => TargetProvenance::NONE,
            };
            let (name, ty) = match field {
                Field::Value { name, ty, .. } => (name.clone(), ty.into()),
                Field::Array { name, element, .. } => {
                    (name.clone(), QueryType::List(Box::new(element.into())))
                }
                Field::Ref {
                    name,
                    target,
                    optional,
                    is_list,
                    ..
                } => {
                    let inner = QueryType::Record(RecordSource::Schema(*target));
                    let ty = if *is_list {
                        QueryType::List(Box::new(inner))
                    } else if *optional {
                        inner.wrap_optional()
                    } else {
                        inner
                    };
                    (name.clone(), ty)
                }
            };
            self.define_with_provenance(name, ty, binding, provenance);
        }
    }
}

fn record_field_by_name(
    record: &QueryType,
    name: &str,
    schema: &ValidatedSchema,
) -> Option<QueryType> {
    match record {
        QueryType::Record(RecordSource::Schema(struct_id)) => schema
            .struct_field(*struct_id, name)
            .map(|(_, field)| QueryType::from(field)),
        QueryType::Record(RecordSource::Projection(fields)) => {
            fields.iter().find(|f| f.name == name).map(|f| f.ty.clone())
        }
        _ => None,
    }
}

fn record_field_provenance(
    record: &QueryType,
    name: &str,
    schema: &ValidatedSchema,
) -> TargetProvenance {
    let QueryType::Record(RecordSource::Schema(struct_id)) = record else {
        return TargetProvenance::NONE;
    };
    let Some((field_id, Field::Ref { .. })) = schema.struct_field(*struct_id, name) else {
        return TargetProvenance::NONE;
    };
    TargetProvenance::ref_collection(field_id)
}

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

fn infer_binary(
    op: &Spanned<BinaryOp>,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_lhs = infer(lhs, env)?;
    let mut typed_rhs = infer(rhs, env)?;
    let span = typed_lhs.span.merge(typed_rhs.span);

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

fn types_compatible(a: &mut QueryType, b: &mut QueryType, schema: &ValidatedSchema) -> bool {
    match (a, b) {
        (QueryType::Unknown, QueryType::Unknown) => true,
        (a @ QueryType::Unknown, b) => {
            *a = b.clone();
            types_compatible(a, b, schema)
        }
        (a, b @ QueryType::Unknown) => {
            *b = a.clone();
            types_compatible(a, b, schema)
        }
        (QueryType::Optional(a), QueryType::Optional(b)) => types_compatible(a, b, schema),
        (QueryType::List(a), QueryType::List(b)) => types_compatible(a, b, schema),
        (QueryType::Scalar(a), QueryType::Scalar(b)) => a == b,
        (QueryType::Enum(a), QueryType::Enum(b)) => a == b,
        (QueryType::Tuple(a), QueryType::Tuple(b)) => {
            a.len() == b.len()
                && a.iter_mut()
                    .zip(b.iter_mut())
                    .all(|(a, b)| types_compatible(a, b, schema))
        }
        (QueryType::Record(a), QueryType::Record(b)) => record_fields_match(a, b, schema),
        (a, b) => a == b,
    }
}

fn record_fields_match(
    a: &mut RecordSource,
    b: &mut RecordSource,
    schema: &ValidatedSchema,
) -> bool {
    match (a, b) {
        (RecordSource::Schema(a_id), RecordSource::Schema(b_id)) => a_id == b_id,
        (RecordSource::Schema(sid), RecordSource::Projection(proj))
        | (RecordSource::Projection(proj), RecordSource::Schema(sid)) => {
            record_schema_matches_projection(*sid, proj, schema)
        }
        (RecordSource::Projection(a_proj), RecordSource::Projection(b_proj)) => {
            if a_proj.len() != b_proj.len() {
                return false;
            }
            a_proj.iter_mut().all(|a_field| {
                b_proj.iter_mut().any(|b_field| {
                    b_field.name == a_field.name
                        && types_compatible(&mut a_field.ty, &mut b_field.ty, schema)
                })
            })
        }
    }
}

fn record_schema_matches_projection(
    sid: StructId,
    proj: &mut Vec<ProjectionField>,
    schema: &ValidatedSchema,
) -> bool {
    let struct_ = &schema.structs[sid.index()];
    for proj_field in proj {
        let Some(schema_field) = struct_.fields.iter().find(|f| f.name() == proj_field.name) else {
            return false;
        };
        let mut schema_ty = QueryType::from(schema_field);
        if !types_compatible(&mut schema_ty, &mut proj_field.ty, schema) {
            return false;
        }
    }
    true
}

fn check(
    expr: &mut TypedExpr,
    expected: &mut QueryType,
    env: &mut TypeEnv,
) -> Result<(), TypeError> {
    let type_mismatch = |exp: String, got: String| {
        Err(TypeError::UnexpectedType {
            expected: exp.to_string(),
            got: got.to_string(),
            span: expr.span,
        })
    };

    if !types_compatible(&mut expr.ty, expected, env.schema()) {
        return type_mismatch(expected.to_string(), expr.ty.to_string());
    }

    if expr.ty.is_unknown() {
        return Err(TypeError::AmbiguousType { span: expr.span });
    }

    match &mut expr.kind {
        TypedExprKind::Literal(_)
        | TypedExprKind::Ident { .. }
        | TypedExprKind::EnumVariant { .. } => {}
        // no propagation
        TypedExprKind::Field { base, .. } => {
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
        }
        // no propagation
        TypedExprKind::TupleAccess { base, .. } => {
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
        }
        // no propagation
        TypedExprKind::Method { base, args, .. } => {
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
            for arg in args {
                let mut arg_ty = arg.expr.ty.clone();
                check(&mut arg.expr, &mut arg_ty, env)?;
            }
        }
        // no propagation
        TypedExprKind::Binary { lhs, rhs, .. } => {
            let mut lhs_ty = lhs.ty.clone();
            let mut rhs_ty = rhs.ty.clone();
            check(lhs, &mut lhs_ty, env)?;
            check(rhs, &mut rhs_ty, env)?;
        }
        TypedExprKind::Unary { expr: operand, .. } => {
            check(operand, &mut expr.ty, env)?;
        }
        // no propagation
        TypedExprKind::Cast { expr: operand, .. } => {
            let mut ty = operand.ty.clone();
            check(operand, &mut ty, env)?;
        }
        TypedExprKind::If { arms, default } => {
            arms.iter_mut()
                .map(|(_, b)| b)
                .chain(std::iter::once(default.as_mut()))
                .try_for_each(|b| check(b, expected, env))?;
        }
        TypedExprKind::Some { value } => {
            let QueryType::Optional(inner) = &mut expr.ty else {
                return type_mismatch("?T".into(), expr.ty.to_string());
            };
            check(value.as_mut(), inner, env)?;
        }
        TypedExprKind::Tuple { elements } => {
            let QueryType::Tuple(inners) = &mut expr.ty else {
                return type_mismatch("Tuple".into(), expr.ty.to_string());
            };
            elements
                .iter_mut()
                .zip(inners.iter_mut())
                .try_for_each(|(elem, ty)| check(elem, ty, env))?;
        }
        TypedExprKind::Array { elements } => {
            let QueryType::List(inner) = &mut expr.ty else {
                return type_mismatch("List<T>".into(), expr.ty.to_string());
            };
            elements.iter_mut().try_for_each(|e| check(e, inner, env))?;
        }
        TypedExprKind::Struct { fields } => {
            let QueryType::Record(RecordSource::Projection(proj_fields)) = &mut expr.ty else {
                return type_mismatch("Record".into(), expr.ty.to_string());
            };
            fields.iter_mut().try_for_each(|(name, expr)| {
                let Some(field) = proj_fields
                    .iter_mut()
                    .find(|field| field.name == name.as_str())
                else {
                    return type_mismatch(
                        format!("Record with field `{}`", name.as_str()),
                        expr.ty.to_string(),
                    );
                };
                check(expr, &mut field.ty, env)
            })?;
        }
        // no propagation
        TypedExprKind::Projection { base, items } => {
            items.iter_mut().try_for_each(|item| {
                let mut item_ty = item.value.ty.clone();
                check(&mut item.value, &mut item_ty, env)
            })?;
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
        }
        TypedExprKind::TypeConstant { .. } => {}
    }
    Ok(())
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
            let struct_id = check_insert_base(&typed_base, mutation.opt_depth, env.schema())?;
            let mut typed_arg = infer(&mutation.args[0], env)?;
            validate_insert(struct_id, &mut typed_arg, env.schema())?;
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
            let mut typed_arg = infer(&mutation.args[0], env)?;
            env.pop_scope();
            validate_update(struct_id, &mut typed_arg, env.schema())?;
            Ok(TypedStatement::Mutation(TypedMutationStmt::Update {
                span: mutation.kind.span,
                base: typed_base,
                arg: typed_arg,
                opt_depth: mutation.opt_depth,
            }))
        }
    }
}

fn check_mutation_base(typed_base: &TypedExpr, opt_depth: usize) -> Result<StructId, TypeError> {
    let Some(ty) = typed_base.ty.unwrap_optional_depth(opt_depth) else {
        return Err(TypeError::OptionalDepthMismatch {
            depth: opt_depth,
            ty: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    };
    if ty.is_optional() {
        return Err(TypeError::MutationOnOptional {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    }

    let struct_id = match ty {
        QueryType::Record(RecordSource::Schema(id)) => *id,
        QueryType::List(inner) => match inner.as_ref() {
            QueryType::Record(RecordSource::Schema(id)) => *id,
            QueryType::Record(RecordSource::Projection(_)) => {
                return Err(TypeError::MutationOnProjection {
                    span: typed_base.span,
                });
            }
            _ => {
                return Err(TypeError::MutationOnNonList {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            }
        },
        QueryType::Record(RecordSource::Projection(_)) => {
            return Err(TypeError::MutationOnProjection {
                span: typed_base.span,
            });
        }
        _ => {
            return Err(TypeError::MutationOnNonList {
                got: typed_base.ty.to_string(),
                span: typed_base.span,
            });
        }
    };

    if !typed_base.provenance.rows {
        return Err(TypeError::MutationTargetNotTraceable {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    }
    Ok(struct_id)
}

fn check_insert_base(
    typed_base: &TypedExpr,
    opt_depth: usize,
    schema: &ValidatedSchema,
) -> Result<StructId, TypeError> {
    let Some(ty) = typed_base.ty.unwrap_optional_depth(opt_depth) else {
        return Err(TypeError::OptionalDepthMismatch {
            depth: opt_depth,
            ty: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    };
    if ty.is_optional() {
        return Err(TypeError::MutationOnOptional {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    }

    let struct_id = match ty {
        QueryType::List(inner) => match inner.as_ref() {
            QueryType::Record(RecordSource::Schema(id)) => *id,
            QueryType::Record(RecordSource::Projection(_)) => {
                return Err(TypeError::MutationOnProjection {
                    span: typed_base.span,
                });
            }
            _ => {
                return Err(TypeError::MutationOnNonList {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            }
        },
        QueryType::Record(RecordSource::Schema(_))
        | QueryType::Record(RecordSource::Projection(_)) => {
            return Err(TypeError::InsertOnRecord {
                got: typed_base.ty.to_string(),
                span: typed_base.span,
            });
        }
        _ => {
            return Err(TypeError::MutationOnNonList {
                got: typed_base.ty.to_string(),
                span: typed_base.span,
            });
        }
    };

    if !typed_base.provenance.rows {
        return Err(TypeError::MutationTargetNotTraceable {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    }

    let Some(collection) = typed_base.provenance.collection else {
        return Err(TypeError::InsertTargetNotCollection {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    };
    match collection {
        CollectionOriginKind::Root(root_idx) => {
            if let Some(root) = schema.roots.get(root_idx)
                && root.struct_id == struct_id
            {
            } else {
                return Err(TypeError::MutationTargetNotTraceable {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            };
        }
        CollectionOriginKind::Ref(field_id) => {
            let Some(Field::Ref {
                name,
                target,
                is_list,
                owning,
                ..
            }) = schema.field(field_id)
            else {
                return Err(TypeError::MutationTargetNotTraceable {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            };
            if *target != struct_id {
                return Err(TypeError::MutationTargetNotTraceable {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            }
            if !*is_list {
                return Err(TypeError::InsertOnRecord {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            }
            if !*owning {
                let struct_name = schema.structs[field_id.struct_id().index()].name.clone();
                return Err(TypeError::InsertOnNonOwningCollection {
                    field: name.clone(),
                    struct_name,
                    span: typed_base.span,
                });
            }
        }
    }
    Ok(struct_id)
}

fn expect_constructed_record(
    ty: &mut QueryType,
    span: Span,
    mutation_kind: MutationKind,
) -> Result<&mut Vec<ProjectionField>, TypeError> {
    match ty {
        QueryType::Record(RecordSource::Projection(p)) => Ok(p),
        ty => Err(TypeError::ArgTypeMismatch {
            method: mutation_kind.to_string(),
            expected: "constructed record".into(),
            got: ty.to_string(),
            span,
        }),
    }
}

fn validate_insert(
    sid: StructId,
    typed_arg: &mut TypedExpr,
    schema: &ValidatedSchema,
) -> Result<(), TypeError> {
    let record_ty = match &mut typed_arg.ty {
        QueryType::List(inner) => inner.as_mut(),
        ty => ty,
    };
    let proj_fields = expect_constructed_record(record_ty, typed_arg.span, MutationKind::Insert)?;

    let struct_fields = &schema.structs[sid.index()].fields;
    let struct_name = &schema.structs[sid.index()].name;
    for schema_field in struct_fields {
        let (expected_name, is_optional) = match &schema_field {
            Field::Value {
                name,
                ty: ValueType::Optional(_),
                ..
            } => (name, true),
            Field::Value { name, .. } | Field::Array { name, .. } => (name, false),
            Field::Ref {
                name,
                owning: false,
                ..
            } => {
                if proj_fields.iter().any(|f| &f.name == name) {
                    return Err(TypeError::RefFieldInMutation {
                        field: name.clone(),
                        struct_name: struct_name.clone(),
                        span: typed_arg.span,
                    });
                }
                continue;
            }
            Field::Ref { name, optional, .. } => (name, *optional),
        };

        if proj_fields.iter().any(|f| &f.name == expected_name) {
            continue;
        }

        if is_optional {
            proj_fields.push(ProjectionField {
                name: expected_name.clone(),
                ty: QueryType::from(schema_field),
            });
        } else {
            return Err(TypeError::InsertMissingRequiredField {
                field: expected_name.clone(),
                struct_name: struct_name.clone(),
                span: typed_arg.span,
            });
        }
    }

    for proj_field in proj_fields {
        let schema_field = struct_fields.iter().find(|f| match f {
            Field::Value { name, .. } | Field::Array { name, .. } | Field::Ref { name, .. } => {
                name == &proj_field.name
            }
        });
        let Some(mut schema_ty) = schema_field.map(QueryType::from) else {
            return Err(TypeError::StructFieldNotInSchema {
                field: proj_field.name.clone(),
                struct_name: struct_name.clone(),
                span: typed_arg.span,
            });
        };

        if !types_compatible(&mut schema_ty, &mut proj_field.ty, schema) {
            return Err(TypeError::StructFieldTypeMismatch {
                field: proj_field.name.clone(),
                struct_name: struct_name.clone(),
                expected: schema_ty.to_string(),
                got: proj_field.ty.to_string(),
                span: typed_arg.span,
            });
        }
    }

    Ok(())
}

fn validate_update(
    sid: StructId,
    typed_arg: &mut TypedExpr,
    schema: &ValidatedSchema,
) -> Result<(), TypeError> {
    let struct_ = &schema.structs[sid.index()];
    let proj_fields =
        expect_constructed_record(&mut typed_arg.ty, typed_arg.span, MutationKind::Update)?;

    for proj_field in proj_fields {
        let Some(schema_field) = struct_.fields.iter().find(|f| match f {
            Field::Value { name, .. } | Field::Array { name, .. } | Field::Ref { name, .. } => {
                name == &proj_field.name
            }
        }) else {
            return Err(TypeError::StructFieldNotInSchema {
                field: proj_field.name.clone(),
                struct_name: struct_.name.clone(),
                span: typed_arg.span,
            });
        };
        if let Field::Ref { owning: false, .. } = schema_field {
            return Err(TypeError::RefFieldInMutation {
                field: proj_field.name.clone(),
                struct_name: struct_.name.clone(),
                span: typed_arg.span,
            });
        }
        let mut schema_ty = QueryType::from(schema_field);

        if !types_compatible(&mut schema_ty, &mut proj_field.ty, schema) {
            return Err(TypeError::StructFieldTypeMismatch {
                field: proj_field.name.clone(),
                struct_name: struct_.name.clone(),
                expected: schema_ty.to_string(),
                got: proj_field.ty.to_string(),
                span: typed_arg.span,
            });
        }
    }
    Ok(())
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
mod tests {
    use super::*;

    const TEST_SCHEMA_SRC: &str = r#"
        config {
            int_arithmetic = "checked",
            float_checks = true,
            dec_arithmetic = "checked",
        }
        root users: User;
        struct User {
            name: String,
            age: Int,
            score: Float,
            balance: Dec,
            active: Bool,
            created: Instant,
            profile: ?Profile,
            maybe_pos: ?(Float, Float)@(mlat, mlng),
        }
        struct Profile {
            user: &User,
            username: String
        }
        rel Profile.user <-> User.profile (profiles.user_id -> users.id);
    "#;

    fn test_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(TEST_SCHEMA_SRC)
                .0
                .expect("invalid test schema"),
        )
        .0
        .expect("invalid test schema")
    }

    const MUTATION_SCHEMA_SRC: &str = r#"
        root users: User;
        root images: Image;
        struct User {
            id: Int,
            name: String,
            orders: List<Order>,
        }
        struct Order {
            id: Int,
            total: Int,
            user: &User,
            image: &?Image,
        }
        struct Image {
            id: Int,
            title: String,
            orders: &List<Order>,
        }
        rel Order.user <<-> User.orders (orders.user_id -> users.id);
        rel Image.orders <->> Order.image (orders.image_id -> images.id);
    "#;

    fn mutation_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(MUTATION_SCHEMA_SRC)
                .0
                .expect("invalid mutation schema"),
        )
        .0
        .expect("invalid mutation schema")
    }

    const ENUM_SCHEMA_SRC: &str = r#"
        root tickets: Ticket;
        enum Priority { Low => "low", Medium => "medium", High => "high" }
        enum Level { Low => 0, High => 2 }
        struct Ticket {
            id: Int,
            priority: Priority,
            level: Level,
            maybe_priority: ?Priority,
            priorities: List<Priority>@priorities_ref via ticket_priorities[ticket_key, priority],
            pair: (Priority, Level)@(pair_priority, pair_level),
        }
    "#;

    fn enum_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(ENUM_SCHEMA_SRC)
                .0
                .expect("invalid enum test schema"),
        )
        .0
        .expect("invalid enum test schema")
    }

    fn error_codes(source: &str, schema: &ValidatedSchema) -> Vec<String> {
        let (file, parse_diags) = crate::parse_query(source);
        assert!(parse_diags.is_empty(), "parse diagnostics: {parse_diags:?}");
        let (typed, diags) = typecheck(file.unwrap(), schema);
        assert!(typed.is_none(), "expected type error for `{source}`");
        diags.iter().map(|d| d.code.to_string()).collect()
    }

    fn infer_ok(source: &str, schema: &ValidatedSchema) -> TypedExpr {
        let mut env = TypeEnv::new(schema);
        let (file, parse_diags) = crate::parse_query(source);
        assert!(parse_diags.is_empty(), "parse diagnostics: {parse_diags:?}");
        infer(file.unwrap().output().unwrap(), &mut env).unwrap()
    }

    fn typecheck_ok(source: &str, schema: &ValidatedSchema) -> TypedQueryFile {
        let (file, parse_diags) = crate::parse_query(source);
        assert!(parse_diags.is_empty(), "parse diagnostics: {parse_diags:?}");
        let (typed, diags) = typecheck(file.unwrap(), schema);
        assert!(diags.is_empty(), "type diagnostics: {diags:?}");
        typed.unwrap()
    }

    const ANNOTATION_SCHEMA_SRC: &str = r#"
        root items: Item;
        struct Item {
            a: ?String,
            b: (Int, Int)@(bx, by),
        }
    "#;

    fn annotation_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(ANNOTATION_SCHEMA_SRC)
                .0
                .expect("invalid annotation test schema"),
        )
        .0
        .expect("invalid annotation test schema")
    }

    #[test]
    fn infer_enum_variant() {
        let schema = enum_schema();
        let priority_id = schema.enum_by_name("Priority").unwrap().0;
        let result = infer_ok("Priority::Medium", &schema);
        assert_eq!(result.ty, QueryType::Enum(priority_id));
        assert!(matches!(
            result.kind,
            TypedExprKind::EnumVariant {
                enum_id,
                variant: 1
            } if enum_id == priority_id
        ));
    }

    #[test]
    fn enum_field_type() {
        let schema = enum_schema();
        let priority_id = schema.enum_by_name("Priority").unwrap().0;
        assert_eq!(
            infer_ok("tickets.first().unwrap().priority", &schema).ty,
            QueryType::Enum(priority_id)
        );
    }

    #[test]
    fn unknown_enum_rejected() {
        assert_eq!(error_codes("Missing::High", &enum_schema()), vec!["QT0030"]);
    }

    #[test]
    fn unknown_enum_variant_rejected() {
        assert_eq!(
            error_codes("Priority::Urgent", &enum_schema()),
            vec!["QT0031"]
        );
    }

    #[test]
    fn enum_equality_and_membership() {
        for source in [
            "Priority::High == Priority::Low",
            "Priority::High != Priority::Low",
            "Priority::High in (Priority::Low, Priority::High)",
            "[Priority::Low, Priority::High].contains(Priority::High)",
        ] {
            assert_eq!(
                infer_ok(source, &enum_schema()).ty,
                QueryType::Scalar(ScalarType::Bool),
                "source: {source}"
            );
        }
    }

    #[test]
    fn enum_raw_value_comparison_rejected() {
        assert_eq!(
            error_codes(r#"Priority::High == "high""#, &enum_schema()),
            vec!["QT0007"]
        );
    }

    #[test]
    fn different_enum_comparison_rejected() {
        assert_eq!(
            error_codes("Priority::Low == Level::Low", &enum_schema()),
            vec!["QT0007"]
        );
    }

    #[test]
    fn enum_ordering_rejected() {
        let schema = enum_schema();
        assert_eq!(
            error_codes("Priority::High > Priority::Low", &schema),
            vec!["QT0007"]
        );
        assert_eq!(
            error_codes("[Priority::Low, Priority::High].sort()", &schema),
            vec!["QT0003"]
        );
    }

    #[test]
    fn enum_cast_to_underlying_value() {
        let schema = enum_schema();

        let result = infer_ok("Priority::High as String", &schema);
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
        assert!(matches!(result.kind, TypedExprKind::Cast { .. }));

        assert_eq!(
            infer_ok("Level::High as Int", &schema).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn enum_cast_to_wrong_underlying_type_rejected() {
        let schema = enum_schema();
        assert_eq!(
            error_codes("Priority::High as Int", &schema),
            vec!["QT0013"]
        );
        assert_eq!(
            error_codes("Level::High as String", &schema),
            vec!["QT0013"]
        );
    }

    #[test]
    fn enum_optional_and_projection() {
        let schema = enum_schema();
        let priority_id = schema.enum_by_name("Priority").unwrap().0;

        assert_eq!(
            infer_ok("some(Priority::High)", &schema).ty,
            QueryType::Optional(Box::new(QueryType::Enum(priority_id)))
        );

        let result = infer_ok("tickets { priority }", &schema);
        match result.ty {
            QueryType::List(inner) => match *inner {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields[0].ty, QueryType::Enum(priority_id));
                }
                other => panic!("expected projection record, got {other:?}"),
            },
            other => panic!("expected list projection, got {other:?}"),
        }
    }

    #[test]
    fn infer_int_literal() {
        assert_eq!(
            infer_ok("42", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn infer_float_literal() {
        assert_eq!(
            infer_ok("3.14f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn infer_dec_literal() {
        assert_eq!(
            infer_ok("3.14", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn infer_string_literal() {
        assert_eq!(
            infer_ok("\"hello\"", &test_schema()).ty,
            QueryType::Scalar(ScalarType::String)
        );
    }

    #[test]
    fn infer_bytes_literal() {
        for source in [r#"hex"deadbeef""#, r#"b64"3q2+7w==""#] {
            assert_eq!(
                infer_ok(source, &test_schema()).ty,
                QueryType::Scalar(ScalarType::Bytes),
                "source: {source}"
            );
        }
    }

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
    fn bytes_cast_from_string() {
        assert_eq!(
            infer_ok(r#""hello" as Bytes"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bytes)
        );
    }

    #[test]
    fn annotations_disambiguation_only() {
        for (source, expected) in [
            (
                "[] as List<Int>",
                QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int))),
            ),
            (
                "none as ?String",
                QueryType::Scalar(ScalarType::String).wrap_optional(),
            ),
            (
                "some(none) as ??Int",
                QueryType::Scalar(ScalarType::Int)
                    .wrap_optional()
                    .wrap_optional(),
            ),
            ("1 as Int", QueryType::Scalar(ScalarType::Int)),
        ] {
            let result = infer_ok(source, &test_schema());
            assert_eq!(result.ty, expected, "source: {source}");
            assert!(
                !matches!(&result.kind, TypedExprKind::Cast { .. }),
                "source should be a no-op: {source}"
            );
        }
    }

    #[test]
    fn annotations_complex_and_named() {
        let schema = test_schema();
        let result = infer_ok(
            "[] as List<(?Int, List<Duration>, ??String, {a: Instant, d: Dec})>",
            &schema,
        );
        assert!(!result.ty.is_unknown());
        assert!(matches!(result.ty, QueryType::List(_)));
        assert!(!matches!(&result.kind, TypedExprKind::Cast { .. }));

        let user_id = schema.roots[0].struct_id;
        assert_eq!(
            infer_ok("none as ?User", &schema).ty,
            QueryType::Record(RecordSource::Schema(user_id)).wrap_optional()
        );

        let schema = enum_schema();
        let priority_id = schema.enum_by_name("Priority").unwrap().0;
        assert_eq!(
            infer_ok("none as ?Priority", &schema).ty,
            QueryType::Enum(priority_id).wrap_optional()
        );
    }

    #[test]
    fn annotations_record_equality() {
        let schema = annotation_schema();
        for source in [
            "{a = none, b = (1, 2)} as Item",
            "{b = (1, 2), a = none} as {b: (Int, Int), a: ?String}",
            "items.first().unwrap() as {a: ?String, b: (Int, Int)}",
        ] {
            let result = infer_ok(source, &schema);
            assert!(!result.ty.is_unknown(), "source: {source}");
            assert!(
                !matches!(&result.kind, TypedExprKind::Cast { .. }),
                "source should be a no-op: {source}"
            );
        }
    }

    #[test]
    fn annotations_ambiguous_and_invalid_casts() {
        let schema = test_schema();
        for source in ["none as Int", "[] as Int", "[none] as List<Int>"] {
            assert_eq!(
                error_codes(source, &schema),
                vec!["QT0034"],
                "source: {source}"
            );
        }
        assert_eq!(
            error_codes("{a = none} as Item", &annotation_schema()),
            vec!["QT0034"]
        );

        for source in [
            "none as ?String as Int",
            "[] as List<Int> as List<Float>",
            "1 as Bool",
            "1 as User",
        ] {
            assert_eq!(
                error_codes(source, &schema),
                vec!["QT0013"],
                "source: {source}"
            );
        }
    }

    #[test]
    fn bytes_comparison() {
        let schema = test_schema();
        for source in [
            r#"hex"00" == hex"01""#,
            r#"hex"00" != hex"01""#,
            r#"hex"00" < hex"01""#,
            r#"hex"00" <= hex"01""#,
            r#"hex"00" > hex"01""#,
            r#"hex"00" >= hex"01""#,
            r#"hex"00" in (hex"00", hex"01")"#,
        ] {
            assert_eq!(
                infer_ok(source, &schema).ty,
                QueryType::Scalar(ScalarType::Bool),
                "source: {source}"
            );
        }
        assert_eq!(
            infer_ok(r#"[hex"02", hex"01"].sort()"#, &schema).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Bytes)))
        );
        assert_eq!(
            infer_ok(r#"[hex"02", hex"01"].min()"#, &schema).ty,
            QueryType::Scalar(ScalarType::Bytes).wrap_optional()
        );
        assert_eq!(
            infer_ok(r#"[hex"02", hex"01"].max()"#, &schema).ty,
            QueryType::Scalar(ScalarType::Bytes).wrap_optional()
        );
    }

    #[test]
    fn infer_bool_literal() {
        assert_eq!(
            infer_ok("true", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn infer_instant_literal() {
        assert_eq!(
            infer_ok("@now", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Instant)
        );
    }

    #[test]
    fn infer_duration_literal() {
        assert_eq!(
            infer_ok("#30d", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn none_without_context_unknown() {
        assert_eq!(
            infer_ok("none", &test_schema()).ty,
            QueryType::Unknown.wrap_optional()
        );
    }

    #[test]
    fn root_lookup() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (entry, depth) = env.resolve("users").unwrap();
        assert!(matches!(entry.ty, QueryType::List(_)));
        assert_eq!(depth, 0);
    }

    #[test]
    fn resolve_returns_depth() {
        let schema = test_schema();
        let user = schema.roots[0].struct_id;
        let mut env = TypeEnv::new(&schema);
        assert_eq!(env.resolve("users").unwrap().1, 0);
        env.push_scope();
        env.define(
            "name".to_string(),
            QueryType::Scalar(ScalarType::String),
            field_binding(&schema, user, "name"),
        );
        assert_eq!(env.resolve("name").unwrap().1, 0);
        assert_eq!(env.resolve("users").unwrap().1, 1);
        assert!(env.resolve("missing").is_none());
    }

    fn field_binding(schema: &ValidatedSchema, struct_id: StructId, field: &str) -> IdentBinding {
        let local = schema.structs[struct_id.index()]
            .fields
            .iter()
            .position(|f| f.name() == field)
            .unwrap();
        IdentBinding::Field(FieldId::new(struct_id, local))
    }

    #[test]
    fn ident_binding_root() {
        let result = infer_ok("users", &test_schema());
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &result.kind
        else {
            panic!("expected ident, got {:?}", result.kind);
        };
        assert_eq!(name.value, "users");
        assert_eq!(*binding, IdentBinding::Root { root_idx: 0 });
        assert_eq!(*depth, 0);
    }

    #[test]
    fn ident_binding_scoped_field() {
        let schema = test_schema();
        let user = schema.roots[0].struct_id;
        let result = infer_ok("users[active]", &schema);
        let TypedExprKind::Method { args, .. } = &result.kind else {
            panic!("expected method, got {:?}", result.kind);
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &args[0].expr.kind
        else {
            panic!("expected ident, got {:?}", args[0].expr.kind);
        };
        assert_eq!(name.value, "active");
        assert_eq!(*binding, field_binding(&schema, user, "active"));
        assert_eq!(*depth, 0);
    }

    #[test]
    fn ident_binding_scoped_ref_field() {
        let schema = test_schema();
        let user = schema.roots[0].struct_id;
        let result = infer_ok("users[profile.is_some()]", &schema);
        let TypedExprKind::Method { args, .. } = &result.kind else {
            panic!("expected method, got {:?}", result.kind);
        };
        let TypedExprKind::Method { base, .. } = &args[0].expr.kind else {
            panic!("expected method, got {:?}", args[0].expr.kind);
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &base.kind
        else {
            panic!("expected ident, got {:?}", base.kind);
        };
        assert_eq!(name.value, "profile");
        assert_eq!(*binding, field_binding(&schema, user, "profile"));
        assert_eq!(*depth, 0);
    }

    #[test]
    fn ident_binding_projected_field() {
        let result = infer_ok("users { name } { name }", &test_schema());
        let TypedExprKind::Projection { items, .. } = &result.kind else {
            panic!("expected projection, got {:?}", result.kind);
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &items[0].value.kind
        else {
            panic!("expected ident, got {:?}", items[0].value.kind);
        };
        assert_eq!(name.value, "name");
        assert_eq!(
            *binding,
            IdentBinding::ProjectedField {
                name: "name".to_string()
            }
        );
        assert_eq!(*depth, 0);
    }

    #[test]
    fn ident_binding_projected_scoped_field() {
        let result = infer_ok(r#"users { name }[name == "Alice"]"#, &test_schema());
        let TypedExprKind::Method { args, .. } = &result.kind else {
            panic!("expected method, got {:?}", result.kind);
        };
        let TypedExprKind::Binary { lhs, .. } = &args[0].expr.kind else {
            panic!("expected binary, got {:?}", args[0].expr.kind);
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &lhs.kind
        else {
            panic!("expected ident, got {:?}", lhs.kind);
        };
        assert_eq!(name.value, "name");
        assert_eq!(
            *binding,
            IdentBinding::ProjectedField {
                name: "name".to_string()
            }
        );
        assert_eq!(*depth, 0);
    }

    #[test]
    fn let_statement_binds_value_type() {
        let (file, _) = crate::parse_query("let x = 42; x");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let TypedStatement::Let(let_stmt) = &typed.statements[0] else {
            panic!("expected let statement");
        };
        assert_eq!(let_stmt.name.value, "x");
        assert_eq!(let_stmt.value.ty, QueryType::Scalar(ScalarType::Int));
        let TypedExprKind::Ident { binding, depth, .. } = &typed.result.as_ref().unwrap().kind
        else {
            panic!("expected ident result");
        };
        assert_eq!(*binding, IdentBinding::Let);
        assert_eq!(*depth, 0);
        assert_eq!(
            typed.result.as_ref().unwrap().ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn statement_only_file_has_no_result() {
        let (file, _) = crate::parse_query("let x = 1;");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        assert_eq!(typed.statements.len(), 1);
        assert!(typed.result.is_none());
    }

    #[test]
    fn trailing_semicolon_result_valid() {
        let (file, _) = crate::parse_query("42;");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        assert!(typed.statements.is_empty());
        assert_eq!(
            typed.result.as_ref().unwrap().ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn type_bindings_manage_entries() {
        let mut bindings = TypeBindings::new();
        assert!(bindings.is_empty());
        bindings.define("n", QueryType::Scalar(ScalarType::Int));
        bindings.define("s", QueryType::Scalar(ScalarType::String));
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings.get("n"), Some(&QueryType::Scalar(ScalarType::Int)));
        assert_eq!(bindings.iter().count(), 2);
        assert_eq!(
            bindings.remove("n"),
            Some(QueryType::Scalar(ScalarType::Int))
        );
        assert!(bindings.get("n").is_none());
        bindings.clear();
        assert!(bindings.is_empty());
    }

    #[test]
    fn session_binding_resolves_at_depth_one() {
        let mut bindings = TypeBindings::new();
        bindings.define("min", QueryType::Scalar(ScalarType::Int));

        let (file, _) = crate::parse_query("min");
        let (typed, diags) = typecheck_with_bindings(file.unwrap(), &test_schema(), &bindings);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let output = typed.result.as_ref().unwrap();
        assert_eq!(output.ty, QueryType::Scalar(ScalarType::Int));
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &output.kind
        else {
            panic!("expected ident output");
        };
        assert_eq!(name.value, "min");
        assert_eq!(*binding, IdentBinding::Let);
        assert_eq!(*depth, 1);
    }

    #[test]
    fn session_binding_shadow_root() {
        let mut bindings = TypeBindings::new();
        bindings.define("users", QueryType::Scalar(ScalarType::Int));

        let (file, _) = crate::parse_query("users");
        let (typed, diags) = typecheck_with_bindings(file.unwrap(), &test_schema(), &bindings);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let output = typed.result.as_ref().unwrap();
        assert_eq!(output.ty, QueryType::Scalar(ScalarType::Int));
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &output.kind
        else {
            panic!("expected ident output");
        };
        assert_eq!(name.value, "users");
        assert_eq!(*binding, IdentBinding::Let);
        assert_eq!(*depth, 1);
    }

    #[test]
    fn turn_let_shadows_session_binding() {
        let mut bindings = TypeBindings::new();
        bindings.define("min", QueryType::Scalar(ScalarType::Int));

        let (file, _) = crate::parse_query("let min = 5; users[age > min]");
        let (typed, diags) = typecheck_with_bindings(file.unwrap(), &test_schema(), &bindings);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let output = typed.result.as_ref().unwrap();
        let TypedExprKind::Method { args, .. } = &output.kind else {
            panic!("expected filter method");
        };
        let TypedExprKind::Binary { rhs, .. } = &args[0].expr.kind else {
            panic!("expected binary argument");
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &rhs.kind
        else {
            panic!("expected ident");
        };
        assert_eq!(name.value, "min");
        assert_eq!(*binding, IdentBinding::Let);
        assert_eq!(*depth, 1);
    }

    #[test]
    fn typed_file_exposes_output_type_and_lets() {
        let (file, _) = crate::parse_query("let x = 1; x");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        assert_eq!(
            typed.output_type(),
            Some(&QueryType::Scalar(ScalarType::Int))
        );
        let lets: Vec<_> = typed
            .let_bindings()
            .map(|(name, ty)| (name.to_string(), ty.clone()))
            .collect();
        assert_eq!(
            lets,
            vec![("x".to_string(), QueryType::Scalar(ScalarType::Int))]
        );
    }

    #[test]
    fn let_chain_scope_visible() {
        let (file, _) = crate::parse_query("let a = 1; let b = a; b");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        assert_eq!(typed.unwrap().statements.len(), 2);
    }

    #[test]
    fn let_collection_type() {
        let (file, _) = crate::parse_query("let names = users { name }; names");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let TypedStatement::Let(let_stmt) = &typed.statements[0] else {
            panic!("expected let statement");
        };
        assert!(matches!(let_stmt.value.ty, QueryType::List(_)));
        assert_eq!(typed.result.as_ref().unwrap().ty, let_stmt.value.ty);
    }

    #[test]
    fn let_use_before_def() {
        let (file, _) = crate::parse_query("let x = y; 0");
        let (_, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QT0001"));
    }

    #[test]
    fn let_self_reference_error() {
        let (file, _) = crate::parse_query("let x = x; 0");
        let (_, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QT0001"));
    }

    #[test]
    fn let_shadows_root() {
        let (file, _) = crate::parse_query("let users = 1; users");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let TypedExprKind::Ident { binding, .. } = &typed.result.as_ref().unwrap().kind else {
            panic!("expected ident result");
        };
        assert_eq!(*binding, IdentBinding::Let);
    }

    #[test]
    fn let_rebind_same_scope() {
        let (file, _) = crate::parse_query("let x = 1; let x = 2; x");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        assert_eq!(
            typed.unwrap().result.as_ref().unwrap().ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn let_in_query() {
        let (file, _) = crate::parse_query("let limit = 30; users[age > limit]");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        assert!(matches!(
            typed.unwrap().result.as_ref().unwrap().ty,
            QueryType::List(_)
        ));
    }

    #[test]
    fn ident_binding_prev() {
        let schema = test_schema();
        let user = schema.roots[0].struct_id;
        let (file, _) = crate::parse_query(r#"users[age == 30].update({ age = prev.age + 1 }); 0"#);
        let (typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let TypedStatement::Mutation(TypedMutationStmt::Update { arg, .. }) =
            &typed.unwrap().statements[0]
        else {
            panic!("expected update statement");
        };
        let TypedExprKind::Struct { fields, .. } = &arg.kind else {
            panic!("expected struct, got {:?}", arg.kind);
        };
        let TypedExprKind::Binary { lhs, .. } = &fields[0].1.kind else {
            panic!("expected binary, got {:?}", fields[0].1.kind);
        };
        let TypedExprKind::Field { base, .. } = &lhs.kind else {
            panic!("expected field, got {:?}", lhs.kind);
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &base.kind
        else {
            panic!("expected ident, got {:?}", base.kind);
        };
        assert_eq!(name.value, "prev");
        assert_eq!(*binding, IdentBinding::Prev { struct_id: user });
        assert_eq!(*depth, 0);
    }

    #[test]
    fn root_unknown_identifier() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("posts");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn field_access_struct() {
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
    fn optional_field_access() {
        assert!(
            matches!(infer_ok("users.profile", &test_schema()).ty, QueryType::List(inner) if matches!(*inner, QueryType::Optional(_)))
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
    fn string_split_and_scalar_casts() {
        assert_eq!(
            infer_ok(r#""a,b".split(",")"#, &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
        );
        assert_eq!(
            infer_ok("42 as String", &test_schema()).ty,
            QueryType::Scalar(ScalarType::String)
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

    fn collect_node_ids(expr: &TypedExpr, ids: &mut Vec<NodeId>) {
        ids.push(expr.id);
        match &expr.kind {
            TypedExprKind::Literal(_)
            | TypedExprKind::Ident { .. }
            | TypedExprKind::EnumVariant { .. }
            | TypedExprKind::TypeConstant { .. } => {}
            TypedExprKind::Some { value } => collect_node_ids(value, ids),
            TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
                for element in elements {
                    collect_node_ids(element, ids);
                }
            }
            TypedExprKind::Struct { fields } => {
                for (_, value) in fields {
                    collect_node_ids(value, ids);
                }
            }
            TypedExprKind::Projection { base, items } => {
                collect_node_ids(base, ids);
                for item in items {
                    collect_node_ids(&item.value, ids);
                }
            }
            TypedExprKind::Binary { lhs, rhs, .. } => {
                collect_node_ids(lhs, ids);
                collect_node_ids(rhs, ids);
            }
            TypedExprKind::Unary { expr, .. } | TypedExprKind::Cast { expr, .. } => {
                collect_node_ids(expr, ids);
            }
            TypedExprKind::Method { base, args, .. } => {
                collect_node_ids(base, ids);
                for arg in args {
                    collect_node_ids(&arg.expr, ids);
                }
            }
            TypedExprKind::If { arms, default } => {
                for (cond, value) in arms {
                    collect_node_ids(cond, ids);
                    collect_node_ids(value, ids);
                }
                collect_node_ids(default, ids);
            }
            TypedExprKind::Field { base, .. } | TypedExprKind::TupleAccess { base, .. } => {
                collect_node_ids(base, ids);
            }
        }
    }

    #[test]
    fn node_ids_are_unique() {
        let result = infer_ok(
            r#"users { name, shout = name.to_upper() } { name }"#,
            &test_schema(),
        );
        let mut ids = Vec::new();
        collect_node_ids(&result, &mut ids);
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "duplicate node ids: {ids:?}");
        assert!(!ids.is_empty());
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

    const NESTED_SCHEMA_SRC: &str = r#"
        root users: User;
        struct User {
            name: String,
            active: Bool,
            manager: &User,
            subordinates: &List<User>,
            orders: List<Order>,
        }
        struct Order {
            total: Dec,
            user: &User,
            items: List<Item>,
        }
        struct Item {
            sku: String,
            order: &Order,
        }
        rel User.manager <<-> User.subordinates (users.manager_id -> users.id);
        rel Order.user <<-> User.orders (orders.user_id -> users.id);
        rel Item.order <<-> Order.items (items.order_id -> orders.id);
    "#;

    fn nested_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(NESTED_SCHEMA_SRC)
                .0
                .expect("invalid test schema"),
        )
        .0
        .expect("invalid test schema")
    }

    fn nested_struct(schema: &ValidatedSchema, name: &str) -> StructId {
        StructId::new(
            schema
                .structs
                .iter()
                .position(|s| s.name == name)
                .unwrap_or_else(|| panic!("missing struct {name}")),
        )
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
    fn sort_no_arg_over_tuple() {
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

    #[test]
    fn binary_int_add() {
        assert_eq!(
            infer_ok("1 + 2", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_int_sub() {
        assert_eq!(
            infer_ok("5 - 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_int_mul() {
        assert_eq!(
            infer_ok("4 * 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_int_div() {
        assert_eq!(
            infer_ok("10 / 2", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_int_rem() {
        assert_eq!(
            infer_ok("10 % 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_int_mod() {
        assert_eq!(
            infer_ok("10 mod 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_float_add() {
        assert_eq!(
            infer_ok("1.0f + 2.0f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn binary_dec_add() {
        assert_eq!(
            infer_ok("1.0 + 2.0", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn binary_int_float_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 + 2.0f");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_float_dec_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1.0f + 2.0");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_duration_add() {
        assert_eq!(
            infer_ok("#30d + #5h", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_sub() {
        assert_eq!(
            infer_ok("#30d - #5h", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_mul_int() {
        assert_eq!(
            infer_ok("#30d * 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_int_mul_duration() {
        assert_eq!(
            infer_ok("3 * #30d", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_mul_float() {
        assert_eq!(
            infer_ok("#30d * 1.5f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_mul_dec() {
        assert_eq!(
            infer_ok("#30d * 1.5", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_div_int() {
        assert_eq!(
            infer_ok("#30d / 2", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_rem_int_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d % 7");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_duration_mod_int_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d mod 7");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_duration_div_duration() {
        assert_eq!(
            infer_ok("#90s / #30s", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn binary_duration_rem_duration() {
        assert_eq!(
            infer_ok("#90s % #7s", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_mod_duration() {
        assert_eq!(
            infer_ok("#90s mod #7s", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn binary_duration_add_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d + 3");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_duration_mul_duration_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d * #5h");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_comparison_eq() {
        assert_eq!(
            infer_ok("1 == 2", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_ne() {
        assert_eq!(
            infer_ok("1.0f != 2e5f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_lt() {
        assert_eq!(
            infer_ok("1 < 2", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_gt() {
        assert_eq!(
            infer_ok("10.5 > 11.0", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_le() {
        assert_eq!(
            infer_ok("20 <= 40", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_ge() {
        assert_eq!(
            infer_ok("1001 >= 1000", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_string_eq() {
        assert_eq!(
            infer_ok(r#""a" == "b""#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_string_lt() {
        assert_eq!(
            infer_ok(r#""x" < "y""#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_comparison_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"1 == "a""#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_logical_and() {
        assert_eq!(
            infer_ok("true && false", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_logical_or() {
        assert_eq!(
            infer_ok("false || true", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_logical_and_non_bool_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 && 2");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_in_string_tuple() {
        assert_eq!(
            infer_ok(r#""a" in ("a", "b", "c")"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_in_non_tuple_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 in 2");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_in_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"1 in ("a", "b")"#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn optional_eq_optional() {
        assert_eq!(
            infer_ok(
                "users.first().unwrap().profile == users.first().unwrap().profile",
                &test_schema(),
            )
            .ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn optional_eq_non_optional_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("none != 1");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn optional_ordering_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("none < none");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn unary_neg_int() {
        assert_eq!(
            infer_ok("-1", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn unary_neg_float() {
        assert_eq!(
            infer_ok("-3e6f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn unary_neg_dec() {
        assert_eq!(
            infer_ok("-12.5", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn unary_neg_duration() {
        assert_eq!(
            infer_ok("-#30d", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn unary_neg_instant_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("-@now");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn unary_neg_string_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"-"hello""#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn unary_not_bool() {
        assert_eq!(
            infer_ok("!true", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn unary_not_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("!1");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_complex_arithmetic() {
        assert_eq!(
            infer_ok("1 + 2 * (3 / 2) % (7 - 2)", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_arithmetic_comparison() {
        assert_eq!(
            infer_ok("(1 + 2 - 3) >= 3", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_field_arithmetic() {
        assert_eq!(
            infer_ok("users.first().unwrap().age + 1", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn binary_field_comparison() {
        assert_eq!(
            infer_ok("users.first().unwrap().age < 18", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn binary_duration_arithmetic() {
        assert_eq!(
            infer_ok("#30d + #24h - #7d + #7.8s", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Duration)
        );
    }

    #[test]
    fn if_else_basic() {
        assert_eq!(
            infer_ok(r#"if true { 1 } else { 2 }"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn if_else_bool_result() {
        assert_eq!(
            infer_ok(r#"if true { false } else { true }"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn if_else_condition_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"if 42 { 1 } else { 2 }"#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn if_else_branch_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"if true { 1 } else { "two" }"#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn if_else_if_chain() {
        assert_eq!(
            infer_ok(
                r#"if true { "A" } else if false { "B" } else { "C" }"#,
                &test_schema(),
            )
            .ty,
            QueryType::Scalar(ScalarType::String)
        );
    }

    #[test]
    fn if_else_with_comparison_condition() {
        assert_eq!(
            infer_ok(
                r#"if users.first().unwrap().age > 18 { "adult" } else { "minor" }"#,
                &test_schema(),
            )
            .ty,
            QueryType::Scalar(ScalarType::String)
        );
    }

    #[test]
    fn cast_int_to_float() {
        assert_eq!(
            infer_ok("42 as Float", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn cast_int_to_dec() {
        assert_eq!(
            infer_ok("42 as Dec", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn cast_float_to_int() {
        assert_eq!(
            infer_ok("1.0f as Int", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn cast_float_to_dec() {
        assert_eq!(
            infer_ok("1.0f as Dec", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn cast_dec_to_int() {
        assert_eq!(
            infer_ok("1.0 as Int", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn cast_dec_to_float() {
        assert_eq!(
            infer_ok("1.0 as Float", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn cast_bool_to_int() {
        assert_eq!(
            infer_ok("true as Int", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn cast_disallowed_type() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#""hello" as Int"#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn cast_complex_expr() {
        assert_eq!(
            infer_ok("users.first().unwrap().age as Float + 1.0f", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn fallible_cast_optional_result() {
        assert_eq!(
            infer_ok(r#""42" as? Int"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
        assert_eq!(
            infer_ok(r#""true" as? Bool"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool).wrap_optional()
        );
    }

    #[test]
    fn fallible_cast_strict_rejection() {
        for source in [
            "1 as? Int",
            "some(1) as? Float",
            "1 as? ?Int",
            "true as? Float",
        ] {
            assert_eq!(
                error_codes(source, &test_schema()),
                vec!["QT0013"],
                "source: {source}"
            );
        }
    }

    #[test]
    fn fallible_enum_cast_matching_underlying_type_required() {
        let schema = enum_schema();
        assert_eq!(error_codes(r#""low" as? Level"#, &schema), vec!["QT0013"]);
        assert_eq!(error_codes("2 as? Priority", &schema), vec!["QT0013"]);
    }

    #[test]
    fn some_basic() {
        assert_eq!(
            infer_ok(r#"some("hello")"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::String).wrap_optional()
        );
    }

    #[test]
    fn some_complex_expr() {
        assert_eq!(
            infer_ok("some(users.first().unwrap().age)", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
    }

    #[test]
    fn some_none() {
        assert_eq!(
            infer_ok("none", &test_schema()).ty,
            QueryType::Unknown.wrap_optional()
        );
    }

    #[test]
    fn some_nested_optional() {
        assert_eq!(
            infer_ok(r#"some(some("a"))"#, &test_schema()).ty,
            QueryType::Optional(Box::new(QueryType::Optional(Box::new(QueryType::Scalar(
                ScalarType::String
            )))))
        );
    }

    #[test]
    fn infer_nested_optional() {
        assert_eq!(
            infer_ok(r#"some(some("a")) != some(none)"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn array_int() {
        assert_eq!(
            infer_ok("[1, 2, 3]", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
        );
    }

    #[test]
    fn array_string() {
        assert_eq!(
            infer_ok(r#"["a", "b", "c"]"#, &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
        );
    }

    #[test]
    fn array_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"[1, "two", 3]"#);
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn array_types_inferred() {
        let result = infer_ok("[none, some(1), none]", &test_schema());
        assert_eq!(
            result.ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int).wrap_optional()))
        );
        match result.kind {
            TypedExprKind::Array { elements } => {
                assert_eq!(
                    elements.into_iter().map(|e| e.ty).collect::<Vec<_>>(),
                    vec![QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int))); 3]
                )
            }
            _ => panic!(),
        }
    }

    #[test]
    fn array_empty() {
        assert_eq!(
            infer_ok("[]", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Unknown))
        );
    }

    #[test]
    fn tuple_mixed_types() {
        assert_eq!(
            infer_ok(r#"(1, "hello", false, @now, 1.7e-5)"#, &test_schema()).ty,
            QueryType::Tuple(vec![
                QueryType::Scalar(ScalarType::Int),
                QueryType::Scalar(ScalarType::String),
                QueryType::Scalar(ScalarType::Bool),
                QueryType::Scalar(ScalarType::Instant),
                QueryType::Scalar(ScalarType::Dec),
            ])
        );
    }

    #[test]
    fn tuple_in_membership() {
        assert_eq!(
            infer_ok(r#"1 in (1, 2, 3)"#, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bool)
        );
    }

    #[test]
    fn projection_basic() {
        assert!(matches!(
            infer_ok("users { name, age }", &test_schema()).ty,
            QueryType::List(inner)
            if matches!(
                *inner,
                QueryType::Record(RecordSource::Projection(ref fields))
                if fields.len() == 2
            )
        ));
    }

    #[test]
    fn projection_single_field() {
        let result = infer_ok("users { name }", &test_schema());
        match &result.ty {
            QueryType::List(inner) => match inner.as_ref() {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields.len(), 1);
                    assert_eq!(fields[0].name, "name");
                    assert_eq!(fields[0].ty, QueryType::Scalar(ScalarType::String));
                }
                other => panic!("expected Record, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn projection_computed_field() {
        let result = infer_ok(
            "users { name, adult = users.first().unwrap().age > 18 }",
            &test_schema(),
        );
        match &result.ty {
            QueryType::List(inner) => match inner.as_ref() {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].name, "name");
                    assert_eq!(fields[1].name, "adult");
                    assert_eq!(fields[1].ty, QueryType::Scalar(ScalarType::Bool));
                }
                other => panic!("expected Record, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    fn projected_fields(expr: TypedExpr) -> (Vec<String>, Vec<QueryType>) {
        let ty = match expr.ty {
            QueryType::List(inner) => *inner,
            ty => ty,
        };
        let QueryType::Record(RecordSource::Projection(fields)) = ty else {
            panic!("expected projection record, got {ty:?}");
        };
        fields
            .into_iter()
            .map(|field| (field.name, field.ty))
            .unzip()
    }

    #[test]
    fn projection_spread_all_fields() {
        let result = infer_ok("users { ... }", &test_schema());
        assert_eq!(
            projected_fields(result).0,
            [
                "name",
                "age",
                "score",
                "balance",
                "active",
                "created",
                "profile",
                "maybe_pos",
            ]
        );
    }

    #[test]
    fn projection_spread_existing_preserved() {
        let result = infer_ok("users { active, ... }", &test_schema());
        assert_eq!(
            projected_fields(result).0,
            [
                "active",
                "name",
                "age",
                "score",
                "balance",
                "created",
                "profile",
                "maybe_pos",
            ]
        );
    }

    #[test]
    fn projection_spread_match_by_name() {
        let result = infer_ok("users { n = name, ... }", &test_schema());
        assert_eq!(
            projected_fields(result).0,
            [
                "n",
                "name",
                "age",
                "score",
                "balance",
                "active",
                "created",
                "profile",
                "maybe_pos",
            ]
        );

        let result = infer_ok("users { name = 1, ... }", &test_schema());
        let fields = projected_fields(result);
        assert_eq!(fields.0[0], "name");
        assert_eq!(fields.1[0], QueryType::Scalar(ScalarType::Int));
        assert_eq!(
            fields.0,
            [
                "name",
                "age",
                "score",
                "balance",
                "active",
                "created",
                "profile",
                "maybe_pos",
            ]
        );
    }

    #[test]
    fn projection_spread_chained_projection() {
        let result = infer_ok("users { name, age } { age, ... }", &test_schema());
        assert_eq!(projected_fields(result).0, ["age", "name"]);
    }

    #[test]
    fn projection_spread_literal() {
        let result = infer_ok("{ a = 1, b = 2 } { b, ... }", &test_schema());
        assert_eq!(projected_fields(result).0, ["b", "a"]);
    }

    #[test]
    fn projection_spread_noop_all_fields_named() {
        let result = infer_ok(
            "users { name, age, score, balance, active, created, profile, maybe_pos, ... }",
            &test_schema(),
        );
        assert_eq!(projected_fields(result).0.len(), 8);
    }

    #[test]
    fn projection_empty_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users { }");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn projection_duplicate_field_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users { name, name }");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn projection_on_non_record_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("42 { name }");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn projection_derived_alias() {
        let result = infer_ok("users { name, profile?.username }", &test_schema());
        match &result.ty {
            QueryType::List(inner) => match inner.as_ref() {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].name, "name");
                    assert_eq!(fields[1].name, "username");
                    assert_eq!(
                        fields[1].ty,
                        QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::String)))
                    );
                }
                other => panic!("expected Record, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn projection_single_record_base() {
        assert!(matches!(
            infer_ok("users.first().unwrap() { name, age }", &test_schema()).ty,
            QueryType::Record(RecordSource::Projection(_))
        ));
    }

    #[test]
    fn mutation_insert() {
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_update() {
        let (file, _diags) = crate::parse_query(r#"users[age == 30].update({ age = 31 }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_update_with_prev() {
        let (file, _diags) =
            crate::parse_query(r#"users[age == 30].update({ age = prev.age + 1 }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_delete() {
        let (file, _diags) = crate::parse_query(r#"users[!active].delete(); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_on_projection_error() {
        let (file, _diags) = crate::parse_query(r#"users { name }.insert({ name = "Alice" }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_on_scalar_error() {
        let (file, _diags) = crate::parse_query(r#"42.insert({ name = "Alice" }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_delete_arg_error() {
        let (file, _diags) = crate::parse_query(r#"users[!active].delete("abc"); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_extra_args_error() {
        let schema = test_schema();
        for source in [
            r#"users.insert({ name = "A" }, { name = "B" }); 0"#,
            r#"users.update({ age = 1 }, { age = 2 }); 0"#,
            r#"users.delete(1); 0"#,
        ] {
            assert_eq!(error_codes(source, &schema), ["QT0004"], "{source}");
        }
    }

    #[test]
    fn mutation_missing_insert_arg_error() {
        assert_eq!(error_codes("users.insert(); 0", &test_schema()), ["QT0004"]);
    }

    #[test]
    fn mutation_optional_depth_preserved() {
        let typed = typecheck_ok("users.first()?.delete(); 0", &test_schema());
        let TypedStatement::Mutation(TypedMutationStmt::Delete { opt_depth, .. }) =
            &typed.statements[0]
        else {
            panic!("expected delete mutation");
        };
        assert_eq!(*opt_depth, 1);
    }

    #[test]
    fn mutation_record_targets() {
        let schema = test_schema();
        typecheck_ok("users.first().unwrap().delete(); 0", &schema);
        typecheck_ok("users.first().unwrap().update({ age = 31 }); 0", &schema);
        typecheck_ok("users.first()?.delete(); 0", &schema);
        typecheck_ok("users.first()?.update({ age = 31 }); 0", &schema);
    }

    #[test]
    fn mutation_optional_target_errors() {
        let schema = test_schema();
        assert_eq!(
            error_codes("users.first().delete(); 0", &schema),
            ["QT0035"]
        );
        assert_eq!(
            error_codes("users.first().update({ age = 31 }); 0", &schema),
            ["QT0035"]
        );
        assert_eq!(
            error_codes("users.first()??.delete(); 0", &schema),
            ["QT0032"]
        );
    }

    #[test]
    fn mutation_let_targets_rejected() {
        let schema = mutation_schema();
        assert_eq!(
            error_codes("let u = users; u.delete(); 0", &schema),
            ["QT0036"]
        );
        assert_eq!(
            error_codes(
                r#"let u = users; u.insert({ id = 1, name = "A", orders = [] }); 0"#,
                &schema
            ),
            ["QT0036"]
        );
    }

    #[test]
    fn mutation_insert_targets() {
        let schema = mutation_schema();
        typecheck_ok(
            r#"users.insert({ id = 1, name = "A", orders = [] }); 0"#,
            &schema,
        );
        typecheck_ok(
            r#"users[id > 0].insert({ id = 1, name = "A", orders = [] }); 0"#,
            &schema,
        );
        typecheck_ok(
            r#"users.first().unwrap().orders.insert({ id = 1, total = 10 }); 0"#,
            &schema,
        );
        typecheck_ok(
            r#"users.first()?.orders?.insert({ id = 1, total = 10 }); 0"#,
            &schema,
        );

        assert_eq!(
            error_codes(
                r#"users.first().unwrap().insert({ id = 1, name = "A", orders = [] }); 0"#,
                &schema
            ),
            ["QT0037"]
        );
        assert_eq!(
            error_codes(
                r#"users.orders.flatten().insert({ id = 1, total = 10 }); 0"#,
                &schema
            ),
            ["QT0038"]
        );
        assert_eq!(
            error_codes(
                r#"users.extend(users.take(0)).insert({ id = 1, name = "A", orders = [] }); 0"#,
                &schema
            ),
            ["QT0038"]
        );
        assert_eq!(
            error_codes(
                r#"images.first().unwrap().orders.insert({ id = 1, total = 10 }); 0"#,
                &schema
            ),
            ["QT0039"]
        );
    }

    #[test]
    fn mutation_nested_and_non_owning_delete_targets() {
        let schema = mutation_schema();
        typecheck_ok("users.orders.flatten().delete(); 0", &schema);
        typecheck_ok("users.orders.flatten().update({ total = 1 }); 0", &schema);
        typecheck_ok("users.first()?.orders?.delete(); 0", &schema);
        typecheck_ok("images.first().unwrap().orders.delete(); 0", &schema);
    }

    #[test]
    fn mutation_projected_and_untraceable_targets() {
        let schema = mutation_schema();
        assert_eq!(
            error_codes(
                r#"users { id }.insert({ id = 1, name = "A", orders = [] }); 0"#,
                &schema
            ),
            ["QT0019"]
        );
        assert_eq!(error_codes("users { id }.delete(); 0", &schema), ["QT0019"]);
        assert_eq!(
            error_codes(
                r#"users.map($).insert({ id = 1, name = "A", orders = [] }); 0"#,
                &schema
            ),
            ["QT0036"]
        );
        assert_eq!(error_codes("users.map($).delete(); 0", &schema), ["QT0036"]);
    }

    #[test]
    fn mutation_target_provenance_tracked() {
        let schema = test_schema();
        let user = schema.roots[0].struct_id;
        let profile_field = schema.struct_field(user, "profile").unwrap().0;

        assert_eq!(
            infer_ok("users", &schema).provenance,
            TargetProvenance::root(0)
        );
        assert_eq!(
            infer_ok("users.filter(active)", &schema).provenance,
            TargetProvenance::root(0)
        );
        assert_eq!(
            infer_ok("users.first().unwrap()", &schema).provenance,
            TargetProvenance {
                rows: true,
                collection: None,
            }
        );
        assert_eq!(
            infer_ok("users.first().unwrap().profile", &schema).provenance,
            TargetProvenance::ref_collection(profile_field)
        );
        assert_eq!(
            infer_ok("users.profile", &schema).provenance,
            TargetProvenance::ref_collection(profile_field)
        );
        assert_eq!(
            infer_ok("users.name", &schema).provenance,
            TargetProvenance::NONE
        );

        assert_eq!(
            infer_ok("some(users)", &schema).provenance,
            TargetProvenance::root(0)
        );
        assert_eq!(
            infer_ok("if true { users } else { users }", &schema).provenance,
            TargetProvenance::root(0)
        );
        assert_eq!(
            infer_ok("users.extend(users.take(0))", &schema).provenance,
            TargetProvenance {
                rows: true,
                collection: None,
            }
        );
        assert_eq!(
            infer_ok("users.map($)", &schema).provenance,
            TargetProvenance::NONE
        );
        assert_eq!(
            infer_ok("users { name }", &schema).provenance,
            TargetProvenance::NONE
        );
    }

    #[test]
    fn mutation_target_provenance_not_propagated_through_let() {
        let typed = typecheck_ok("let u = users; u", &test_schema());
        assert_eq!(typed.result.unwrap().provenance, TargetProvenance::NONE);
    }

    #[test]
    fn type_constant_int() {
        assert_eq!(
            infer_ok("Int::MAX", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn type_constant_float() {
        assert_eq!(
            infer_ok("Float::MIN", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Float)
        );
    }

    #[test]
    fn type_constant_dec() {
        assert_eq!(
            infer_ok("Dec::MAX", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Dec)
        );
    }

    #[test]
    fn mutation_insert_missing_required() {
        let (file, _diags) = crate::parse_query(r#"users.insert({ name = "Alice" }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_insert_unknown_field() {
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now, bogus = 1 }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_insert_wrong_type() {
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = 42, age = 30, score = 1.0f, balance = 0.0, active = true, created = @now }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_update_unknown_field() {
        let (file, _diags) = crate::parse_query(r#"users[name == "Bob"].update({ bogus = 2 }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_update_subset() {
        let (file, _diags) =
            crate::parse_query(r#"users[name == "Bob"].update({ age = 30, score = 2.5f }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_duplicate_field() {
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, name = "Bob", created = @now }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_insert_optional_field_omitted() {
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_insert_with_ref_field() {
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now, profile = none }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty());
    }

    #[test]
    fn file_with_mutations_result_expr() {
        let (file, _diags) = crate::parse_query(
            r#"
            users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now });
            users.insert({ name = "Bob", age = 25, score = 2.0f, balance = 0.0, active = true, created = @now });
            users[!(name in ("Bob", "Alice"))].delete();
            users[name == "Bob"].update({ score = prev.score + 0.4f });
            users { name, age }
            "#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn batch_insert() {
        let (file, _diags) = crate::parse_query(
            r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = "Bob", age = 25, score = 2.0f, balance = 2.0, active = false, created = @today },
            ]); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn batch_insert_element_missing_required() {
        let (file, _diags) = crate::parse_query(
            r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = "Bob", score = 2.0f, balance = 0.0, active = true, created = @now },
            ]); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn batch_insert_element_mismatched_type() {
        let (file, _diags) = crate::parse_query(
            r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = 42, age = 25, score = 2.0f, balance = 0.0, active = true, created = @now },
            ]); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn batch_insert_element_unknown_field() {
        let (file, _diags) = crate::parse_query(
            r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = "Bob", age = 25, score = 2.0f, balance = 0.0, active = true, created = @now, foo = 1 },
            ]); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_if_else_optional_list() {
        assert_eq!(
            infer_ok(
                "if true { none }
                else {
                    if false { some([]) }
                    else { some([1, 2, 3]) }
                }",
                &test_schema(),
            )
            .ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int))).wrap_optional()
        );
    }

    #[test]
    fn typecheck_if_else_ambiguous() {
        let (file, _) = crate::parse_query("if true { none } else { none }");
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_array_optional() {
        assert_eq!(
            infer_ok("[none, some(3), none]", &test_schema()).ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int).wrap_optional()))
        );
    }

    #[test]
    fn typecheck_array_empty() {
        let (file, _) = crate::parse_query("[]");
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_array_mismatched_types() {
        let (file, _) = crate::parse_query(r#"[1, "two", 3]"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_some_none_if_else() {
        assert_eq!(
            infer_ok("if true { some(42) } else { none }", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
    }

    #[test]
    fn typecheck_method_chain() {
        assert_eq!(
            infer_ok("users.first().unwrap().profile?.username", &test_schema()).ty,
            QueryType::Scalar(ScalarType::String).wrap_optional()
        );
    }

    #[test]
    fn typecheck_filter_project() {
        assert!(matches!(
            infer_ok("users[age > 18] { name, profile?.username }", &test_schema()).ty,
            QueryType::List(inner) if matches!(*inner, QueryType::Record(RecordSource::Projection(_)))
        ));
    }

    #[test]
    fn typecheck_optional_comparison_error() {
        let (file, _) = crate::parse_query("none == 1");
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_optional_ambiguous() {
        let (file, _) = crate::parse_query("none == none");
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_filter_method_optional() {
        assert_eq!(
            infer_ok("users[age > 18].first()?.age", &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
    }

    #[test]
    fn typecheck_tuple() {
        assert_eq!(
            infer_ok("if true { (1, 2) } else { (3, 4) }", &test_schema()).ty,
            QueryType::Tuple(vec![
                QueryType::Scalar(ScalarType::Int),
                QueryType::Scalar(ScalarType::Int),
            ])
        );
    }

    #[test]
    fn typecheck_tuple_mismatch() {
        let (file, _) = crate::parse_query(r#"if true { (1, 2, 4) } else { (4, "abc") }"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }
}
