use grove_schema::validated::{Field, FieldId};

use crate::error::ExecError;
use crate::eval::EvalCtx;
use crate::value::Val;

/// Read root table into a List<Record> (excluding Refs)
pub(crate) fn exec_root(ctx: &mut EvalCtx, root_idx: usize) -> Result<Val, ExecError> {
    let root = ctx
        .schema
        .roots
        .get(root_idx)
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown root {root_idx} (internal error)"),
        })?;
    let mut fields = Vec::new();
    let mut names = Vec::new();
    for (local, field) in ctx.schema.structs[root.struct_id.index()]
        .fields
        .iter()
        .enumerate()
    {
        if matches!(field, Field::Ref { .. }) {
            continue;
        }
        fields.push(FieldId::new(root.struct_id, local));
        names.push(field.name());
    }
    let rows = ctx.storage.fetch(root.struct_id, &fields)?;
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        if row.values.len() != names.len() {
            return Err(ExecError::Integrity {
                message: format!(
                    "root row has {} values for {} fields (internal error)",
                    row.values.len(),
                    names.len()
                ),
            });
        }
        let mut record = Vec::with_capacity(names.len());
        for (name, value) in names.iter().zip(row.values) {
            record.push((ctx.interner.intern(name), value));
        }
        items.push(Val::Record(record));
    }
    Ok(Val::List(items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use chrono::{DateTime, Utc};
    use rusqlite::Connection;

    use crate::eval::{Env, EvalCtx, eval, eval_let};
    use crate::sqlite::SqliteBackend;
    use crate::storage::Storage;
    use crate::testdata;
    use crate::value::Scalar;
    use grove_query::typecheck::types::TypedStatement;
    use grove_schema::validated::ValidatedSchema;

    const FIXED_NOW: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

    fn run_with_backend<S: Storage>(
        src: &str,
        schema: &ValidatedSchema,
        storage: &S,
    ) -> Result<Val, ExecError> {
        let (file, parse_diags) = grove_query::parse_query(src);
        assert!(parse_diags.is_empty(), "{parse_diags:?}");
        let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), schema);
        assert!(diags.is_empty(), "{diags:?}");
        let file = typed.unwrap();
        let mut ctx = EvalCtx::new(Env::new(), FIXED_NOW, storage, schema);
        for stmt in &file.statements {
            match stmt {
                TypedStatement::Let(let_stmt) => eval_let(let_stmt, &mut ctx)?,
                TypedStatement::Expr(expr) => {
                    eval(expr, &mut ctx)?;
                }
                TypedStatement::Mutation(_) => todo!("mutations"),
            }
        }
        eval(&file.result, &mut ctx)
    }

    fn run_seeded(src: &str) -> Result<Val, ExecError> {
        let schema = testdata::schema();
        let mut conn = Connection::open_in_memory().expect("in-memory database should open");
        testdata::seed(&mut conn).expect("testdata initialization should succeed");
        let backend = SqliteBackend::new(conn, &schema).expect("backend should verify");
        run_with_backend(src, &schema, &backend)
    }

    fn str(s: &str) -> Val {
        Val::Scalar(Scalar::String(s.to_string()))
    }

    fn some(v: Val) -> Val {
        Val::Some(Box::new(v))
    }

    fn rec(fields: Vec<(&str, Val)>) -> Val {
        Val::Record(
            fields
                .into_iter()
                .map(|(name, value)| (Arc::from(name), value))
                .collect(),
        )
    }

    #[test]
    fn root_fetch_skips_ref_fields() {
        match run_seeded("users") {
            Ok(Val::List(rows)) => {
                assert_eq!(rows.len(), 5);
                for row in &rows {
                    let Val::Record(fields) = row else {
                        panic!("expected record, got {row}");
                    };
                    assert!(fields.iter().all(|(name, _)| {
                        !["profile", "orders", "roles", "manager"].contains(&name.as_ref())
                    }));
                }
                let Val::Record(first) = &rows[0] else {
                    panic!("expected record");
                };
                assert!(first.iter().any(|(name, value)| {
                    name.as_ref() == "nickname" && *value == some(str("A"))
                }));
                let Val::Record(second) = &rows[1] else {
                    panic!("expected record");
                };
                assert!(
                    second.iter().any(|(name, value)| {
                        name.as_ref() == "nickname" && *value == Val::None
                    })
                );
            }
            got => panic!("expected user rows, got {got:?}"),
        }
    }

    #[test]
    fn root_fetch_let_binding() {
        assert_eq!(
            run_seeded("let r = roles; r"),
            Ok(Val::List(vec![
                rec(vec![("name", str("admin"))]),
                rec(vec![("name", str("editor"))]),
            ]))
        );
    }
}
