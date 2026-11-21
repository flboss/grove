use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use chrono::{DateTime, Utc};
use grove_query::typecheck::types::{
    IdentBinding, TypedExpr, TypedExprKind, TypedLetStmt, TypedQueryFile,
};
use grove_schema::validated::ValidatedSchema;
use grove_types::NodeId;

use crate::error::ExecError;
use crate::pushdown::PushdownPlan;
use crate::storage::Storage;
use crate::value::{Interner, Scalar, Val, rewrap_optional, unwrap_optional_depth};

mod arithmetic;
pub mod composite;
pub mod data;
pub(crate) mod literals;
mod methods;
mod operators;
mod session;

pub use session::ExecSession;

use composite::{eval_array, eval_projection, eval_struct, eval_tuple};
use data::{exec_field, exec_root, resolve_base_link, resolve_ident_value, resolve_owning_values};
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

    pub fn define(&mut self, name: impl Into<Rc<str>>, value: Val) {
        self.scopes
            .last_mut()
            .expect("env always has a scope")
            .insert(name.into(), value);
    }

    pub fn get(&self, name: &str) -> Option<&Val> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    pub fn remove(&mut self, name: &str) -> Option<Val> {
        self.scopes
            .iter_mut()
            .rev()
            .find_map(|scope| scope.remove(name))
    }

    pub fn clear(&mut self) {
        self.scopes.clear();
        self.scopes.push(HashMap::new());
        self.element_depth = 0;
    }

    /// Committed bindings in the base scope
    pub fn bindings(&self) -> impl Iterator<Item = (&str, &Val)> {
        self.scopes
            .first()
            .into_iter()
            .flat_map(|scope| scope.iter().map(|(name, value)| (name.as_ref(), value)))
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

    pub(crate) fn begin_turn(&mut self) {
        debug_assert_eq!(self.scopes.len(), 1, "turn must start at the base scope");
        debug_assert_eq!(self.element_depth, 0, "turn leaked an element scope");
        self.push_scope(HashMap::new());
    }

    pub(crate) fn commit_turn(&mut self) {
        debug_assert_eq!(
            self.scopes.len(),
            2,
            "successful turn left temporary scopes behind"
        );
        if self.scopes.len() > 1 {
            let turn = self.scopes.pop().expect("turn scope exists");
            if let Some(base) = self.scopes.last_mut() {
                base.extend(turn);
            }
        }
        self.element_depth = 0;
    }

    pub(crate) fn rollback_turn(&mut self) {
        self.scopes.truncate(1);
        self.element_depth = 0;
    }
}

pub struct EvalCtx<'a> {
    env: &'a mut Env,
    now: DateTime<Utc>,
    pub(crate) interner: Interner,
    pub(crate) storage: &'a dyn Storage,
    pub(crate) schema: &'a ValidatedSchema,
    pub(crate) plan: &'a PushdownPlan,
    pub(crate) applied_triggers: HashSet<NodeId>,
}

impl<'a> EvalCtx<'a> {
    pub fn new(
        env: &'a mut Env,
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
            applied_triggers: HashSet::new(),
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
                    ctx.env
                        .lookup(name, *depth)
                        .cloned()
                        .ok_or_else(|| ExecError::Integrity {
                            message: format!("unbound variable `{}` (internal error)", name.value),
                        })
                }
                IdentBinding::Root { root_idx } => exec_root(ctx, *root_idx, expr.id),
                IdentBinding::Field(field) => {
                    let value = ctx.env.lookup(name, *depth).cloned().ok_or_else(|| {
                        ExecError::Integrity {
                            message: format!("unbound field `{}` (internal error)", name.value),
                        }
                    })?;
                    resolve_ident_value(ctx, expr.id, *field, value)
                }
                IdentBinding::ProjectedField { .. } => ctx
                    .env
                    .lookup(name, *depth)
                    .cloned()
                    .ok_or_else(|| ExecError::Integrity {
                        message: format!("unbound field `{}` (internal error)", name.value),
                    }),
                IdentBinding::ListElement => {
                    ctx.env
                        .lookup(name, *depth)
                        .cloned()
                        .ok_or_else(|| ExecError::Integrity {
                            message: format!("unbound element `{}` (internal error)", name.value),
                        })
                }
                IdentBinding::Prev { .. } => Err(ExecError::Unsupported {
                    message: "`prev` with mutations is not implemented yet".to_string(),
                }),
            }
        }
        TypedExprKind::EnumVariant { enum_id, variant } => Ok(Val::Enum(*enum_id, *variant)),
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
            opt_depth,
        } => exec_field(base, name, *opt_depth, expr.id, ctx),
        TypedExprKind::TupleAccess {
            base,
            index,
            opt_depth,
        } => eval_tuple_access(base, *index, *opt_depth, ctx),
        TypedExprKind::Method { .. } => eval_method(expr, ctx),
        TypedExprKind::Cast { expr: inner, kind } => {
            let value = eval(inner, ctx)?;
            eval_cast(*kind, value, expr.span, ctx.schema)
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
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = resolve_base_link(ctx, base)?;
    let (base, peeled) = unwrap_optional_depth(base, opt_depth)?;
    if peeled < opt_depth {
        return Ok(rewrap_optional(peeled, Val::None));
    }
    let element = match base {
        Val::Tuple(elements) => {
            elements
                .into_iter()
                .nth(index)
                .ok_or_else(|| ExecError::Integrity {
                    message: format!("tuple index {index} out of range (internal error)"),
                })?
        }
        other => {
            return Err(ExecError::Integrity {
                message: format!("tuple access on non-tuple value: {other} (internal error)"),
            });
        }
    };
    Ok(rewrap_optional(opt_depth, element))
}

/// Evaluate a `let` and bind its value
pub fn eval_let(stmt: &TypedLetStmt, ctx: &mut EvalCtx<'_>) -> Result<(), ExecError> {
    let mut value = eval(&stmt.value, ctx)?;
    resolve_owning_values(ctx, std::slice::from_mut(&mut value), None)?;
    ctx.env.define(ctx.interner.intern(&stmt.name.value), value);
    Ok(())
}

/// Execution options
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOptions {
    /// Push simple operations into the storage backend
    pub pushdown: bool,
    /// Materialize complete values for let bindings (for turn-based repl mode)
    pub materialize_lets: bool,
    /// Reject mutation statements before evaluation
    pub read_only: bool,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            pushdown: true,
            materialize_lets: false,
            read_only: false,
        }
    }
}

/// Run a typed query file against storage
pub fn exec_file(
    file: &TypedQueryFile,
    storage: &dyn Storage,
    schema: &ValidatedSchema,
    now: DateTime<Utc>,
) -> Result<Option<Val>, ExecError> {
    exec_file_with_options(file, storage, schema, now, ExecOptions::default())
}

/// Run a typed query file against storage with explicit execution options
pub fn exec_file_with_options(
    file: &TypedQueryFile,
    storage: &dyn Storage,
    schema: &ValidatedSchema,
    now: DateTime<Utc>,
    options: ExecOptions,
) -> Result<Option<Val>, ExecError> {
    ExecSession::new(storage, schema).execute(file, now, options)
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
            ExecOptions {
                pushdown: false,
                materialize_lets: false,
                read_only: false,
            },
        );
        assert_pushdown_parity(src, &pushed, &unpushed);
        pushed.map(|value| value.expect("query produced no output"))
    }

    fn assert_pushdown_parity(
        src: &str,
        pushed: &Result<Option<Val>, ExecError>,
        unpushed: &Result<Option<Val>, ExecError>,
    ) {
        match (pushed, unpushed) {
            (Ok(Some(left)), Ok(Some(right))) => assert!(
                same_value(left, right),
                "pushdown changed the result for `{src}`: {left:?} vs {right:?}"
            ),
            (Ok(None), Ok(None)) => {}
            (Err(left), Err(right)) => {
                assert_eq!(left, right, "pushdown changed the error for `{src}`")
            }
            _ => panic!("pushdown changed success/error for `{src}`: {pushed:?} vs {unpushed:?}"),
        }
    }

    fn same_value(left: &Val, right: &Val) -> bool {
        if left.row_origin() != right.row_origin()
            || left.collection_origin() != right.collection_origin()
        {
            return false;
        }
        match (left, right) {
            (Val::Scalar(Scalar::Float(a)), Val::Scalar(Scalar::Float(b))) => {
                (a.is_nan() && b.is_nan()) || a == b
            }
            (Val::Some(a), Val::Some(b)) => same_value(a, b),
            (Val::List(a), Val::List(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_value(a, b))
            }
            (Val::Tuple(a), Val::Tuple(b)) => {
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
            .map(|value| value.expect("query produced no output"))
    }

    pub(crate) fn run_optional_with_storage<S: Storage>(
        src: &str,
        schema: &ValidatedSchema,
        storage: &S,
    ) -> Result<Option<Val>, ExecError> {
        let typed = typed_for(src, schema);
        exec_file_with_options(&typed, storage, schema, FIXED_NOW, ExecOptions::default())
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

    pub(crate) fn bytes(v: &[u8]) -> Val {
        Val::Scalar(crate::value::Scalar::Bytes(v.to_vec()))
    }

    pub(crate) fn bool(b: bool) -> Val {
        Val::Scalar(crate::value::Scalar::Bool(b))
    }

    pub(crate) fn rec(fields: Vec<(&str, Val)>) -> Val {
        Val::record(
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
    use super::{Env, EvalCtx, ExecOptions, ExecSession, eval};
    use crate::error::ExecError;
    use crate::pushdown::PushdownPlan;
    use crate::testdata;
    use crate::value::Val;
    use grove_query::TypeBindings;
    use grove_query::typecheck::types::{
        IdentBinding, QueryType, TargetProvenance, TypedExpr, TypedExprKind, TypedQueryFile,
    };
    use grove_schema::validated::{ScalarType, ValidatedSchema};
    use grove_types::{Span, Spanned};
    use rust_decimal::dec;
    use std::rc::Rc;

    fn typed_session(src: &str, schema: &ValidatedSchema, types: &TypeBindings) -> TypedQueryFile {
        let (file, parse_diags) = grove_query::parse_query(src);
        assert!(
            parse_diags
                .iter()
                .all(|d| d.severity == grove_types::Severity::Warning),
            "{parse_diags:?}"
        );
        let (typed, diags) =
            grove_query::typecheck::typecheck_with_bindings(file.unwrap(), schema, types);
        assert!(diags.is_empty(), "{diags:?}");
        typed.unwrap()
    }

    fn record_field<'a>(value: &'a Val, name: &str) -> &'a Val {
        let Val::Record(fields) = value else {
            panic!("expected record, got {value:?}");
        };
        fields
            .iter()
            .find(|(field_name, _)| field_name.as_ref() == name)
            .map(|(_, value)| value)
            .expect("expected record field")
    }

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
    fn eval_deep_optional_tuple_access() {
        assert_eq!(run_src("some(some((1, 2)))??.1"), Ok(some(some(int(2)))));
        assert_eq!(
            run_src("some(if true {none} else {some((1, 2))})??.0"),
            Ok(some(Val::None))
        );
    }

    #[test]
    fn deep_optional_chain_pushdown_parity() {
        assert_eq!(
            run_seeded("users.first()?.nickname??.to_upper()"),
            Ok(some(some(str("A"))))
        );
        assert_eq!(
            run_seeded("users.first()?.manager??.name"),
            Ok(some(some(str("Bob"))))
        );
    }

    #[test]
    fn eval_deep_optional_methods() {
        assert_eq!(
            run_src("some(some([1, 2]))??.map($ + 1)"),
            Ok(some(some(Val::list(vec![int(2), int(3)]))))
        );
        assert_eq!(
            run_src("some(some([1, 2]))??.filter($ > 1)"),
            Ok(some(some(Val::list(vec![int(2)]))))
        );
        assert_eq!(
            run_src("some(some([3, 1]))??.sort()"),
            Ok(some(some(Val::list(vec![int(1), int(3)]))))
        );
        assert_eq!(
            run_src("some(some(\"hi\"))??.len()"),
            Ok(some(some(int(2))))
        );
        assert_eq!(
            run_src("some(some([1, 2]))??.nth(0)"),
            Ok(some(some(some(int(1)))))
        );
        assert_eq!(
            run_src("some(some(some(1)))??.unwrap()"),
            Ok(some(some(int(1))))
        );
        assert_eq!(
            run_src("some(if true {none} else {some([1])})??.len()"),
            Ok(some(Val::None))
        );
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
        env.define(Rc::from("xs"), Val::list(vec![int(1), Val::None]));
        let schema = ValidatedSchema::default();
        let plan = PushdownPlan::disabled(&schema);
        let mut ctx = EvalCtx::new(&mut env, FIXED_NOW, &NoStorage, &schema, &plan);
        let span = Span::new(0, 0);
        let expr = TypedExpr {
            kind: TypedExprKind::Ident {
                name: Spanned::new(span, "xs".to_string()),
                binding: IdentBinding::Let,
                depth: 0,
            },
            ty: QueryType::List(Box::new(QueryType::Optional(Box::new(QueryType::Scalar(
                ScalarType::Int,
            ))))),
            provenance: TargetProvenance::NONE,
            span,
            id: grove_types::NodeId(0),
        };
        assert_eq!(
            eval(&expr, &mut ctx),
            Ok(Val::list(vec![int(1), Val::None]))
        );
    }

    #[test]
    fn enum_construction_and_casts() {
        let schema = testdata::schema();
        let priority = testdata::enum_id(&schema, "Priority");

        assert_eq!(
            run_with("Priority::High", schema.clone()),
            Ok(Val::Enum(priority, 2))
        );
        assert_eq!(
            run_with("Priority::High as String", schema.clone()),
            Ok(str("high"))
        );
        assert_eq!(run_with("Level::High as Int", schema), Ok(int(2)));
    }

    #[test]
    fn enum_fields_decode_cast_and_filter() {
        let schema = testdata::schema();
        let priority = testdata::enum_id(&schema, "Priority");
        let level = testdata::enum_id(&schema, "Level");

        assert_eq!(
            run_seeded("settings.first().unwrap().priority as String"),
            Ok(str("high"))
        );
        assert_eq!(
            run_seeded("settings.first().unwrap().level as Int"),
            Ok(int(2))
        );
        assert_eq!(
            run_seeded("settings.first().unwrap().fallback"),
            Ok(some(Val::Enum(priority, 0)))
        );
        assert_eq!(
            run_seeded("settings.nth(1).unwrap().fallback"),
            Ok(Val::None)
        );
        assert_eq!(
            run_seeded("settings.first().unwrap().history"),
            Ok(Val::list(vec![
                Val::Enum(priority, 0),
                Val::Enum(priority, 2)
            ]))
        );
        assert_eq!(
            run_seeded("settings.first().unwrap().pair"),
            Ok(Val::Tuple(vec![
                Val::Enum(priority, 2),
                Val::Enum(level, 2)
            ]))
        );
        assert_eq!(
            run_seeded("settings[priority == Priority::High] { name }"),
            Ok(Val::list(vec![rec(vec![("name", str("alpha"))])]))
        );
        assert_eq!(
            run_seeded("settings[level in (Level::High, Level::Low)] { name }"),
            Ok(Val::list(vec![
                rec(vec![("name", str("alpha"))]),
                rec(vec![("name", str("beta"))]),
            ]))
        );
    }

    #[test]
    fn exec_file_discarded_exprs() {
        assert_eq!(run_seeded("let x = 40 + 2; 1 / 0; x"), Ok(int(42)));
        assert_eq!(run_seeded("let a = 1; let b = a + 2; b"), Ok(int(3)));
        assert_eq!(
            run_seeded("users { name, x = 1 / 0 }; roles { name }"),
            Ok(Val::list(vec![
                rec(vec![("name", str("admin"))]),
                rec(vec![("name", str("editor"))]),
            ]))
        );
    }

    #[test]
    fn exec_statement_only_files() {
        let schema = ValidatedSchema::default();
        assert_eq!(
            run_optional_with_storage("let x = 1;", &schema, &NoStorage),
            Ok(None)
        );
        assert_eq!(run_optional_with_storage("", &schema, &NoStorage), Ok(None));
        assert_eq!(
            run_optional_with_storage("let x = 1; x", &schema, &NoStorage),
            Ok(Some(int(1)))
        );
        assert_eq!(run_src("42;"), Ok(int(42)));
    }

    #[test]
    fn session_bindings_persistence_alignment() {
        let schema = ValidatedSchema::default();
        let mut session = ExecSession::new(&NoStorage, &schema);
        let mut types = TypeBindings::new();

        let first = typed_session("let x = 41;", &schema, &types);
        assert_eq!(
            session.execute(&first, FIXED_NOW, ExecOptions::default()),
            Ok(None)
        );
        for (name, ty) in first.let_bindings() {
            types.define(name, ty.clone());
        }
        assert_eq!(session.binding("x"), Some(&int(41)));

        let second = typed_session("x + 1", &schema, &types);
        assert_eq!(
            session.execute(&second, FIXED_NOW, ExecOptions::default()),
            Ok(Some(int(42)))
        );
        assert_eq!(session.bindings().count(), 1);
    }

    #[test]
    fn session_rollback_failed_turn() {
        let schema = ValidatedSchema::default();
        let mut session = ExecSession::new(&NoStorage, &schema);
        let mut types = TypeBindings::new();

        let first = typed_session("let x = 1;", &schema, &types);
        assert_eq!(
            session.execute(&first, FIXED_NOW, ExecOptions::default()),
            Ok(None)
        );
        for (name, ty) in first.let_bindings() {
            types.define(name, ty.clone());
        }

        let failed = typed_session("let y = 2; 1 / 0", &schema, &types);
        let err = session
            .execute(&failed, FIXED_NOW, ExecOptions::default())
            .unwrap_err();
        assert!(matches!(err, ExecError::Abort { .. }));
        assert!(session.binding("y").is_none());
        assert_eq!(session.binding("x"), Some(&int(1)));

        let third = typed_session("x", &schema, &types);
        assert_eq!(
            session.execute(&third, FIXED_NOW, ExecOptions::default()),
            Ok(Some(int(1)))
        );
    }

    #[test]
    fn session_binding_depth_matches_scoped_methods() {
        let schema = ValidatedSchema::default();
        let mut session = ExecSession::new(&NoStorage, &schema);
        let mut types = TypeBindings::new();

        let first = typed_session("let min = 1;", &schema, &types);
        session
            .execute(&first, FIXED_NOW, ExecOptions::default())
            .unwrap();
        for (name, ty) in first.let_bindings() {
            types.define(name, ty.clone());
        }

        let second = typed_session("[1, 2, 3].filter($ > min)", &schema, &types);
        assert_eq!(
            session.execute(&second, FIXED_NOW, ExecOptions::default()),
            Ok(Some(Val::list(vec![int(2), int(3)])))
        );
    }

    #[test]
    fn mutations_writable_storage_required() {
        let schema = testdata::schema();
        let mut session = ExecSession::new(&NoStorage, &schema);
        let typed = typed_session("users.delete();", &schema, &TypeBindings::new());

        let err = session
            .execute(&typed, FIXED_NOW, ExecOptions::default())
            .unwrap_err();
        assert!(matches!(err, ExecError::ReadOnly { .. }), "{err:?}");
        assert_eq!(session.bindings().count(), 0);
    }

    #[test]
    fn let_owning_refs_resolved() {
        let schema = testdata::schema();
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory database");
        testdata::seed(&mut conn).expect("testdata initialization");
        let backend = crate::sqlite::SqliteBackend::new(conn, &schema).expect("backend");
        let mut session = ExecSession::new(&backend, &schema);

        let typed = typed_session(
            "let u = users.first().unwrap();",
            &schema,
            &TypeBindings::new(),
        );
        assert_eq!(
            session.execute(&typed, FIXED_NOW, ExecOptions::default()),
            Ok(None)
        );
        assert!(matches!(
            record_field(session.binding("u").expect("u binding"), "profile"),
            Val::Some(inner) if matches!(inner.as_ref(), Val::Record(_))
        ));
    }

    #[test]
    fn session_remove_and_clear() {
        let schema = ValidatedSchema::default();
        let mut session = ExecSession::new(&NoStorage, &schema);
        let mut types = TypeBindings::new();

        for (src, name) in [("let x = 1;", "x"), ("let y = 2;", "y")] {
            let typed = typed_session(src, &schema, &types);
            session
                .execute(&typed, FIXED_NOW, ExecOptions::default())
                .unwrap();
            for (name, ty) in typed.let_bindings() {
                types.define(name, ty.clone());
            }
            assert!(session.binding(name).is_some());
        }

        assert_eq!(session.remove("x"), Some(int(1)));
        assert!(session.binding("x").is_none());
        assert!(session.binding("y").is_some());

        session.clear();
        assert_eq!(session.bindings().count(), 0);

        let typed = typed_session("1", &schema, &TypeBindings::new());
        assert_eq!(
            session.execute(&typed, FIXED_NOW, ExecOptions::default()),
            Ok(Some(int(1)))
        );
    }
}
