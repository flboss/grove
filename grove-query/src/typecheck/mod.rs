pub mod error;
pub mod types;

use std::collections::HashMap;

use crate::ast::{Expr, Literal, QueryFile, Statement};
use crate::typecheck::error::TypeError;
use crate::typecheck::types::*;
use grove_schema::validated::{Field, ScalarType, ValidatedSchema, ValueType};
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

    #[allow(dead_code)] // not yet used
    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    #[allow(dead_code)] // not yet used
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
}

fn value_type_to_query_type(ty: &ValueType) -> QueryType {
    match ty {
        ValueType::Scalar(s) => QueryType::Scalar(*s),
        ValueType::Tuple(types) => {
            QueryType::Tuple(types.iter().map(value_type_to_query_type).collect())
        }
        ValueType::Optional(inner) => {
            QueryType::Optional(Box::new(value_type_to_query_type(inner)))
        }
        ValueType::Array { element, .. } => {
            QueryType::List(Box::new(value_type_to_query_type(element)))
        }
    }
}

fn field_query_type(field: &Field) -> QueryType {
    match field {
        Field::Value { ty, .. } => value_type_to_query_type(ty),
        Field::Array { element, .. } => {
            QueryType::List(Box::new(value_type_to_query_type(element)))
        }
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
                QueryType::Optional(Box::new(inner))
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

fn strip_wrappers(ty: &QueryType) -> (&QueryType, bool, bool) {
    match ty {
        QueryType::List(inner) => {
            let (inner, _is_list, is_optional) = strip_wrappers(inner);
            (inner, true, is_optional)
        }
        QueryType::Optional(inner) => {
            let (inner, is_list, _) = strip_wrappers(inner);
            (inner, is_list, true)
        }
        _ => (ty, false, false),
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
        Literal::None => QueryType::Optional(Box::new(QueryType::Unknown)),
    }
}

fn infer(expr: &Expr, env: &TypeEnv) -> Result<TypedExpr, TypeError> {
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
        _ => Err(TypeError::AmbiguousType { span: expr.span() }),
    }
}

fn infer_field(
    base: TypedExpr,
    name: &Spanned<String>,
    optional: bool,
    schema: &ValidatedSchema,
) -> Result<TypedExpr, TypeError> {
    let (inner, was_list, was_optional) = strip_wrappers(&base.ty);

    let field_ty = record_field_by_name(inner, &name.value, schema).ok_or_else(|| {
        TypeError::UnknownField {
            field: name.value.clone(),
            base_ty: base.ty.to_string(),
            span: name.span,
        }
    })?;

    let mut ty = field_ty;
    if was_list {
        ty = QueryType::List(Box::new(ty));
    }
    if was_optional || optional {
        ty = QueryType::Optional(Box::new(ty));
    }

    let span = Span::new(base.span.start, name.span.end);
    Ok(TypedExpr {
        kind: TypedExprKind::Field {
            base: Box::new(base),
            name: name.clone(),
            optional,
        },
        ty,
        span,
    })
}

#[allow(dead_code)] // not yet used
fn check(expr: &Expr, expected: &QueryType, env: &TypeEnv) -> Result<TypedExpr, TypeError> {
    match expr {
        Expr::Literal(Spanned {
            value: Literal::None,
            span,
        }) => {
            let ty = match expected {
                QueryType::Optional(inner) => QueryType::Optional(inner.clone()),
                other => QueryType::Optional(Box::new(other.clone())),
            };
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

fn typecheck_stmt(stmt: &Statement, env: &TypeEnv) -> Result<TypedStatement, TypeError> {
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
    let env = TypeEnv::new(schema);
    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut statements = Vec::new();

    for stmt in &file.statements {
        match typecheck_stmt(stmt, &env) {
            Ok(typed) => statements.push(typed),
            Err(err) => diags.push(err.into()),
        }
    }

    match infer(&file.result, &env) {
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
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("42");
        let result = infer(&file.unwrap().result, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn infer_float_literal() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("3.14f");
        let result = infer(&file.unwrap().result, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Float));
    }

    #[test]
    fn infer_dec_literal() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("3.14");
        let result = infer(&file.unwrap().result, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Dec));
    }

    #[test]
    fn infer_string_literal() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("\"hello\"");
        let result = infer(&file.unwrap().result, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
    }

    #[test]
    fn infer_bool_literal() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("true");
        let result = infer(&file.unwrap().result, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn infer_instant_literal() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("@now");
        let result = infer(&file.unwrap().result, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Instant));
    }

    #[test]
    fn infer_duration_literal() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("#30d");
        let result = infer(&file.unwrap().result, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn none_inferred_type() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let expected = QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int)));
        let (file, _diags) = crate::parse_query("none");
        let result = check(&file.unwrap().result, &expected, &env).unwrap();
        assert_eq!(result.ty, expected);
    }

    #[test]
    fn none_without_context_unknown() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("none");
        let result = infer(&file.unwrap().result, &env).unwrap();
        assert_eq!(result.ty, QueryType::Optional(Box::new(QueryType::Unknown)));
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
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("posts");
        let result = infer(&file.unwrap().result, &env);
        assert!(result.is_err());
    }

    #[test]
    fn field_access_struct() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.name");
        let result = infer(&file.unwrap().result, &env).unwrap();
        assert_eq!(
            result.ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
        );
    }

    #[test]
    fn field_access_unknown_field() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.unknown");
        let result = infer(&file.unwrap().result, &env);
        assert!(result.is_err());
    }

    #[test]
    fn optional_field_access() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.profile");
        let result = infer(&file.unwrap().result, &env).unwrap();
        assert!(
            matches!(result.ty, QueryType::List(inner) if matches!(*inner, QueryType::Optional(_)))
        );
    }
}
