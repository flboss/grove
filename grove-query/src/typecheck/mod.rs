pub mod error;
pub mod types;

use std::collections::HashMap;

use crate::ast::{Expr, Literal, QueryFile, Statement};
use crate::typecheck::error::TypeError;
use crate::typecheck::types::*;
use grove_schema::validated::{ScalarType, ValidatedSchema};
use grove_types::{Diagnostic, Span, Spanned};

pub(crate) struct TypeEnv {
    scopes: Vec<HashMap<String, QueryType>>,
}

#[allow(dead_code)] // not yet used
impl TypeEnv {
    fn new() -> Self {
        TypeEnv {
            scopes: vec![HashMap::new()],
        }
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
}

fn infer_literal(lit: &Spanned<Literal>) -> QueryType {
    match &lit.value {
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

fn infer(expr: &Expr, _schema: &ValidatedSchema, env: &TypeEnv) -> Result<TypedExpr, TypeError> {
    match expr {
        Expr::Literal(lit) => {
            let ty = infer_literal(lit);
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
        _ => Err(TypeError::AmbiguousType { span: expr.span() }),
    }
}

#[allow(dead_code)] // not yet used
fn check(
    expr: &Expr,
    expected: &QueryType,
    schema: &ValidatedSchema,
    env: &TypeEnv,
) -> Result<TypedExpr, TypeError> {
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
        _ => infer(expr, schema, env),
    }
}

fn typecheck_stmt(
    stmt: &Statement,
    schema: &ValidatedSchema,
    env: &TypeEnv,
) -> Result<TypedStatement, TypeError> {
    match stmt {
        Statement::Expr(expr) => {
            let typed = infer(expr, schema, env)?;
            Ok(TypedStatement::Expr(typed))
        }
        // TODO:
        Statement::Mutation(_) => Err(TypeError::AmbiguousType {
            span: stmt_span(stmt),
        }),
    }
}

fn stmt_span(stmt: &Statement) -> Span {
    match stmt {
        Statement::Expr(expr) => expr.span(),
        Statement::Mutation(m) => m.kind.span,
    }
}

pub fn typecheck(
    file: QueryFile,
    schema: &ValidatedSchema,
) -> (Option<TypedQueryFile>, Vec<Diagnostic>) {
    let env = TypeEnv::new();
    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut statements = Vec::new();

    for stmt in &file.statements {
        match typecheck_stmt(stmt, schema, &env) {
            Ok(typed) => statements.push(typed),
            Err(err) => diags.push(err.into()),
        }
    }

    match infer(&file.result, schema, &env) {
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

    #[test]
    fn infer_int_literal() {
        let schema = ValidatedSchema::default();
        let env = TypeEnv::new();
        let (file, _diags) = crate::parse_query("42");
        let result = infer(&file.unwrap().result, &schema, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn infer_float_literal() {
        let schema = ValidatedSchema::default();
        let env = TypeEnv::new();
        let (file, _diags) = crate::parse_query("3.14f");
        let result = infer(&file.unwrap().result, &schema, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Float));
    }

    #[test]
    fn infer_dec_literal() {
        let schema = ValidatedSchema::default();
        let env = TypeEnv::new();
        let (file, _diags) = crate::parse_query("3.14");
        let result = infer(&file.unwrap().result, &schema, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Dec));
    }

    #[test]
    fn infer_string_literal() {
        let schema = ValidatedSchema::default();
        let env = TypeEnv::new();
        let (file, _diags) = crate::parse_query("\"hello\"");
        let result = infer(&file.unwrap().result, &schema, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
    }

    #[test]
    fn infer_bool_literal() {
        let schema = ValidatedSchema::default();
        let env = TypeEnv::new();
        let (file, _diags) = crate::parse_query("true");
        let result = infer(&file.unwrap().result, &schema, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn infer_instant_literal() {
        let schema = ValidatedSchema::default();
        let env = TypeEnv::new();
        let (file, _diags) = crate::parse_query("@now");
        let result = infer(&file.unwrap().result, &schema, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Instant));
    }

    #[test]
    fn infer_duration_literal() {
        let schema = ValidatedSchema::default();
        let env = TypeEnv::new();
        let (file, _diags) = crate::parse_query("#30d");
        let result = infer(&file.unwrap().result, &schema, &env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn none_inferred_type() {
        let schema = ValidatedSchema::default();
        let env = TypeEnv::new();
        let expected = QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int)));
        let (file, _diags) = crate::parse_query("none");
        let result = check(&file.unwrap().result, &expected, &schema, &env).unwrap();
        assert_eq!(result.ty, expected);
    }

    #[test]
    fn none_without_context_unknown() {
        let schema = ValidatedSchema::default();
        let env = TypeEnv::new();
        let (file, _diags) = crate::parse_query("none");
        let result = infer(&file.unwrap().result, &schema, &env).unwrap();
        assert_eq!(result.ty, QueryType::Optional(Box::new(QueryType::Unknown)));
    }

    #[test]
    fn env_scope_resolution() {
        let mut env = TypeEnv::new();
        env.define("x".into(), QueryType::Scalar(ScalarType::Int));
        assert_eq!(env.resolve("x"), Some(&QueryType::Scalar(ScalarType::Int)));

        env.push_scope();
        env.define("y".into(), QueryType::Scalar(ScalarType::String));
        assert_eq!(env.resolve("x"), Some(&QueryType::Scalar(ScalarType::Int)));
        assert_eq!(
            env.resolve("y"),
            Some(&QueryType::Scalar(ScalarType::String))
        );

        env.pop_scope();
        assert_eq!(env.resolve("x"), Some(&QueryType::Scalar(ScalarType::Int)));
        assert_eq!(env.resolve("y"), None);
    }

    #[test]
    fn env_shadowing() {
        let mut env = TypeEnv::new();
        env.define("x".into(), QueryType::Scalar(ScalarType::Int));

        env.push_scope();
        env.define("x".into(), QueryType::Scalar(ScalarType::String));
        assert_eq!(
            env.resolve("x"),
            Some(&QueryType::Scalar(ScalarType::String))
        );

        env.pop_scope();
        assert_eq!(env.resolve("x"), Some(&QueryType::Scalar(ScalarType::Int)));
    }
}
