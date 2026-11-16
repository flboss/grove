use chrono::{DateTime, Utc};
use grove_query::typecheck::types::{TypedQueryFile, TypedStatement};
use grove_schema::validated::ValidatedSchema;

use crate::error::ExecError;
use crate::pushdown::PushdownPlan;
use crate::storage::Storage;
use crate::value::Val;

use super::{Env, EvalCtx, ExecOptions, eval, eval_let, resolve_owning_values};

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
        let outcome = self.execute_inner(file, now, options);
        match outcome {
            Ok(result) => {
                self.env.commit_turn();
                Ok(result)
            }
            Err(err) => {
                self.env.rollback_turn();
                Err(err)
            }
        }
    }

    fn execute_inner(
        &mut self,
        file: &TypedQueryFile,
        now: DateTime<Utc>,
        options: ExecOptions,
    ) -> Result<Option<Val>, ExecError> {
        let plan = if options.pushdown {
            PushdownPlan::analyze(file, self.schema, now, options.materialize_lets)
        } else {
            PushdownPlan::disabled(self.schema)
        };
        let mut ctx = EvalCtx::new(&mut self.env, now, self.storage, self.schema, &plan);
        for stmt in &file.statements {
            match stmt {
                TypedStatement::Let(let_stmt) => eval_let(let_stmt, &mut ctx)?,
                TypedStatement::Expr(_) => {}
                TypedStatement::Mutation(_) => {
                    return Err(ExecError::Unsupported {
                        message: "mutations are not yet implemented".to_string(),
                    });
                }
            }
        }

        let result = if let Some(expr) = &file.result {
            let mut result = eval(expr, &mut ctx)?;
            resolve_owning_values(&mut ctx, std::slice::from_mut(&mut result), None)?;
            Some(result)
        } else {
            None
        };
        Ok(result)
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
