pub mod error;
pub mod types;

use std::collections::HashMap;

use crate::ast::{Arg, Expr, Literal, QueryFile, Statement};
use crate::typecheck::error::TypeError;
use crate::typecheck::types::*;
use grove_schema::validated::{Field, ScalarType, StructId, ValidatedSchema};
use grove_types::{Diagnostic, Span, Spanned};

struct TypeEnv<'s> {
    scopes: Vec<HashMap<String, QueryType>>,
    schema: &'s ValidatedSchema,
}

impl<'s> TypeEnv<'s> {
    fn new(schema: &'s ValidatedSchema) -> Self {
        let mut env = TypeEnv {
            scopes: vec![HashMap::new()],
            schema,
        };

        for root in &schema.roots {
            let record_ty = QueryType::Record(RecordSource::Schema(root.struct_id));
            let list_ty = QueryType::List(Box::new(record_ty));
            env.define(root.name.clone(), list_ty);
        }

        env
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: String, ty: QueryType) {
        self.scopes.last_mut().unwrap().insert(name, ty);
    }

    fn resolve(&self, name: &str) -> Option<&QueryType> {
        for scope in self.scopes.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Some(ty);
            }
        }
        None
    }

    fn schema(&self) -> &ValidatedSchema {
        self.schema
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
                    self.define(field.name.clone(), field.ty.clone());
                }
                true
            }
            QueryType::List(inner) => self.push_record_fields(inner),
            _ => false,
        }
    }

    fn push_schema_struct_fields(&mut self, struct_id: StructId) {
        let struct_ = &self.schema.structs[struct_id.index()];
        for field in &struct_.fields {
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
            self.define(name, ty);
        }
    }
}

fn field_query_type(field: &Field) -> QueryType {
    match field {
        Field::Value { ty, .. } => ty.into(),
        Field::Array { element, .. } => QueryType::List(Box::new(element.into())),
        Field::Ref {
            target,
            optional,
            is_list,
            ..
        } => {
            let inner = QueryType::Record(RecordSource::Schema(*target));
            if *is_list {
                QueryType::List(Box::new(inner))
            } else if *optional {
                inner.wrap_optional()
            } else {
                inner
            }
        }
    }
}

fn record_field_by_name(
    record: &QueryType,
    name: &str,
    schema: &ValidatedSchema,
) -> Option<QueryType> {
    match record {
        QueryType::Record(RecordSource::Schema(struct_id)) => {
            schema.struct_field(*struct_id, name).map(field_query_type)
        }
        QueryType::Record(RecordSource::Projection(fields)) => {
            fields.iter().find(|f| f.name == name).map(|f| f.ty.clone())
        }
        _ => None,
    }
}

fn infer_literal(literal: &Literal) -> QueryType {
    match literal {
        Literal::Int(_) => QueryType::Scalar(ScalarType::Int),
        Literal::Float(_) => QueryType::Scalar(ScalarType::Float),
        Literal::Dec(_) => QueryType::Scalar(ScalarType::Dec),
        Literal::String(_) => QueryType::Scalar(ScalarType::String),
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
            let ty = infer_literal(&lit.value);
            Ok(TypedExpr {
                kind: TypedExprKind::Literal(lit.clone()),
                ty,
                span: lit.span,
            })
        }
        Expr::Ident(name) => {
            let ty =
                env.resolve(&name.value)
                    .cloned()
                    .ok_or_else(|| TypeError::UnknownIdentifier {
                        name: name.value.clone(),
                        span: name.span,
                    })?;
            Ok(TypedExpr {
                kind: TypedExprKind::Ident(name.clone()),
                ty,
                span: name.span,
            })
        }
        Expr::Field {
            base,
            name,
            optional,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_field(typed_base, name, *optional, env.schema())
        }
        Expr::Method {
            base,
            name,
            args,
            optional,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_method(typed_base, name, args, *optional, env)
        }
        _ => Err(TypeError::AmbiguousType { span: expr.span() }),
    }
}

fn infer_field(
    base: TypedExpr,
    name: &Spanned<String>,
    optional: bool,
    schema: &ValidatedSchema,
) -> Result<TypedExpr, TypeError> {
    let lookup_ty = if optional {
        match &base.ty {
            QueryType::Optional(inner) => inner.as_ref().clone(),
            other => {
                return Err(TypeError::ArgTypeMismatch {
                    method: name.value.clone(),
                    expected: "optional type".into(),
                    got: other.to_string(),
                    span: name.span,
                });
            }
        }
    } else {
        base.ty.clone()
    };

    let (inner, was_list) = match &lookup_ty {
        QueryType::List(inner) => (inner.as_ref().clone(), true),
        other => (other.clone(), false),
    };

    let mut field_ty = record_field_by_name(&inner, &name.value, schema).ok_or_else(|| {
        TypeError::UnknownField {
            field: name.value.clone(),
            base_ty: base.ty.to_string(),
            span: name.span,
        }
    })?;

    if was_list {
        field_ty = QueryType::List(Box::new(field_ty));
    }
    if optional {
        field_ty = field_ty.wrap_optional();
    }

    let span = Span::new(base.span.start, name.span.end);
    Ok(TypedExpr {
        kind: TypedExprKind::Field {
            base: Box::new(base),
            name: name.clone(),
            optional,
        },
        ty: field_ty,
        span,
    })
}

fn infer_method(
    base: TypedExpr,
    name: &Spanned<String>,
    args: &[Arg],
    optional: bool,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let resolve_ty = if optional {
        match &base.ty {
            QueryType::Optional(inner) => inner.as_ref().clone(),
            other => {
                return Err(TypeError::ArgTypeMismatch {
                    method: name.value.clone(),
                    expected: "optional type".into(),
                    got: other.to_string(),
                    span: name.span,
                });
            }
        }
    } else {
        base.ty.clone()
    };

    let signature =
        method_signature(&resolve_ty, &name.value).ok_or_else(|| TypeError::UnknownMethod {
            method: name.value.clone(),
            base_ty: base.ty.to_string(),
            span: name.span,
        })?;

    match &signature.args {
        ArgCheck::Fixed(expected) => {
            if args.len() != expected.len() {
                return Err(TypeError::WrongArgCount {
                    method: name.value.clone(),
                    expected: expected.len(),
                    got: args.len(),
                    span: name.span,
                });
            }
        }
        ArgCheck::Scoped { count, .. } => match count {
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

    let is_scoped = matches!(&signature.args, ArgCheck::Scoped { .. });
    let pushed_scope = is_scoped && env.push_record_fields(&resolve_ty);

    let mut typed_args = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let typed = infer(&arg.expr, env)?;
        match &signature.args {
            ArgCheck::Fixed(expected) => {
                if let Some(exp_ty) = expected.get(i)
                    && !types_compatible(&typed.ty, exp_ty)
                {
                    return Err(TypeError::ArgTypeMismatch {
                        method: name.value.clone(),
                        expected: exp_ty.to_string(),
                        got: typed.ty.to_string(),
                        span: arg.expr.span(),
                    });
                }
            }
            ArgCheck::Scoped { constraint, .. } => {
                if !constraint(&typed.ty) {
                    return Err(TypeError::ArgTypeMismatch {
                        method: name.value.clone(),
                        expected: "valid argument type".into(),
                        got: typed.ty.to_string(),
                        span: arg.expr.span(),
                    });
                }
            }
        }
        typed_args.push(typed);
    }

    if pushed_scope {
        env.pop_scope();
    }

    let return_ty = if optional {
        signature.return_type.wrap_optional()
    } else {
        signature.return_type
    };

    let span = Span::new(base.span.start, name.span.end);
    Ok(TypedExpr {
        kind: TypedExprKind::Method {
            base: Box::new(base),
            name: name.clone(),
            args: typed_args,
            optional,
        },
        ty: return_ty,
        span,
    })
}

fn types_compatible(actual: &QueryType, expected: &QueryType) -> bool {
    match (actual, expected) {
        (QueryType::Unknown, _) | (_, QueryType::Unknown) => true,
        (QueryType::Optional(a), QueryType::Optional(b)) => types_compatible(a, b),
        (a, QueryType::Optional(b)) => types_compatible(a, b),
        (QueryType::List(a), QueryType::List(b)) => types_compatible(a, b),
        (QueryType::Scalar(a), QueryType::Scalar(b)) => a == b,
        (
            QueryType::Record(RecordSource::Schema(a)),
            QueryType::Record(RecordSource::Schema(b)),
        ) => a == b,
        _ => actual == expected,
    }
}

#[derive(Debug, Clone, Copy)]
enum ArgCount {
    Exact(usize),
    AtLeast(usize),
}

#[derive(Debug, Clone)]
enum ArgCheck {
    Fixed(Vec<QueryType>),
    Scoped {
        count: ArgCount,
        constraint: fn(&QueryType) -> bool,
    },
}

#[derive(Debug, Clone)]
struct MethodSig {
    return_type: QueryType,
    args: ArgCheck,
}

impl MethodSig {
    fn fixed(return_type: QueryType, params: Vec<QueryType>) -> Self {
        MethodSig {
            return_type,
            args: ArgCheck::Fixed(params),
        }
    }

    fn no_args(return_type: QueryType) -> Self {
        Self::fixed(return_type, vec![])
    }

    fn one_arg(return_type: QueryType, param: QueryType) -> Self {
        Self::fixed(return_type, vec![param])
    }

    fn scoped_fixed(return_type: QueryType, n: usize, constraint: fn(&QueryType) -> bool) -> Self {
        MethodSig {
            return_type,
            args: ArgCheck::Scoped {
                count: ArgCount::Exact(n),
                constraint,
            },
        }
    }

    fn scoped_at_least(
        return_type: QueryType,
        min: usize,
        constraint: fn(&QueryType) -> bool,
    ) -> Self {
        MethodSig {
            return_type,
            args: ArgCheck::Scoped {
                count: ArgCount::AtLeast(min),
                constraint,
            },
        }
    }
}

fn method_signature(base: &QueryType, method: &str) -> Option<MethodSig> {
    use ScalarType::*;

    match base {
        QueryType::Scalar(s) => scalar_method(s, method),
        QueryType::Optional(inner) => match method {
            "unwrap" => Some(MethodSig::no_args(inner.as_ref().clone())),
            "unwrap_or" => Some(MethodSig::one_arg(
                inner.as_ref().clone(),
                inner.as_ref().clone(),
            )),
            "is_none" | "is_some" => Some(MethodSig::no_args(QueryType::Scalar(Bool))),
            _ => None,
        },
        QueryType::List(inner) => match method {
            "len" => Some(MethodSig::no_args(QueryType::Scalar(Int))),
            "first" => Some(MethodSig::no_args(inner.as_ref().clone().wrap_optional())),
            "nth" => Some(MethodSig::one_arg(
                inner.as_ref().clone().wrap_optional(),
                QueryType::Scalar(Int),
            )),
            "contains" => Some(MethodSig::one_arg(
                QueryType::Scalar(Bool),
                inner.as_ref().clone(),
            )),
            "sum" if inner.is_summable() => {
                Some(MethodSig::no_args(inner.as_ref().clone().wrap_optional()))
            }
            "avg" if inner.is_summable() => {
                Some(MethodSig::no_args(inner.as_ref().clone().wrap_optional()))
            }
            "max" | "min" if inner.has_defined_order() => {
                Some(MethodSig::no_args(inner.as_ref().clone().wrap_optional()))
            }
            "sort" | "sort_asc" | "sort_desc" => Some(MethodSig::scoped_at_least(
                inner.as_ref().clone(),
                1,
                QueryType::has_defined_order,
            )),
            "sum_by" | "avg_by" if matches!(inner.as_ref(), QueryType::Record(_)) => {
                Some(MethodSig::scoped_fixed(
                    QueryType::Unknown.wrap_optional(),
                    1,
                    QueryType::is_summable,
                ))
            }
            "max_by" | "min_by" if matches!(inner.as_ref(), QueryType::Record(_)) => {
                Some(MethodSig::scoped_fixed(
                    inner.as_ref().clone().wrap_optional(),
                    1,
                    QueryType::has_defined_order,
                ))
            }
            _ => None,
        },
        _ => None,
    }
}

fn scalar_method(s: &ScalarType, method: &str) -> Option<MethodSig> {
    use ScalarType::*;
    match s {
        String => match method {
            "contains" | "starts_with" | "ends_with" => Some(MethodSig::one_arg(
                QueryType::Scalar(Bool),
                QueryType::Scalar(String),
            )),
            "to_upper" | "to_lower" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            "len" => Some(MethodSig::no_args(QueryType::Scalar(Int))),
            _ => None,
        },
        Instant => match method {
            "year" | "month" | "day" | "hour" | "minute" | "weekday" => {
                Some(MethodSig::no_args(QueryType::Scalar(Int)))
            }
            "second" | "epoch" => Some(MethodSig::no_args(QueryType::Scalar(Dec))),
            "to_rfc3339" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            _ => None,
        },
        Duration => match method {
            "as_seconds" | "as_minutes" | "as_hours" | "as_days" => {
                Some(MethodSig::no_args(QueryType::Scalar(Dec)))
            }
            _ => None,
        },
        Dec => match method {
            "round_dp" => Some(MethodSig::one_arg(
                QueryType::Scalar(Dec),
                QueryType::Scalar(Int),
            )),
            "to_string" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            _ => None,
        },
        Int => match method {
            "to_string" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            _ => None,
        },
        Float => match method {
            "to_string" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            "round" | "floor" | "ceil" => Some(MethodSig::no_args(QueryType::Scalar(Float))),
            _ => None,
        },
        Bool => match method {
            "to_string" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            _ => None,
        },
    }
}

#[allow(dead_code)] // not yet used
fn check(expr: &Expr, expected: &QueryType, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    match expr {
        Expr::Literal(Spanned {
            value: Literal::None,
            span,
        }) => {
            let ty = expected.clone().wrap_optional();
            Ok(TypedExpr {
                kind: TypedExprKind::Literal(Spanned {
                    span: *span,
                    value: Literal::None,
                }),
                ty,
                span: *span,
            })
        }
        _ => infer(expr, env),
    }
}

fn typecheck_stmt(stmt: &Statement, env: &mut TypeEnv) -> Result<TypedStatement, TypeError> {
    match stmt {
        Statement::Expr(expr) => {
            let typed = infer(expr, env)?;
            Ok(TypedStatement::Expr(typed))
        }
        Statement::Mutation(_) => todo!(),
    }
}

pub fn typecheck(
    file: QueryFile,
    schema: &ValidatedSchema,
) -> (Option<TypedQueryFile>, Vec<Diagnostic>) {
    let mut env = TypeEnv::new(schema);
    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut statements = Vec::new();

    for stmt in &file.statements {
        match typecheck_stmt(stmt, &mut env) {
            Ok(typed) => statements.push(typed),
            Err(err) => diags.push(err.into()),
        }
    }

    match infer(&file.result, &mut env) {
        Ok(result) => (Some(TypedQueryFile { statements, result }), diags),
        Err(err) => {
            diags.push(err.into());
            (None, diags)
        }
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

    #[test]
    fn infer_int_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("42");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn infer_float_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("3.14f");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Float));
    }

    #[test]
    fn infer_dec_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("3.14");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Dec));
    }

    #[test]
    fn infer_string_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("\"hello\"");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
    }

    #[test]
    fn infer_bool_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("true");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn infer_instant_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("@now");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Instant));
    }

    #[test]
    fn infer_duration_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("#30d");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn none_inferred_type() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let expected = QueryType::Scalar(ScalarType::Int).wrap_optional();
        let (file, _diags) = crate::parse_query("none");
        let result = check(&file.unwrap().result, &expected, &mut env).unwrap();
        assert_eq!(result.ty, expected);
    }

    #[test]
    fn none_without_context_unknown() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("none");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Unknown.wrap_optional());
    }

    #[test]
    fn root_lookup() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let ty = env.resolve("users").unwrap();
        assert!(matches!(ty, QueryType::List(_)));
    }

    #[test]
    fn root_unknown_identifier() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("posts");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn field_access_struct() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.name");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(
            result.ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
        );
    }

    #[test]
    fn field_access_unknown_field() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.unknown");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn optional_field_access() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.profile");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert!(
            matches!(result.ty, QueryType::List(inner) if matches!(*inner, QueryType::Optional(_)))
        );
    }

    #[test]
    fn scalar_method() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query(r#""xyz".starts_with("abc")"#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn list_method_len() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.len()");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn list_method_nth() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.nth(0)");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert!(matches!(result.ty, QueryType::Optional(inner)
            if matches!(*inner, QueryType::Record(_))
        ));
    }

    #[test]
    fn method_unwrap_access() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.first().unwrap().name");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
    }

    #[test]
    fn optional_method_is_some() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.first().is_some()");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn method_unknown() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.name.nonexistent()");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn method_wrong_arg_count() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.name.contains(\"a\", \"b\")");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn method_arg_constraint_valid() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.sum_by(age)");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_ok());
    }

    #[test]
    fn method_arg_constraint_invalid() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.sort_asc(profile)");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }
}
