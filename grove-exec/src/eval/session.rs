use chrono::{DateTime, Utc};
use grove_query::{TypedQueryFile, TypedStatement};
use grove_schema::validated::ValidatedSchema;

use crate::error::ExecError;
use crate::pushdown::PushdownPlan;
use crate::storage::{Storage, WriteTxn};
use crate::value::Val;

use crate::eval::{self, Env, EvalCtx, ExecOptions, data, mutations};

/// Persistent executor state for a sequence of query turns
pub struct ExecSession<'a> {
    storage: &'a dyn Storage,
    schema: &'a ValidatedSchema,
    env: Env,
}

impl<'a> ExecSession<'a> {
    pub fn new(storage: &'a dyn Storage, schema: &'a ValidatedSchema) -> Self {
        ExecSession {
            storage,
            schema,
            env: Env::new(),
        }
    }

    pub fn execute(
        &mut self,
        file: &TypedQueryFile,
        now: DateTime<Utc>,
        options: ExecOptions,
    ) -> Result<Option<Val>, ExecError> {
        self.env.begin_turn();
        let has_mutation = file
            .statements
            .iter()
            .any(|stmt| matches!(stmt, TypedStatement::Mutation(_)));
        let outcome = run_in_write_transaction(
            self.storage,
            options.read_only,
            options.debug_rollback,
            has_mutation,
            |write| {
                execute_inner(
                    &mut self.env,
                    self.storage,
                    self.schema,
                    file,
                    now,
                    options,
                    write,
                )
            },
        );
        match outcome {
            Ok(result) => {
                if options.debug_rollback {
                    self.env.rollback_turn();
                } else {
                    self.env.commit_turn();
                }
                Ok(result)
            }
            Err(err) => {
                self.env.rollback_turn();
                Err(err)
            }
        }
    }

    pub fn bindings(&self) -> impl Iterator<Item = (&str, &Val)> + '_ {
        self.env.bindings()
    }

    pub fn binding(&self, name: &str) -> Option<&Val> {
        self.env.get(name)
    }

    pub fn remove(&mut self, name: &str) -> Option<Val> {
        self.env.remove(name)
    }

    pub fn clear(&mut self) {
        self.env.clear();
    }
}

// The explicit storage lifetime separates the WriteTxn lifetime from the
// short borrow passed to the closure
#[allow(clippy::needless_lifetimes)]
fn run_in_write_transaction<'s, T>(
    storage: &'s dyn Storage,
    read_only: bool,
    rollback: bool,
    needs_write: bool,
    f: impl FnOnce(Option<&mut (dyn WriteTxn + 's)>) -> Result<T, ExecError>,
) -> Result<T, ExecError>
where
    T: 'static,
{
    if needs_write && read_only {
        return Err(ExecError::ReadOnly {
            message: "mutations are disabled in read-only mode".to_string(),
        });
    }
    let mut write = if needs_write {
        Some(storage.begin_write()?)
    } else {
        None
    };
    let outcome = {
        let write_ref = write.as_deref_mut();
        f(write_ref)
    };
    match outcome {
        Ok(result) => {
            if let Some(write) = write.as_mut() {
                if rollback {
                    write.rollback()?;
                } else {
                    write.commit()?;
                }
            }
            Ok(result)
        }
        Err(err) => {
            if let Some(write) = write.as_mut() {
                let _ = write.rollback();
            }
            Err(err)
        }
    }
}

fn execute_inner<'s>(
    env: &mut Env,
    storage: &'s dyn Storage,
    schema: &ValidatedSchema,
    file: &TypedQueryFile,
    now: DateTime<Utc>,
    options: ExecOptions,
    mut write: Option<&mut (dyn WriteTxn + 's)>,
) -> Result<Option<Val>, ExecError> {
    let plan = if options.pushdown {
        PushdownPlan::analyze(file, schema, now, options.materialize_lets)
    } else {
        PushdownPlan::disabled(schema)
    };
    let mut ctx = EvalCtx::new(env, now, storage, schema, &plan);
    for stmt in &file.statements {
        match stmt {
            TypedStatement::Let(let_stmt) => eval::eval_let(let_stmt, &mut ctx)?,
            TypedStatement::Expr(_) => {}
            TypedStatement::Mutation(mutation) => {
                let Some(txn) = write.as_deref_mut() else {
                    return Err(ExecError::Internal {
                        message: "mutation executed without a write transaction".to_string(),
                    });
                };
                mutations::exec_mutation(mutation, &mut ctx, txn)?;
            }
        }
    }

    let result = if let Some(expr) = &file.result {
        let mut result = eval::eval(expr, &mut ctx)?;
        data::resolve_owning_values(&mut ctx, std::slice::from_mut(&mut result), None)?;
        Some(result)
    } else {
        None
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::{FIXED_NOW, NoStorage};
    use crate::sqlite::SqliteBackend;
    use crate::storage::FetchConstraints;
    use crate::testdata;
    use crate::value::{Scalar, Val};
    use grove_query::TypeBindings;
    use grove_schema::validated::{FieldId, StructId};
    use grove_types::Span;
    use rusqlite::Connection;

    fn test_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(
                r#"
                root items: Item;
                struct Item {
                    id: Int,
                    name: String,
                }
                "#,
            )
            .0
            .expect("invalid session test schema"),
        )
        .0
        .expect("invalid session test schema")
    }

    fn test_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE items(id INTEGER, name TEXT)")
            .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

    fn item_fields(schema: &ValidatedSchema) -> (StructId, FieldId, FieldId) {
        let item = schema.roots[0].struct_id;
        (
            item,
            schema.struct_field(item, "id").unwrap().0,
            schema.struct_field(item, "name").unwrap().0,
        )
    }

    fn int(n: i64) -> Val {
        Val::Scalar(Scalar::Int(n))
    }

    fn string(s: &str) -> Val {
        Val::Scalar(Scalar::String(s.to_string()))
    }

    fn typed_delete(schema: &ValidatedSchema) -> TypedQueryFile {
        let (file, parse_diags) = grove_query::parse_query("items.delete();");
        assert!(parse_diags.is_empty(), "{parse_diags:?}");
        let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), schema);
        assert!(diags.is_empty(), "{diags:?}");
        typed.unwrap()
    }

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
        assert!(matches!(err, ExecError::Unsupported { .. }), "{err:?}");
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

    #[test]
    fn write_transaction_commit_on_success() {
        let schema = test_schema();
        let backend = test_backend(&schema);
        let (item, id, name) = item_fields(&schema);

        run_in_write_transaction(&backend, false, false, true, |write| {
            let write = write.expect("write transaction");
            write.insert_row(item, &[(id, int(1)), (name, string("alpha"))])?;
            Ok(())
        })
        .unwrap();

        let rows = backend
            .fetch(item, &[id, name], &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values, vec![int(1), string("alpha")]);
    }

    #[test]
    fn write_transaction_rollback_on_error() {
        let schema = test_schema();
        let backend = test_backend(&schema);
        let (item, id, name) = item_fields(&schema);

        let result: Result<(), ExecError> =
            run_in_write_transaction(&backend, false, false, true, |write| {
                let write = write.expect("write transaction");
                write.insert_row(item, &[(id, int(1)), (name, string("alpha"))])?;
                Err(ExecError::Abort {
                    message: "final expression failed".to_string(),
                    span: Span::new(0, 0),
                })
            });
        assert!(matches!(result, Err(ExecError::Abort { .. })));

        let rows = backend
            .fetch(item, &[id, name], &FetchConstraints::EMPTY)
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn write_transaction_atomic_across_multiple_writes() {
        let schema = test_schema();
        let backend = test_backend(&schema);
        let (item, id, name) = item_fields(&schema);

        let result: Result<(), ExecError> =
            run_in_write_transaction(&backend, false, false, true, |write| {
                let write = write.expect("write transaction");
                write.insert_row(item, &[(id, int(1)), (name, string("alpha"))])?;
                write.insert_row(item, &[(id, int(2)), (name, string("beta"))])?;
                Err(ExecError::Abort {
                    message: "later statement failed".to_string(),
                    span: Span::new(0, 0),
                })
            });
        assert!(matches!(result, Err(ExecError::Abort { .. })));

        let rows = backend
            .fetch(item, &[id, name], &FetchConstraints::EMPTY)
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn write_transaction_read_only_rejected_before_running() {
        let schema = test_schema();
        let backend = test_backend(&schema);
        let result: Result<(), ExecError> =
            run_in_write_transaction(&backend, true, false, true, |_| {
                panic!("closure must not run in read-only mode")
            });
        assert!(matches!(result, Err(ExecError::ReadOnly { .. })));
    }

    #[test]
    fn session_mutation_in_read_only_rejected() {
        let schema = test_schema();
        let backend = test_backend(&schema);
        let file = typed_delete(&schema);
        let mut session = ExecSession::new(&backend, &schema);
        let result = session.execute(
            &file,
            Utc::now(),
            ExecOptions {
                read_only: true,
                ..ExecOptions::default()
            },
        );
        assert!(matches!(result, Err(ExecError::ReadOnly { .. })));
    }
}
