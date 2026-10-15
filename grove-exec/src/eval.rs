use std::collections::HashMap;

use chrono::{DateTime, NaiveTime, Utc};
use grove_query::ast::{ConstantName, Literal, TypeName};
use grove_query::typecheck::types::{IdentBinding, TypedExpr, TypedExprKind, TypedLetStmt};
use grove_types::Spanned;
use rust_decimal::Decimal;

use crate::error::ExecError;
use crate::value::{Scalar, Val};

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

/// Evaluate a scalar expression. `now` fixes the clock for `Now`/`Today`.
pub fn eval(expr: &TypedExpr, env: &mut Env, now: DateTime<Utc>) -> Result<Val, ExecError> {
    match &expr.kind {
        TypedExprKind::Literal(lit) => Ok(decode_literal(lit, now)),
        TypedExprKind::Ident {
            name,
            binding,
            depth,
        } => match binding {
            IdentBinding::Let => {
                env.lookup(&name.value, *depth)
                    .cloned()
                    .ok_or_else(|| ExecError::Integrity {
                        message: format!("unbound variable `{}` (internal error)", name.value),
                    })
            }
            IdentBinding::Root { .. }
            | IdentBinding::Field(_)
            | IdentBinding::ProjectedField { .. } => todo!("data references with row contexts"),
            IdentBinding::Prev { .. } => todo!("prev with mutations"),
            IdentBinding::ListElement => todo!("element references inside collection operators"),
        },
        TypedExprKind::Some { value } => eval(value, env, now).map(|v| Val::Some(Box::new(v))),
        TypedExprKind::TypeConstant { ty, name } => Ok(eval_type_constant(ty.value, name.value)),
        TypedExprKind::If { arms, default } => {
            for (cond, value) in arms {
                match eval(cond, env, now)? {
                    Val::Scalar(Scalar::Bool(true)) => return eval(value, env, now),
                    Val::Scalar(Scalar::Bool(false)) => {}
                    _ => {
                        return Err(ExecError::Integrity {
                            message: "if condition must be a Bool (internal error)".to_string(),
                        });
                    }
                }
            }
            eval(default, env, now)
        }
        TypedExprKind::Field { .. } | TypedExprKind::TupleAccess { .. } => todo!("navigation"),
        TypedExprKind::Method { .. } => todo!("methods"),
        TypedExprKind::Binary { .. } | TypedExprKind::Unary { .. } => todo!("operators"),
        TypedExprKind::Cast { .. } => todo!("casts"),
        TypedExprKind::Tuple { .. }
        | TypedExprKind::Array { .. }
        | TypedExprKind::Struct { .. }
        | TypedExprKind::Projection { .. } => todo!("composite literals"),
    }
}

const MIDNIGHT: NaiveTime = NaiveTime::from_hms_opt(0, 0, 0).expect("midnight is a valid time");

fn decode_literal(lit: &Spanned<Literal>, now: DateTime<Utc>) -> Val {
    match &lit.value {
        Literal::Int(n) => Val::Scalar(Scalar::Int(*n)),
        Literal::Float(x) => Val::Scalar(Scalar::Float(*x)),
        Literal::Dec(d) => Val::Scalar(Scalar::Dec(*d)),
        Literal::String(s) => Val::Scalar(Scalar::String(s.clone())),
        Literal::Bool(b) => Val::Scalar(Scalar::Bool(*b)),
        Literal::Instant(t) => Val::Scalar(Scalar::Instant(*t)),
        Literal::Duration(d) => Val::Scalar(Scalar::Duration(*d)),
        Literal::None => Val::None,
        Literal::Now => Val::Scalar(Scalar::Instant(now)),
        Literal::Today(time) => Val::Scalar(Scalar::Instant(
            now.date_naive()
                .and_time(time.unwrap_or(MIDNIGHT))
                .and_utc(),
        )),
    }
}

fn eval_type_constant(ty: TypeName, name: ConstantName) -> Val {
    let scalar = match (ty, name) {
        (TypeName::Int, ConstantName::Min) => Scalar::Int(i64::MIN),
        (TypeName::Int, ConstantName::Max) => Scalar::Int(i64::MAX),
        (TypeName::Float, ConstantName::Min) => Scalar::Float(f64::MIN),
        (TypeName::Float, ConstantName::Max) => Scalar::Float(f64::MAX),
        (TypeName::Dec, ConstantName::Min) => Scalar::Dec(Decimal::MIN),
        (TypeName::Dec, ConstantName::Max) => Scalar::Dec(Decimal::MAX),
    };
    Val::Scalar(scalar)
}

/// Evaluate a `let` and bind its value
pub fn eval_let(stmt: &TypedLetStmt, env: &mut Env, now: DateTime<Utc>) -> Result<(), ExecError> {
    let value = eval(&stmt.value, env, now)?;
    env.define(stmt.name.value.clone(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use grove_query::typecheck::types::{QueryType, TypedStatement};
    use grove_schema::validated::ValidatedSchema;
    use grove_types::Span;
    use rust_decimal::dec;

    const FIXED_NOW: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

    fn run_src(src: &str) -> Result<Val, ExecError> {
        let (file, parse_diags) = grove_query::parse_query(src);
        assert!(parse_diags.is_empty(), "{parse_diags:?}");
        let schema = ValidatedSchema::default();
        let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "{diags:?}");
        let file = typed.unwrap();
        let mut env = Env::new();
        for stmt in &file.statements {
            match stmt {
                TypedStatement::Let(let_stmt) => eval_let(let_stmt, &mut env, FIXED_NOW)?,
                TypedStatement::Expr(expr) => {
                    eval(expr, &mut env, FIXED_NOW)?;
                }
                TypedStatement::Mutation(_) => todo!("mutations"),
            }
        }
        eval(&file.result, &mut env, FIXED_NOW)
    }

    #[test]
    fn eval_literals() {
        assert_eq!(run_src("42"), Ok(Val::Scalar(Scalar::Int(42))));
        assert_eq!(run_src("1.5f"), Ok(Val::Scalar(Scalar::Float(1.5))));
        assert_eq!(run_src("30.00"), Ok(Val::Scalar(Scalar::Dec(dec!(30.00)))));
        assert_eq!(
            run_src("\"hi\""),
            Ok(Val::Scalar(Scalar::String("hi".to_string())))
        );
        assert_eq!(run_src("true"), Ok(Val::Scalar(Scalar::Bool(true))));
        assert_eq!(
            run_src("some(1)"),
            Ok(Val::Some(Box::new(Val::Scalar(Scalar::Int(1))))),
        );
    }

    #[test]
    fn eval_temporal_literals() {
        assert_eq!(run_src("@now"), Ok(Val::Scalar(Scalar::Instant(FIXED_NOW))));
        let midnight = FIXED_NOW.date_naive().and_time(MIDNIGHT).and_utc();
        assert_eq!(
            run_src("@today"),
            Ok(Val::Scalar(Scalar::Instant(midnight)))
        );
        let noon = FIXED_NOW
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        assert_eq!(
            run_src("@today_12:00"),
            Ok(Val::Scalar(Scalar::Instant(noon)))
        );
        assert_eq!(
            run_src("@2026-10-20"),
            Ok(Val::Scalar(Scalar::Instant(
                DateTime::from_timestamp(1792454400, 0).unwrap()
            )))
        );
        assert_eq!(
            run_src("#90s"),
            Ok(Val::Scalar(Scalar::Duration(TimeDelta::seconds(90))))
        );
    }

    #[test]
    fn eval_type_constants() {
        assert_eq!(run_src("Int::MIN"), Ok(Val::Scalar(Scalar::Int(i64::MIN))));
        assert_eq!(
            run_src("Float::MAX"),
            Ok(Val::Scalar(Scalar::Float(f64::MAX)))
        );
        assert_eq!(
            run_src("Dec::MIN"),
            Ok(Val::Scalar(Scalar::Dec(Decimal::MIN)))
        );
        assert_eq!(
            run_src("Dec::MAX"),
            Ok(Val::Scalar(Scalar::Dec(Decimal::MAX)))
        );
    }

    #[test]
    fn eval_let_binding() {
        assert_eq!(run_src("let x = 1; x"), Ok(Val::Scalar(Scalar::Int(1))));
        assert_eq!(
            run_src("let x = 1; let x = 2; x"),
            Ok(Val::Scalar(Scalar::Int(2)))
        );
        assert_eq!(
            run_src("let x = true; let x = \"hello\"; x"),
            Ok(Val::Scalar(Scalar::String("hello".to_string())))
        );
        assert_eq!(
            run_src("let a = 1; let b = a; b"),
            Ok(Val::Scalar(Scalar::Int(1)))
        );
    }

    #[test]
    fn eval_if_scalar() {
        assert_eq!(
            run_src("if true { 1 } else { 2 }"),
            Ok(Val::Scalar(Scalar::Int(1)))
        );
        assert_eq!(
            run_src("if false { 1 } else { 2 }"),
            Ok(Val::Scalar(Scalar::Int(2)))
        );
        assert_eq!(
            run_src("if false { 1 } else if false { 2 } else { 3 }"),
            Ok(Val::Scalar(Scalar::Int(3)))
        );
    }

    #[test]
    fn eval_env_depth_collections() {
        let mut env = Env::new();
        env.define(
            "xs".to_string(),
            Val::List(vec![Val::Scalar(Scalar::Int(1)), Val::None]),
        );
        let span = Span::new(0, 0);
        let expr = TypedExpr {
            kind: TypedExprKind::Ident {
                name: grove_types::Spanned {
                    span,
                    value: "xs".to_string(),
                },
                binding: IdentBinding::Let,
                depth: 0,
            },
            ty: QueryType::List(Box::new(QueryType::Optional(Box::new(QueryType::Scalar(
                grove_schema::validated::ScalarType::Int,
            ))))),
            span,
        };
        assert_eq!(
            eval(&expr, &mut env, FIXED_NOW),
            Ok(Val::List(vec![Val::Scalar(Scalar::Int(1)), Val::None]))
        );
    }
}
