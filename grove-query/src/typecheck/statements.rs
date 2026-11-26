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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::tests::{error_codes, test_schema};
    use crate::typecheck::{TypedExprKind, typecheck};
    use grove_schema::validated::ScalarType;

    #[test]
    fn let_statement_binds_value_type() {
        let (file, _) = crate::parse_query("let x = 42; x");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let TypedStatement::Let(let_stmt) = &typed.statements[0] else {
            panic!("expected let statement");
        };
        assert_eq!(let_stmt.name.value, "x");
        assert_eq!(let_stmt.value.ty, QueryType::Scalar(ScalarType::Int));
        let TypedExprKind::Ident { binding, depth, .. } = &typed.result.as_ref().unwrap().kind
        else {
            panic!("expected ident result");
        };
        assert_eq!(*binding, IdentBinding::Let);
        assert_eq!(*depth, 0);
        assert_eq!(
            typed.result.as_ref().unwrap().ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn statement_only_file_no_result() {
        let (file, _) = crate::parse_query("let x = 1;");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        assert_eq!(typed.statements.len(), 1);
        assert!(typed.result.is_none());
    }

    #[test]
    fn trailing_semicolon_result_valid() {
        let (file, _) = crate::parse_query("42;");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        assert!(typed.statements.is_empty());
        assert_eq!(
            typed.result.as_ref().unwrap().ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn typed_file_output_type_and_bindings_exposed() {
        let (file, _) = crate::parse_query("let x = 1; x");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        assert_eq!(
            typed.output_type(),
            Some(&QueryType::Scalar(ScalarType::Int))
        );
        let lets: Vec<_> = typed
            .let_bindings()
            .map(|(name, ty)| (name.to_string(), ty.clone()))
            .collect();
        assert_eq!(
            lets,
            vec![("x".to_string(), QueryType::Scalar(ScalarType::Int))]
        );
    }

    #[test]
    fn let_chain_scope_visible() {
        let (file, _) = crate::parse_query("let a = 1; let b = a; b");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        assert_eq!(typed.unwrap().statements.len(), 2);
    }

    #[test]
    fn let_collection_type() {
        let (file, _) = crate::parse_query("let names = users { name }; names");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let TypedStatement::Let(let_stmt) = &typed.statements[0] else {
            panic!("expected let statement");
        };
        assert!(matches!(let_stmt.value.ty, QueryType::List(_)));
        assert_eq!(typed.result.as_ref().unwrap().ty, let_stmt.value.ty);
    }

    #[test]
    fn let_use_before_def() {
        let (file, _) = crate::parse_query("let x = y; let y = 1;");
        let (_, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QT0001"));
    }

    #[test]
    fn let_self_reference_error() {
        let (file, _) = crate::parse_query("let x = x;");
        let (_, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QT0001"));
    }

    #[test]
    fn let_shadows_root() {
        let (file, _) = crate::parse_query("let users = 1; users");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let TypedExprKind::Ident { binding, .. } = &typed.result.as_ref().unwrap().kind else {
            panic!("expected ident result");
        };
        assert_eq!(*binding, IdentBinding::Let);
    }

    #[test]
    fn let_rebind_same_scope() {
        let (file, _) = crate::parse_query("let x = 1; let x = 2; x");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        assert_eq!(
            typed.unwrap().result.as_ref().unwrap().ty,
            QueryType::Scalar(ScalarType::Int)
        );
    }

    #[test]
    fn let_query_usage() {
        let (file, _) = crate::parse_query("let limit = 30; users[age > limit]");
        let (typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        assert!(matches!(
            typed.unwrap().result.as_ref().unwrap().ty,
            QueryType::List(_)
        ));
    }

    #[test]
    fn mutation_update_with_prev() {
        let (file, _) = crate::parse_query(r#"users[age == 30].update({ age = prev.age + 1 });"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_delete_arg_error() {
        let (file, _diags) = crate::parse_query(r#"users[!active].delete("abc");"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_extra_args_error() {
        let schema = test_schema();
        for source in [
            r#"users.insert({ name = "A" }, { name = "B" });"#,
            "users.update({ age = 1 }, { age = 2 });",
            "users.delete(1);",
        ] {
            assert_eq!(error_codes(source, &schema), ["QT0004"], "{source}");
        }
    }

    #[test]
    fn mutation_missing_insert_arg_error() {
        assert_eq!(error_codes("users.insert();", &test_schema()), ["QT0004"]);
    }
}
