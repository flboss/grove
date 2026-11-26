mod arithmetic;
mod composite;
mod context;
mod data;
mod env;
mod expr;
mod methods;
mod mutations;
mod operators;
mod session;

#[cfg(test)]
pub(crate) mod harness;

pub use session::ExecSession;

pub(crate) use context::EvalCtx;
pub(crate) use env::Env;
pub(crate) use expr::{eval, eval_let};

/// Execution options
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecOptions {
    /// Push simple operations into the storage backend
    pub pushdown: bool,
    /// Materialize complete values for let bindings (for turn-based repl mode)
    pub materialize_lets: bool,
    /// Reject mutation statements before evaluation
    pub read_only: bool,
    /// Run mutations against a transaction that is always rolled back
    pub debug_rollback: bool,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            pushdown: true,
            materialize_lets: false,
            read_only: false,
            debug_rollback: false,
        }
    }
}

use chrono::{DateTime, Utc};
use grove_query::TypedQueryFile;
use grove_schema::ValidatedSchema;

use crate::error::ExecError;
use crate::storage::Storage;
use crate::value::Val;

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
mod tests {
    use super::harness::{NoStorage, int, rec, run_optional_with_storage, run_seeded, some, str};
    use crate::value::Val;
    use grove_schema::validated::ValidatedSchema;

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
    fn exec_standalone_statements() {
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
}
