use std::collections::HashMap;
use std::rc::Rc;

use chrono::{DateTime, Utc};
use grove_query::typecheck::types::{
    IdentBinding, TypedExpr, TypedExprKind, TypedLetStmt, TypedQueryFile, TypedStatement,
};
use grove_schema::validated::ValidatedSchema;

use crate::error::ExecError;
use crate::pushdown::PushdownPlan;
use crate::storage::Storage;
use crate::value::{Interner, Scalar, Val};

mod arithmetic;
pub mod composite;
pub mod data;
pub(crate) mod literals;
mod methods;
mod operators;

use composite::{eval_array, eval_projection, eval_struct, eval_tuple};
use data::{exec_field, exec_root, resolve_base_link, resolve_owning_values};
use literals::{decode_literal, eval_type_constant};
use methods::eval_method;
use operators::{eval_binary, eval_cast, eval_unary};

/// Lexical value scope chain
#[derive(Debug, Default)]
pub struct Env {
    scopes: Vec<HashMap<Rc<str>, Val>>,
    pub(crate) element_depth: usize,
}

impl Env {
    pub fn new() -> Self {
        Env {
            scopes: vec![HashMap::new()],
            element_depth: 0,
        }
    }

    pub fn define(&mut self, name: Rc<str>, value: Val) {
        self.scopes
            .last_mut()
            .expect("env always has a scope")
            .insert(name, value);
    }

    /// `depth` counts scopes skipped to reach the identifier
    fn lookup(&self, name: &str, depth: usize) -> Option<&Val> {
        self.scopes.iter().rev().nth(depth)?.get(name)
    }

    pub(crate) fn push_scope(&mut self, scope: HashMap<Rc<str>, Val>) {
        self.scopes.push(scope);
    }

    pub(crate) fn pop_scope(&mut self) {
        if !self.scopes.is_empty() {
            self.scopes.pop();
        }
    }
}

pub struct EvalCtx<'a> {
    env: Env,
    now: DateTime<Utc>,
    pub(crate) interner: Interner,
    pub(crate) storage: &'a dyn Storage,
    pub(crate) schema: &'a ValidatedSchema,
    pub(crate) plan: &'a PushdownPlan,
}

impl<'a> EvalCtx<'a> {
    pub fn new(
        env: Env,
        now: DateTime<Utc>,
        storage: &'a dyn Storage,
        schema: &'a ValidatedSchema,
        plan: &'a PushdownPlan,
    ) -> Self {
        EvalCtx {
            env,
            now,
            interner: Interner::default(),
            storage,
            schema,
            plan,
        }
    }
}

/// Evaluate an expression
pub fn eval(expr: &TypedExpr, ctx: &mut EvalCtx<'_>) -> Result<Val, ExecError> {
    match &expr.kind {
        TypedExprKind::Literal(lit) => Ok(decode_literal(lit, ctx.now)),
        TypedExprKind::Ident {
            name,
            binding,
            depth,
        } => {
            match binding {
                IdentBinding::Let => {
                    ctx.env.lookup(&name.value, *depth).cloned().ok_or_else(|| {
                        ExecError::Integrity {
                            message: format!("unbound variable `{}` (internal error)", name.value),
                        }
                    })
                }
                IdentBinding::Root { root_idx } => exec_root(ctx, *root_idx, expr.id),
                IdentBinding::Field(_) | IdentBinding::ProjectedField { .. } => {
                    ctx.env.lookup(&name.value, *depth).cloned().ok_or_else(|| {
                        ExecError::Integrity {
                            message: format!("unbound field `{}` (internal error)", name.value),
                        }
                    })
                }
                IdentBinding::ListElement => {
                    ctx.env.lookup(&name.value, *depth).cloned().ok_or_else(|| {
                        ExecError::Integrity {
                            message: format!("unbound element `{}` (internal error)", name.value),
                        }
                    })
                }
                IdentBinding::Prev { .. } => todo!("prev with mutations"),
            }
        }
        TypedExprKind::Some { value } => eval(value, ctx).map(|v| Val::Some(Box::new(v))),
        TypedExprKind::TypeConstant { ty, name } => Ok(eval_type_constant(ty.value, name.value)),
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
        TypedExprKind::Field {
            base,
            name,
            optional,
        } => exec_field(base, &name.value, *optional, ctx),
        TypedExprKind::TupleAccess {
            base,
            index,
            optional,
        } => eval_tuple_access(base, *index, *optional, ctx),
        TypedExprKind::Method {
            base,
            name,
            args,
            optional,
        } => eval_method(base, &name.value, args, *optional, expr.id, ctx, expr.span),
        TypedExprKind::Cast { expr: inner, ty } => {
            let value = eval(inner, ctx)?;
            eval_cast(value, ty.value, expr.span)
        }
        TypedExprKind::Tuple { elements } => eval_tuple(elements, ctx),
        TypedExprKind::Array { elements } => eval_array(elements, ctx),
        TypedExprKind::Struct { fields } => eval_struct(fields, ctx),
        TypedExprKind::Projection { base, items } => eval_projection(base, items, ctx),
    }
}

/// Evaluate 0-based tuple element access
pub fn eval_tuple_access(
    base: &TypedExpr,
    index: usize,
    optional: bool,
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = resolve_base_link(ctx, base)?;
    let base = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => *v,
        (_, v) => v,
    };
    match base {
        Val::Tuple(elements) => elements.into_iter().nth(index).map_or_else(
            || {
                Err(ExecError::Integrity {
                    message: format!("tuple index {index} out of range (internal error)"),
                })
            },
            |element| {
                Ok(if optional {
                    Val::Some(Box::new(element))
                } else {
                    element
                })
            },
        ),
        other => Err(ExecError::Integrity {
            message: format!("tuple access on non-tuple value: {other} (internal error)"),
        }),
    }
}

/// Evaluate a `let` and bind its value
pub fn eval_let(stmt: &TypedLetStmt, ctx: &mut EvalCtx<'_>) -> Result<(), ExecError> {
    let value = eval(&stmt.value, ctx)?;
    ctx.env.define(ctx.interner.intern(&stmt.name.value), value);
    Ok(())
}

/// Execution options
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOptions {
    /// Push simple operations into the storage backend
    pub pushdown: bool,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions { pushdown: true }
    }
}

/// Run a typed query file against storage
pub fn exec_file(
    file: &TypedQueryFile,
    storage: &dyn Storage,
    schema: &ValidatedSchema,
    now: DateTime<Utc>,
) -> Result<Val, ExecError> {
    exec_file_with_options(file, storage, schema, now, ExecOptions::default())
}

/// Run a typed query file against storage with explicit execution options
pub fn exec_file_with_options(
    file: &TypedQueryFile,
    storage: &dyn Storage,
    schema: &ValidatedSchema,
    now: DateTime<Utc>,
    options: ExecOptions,
) -> Result<Val, ExecError> {
    let plan = if options.pushdown {
        PushdownPlan::analyze(file, schema, now)
    } else {
        PushdownPlan::disabled(schema)
    };
    let mut ctx = EvalCtx::new(Env::new(), now, storage, schema, &plan);
    for stmt in &file.statements {
        match stmt {
            TypedStatement::Let(let_stmt) => eval_let(let_stmt, &mut ctx)?,
            // discarded `;`-terminated expressions
            TypedStatement::Expr(_) => {}
            TypedStatement::Mutation(_) => todo!("mutations"),
        }
    }
    let mut result = eval(&file.result, &mut ctx)?;
    resolve_owning_values(&mut ctx, std::slice::from_mut(&mut result), None)?;
    Ok(result)
}

#[cfg(test)]
pub(crate) mod harness {
    use crate::{sqlite::SqliteBackend, testdata};

    use super::*;
    use rusqlite::Connection;

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
        run_with_storage(src, &schema, &storage)
    }

    pub(crate) fn run_with_storage<S: Storage>(
        src: &str,
        schema: &ValidatedSchema,
        storage: &S,
    ) -> Result<Val, ExecError> {
        let typed = typed_for(src, schema);
        let pushed =
            exec_file_with_options(&typed, storage, schema, FIXED_NOW, ExecOptions::default());
        let unpushed = exec_file_with_options(
            &typed,
            storage,
            schema,
            FIXED_NOW,
            ExecOptions { pushdown: false },
        );
        assert_pushdown_parity(src, &pushed, &unpushed);
        pushed
    }

    fn assert_pushdown_parity(
        src: &str,
        pushed: &Result<Val, ExecError>,
        unpushed: &Result<Val, ExecError>,
    ) {
        match (pushed, unpushed) {
            (Ok(left), Ok(right)) => assert!(
                same_value(left, right),
                "pushdown changed the result for `{src}`: {left:?} vs {right:?}"
            ),
            (Err(left), Err(right)) => {
                assert_eq!(left, right, "pushdown changed the error for `{src}`")
            }
            _ => panic!("pushdown changed success/error for `{src}`: {pushed:?} vs {unpushed:?}"),
        }
    }

    fn same_value(left: &Val, right: &Val) -> bool {
        match (left, right) {
            (Val::Scalar(Scalar::Float(a)), Val::Scalar(Scalar::Float(b))) => {
                (a.is_nan() && b.is_nan()) || a == b
            }
            (Val::Some(a), Val::Some(b)) => same_value(a, b),
            (Val::List(a), Val::List(b)) | (Val::Tuple(a), Val::Tuple(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_value(a, b))
            }
            (Val::Record(a), Val::Record(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b)
                        .all(|((name_a, a), (name_b, b))| name_a == name_b && same_value(a, b))
            }
            _ => left == right,
        }
    }

    pub(crate) fn run_with_storage_opts<S: Storage>(
        src: &str,
        schema: &ValidatedSchema,
        storage: &S,
        options: ExecOptions,
    ) -> Result<Val, ExecError> {
        let typed = typed_for(src, schema);
        exec_file_with_options(&typed, storage, schema, FIXED_NOW, options)
    }

    fn typed_for(src: &str, schema: &ValidatedSchema) -> TypedQueryFile {
        let (file, parse_diags) = grove_query::parse_query(src);
        assert!(
            parse_diags
                .iter()
                .all(|d| d.severity == grove_types::Severity::Warning),
            "{parse_diags:?}"
        );
        let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), schema);
        assert!(diags.is_empty(), "{diags:?}");
        typed.unwrap()
    }

    pub(crate) fn run_seeded(src: &str) -> Result<Val, ExecError> {
        let schema = testdata::schema();
        let mut conn = Connection::open_in_memory().expect("in-memory database should open");
        testdata::seed(&mut conn).expect("testdata initialization should succeed");
        let backend = SqliteBackend::new(conn, &schema).expect("backend should verify");
        run_with_storage(src, &schema, &backend)
    }

    pub(crate) const FIXED_NOW: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

    pub(crate) struct NoStorage;

    impl Storage for NoStorage {
        fn fetch(
            &self,
            _struct_id: grove_schema::validated::StructId,
            _fields: &[grove_schema::validated::FieldId],
            _constraints: &crate::pushdown::FetchConstraints,
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
            _constraints: &crate::pushdown::FetchConstraints,
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

    pub(crate) fn dec(x: rust_decimal::Decimal) -> Val {
        Val::Scalar(crate::value::Scalar::Dec(x))
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
                .map(|(name, value)| (Rc::from(name), value))
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
    use crate::pushdown::PushdownPlan;
    use crate::value::Val;
    use grove_query::typecheck::types::{IdentBinding, QueryType, TypedExpr, TypedExprKind};
    use grove_schema::validated::{ScalarType, ValidatedSchema};
    use grove_types::{Span, Spanned};
    use rust_decimal::dec;
    use std::rc::Rc;

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
    fn eval_tuple_access() {
        assert_eq!(run_src("(\"a\", 1).0"), Ok(str("a")));
        assert_eq!(run_src("(\"a\", 1).1"), Ok(int(1)));
        assert_eq!(run_src("((1, 2), 3).0.1"), Ok(int(2)));
        assert_eq!(
            run_src("(10, 5, 7, 15, 8, 13, 3, 2, 14, 4, 16, 1, 9, 6, 11, (1,2,3,4)).15.3"),
            Ok(int(4))
        );
        assert_eq!(run_src("(1.5, -2.5).0"), Ok(dec(dec!(1.5))),);
        assert_eq!(run_src("[(\"a\", 1)].first()?.0"), Ok(some(str("a"))));
        assert_eq!(run_src("[(\"a\", 1)].skip(5).first()?.1"), Ok(Val::None));
        match run_seeded("users { x = pos.0 }") {
            Ok(Val::List(rows)) => {
                let firsts: Vec<Val> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "x") {
                                Some((_, value)) => value.clone(),
                                got => panic!("expected x, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(
                    firsts,
                    [float(1.5), float(0.0), float(-1.25), float(2.0), float(9.5)]
                );
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        match run_seeded("users { x = home?.1 }") {
            Ok(Val::List(rows)) => {
                let seconds: Vec<Val> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "x") {
                                Some((_, value)) => value.clone(),
                                got => panic!("expected x, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(
                    seconds,
                    [
                        some(float(4.0)),
                        Val::None,
                        Val::None,
                        some(float(1.0)),
                        Val::None
                    ]
                );
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        assert_eq!(
            run_seeded("users.first().unwrap().extra.1.len()"),
            Ok(int(2))
        );
        match run_seeded("users { e = extra.1.len() }") {
            Ok(Val::List(rows)) => {
                let counts: Vec<Val> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "e") {
                                Some((_, value)) => value.clone(),
                                got => panic!("expected e, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(counts, [int(2), int(0), int(0), int(1), int(0)]);
            }
            got => panic!("expected projected users, got {got:?}"),
        }
        match run_seeded("users { f = scores.first()?.0 }") {
            Ok(Val::List(rows)) => {
                let firsts: Vec<Val> = rows
                    .iter()
                    .map(|row| match row {
                        Val::Record(fields) => {
                            match fields.iter().find(|(name, _)| name.as_ref() == "f") {
                                Some((_, value)) => value.clone(),
                                got => panic!("expected f, got {got:?}"),
                            }
                        }
                        other => panic!("expected record, got {other}"),
                    })
                    .collect();
                assert_eq!(
                    firsts,
                    [some(int(1)), Val::None, Val::None, some(int(3)), Val::None]
                );
            }
            got => panic!("expected projected users, got {got:?}"),
        }
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
        env.define(Rc::from("xs"), Val::List(vec![int(1), Val::None]));
        let schema = ValidatedSchema::default();
        let plan = PushdownPlan::disabled(&schema);
        let mut ctx = EvalCtx::new(env, FIXED_NOW, &NoStorage, &schema, &plan);
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
            Ok(Val::List(vec![int(1), Val::None]))
        );
    }

    #[test]
    fn exec_file_discarded_exprs() {
        assert_eq!(run_seeded("let x = 40 + 2; 1 / 0; x"), Ok(int(42)));
        assert_eq!(run_seeded("let a = 1; let b = a + 2; b"), Ok(int(3)));
        assert_eq!(
            run_seeded("users { name, x = 1 / 0 }; roles { name }"),
            Ok(Val::List(vec![
                rec(vec![("name", str("admin"))]),
                rec(vec![("name", str("editor"))]),
            ]))
        );
    }
}
