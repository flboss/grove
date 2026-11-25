pub mod bindings;
pub(crate) mod error;
pub mod types;

mod compat;
mod env;
mod infer;
mod mutation;
mod statements;
mod validate;

pub use bindings::TypeBindings;
pub use types::{
    CollectionOriginKind, IdentBinding, ProjectionField, QueryType, RecordSource, TargetProvenance,
    TypedExpr, TypedExprKind, TypedLetStmt, TypedMethodArg, TypedMutationStmt, TypedProjectionItem,
    TypedQueryFile, TypedStatement,
};

pub(crate) use env::TypeEnv;
pub(crate) use infer::infer;

use crate::ast::QueryFile;
use crate::typecheck::compat::check;
use crate::typecheck::statements::typecheck_stmt;
use grove_schema::validated::ValidatedSchema;
use grove_types::{Diagnostic, Severity};

pub fn typecheck(
    file: QueryFile,
    schema: &ValidatedSchema,
) -> (Option<TypedQueryFile>, Vec<Diagnostic>) {
    typecheck_with_bindings(file, schema, &TypeBindings::new())
}

pub fn typecheck_with_bindings(
    file: QueryFile,
    schema: &ValidatedSchema,
    bindings: &TypeBindings,
) -> (Option<TypedQueryFile>, Vec<Diagnostic>) {
    let (statements_src, result_src) = file.into_statements_and_output();
    let mut env = TypeEnv::new(schema);
    for (name, ty) in bindings.iter() {
        env.define(name.to_owned(), ty.clone(), IdentBinding::Let);
    }
    // repl turn scope always pushed, executor matching
    env.push_scope();

    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut statements = Vec::new();

    for stmt in &statements_src {
        match typecheck_stmt(stmt, &mut env) {
            Ok(typed) => statements.push(typed),
            Err(err) => diags.push(err.into()),
        }
    }

    let mut result = None;
    if let Some(expr) = result_src {
        match infer(&expr, &mut env) {
            Ok(mut typed) => {
                let mut ty = typed.ty.clone();
                if let Err(err) = check(&mut typed, &mut ty, &mut env) {
                    diags.push(err.into());
                }
                result = Some(typed);
            }
            Err(err) => {
                diags.push(err.into());
            }
        }
    }

    if diags.iter().any(|d| d.severity == Severity::Error) {
        (None, diags)
    } else {
        (Some(TypedQueryFile { statements, result }), diags)
    }
}

#[cfg(test)]
mod tests;
