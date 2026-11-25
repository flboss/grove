use crate::ast::{MutationKind, MutationStmt, Statement};
use crate::typecheck::env::TypeEnv;
use crate::typecheck::error::TypeError;
use crate::typecheck::infer::{infer, infer_mutation_arg};
use crate::typecheck::mutation::{InsertTargetSource, check_insert_base, check_mutation_base};
use crate::typecheck::types::{
    IdentBinding, QueryType, RecordSource, TypedLetStmt, TypedMutationStmt, TypedStatement,
};
use crate::typecheck::validate::{
    validate_insert_arg_non_owning, validate_insert_arg_owning, validate_update_arg,
};

pub(crate) fn typecheck_stmt(
    stmt: &Statement,
    env: &mut TypeEnv,
) -> Result<TypedStatement, TypeError> {
    match stmt {
        Statement::Expr(expr) => {
            let typed = infer(expr, env)?;
            Ok(TypedStatement::Expr(typed))
        }
        Statement::Mutation(mutation) => typecheck_mutation(mutation, env),
        Statement::Let(let_stmt) => {
            let value = infer(&let_stmt.value, env)?;
            env.define(
                let_stmt.name.value.clone(),
                value.ty.clone(),
                IdentBinding::Let,
            );
            Ok(TypedStatement::Let(TypedLetStmt {
                name: let_stmt.name.clone(),
                span: let_stmt.value.span(),
                value,
            }))
        }
    }
}

pub(crate) fn typecheck_mutation(
    mutation: &MutationStmt,
    env: &mut TypeEnv,
) -> Result<TypedStatement, TypeError> {
    let kind = mutation.kind.value;

    if kind == MutationKind::Delete && !mutation.args.is_empty() {
        return Err(TypeError::WrongArgCount {
            method: kind.to_string(),
            expected: 0,
            got: mutation.args.len(),
            span: mutation.kind.span,
        });
    }

    if matches!(kind, MutationKind::Insert | MutationKind::Update) && mutation.args.len() != 1 {
        return Err(TypeError::WrongArgCount {
            method: kind.to_string(),
            expected: 1,
            got: mutation.args.len(),
            span: mutation.kind.span,
        });
    }

    match kind {
        MutationKind::Delete => {
            let typed_base = infer(&mutation.base, env)?;
            check_mutation_base(&typed_base, mutation.opt_depth)?;
            Ok(TypedStatement::Mutation(TypedMutationStmt::Delete {
                span: mutation.kind.span,
                base: typed_base,
                opt_depth: mutation.opt_depth,
            }))
        }
        MutationKind::Insert => {
            let typed_base = infer(&mutation.base, env)?;
            let (struct_id, source) =
                check_insert_base(&typed_base, mutation.opt_depth, env.schema())?;
            let mut typed_arg = infer_mutation_arg(&mutation.args[0], env)?;
            match source {
                InsertTargetSource::Root | InsertTargetSource::OwningRef => {
                    validate_insert_arg_owning(struct_id, &mut typed_arg, env.schema())?;
                }
                InsertTargetSource::NonOwningRef { field } => {
                    validate_insert_arg_non_owning(field, struct_id, &mut typed_arg, env.schema())?;
                }
            }
            Ok(TypedStatement::Mutation(TypedMutationStmt::Insert {
                span: mutation.kind.span,
                base: typed_base,
                arg: typed_arg,
                opt_depth: mutation.opt_depth,
            }))
        }
        MutationKind::Update => {
            let typed_base = infer(&mutation.base, env)?;
            let struct_id = check_mutation_base(&typed_base, mutation.opt_depth)?;
            env.push_scope();
            env.define(
                "prev".to_string(),
                QueryType::Record(RecordSource::Schema(struct_id)),
                IdentBinding::Prev { struct_id },
            );
            let mut typed_arg = infer_mutation_arg(&mutation.args[0], env)?;
            env.pop_scope();
            validate_update_arg(struct_id, &mut typed_arg, env.schema())?;
            Ok(TypedStatement::Mutation(TypedMutationStmt::Update {
                span: mutation.kind.span,
                base: typed_base,
                arg: typed_arg,
                opt_depth: mutation.opt_depth,
            }))
        }
    }
}
