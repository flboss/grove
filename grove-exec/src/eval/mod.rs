use std::collections::HashMap;

use chrono::{DateTime, Utc};
use grove_query::typecheck::types::{IdentBinding, TypedExpr, TypedExprKind, TypedLetStmt};
use grove_schema::validated::ValidatedSchema;

use crate::error::ExecError;
use crate::storage::Storage;
use crate::value::{Interner, Scalar, Val};

pub mod composite;
pub mod data;
pub mod scalar;

use composite::{eval_array, eval_struct, eval_tuple};
use data::exec_root;
use scalar::{eval_binary, eval_cast, eval_method, eval_unary};

/// Lexical value scope chain
#[derive(Debug, Default)]
pub struct Env {
    scopes: Vec<HashMap<String, Val>>,
}

impl Env {
    pub fn new() -> Self {
        Env {
            scopes: vec![HashMap::new()],
        }
    }

    pub fn define(&mut self, name: String, value: Val) {
        self.scopes
            .last_mut()
            .expect("env always has a scope")
            .insert(name, value);
    }

    /// `depth` counts scopes skipped to reach the identifier
    fn lookup(&self, name: &str, depth: usize) -> Option<&Val> {
        self.scopes.iter().rev().nth(depth)?.get(name)
    }
}

pub struct EvalCtx<'a> {
    env: Env,
    now: DateTime<Utc>,
    pub(crate) interner: Interner,
    pub(crate) storage: &'a dyn Storage,
    pub(crate) schema: &'a ValidatedSchema,
}

impl<'a> EvalCtx<'a> {
    pub fn new(
        env: Env,
        now: DateTime<Utc>,
        storage: &'a dyn Storage,
        schema: &'a ValidatedSchema,
    ) -> Self {
        EvalCtx {
            env,
            now,
            interner: Interner::default(),
            storage,
            schema,
        }
    }
}

/// Evaluate an expression
pub fn eval(expr: &TypedExpr, ctx: &mut EvalCtx<'_>) -> Result<Val, ExecError> {
    match &expr.kind {
        TypedExprKind::Literal(lit) => Ok(scalar::decode_literal(lit, ctx.now)),
        TypedExprKind::Ident {
            name,
            binding,
            depth,
        } => match binding {
            IdentBinding::Let => {
                ctx.env
                    .lookup(&name.value, *depth)
                    .cloned()
                    .ok_or_else(|| ExecError::Integrity {
                        message: format!("unbound variable `{}` (internal error)", name.value),
                    })
            }
            IdentBinding::Root { root_idx } => exec_root(ctx, *root_idx),
            IdentBinding::Field(_) | IdentBinding::ProjectedField { .. } => {
                todo!("data references with row contexts")
            }
            IdentBinding::Prev { .. } => todo!("prev with mutations"),
            IdentBinding::ListElement => todo!("element references inside collection operators"),
        },
        TypedExprKind::Some { value } => eval(value, ctx).map(|v| Val::Some(Box::new(v))),
        TypedExprKind::TypeConstant { ty, name } => {
            Ok(scalar::eval_type_constant(ty.value, name.value))
        }
        TypedExprKind::If { arms, default } => {
            for (cond, value) in arms {
                match eval(cond, ctx)? {
                    Val::Scalar(Scalar::Bool(true)) => return eval(value, ctx),
                    Val::Scalar(Scalar::Bool(false)) => {}
                    _ => {
                        return Err(ExecError::Integrity {
                            message: "if condition must be a Bool (internal error)".to_string(),
                        });
                    }
                }
            }
            eval(default, ctx)
        }
        TypedExprKind::Binary { op, lhs, rhs } => eval_binary(op, lhs, rhs, ctx, expr.span),
        TypedExprKind::Unary { op, expr: inner } => eval_unary(op, inner, ctx, expr.span),
        TypedExprKind::Field { .. } | TypedExprKind::TupleAccess { .. } => todo!("navigation"),
        TypedExprKind::Method {
            base,
            name,
            args,
            optional,
        } => eval_method(base, &name.value, args, *optional, ctx, expr.span),
        TypedExprKind::Cast { expr: inner, ty } => {
            let value = eval(inner, ctx)?;
            eval_cast(value, ty.value, expr.span)
        }
        TypedExprKind::Tuple { elements } => eval_tuple(elements, ctx),
        TypedExprKind::Array { elements } => eval_array(elements, ctx),
        TypedExprKind::Struct { fields } => eval_struct(fields, ctx),
        TypedExprKind::Projection { .. } => todo!("projections"),
    }
}

/// Evaluate a `let` and bind its value
pub fn eval_let(stmt: &TypedLetStmt, ctx: &mut EvalCtx<'_>) -> Result<(), ExecError> {
    let value = eval(&stmt.value, ctx)?;
    ctx.env.define(stmt.name.value.clone(), value);
    Ok(())
}

#[cfg(test)]
pub(crate) mod harness {
    use super::*;
    use grove_query::typecheck::types::TypedStatement;

    pub(crate) fn run_src(src: &str) -> Result<Val, ExecError> {
        run_with(src, ValidatedSchema::default())
    }

    pub(crate) fn run_src_config(src: &str, config_block: &str) -> Result<Val, ExecError> {
        let schema_src =
            format!("{config_block}\nroot users: User; struct User {{ name: String }}");
        let (schema, schema_diags) = grove_schema::parse_schema(&schema_src);
        assert!(schema_diags.is_empty(), "{schema_diags:?}");
        let (schema, schema_diags) = grove_schema::validate(schema.unwrap());
        assert!(schema_diags.is_empty(), "{schema_diags:?}");
        run_with(src, schema.unwrap())
    }

    pub(crate) fn run_with(src: &str, schema: ValidatedSchema) -> Result<Val, ExecError> {
        let storage = NoStorage;
        run_with_storage(src, schema, &storage)
    }

    pub(crate) fn run_with_storage<S: Storage>(
        src: &str,
        schema: ValidatedSchema,
        storage: &S,
    ) -> Result<Val, ExecError> {
        let (file, parse_diags) = grove_query::parse_query(src);
        assert!(parse_diags.is_empty(), "{parse_diags:?}");
        let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "{diags:?}");
        let file = typed.unwrap();
        let mut ctx = EvalCtx::new(Env::new(), FIXED_NOW, storage, &schema);
        for stmt in &file.statements {
            match stmt {
                TypedStatement::Let(let_stmt) => eval_let(let_stmt, &mut ctx)?,
                TypedStatement::Expr(expr) => {
                    eval(expr, &mut ctx)?;
                }
                TypedStatement::Mutation(_) => todo!("mutations"),
            }
        }
        eval(&file.result, &mut ctx)
    }

    pub(crate) const FIXED_NOW: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

    pub(crate) struct NoStorage;

    impl Storage for NoStorage {
        fn fetch(
            &self,
            _struct_id: grove_schema::validated::StructId,
            _fields: &[grove_schema::validated::FieldId],
        ) -> Result<Vec<crate::storage::Row>, ExecError> {
            Err(ExecError::Integrity {
                message: "NoStorage backend cannot provide storage (internal error)".to_string(),
            })
        }

        fn fetch_related(
            &self,
            _rel: grove_schema::validated::RelationId,
            _fetch_from_first: bool,
            _parents: &[crate::storage::Key],
            _fields: &[grove_schema::validated::FieldId],
        ) -> Result<Vec<Vec<crate::storage::Row>>, ExecError> {
            Err(ExecError::Integrity {
                message: "NoStorage backend cannot provide storage (internal error)".to_string(),
            })
        }
    }

    pub(crate) fn int(n: i64) -> Val {
        Val::Scalar(crate::value::Scalar::Int(n))
    }

    pub(crate) fn float(x: f64) -> Val {
        Val::Scalar(crate::value::Scalar::Float(x))
    }

    pub(crate) fn str(s: &str) -> Val {
        Val::Scalar(crate::value::Scalar::String(s.to_string()))
    }

    pub(crate) fn bool(b: bool) -> Val {
        Val::Scalar(crate::value::Scalar::Bool(b))
    }

    pub(crate) fn rec(fields: Vec<(&str, Val)>) -> Val {
        Val::Record(
            fields
                .into_iter()
                .map(|(name, value)| (std::sync::Arc::from(name), value))
                .collect(),
        )
    }

    pub(crate) fn some(v: Val) -> Val {
        Val::Some(Box::new(v))
    }
}

#[cfg(test)]
mod tests {
    use super::harness::*;
    use super::{Env, EvalCtx, eval};
    use crate::value::{Scalar, Val};
    use grove_query::typecheck::types::{IdentBinding, QueryType, TypedExpr, TypedExprKind};
    use grove_schema::validated::{ScalarType, ValidatedSchema};
    use grove_types::{Span, Spanned};

    #[test]
    fn eval_let_binding() {
        assert_eq!(run_src("let x = 1; x"), Ok(int(1)));
        assert_eq!(run_src("let x = 1; let x = 2; x"), Ok(int(2)));
        assert_eq!(
            run_src("let x = true; let x = \"hello\"; x"),
            Ok(str("hello"))
        );
        assert_eq!(run_src("let a = 1; let b = a; b"), Ok(int(1)));
    }

    #[test]
    fn eval_if_scalar() {
        assert_eq!(run_src("if true { 1 } else { 2 }"), Ok(int(1)));
        assert_eq!(run_src("if false { 1 } else { 2 }"), Ok(int(2)));
        assert_eq!(
            run_src("if false { 1 } else if false { 2 } else { 3 }"),
            Ok(int(3))
        );
    }

    #[test]
    fn eval_env_depth_collections() {
        let mut env = Env::new();
        env.define(
            "xs".to_string(),
            Val::List(vec![Val::Scalar(Scalar::Int(1)), Val::None]),
        );
        let schema = ValidatedSchema::default();
        let mut ctx = EvalCtx::new(env, FIXED_NOW, &NoStorage, &schema);
        let span = Span::new(0, 0);
        let expr = TypedExpr {
            kind: TypedExprKind::Ident {
                name: Spanned {
                    span,
                    value: "xs".to_string(),
                },
                binding: IdentBinding::Let,
                depth: 0,
            },
            ty: QueryType::List(Box::new(QueryType::Optional(Box::new(QueryType::Scalar(
                ScalarType::Int,
            ))))),
            span,
            id: grove_types::NodeId(0),
        };
        assert_eq!(
            eval(&expr, &mut ctx),
            Ok(Val::List(vec![Val::Scalar(Scalar::Int(1)), Val::None]))
        );
    }
}
