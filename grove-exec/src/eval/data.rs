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
    use rusqlite::Connection;

    use crate::eval::harness::*;
    use crate::sqlite::SqliteBackend;
    use crate::testdata;

    fn run_seeded(src: &str) -> Result<Val, ExecError> {
        let schema = testdata::schema();
        let mut conn = Connection::open_in_memory().expect("in-memory database should open");
        testdata::seed(&mut conn).expect("testdata initialization should succeed");
        let backend = SqliteBackend::new(conn, &schema).expect("backend should verify");
        run_with_storage(src, schema.clone(), &backend)
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
