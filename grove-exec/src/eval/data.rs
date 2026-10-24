use std::sync::Arc;

use grove_query::typecheck::types::TypedExpr;
use grove_schema::validated::{Field, FieldId};

use crate::error::ExecError;
use crate::eval::{EvalCtx, eval};
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

/// Perform a record field access
pub(crate) fn exec_field(
    base: &TypedExpr,
    name: &str,
    optional: bool,
    ctx: &mut EvalCtx,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = match (optional, base) {
        (true, Val::None) => return Ok(Val::None),
        (true, Val::Some(v)) => *v,
        (_, v) => v,
    };
    match base {
        Val::Record(fields) => record_field(fields, name),
        Val::List(items) => {
            let mut mapped = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Val::Record(fields) => mapped.push(record_field(fields, name)?),
                    Val::None => mapped.push(Val::None),
                    other => {
                        return Err(ExecError::Integrity {
                            message: format!(
                                "field `{name}` on non-record list element: {other} (internal error)"
                            ),
                        });
                    }
                }
            }
            Ok(Val::List(mapped))
        }
        _ => Err(ExecError::Integrity {
            message: format!("field `{name}` on non-record value (internal error)"),
        }),
    }
}

fn record_field(
    fields: impl IntoIterator<Item = (Arc<str>, Val)>,
    name: &str,
) -> Result<Val, ExecError> {
    fields
        .into_iter()
        .find(|(field_name, _)| field_name.as_ref() == name)
        .map(|(_, value)| value)
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown field `{name}` (internal error)"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::eval::harness::*;

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
