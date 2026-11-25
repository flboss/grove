mod cascade;
mod delete;
mod insert;
mod links;
mod records;
mod update;

use delete::exec_delete;
use insert::exec_insert;
use update::exec_update;

use crate::error::ExecError;
use crate::eval::EvalCtx;
use crate::storage::WriteTxn;

#[cfg(test)]
mod tests;

use grove_query::TypedMutationStmt;

/// Dispatch typed mutation statement to its executor
pub(super) fn exec_mutation(
    stmt: &TypedMutationStmt,
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    match stmt {
        TypedMutationStmt::Insert {
            base,
            arg,
            opt_depth,
            ..
        } => exec_insert(base, arg, *opt_depth, ctx, write),
        TypedMutationStmt::Delete {
            base, opt_depth, ..
        } => exec_delete(base, *opt_depth, ctx, write),
        TypedMutationStmt::Update {
            base,
            arg,
            opt_depth,
            ..
        } => exec_update(base, arg, *opt_depth, ctx, write),
    }
}
