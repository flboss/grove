use grove_query::typecheck::types::{CollectionOriginKind, TypedExpr, TypedMutationStmt};
use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema, ValueType};

use crate::error::ExecError;
use crate::storage::WriteTxn;
use crate::value::{RecordVal, Val, unwrap_optional_depth};

use super::{EvalCtx, eval};

pub(crate) fn exec_mutation(
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
        TypedMutationStmt::Update { .. } | TypedMutationStmt::Delete { .. } => {
            Err(ExecError::Unsupported {
                message: "update/delete mutations are not yet implemented".to_string(),
            })
        }
    }
}

fn exec_insert(
    base: &TypedExpr,
    arg: &TypedExpr,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
    write: &mut dyn WriteTxn,
) -> Result<(), ExecError> {
    let Some(CollectionOriginKind::Root(root_idx)) = base.provenance.collection else {
        return Err(ExecError::Unsupported {
            message: "nested insert targets are not yet implemented".to_string(),
        });
    };
    let root = ctx
        .schema
        .roots
        .get(root_idx)
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown root {root_idx} (internal error)"),
        })?;
    let struct_id = root.struct_id;

    if opt_depth > 0 {
        let target = eval(base, ctx)?;
        let (_, peeled) = unwrap_optional_depth(target, opt_depth)?;
        if peeled < opt_depth {
            return Ok(());
        }
    }

    let arg = eval(arg, ctx)?;
    let records = collect_insert_records(arg)?;
    if records.is_empty() {
        return Ok(());
    }

    let values = records
        .into_iter()
        .map(|record| record_values(ctx.schema, struct_id, record))
        .collect::<Result<Vec<_>, _>>()?;
    write.insert_rows(struct_id, &values)?;
    Ok(())
}

fn collect_insert_records(value: Val) -> Result<Vec<RecordVal>, ExecError> {
    match value {
        Val::Record(record) => Ok(vec![record]),
        Val::List(list) => list
            .into_iter()
            .map(|item| match item {
                Val::Record(record) => Ok(record),
                other => Err(ExecError::Integrity {
                    message: format!(
                        "insert argument list contains non-record value {other} (internal error)"
                    ),
                }),
            })
            .collect(),
        other => Err(ExecError::Integrity {
            message: format!(
                "insert argument is not a record or list of records: {other} (internal error)"
            ),
        }),
    }
}

fn record_values(
    schema: &ValidatedSchema,
    struct_id: StructId,
    mut record: RecordVal,
) -> Result<Vec<(FieldId, Val)>, ExecError> {
    let struct_def = schema
        .structs
        .get(struct_id.index())
        .ok_or_else(|| ExecError::Integrity {
            message: format!("unknown struct id {} (internal error)", struct_id.index()),
        })?;
    let mut values = Vec::new();
    for (local, field) in struct_def.fields.iter().enumerate() {
        let field_id = FieldId::new(struct_id, local);
        if let Some(index) = record
            .iter()
            .position(|(field_name, _)| field_name.as_ref() == field.name())
        {
            let value = record.fields.swap_remove(index).1;
            values.push((field_id, value));
        } else if matches!(
            field,
            Field::Value {
                ty: ValueType::Optional(_),
                ..
            } | Field::Ref {
                optional: true,
                owning: true,
                ..
            }
        ) {
            values.push((field_id, Val::None));
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::*;
    use crate::eval::{ExecOptions, exec_file_with_options};
    use crate::pushdown::FetchConstraints;
    use crate::sqlite::SqliteBackend;
    use crate::storage::Storage;
    use crate::value::Scalar;
    use chrono::Utc;
    use grove_query::typecheck::types::TypedQueryFile;
    use rusqlite::Connection;

    fn test_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(
                r#"
                root items: Item;
                enum Kind { A => 1, B => 2 }
                struct Item {
                    id: Int,
                    name: String,
                    active: Bool,
                    note: ?String,
                    pos: (Float, Float)@(x, y),
                    kind: Kind,
                    maybe_kind: ?Kind,
                }
                "#,
            )
            .0
            .expect("invalid mutation test schema"),
        )
        .0
        .expect("invalid mutation test schema")
    }

    fn test_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items(id INTEGER, name TEXT, active INTEGER, note TEXT,
             x REAL, y REAL, kind INTEGER, maybe_kind INTEGER)",
        )
        .unwrap();
        SqliteBackend::new(conn, schema).unwrap()
    }

    fn typed(source: &str, schema: &ValidatedSchema) -> TypedQueryFile {
        let (file, parse_diags) = grove_query::parse_query(source);
        assert!(parse_diags.is_empty(), "{parse_diags:?}");
        let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), schema);
        assert!(diags.is_empty(), "{diags:?}");
        typed.unwrap()
    }

    fn run(source: &str, schema: &ValidatedSchema, backend: &SqliteBackend<'_>) -> Option<Val> {
        let file = typed(source, schema);
        exec_file_with_options(&file, backend, schema, Utc::now(), ExecOptions::default()).unwrap()
    }

    fn fields(schema: &ValidatedSchema) -> (StructId, Vec<FieldId>) {
        let item = schema.roots[0].struct_id;
        let fields = ["id", "name", "active", "note", "pos", "kind", "maybe_kind"]
            .iter()
            .map(|name| schema.struct_field(item, name).unwrap().0)
            .collect();
        (item, fields)
    }

    #[test]
    fn insert_root_record_scalar_enum_tuple_optional() {
        let schema = test_schema();
        let backend = test_backend(&schema);
        let (item, fields) = fields(&schema);
        let kind = schema.enum_by_name("Kind").unwrap().0;

        run(
            r#"items.insert({
                id = 1,
                name = "alpha",
                active = true,
                note = some("hello"),
                pos = (1.5f, -2.5f),
                kind = Kind::B,
                maybe_kind = some(Kind::A)
            }); 0"#,
            &schema,
            &backend,
        );

        let rows = backend
            .fetch(item, &fields, &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values,
            vec![
                int(1),
                str("alpha"),
                bool(true),
                some(str("hello")),
                Val::Tuple(vec![float(1.5), float(-2.5),]),
                Val::Enum(kind, 1),
                some(Val::Enum(kind, 0)),
            ]
        );
    }

    #[test]
    fn insert_root_bulk_list() {
        let schema = test_schema();
        let backend = test_backend(&schema);
        let (item, fields) = fields(&schema);

        run(
            r#"items.insert([
                { id = 1, name = "alpha", active = true, pos = (1.0f, 2.0f), kind = Kind::A },
                { id = 2, name = "beta", active = false, pos = (3.0f, 4.0f), kind = Kind::B },
            ]);"#,
            &schema,
            &backend,
        );

        let rows = backend
            .fetch(item, &fields, &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].values[0], Val::Scalar(Scalar::Int(1)));
        assert_eq!(rows[1].values[0], Val::Scalar(Scalar::Int(2)));
        assert_eq!(rows[0].values[6], Val::None);
        assert_eq!(
            rows[1].values[1],
            Val::Scalar(Scalar::String("beta".to_string()))
        );
    }

    #[test]
    fn insert_filtered_root_and_same_turn_read() {
        let schema = test_schema();
        let backend = test_backend(&schema);

        let result = run(
            r#"items[id > 0].insert({
                id = 7,
                name = "gamma",
                active = false,
                pos = (0.0f, 0.0f),
                kind = Kind::A
            }); items.len()"#,
            &schema,
            &backend,
        );

        assert_eq!(result, Some(Val::Scalar(Scalar::Int(1))));
    }
}
