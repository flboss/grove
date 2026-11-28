use std::collections::HashMap;
use std::sync::Arc;

use grove_schema::validated::{
    Field, FieldId, Relation, RelationId, ScalarType, StructId, ValueType,
};
use rusqlite::types::Value;
use rusqlite::{Transaction, TransactionBehavior};

use super::SqliteBackend;
use super::cache::{self, SqlCacheKey};
use super::decode::{
    ArraySlot, NodeDraft, Slot, ValueArraySlot, ValueSlot, split_roots, walk_element,
};
use super::types::{quote, saturating_i64, scalar_value};
use super::vtab_keys::TABLE as KEYS_TABLE;
use super::write::SqliteWriteTxn;
use crate::error::ExecError;
use crate::storage::{CmpOp, FetchConstraints, PushExpr, Row, Storage, StringOp, WriteTxn};
use crate::value::Key;

impl SqliteBackend<'_> {
    fn pushdown_field(
        &self,
        struct_id: StructId,
        field: FieldId,
    ) -> Result<(String, ScalarType), ExecError> {
        if field.struct_id() != struct_id {
            return Err(ExecError::Internal {
                message: format!(
                    "pushdown field {:?} does not belong to struct {}",
                    field,
                    struct_id.index()
                ),
            });
        }
        let struct_def =
            self.schema
                .structs
                .get(struct_id.index())
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown struct id {}", struct_id.index()),
                })?;
        match struct_def.fields.get(field.local()) {
            Some(Field::Value {
                ty: ValueType::Scalar(scalar),
                columns,
                ..
            }) if columns.len() == 1 => Ok((quote(self.schema.column_name(columns[0])), *scalar)),
            Some(Field::Value {
                ty: ValueType::Enum(id),
                columns,
                ..
            }) if columns.len() == 1 => {
                let enum_def =
                    self.schema
                        .enums
                        .get(id.index())
                        .ok_or_else(|| ExecError::Internal {
                            message: format!("unknown enum id {}", id.index()),
                        })?;
                Ok((
                    quote(self.schema.column_name(columns[0])),
                    enum_def.underlying.scalar_type(),
                ))
            }
            Some(other) => Err(ExecError::Internal {
                message: format!(
                    "pushdown field `{}.{}` is not a single-column scalar",
                    struct_def.name,
                    other.name()
                ),
            }),
            None => Err(ExecError::Internal {
                message: format!("unknown field {field:?}"),
            }),
        }
    }

    /// Render a pushdown expression
    fn push_expr_sql(
        &self,
        struct_id: StructId,
        expr: &PushExpr,
        qualifier: &str,
        params: &mut Vec<Value>,
    ) -> Result<String, ExecError> {
        match expr {
            PushExpr::Field(field) => {
                let (column, scalar) = self.pushdown_field(struct_id, *field)?;
                if scalar == ScalarType::Bool {
                    // TODO: reject non-0/1?
                    Ok(format!("({qualifier}.{column} = 1)"))
                } else {
                    Ok(format!("{qualifier}.{column}"))
                }
            }
            PushExpr::Literal(value) => {
                params.push(scalar_value(value));
                Ok("?".to_string())
            }
            PushExpr::Cmp { op, lhs, rhs } => {
                let lhs = self.push_expr_sql(struct_id, lhs, qualifier, params)?;
                let rhs = self.push_expr_sql(struct_id, rhs, qualifier, params)?;
                let op = match op {
                    CmpOp::Eq => "=",
                    CmpOp::Ne => "<>",
                    CmpOp::Lt => "<",
                    CmpOp::Le => "<=",
                    CmpOp::Gt => ">",
                    CmpOp::Ge => ">=",
                };
                Ok(format!("({lhs} {op} {rhs})"))
            }
            PushExpr::And(lhs, rhs) => {
                let lhs = self.push_expr_sql(struct_id, lhs, qualifier, params)?;
                let rhs = self.push_expr_sql(struct_id, rhs, qualifier, params)?;
                Ok(format!("({lhs} AND {rhs})"))
            }
            PushExpr::Or(lhs, rhs) => {
                let lhs = self.push_expr_sql(struct_id, lhs, qualifier, params)?;
                let rhs = self.push_expr_sql(struct_id, rhs, qualifier, params)?;
                Ok(format!("({lhs} OR {rhs})"))
            }
            PushExpr::Not(inner) => {
                let inner = self.push_expr_sql(struct_id, inner, qualifier, params)?;
                Ok(format!("(NOT {inner})"))
            }
            PushExpr::In { lhs, items } => {
                let lhs = self.push_expr_sql(struct_id, lhs, qualifier, params)?;
                let mut rendered = Vec::with_capacity(items.len());
                for item in items {
                    rendered.push(self.push_expr_sql(struct_id, item, qualifier, params)?);
                }
                Ok(format!("({lhs} IN ({}))", rendered.join(", ")))
            }
            PushExpr::StringOp { op, base, needle } => match op {
                StringOp::Contains => {
                    let base = self.push_expr_sql(struct_id, base, qualifier, params)?;
                    let needle = self.push_expr_sql(struct_id, needle, qualifier, params)?;
                    Ok(format!("(instr({base}, {needle}) > 0)"))
                }
                StringOp::StartsWith => {
                    let base = self.push_expr_sql(struct_id, base, qualifier, params)?;
                    let needle = self.push_expr_sql(struct_id, needle, qualifier, params)?;
                    Ok(format!("(instr({base}, {needle}) = 1)"))
                }
                StringOp::EndsWith => {
                    let needle_len = self.push_expr_sql(struct_id, needle, qualifier, params)?;
                    let base = self.push_expr_sql(struct_id, base, qualifier, params)?;
                    let needle_len2 = self.push_expr_sql(struct_id, needle, qualifier, params)?;
                    let needle = self.push_expr_sql(struct_id, needle, qualifier, params)?;
                    Ok(format!(
                        "(length({needle_len}) = 0 OR \
                        substr({base}, -length({needle_len2})) = {needle})"
                    ))
                }
            },
            PushExpr::StringLen(inner) => {
                let inner = self.push_expr_sql(struct_id, inner, qualifier, params)?;
                Ok(format!("(length({inner}))"))
            }
        }
    }

    /// Plan field slots, appending qualified columns to `select`
    fn plan_fields(
        &self,
        struct_id: StructId,
        fields: &[FieldId],
        qualifier: &str,
        select: &mut Vec<String>,
        ctx: &str,
    ) -> Result<Vec<Slot<'_, '_>>, ExecError> {
        let struct_def =
            self.schema
                .structs
                .get(struct_id.index())
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown struct id {}", struct_id.index()),
                })?;
        let struct_name = &struct_def.name;
        let mut slots = Vec::with_capacity(fields.len());
        for field_id in fields {
            if field_id.struct_id() != struct_id {
                return Err(ExecError::Internal {
                    message: format!("fetch field {field_id:?} belongs to another struct"),
                });
            }
            let field =
                struct_def
                    .fields
                    .get(field_id.local())
                    .ok_or_else(|| ExecError::Internal {
                        message: format!("unknown field {field_id:?} on struct `{struct_name}`"),
                    })?;
            match field {
                Field::Value { name, ty, columns } => {
                    let start = select.len();
                    for col in columns {
                        select.push(format!(
                            "{qualifier}.{}",
                            quote(self.schema.column_name(*col))
                        ));
                    }
                    let label = format!("{struct_name}.{name}");
                    if ty.has_array() {
                        let mut drafts = Vec::new();
                        let mut counter = 0;
                        let plan = walk_element(ty, None, &mut drafts, &mut counter);
                        let (nodes, stmts) = self.prepare_array_nodes(&drafts, &label, ctx)?;
                        let subtrees = split_roots(&nodes);
                        slots.push(Slot::ValueArray(ValueArraySlot {
                            label,
                            start,
                            len: columns.len(),
                            plan,
                            nodes,
                            stmts,
                            subtrees,
                        }));
                    } else {
                        slots.push(Slot::Value(ValueSlot {
                            label,
                            ty,
                            start,
                            len: columns.len(),
                        }));
                    }
                }
                Field::Array {
                    name,
                    element,
                    link,
                    storage,
                } => {
                    let link_idx = select.len();
                    select.push(format!(
                        "{qualifier}.{}",
                        quote(self.schema.column_name(*link))
                    ));
                    let label = format!("{struct_name}.{name}");
                    let mut drafts = vec![NodeDraft {
                        storage,
                        parent: None,
                        plan: None,
                    }];
                    let mut counter = 0;
                    let plan = walk_element(element, Some(0), &mut drafts, &mut counter);
                    drafts[0].plan = Some(plan);
                    let (nodes, stmts) = self.prepare_array_nodes(&drafts, &label, ctx)?;
                    let subtree: Vec<usize> = (0..nodes.len()).collect();
                    slots.push(Slot::Array(ArraySlot {
                        label,
                        link_idx,
                        nodes,
                        stmts,
                        subtree,
                    }));
                }
                Field::Ref { name, .. } => {
                    return Err(ExecError::Internal {
                        message: format!(
                            "cannot fetch Ref field `{struct_name}.{name}`: navigation uses fetch_related"
                        ),
                    });
                }
            }
        }
        Ok(slots)
    }
}

impl Storage for SqliteBackend<'_> {
    fn begin_write(&self) -> Result<Box<dyn WriteTxn + '_>, ExecError> {
        if self
            .conn
            .is_readonly("main")
            .map_err(|err| Self::storage_error("check read-only database", err))?
        {
            return Err(ExecError::ReadOnly {
                message: "database connection is read-only".to_string(),
            });
        }
        let txn = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(|err| Self::storage_error("begin write transaction", err))?;
        Ok(Box::new(SqliteWriteTxn {
            txn: Some(txn),
            schema: self.schema,
            keys: Arc::clone(&self.vtab_keys),
        }))
    }

    fn fetch(
        &self,
        struct_id: StructId,
        fields: &[FieldId],
        constraints: &FetchConstraints,
    ) -> Result<Vec<Row>, ExecError> {
        let struct_def =
            self.schema
                .structs
                .get(struct_id.index())
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown struct id {}", struct_id.index()),
                })?;
        let struct_name = &struct_def.name;
        let table = quote(&self.schema.tables[struct_def.table.index()].name);
        let ctx = format!("fetch {struct_name}");
        let mut select = vec!["_rowid_".to_string()];
        let mut slots = self.plan_fields(struct_id, fields, &table, &mut select, &ctx)?;
        let fingerprint = cache::fingerprint_fetch(struct_id, fields, constraints);
        let cached = self.cached_sql(
            fingerprint,
            |key| key.matches_fetch(struct_id, fields, constraints),
            || SqlCacheKey::Fetch {
                struct_id,
                fields: fields.to_vec(),
                constraints: constraints.clone(),
            },
            || {
                let mut params = Vec::new();
                let mut filters = Vec::with_capacity(constraints.filters.len());
                for filter in &constraints.filters {
                    filters.push(self.push_expr_sql(struct_id, filter, &table, &mut params)?);
                }
                let mut sort = Vec::with_capacity(constraints.sort.len() + 1);
                for key in &constraints.sort {
                    let (column, _) = self.pushdown_field(struct_id, key.field)?;
                    let direction = if key.desc { "DESC" } else { "ASC" };
                    sort.push(format!("{table}.{column} {direction}"));
                }
                let tie_direction = if constraints.tie_desc { "DESC" } else { "ASC" };
                sort.push(format!("_rowid_ {tie_direction}"));
                let mut sql = format!("SELECT {} FROM {table}", select.join(", "));
                if !filters.is_empty() {
                    sql.push_str(" WHERE ");
                    sql.push_str(&filters.join(" AND "));
                }
                sql.push_str(" ORDER BY ");
                sql.push_str(&sort.join(", "));
                if let Some(limit) = constraints.limit {
                    sql.push_str(&format!(" LIMIT {limit}"));
                    if constraints.offset != 0 {
                        sql.push_str(&format!(" OFFSET {}", constraints.offset));
                    }
                } else if constraints.offset != 0 {
                    sql.push_str(&format!(" LIMIT -1 OFFSET {}", constraints.offset));
                }
                Ok((sql, params))
            },
        )?;
        let mut stmt = self
            .conn
            .prepare_cached(&cached.sql)
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(&cached.params))
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|err| Self::storage_error(&ctx, err))? {
            let (rowid, values) = self.decode_row(&mut slots, row, &ctx)?;
            out.push(Row { rowid, values });
        }
        Ok(out)
    }

    fn fetch_related(
        &self,
        rel: RelationId,
        fetch_from_first: bool,
        parents: &[Key],
        fields: &[FieldId],
        constraints: &FetchConstraints,
    ) -> Result<Vec<Vec<Row>>, ExecError> {
        let mut groups: Vec<Vec<Row>> = parents.iter().map(|_| Vec::new()).collect();
        if parents.is_empty() {
            return Ok(groups);
        }
        let schema = &self.schema;
        let relation = schema
            .relations
            .get(rel.index())
            .ok_or_else(|| ExecError::Internal {
                message: format!("unknown relation id {}", rel.index()),
            })?;
        let fetch_ep = if fetch_from_first {
            relation.endpoints()[0]
        } else {
            relation.endpoints()[1]
        };
        // Parents are rows of the navigated endpoint's struct;
        // fetched rows are the endpoint target
        let parent_struct = fetch_ep.struct_id();
        let parent_def = &schema.structs[parent_struct.index()];
        let (target, field_name) = match parent_def.fields.get(fetch_ep.local()) {
            Some(Field::Ref { target, name, .. }) => (*target, name.clone()),
            _ => {
                return Err(ExecError::Internal {
                    message: format!("relation endpoint {fetch_ep:?} is not a reference"),
                });
            }
        };
        let ctx = format!("fetch_related {}.{field_name}", parent_def.name);
        let mut select = vec!["g$f._rowid_".to_string()];
        let mut slots = self.plan_fields(target, fields, "g$f", &mut select, &ctx)?;
        let parent_col = select.len();
        let key_bucket = cache::key_count_bucket(parents.len());
        let fingerprint = cache::fingerprint_fetch_related(
            rel,
            fetch_from_first,
            fields,
            constraints,
            key_bucket,
        );
        let cached = self.cached_sql(
            fingerprint,
            |key| key.matches_related(rel, fetch_from_first, fields, constraints, key_bucket),
            || SqlCacheKey::Related {
                rel,
                fetch_from_first,
                fields: fields.to_vec(),
                constraints: constraints.clone(),
                key_bucket,
            },
            || {
                let target_def = &schema.structs[target.index()];
                let fetched_table = quote(&schema.tables[target_def.table.index()].name);
                let known_table = quote(&schema.tables[parent_def.table.index()].name);
                let mut from = match relation {
                    Relation::OneToOne {
                        child_ref, fk, pk, ..
                    }
                    | Relation::ManyToOne {
                        child_ref, fk, pk, ..
                    } => {
                        let fk_col = quote(schema.column_name(*fk));
                        let pk_col = quote(schema.column_name(*pk));
                        let on = if fetch_ep != *child_ref {
                            format!("g$f.{fk_col} = g$k.{pk_col}")
                        } else {
                            format!("g$f.{pk_col} = g$k.{fk_col}")
                        };
                        format!("{fetched_table} AS g$f JOIN {known_table} AS g$k ON {on}")
                    }
                    Relation::OneToMany { b_ref, fk, pk, .. } => {
                        let fk_col = quote(schema.column_name(*fk));
                        let pk_col = quote(schema.column_name(*pk));
                        let on = if fetch_ep != *b_ref {
                            format!("g$f.{fk_col} = g$k.{pk_col}")
                        } else {
                            format!("g$f.{pk_col} = g$k.{fk_col}")
                        };
                        format!("{fetched_table} AS g$f JOIN {known_table} AS g$k ON {on}")
                    }
                    Relation::ManyToMany {
                        a_ref,
                        b_ref: _,
                        join_table,
                        a_col,
                        a_pk,
                        b_col,
                        b_pk,
                    } => {
                        // Endpoints point across (b endpoint's target is the a-side struct),
                        // so fetching through b reads a-side rows
                        let (x_col, x_pk, y_col, y_pk) = if fetch_ep != *a_ref {
                            (a_col, a_pk, b_col, b_pk)
                        } else {
                            (b_col, b_pk, a_col, a_pk)
                        };
                        let join_table = quote(&schema.tables[join_table.index()].name);
                        let x_col = quote(schema.column_name(*x_col));
                        let x_pk = quote(schema.column_name(*x_pk));
                        let y_col = quote(schema.column_name(*y_col));
                        let y_pk = quote(schema.column_name(*y_pk));
                        format!(
                            "{fetched_table} AS g$f \
                            JOIN {join_table} AS g$j ON g$j.{x_col} = g$f.{x_pk} \
                            JOIN {known_table} AS g$k ON g$j.{y_col} = g$k.{y_pk}"
                        )
                    }
                };
                from.push_str(&format!(
                    " JOIN {KEYS_TABLE} AS g$keys ON g$keys.value = g$k._rowid_"
                ));
                let mut params = Vec::new();
                let mut filters = Vec::with_capacity(constraints.filters.len());
                for filter in &constraints.filters {
                    filters.push(self.push_expr_sql(target, filter, "g$f", &mut params)?);
                }
                let mut sort = Vec::with_capacity(constraints.sort.len() + 1);
                for key in &constraints.sort {
                    let (column, _) = self.pushdown_field(target, key.field)?;
                    let direction = if key.desc { "DESC" } else { "ASC" };
                    sort.push(format!("g$f.{column} {direction}"));
                }
                let tie_direction = if constraints.tie_desc { "DESC" } else { "ASC" };
                sort.push(format!("g$f._rowid_ {tie_direction}"));
                let where_sql = if filters.is_empty() {
                    String::new()
                } else {
                    format!(" WHERE {}", filters.join(" AND "))
                };
                let caps = constraints.limit.is_some() || constraints.offset != 0;
                let sql = if caps {
                    let mut inner = select;
                    inner.push("g$k._rowid_ AS g$p".to_string());
                    inner.push(format!(
                        "ROW_NUMBER() OVER (PARTITION BY g$k._rowid_ ORDER BY {}) AS g$rn",
                        sort.join(", ")
                    ));
                    let mut outer_where = Vec::new();
                    if constraints.offset != 0 {
                        outer_where.push("g$rn > ?".to_string());
                        params.push(Value::Integer(saturating_i64(constraints.offset)));
                    }
                    if let Some(limit) = constraints.limit {
                        outer_where.push("g$rn <= ?".to_string());
                        params.push(Value::Integer(saturating_i64(
                            constraints.offset.saturating_add(limit),
                        )));
                    }
                    format!(
                        "SELECT * FROM (SELECT {} FROM {from}{where_sql}) \
                        WHERE {} ORDER BY g$p, g$rn",
                        inner.join(", "),
                        outer_where.join(" AND ")
                    )
                } else {
                    select.push("g$k._rowid_".to_string());
                    format!(
                        "SELECT {} FROM {from}{where_sql} ORDER BY {}",
                        select.join(", "),
                        sort.join(", ")
                    )
                };
                let sql = format!("{sql} /*g$keys:{key_bucket}*/");
                Ok((sql, params))
            },
        )?;
        self.vtab_keys.set(parents.iter().map(|key| key.0));
        let mut stmt = self
            .conn
            .prepare_cached(&cached.sql)
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(&cached.params))
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut index: HashMap<Key, Vec<usize>> = HashMap::new();
        for (i, key) in parents.iter().enumerate() {
            index.entry(*key).or_default().push(i);
        }
        while let Some(row) = rows.next().map_err(|err| Self::storage_error(&ctx, err))? {
            let (rowid, values) = self.decode_row(&mut slots, row, &ctx)?;
            let parent: i64 = row
                .get(parent_col)
                .map_err(|err| Self::storage_error(&ctx, err))?;
            if let Some(positions) = index.get(&Key(parent)) {
                if positions.len() == 1 {
                    groups[positions[0]].push(Row { rowid, values });
                } else {
                    for &i in positions {
                        groups[i].push(Row {
                            rowid,
                            values: values.clone(),
                        });
                    }
                }
            }
        }
        Ok(groups)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::tests::*;
    use crate::storage::SortKey;
    use crate::testdata;
    use crate::value::{Scalar, Val};
    use chrono::{DateTime, TimeDelta};
    use grove_schema::validated::ValidatedSchema;
    use rusqlite::Connection;
    use rust_decimal::dec;

    #[test]
    fn fetch_null_float_checks() {
        use grove_schema::validated::{
            Column, ColumnId, Config, Field, FieldId, Struct, StructId, Table, TableId, ValueType,
        };
        fn schema_with_checks(float_checks: bool) -> ValidatedSchema {
            ValidatedSchema {
                config: Config {
                    float_checks,
                    ..Default::default()
                },
                tables: vec![Table {
                    name: "t".to_string(),
                    columns: vec![Column {
                        name: "x".to_string(),
                        ty: Some(ScalarType::Float),
                    }],
                }],
                structs: vec![Struct {
                    name: "S".to_string(),
                    table: TableId::new(0),
                    fields: vec![Field::Value {
                        name: "x".to_string(),
                        ty: ValueType::Scalar(ScalarType::Float),
                        columns: vec![ColumnId::new(TableId::new(0), 0)],
                    }],
                }],
                ..Default::default()
            }
        }
        fn fetch_null_float(float_checks: bool) -> Result<Vec<Row>, ExecError> {
            let schema = schema_with_checks(float_checks);
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch("CREATE TABLE t(x REAL); INSERT INTO t(x) VALUES (NULL);")
                .unwrap();
            let backend = SqliteBackend::new(conn, &schema).unwrap();
            let sid = StructId::new(0);
            backend.fetch(sid, &[FieldId::new(sid, 0)], &FetchConstraints::EMPTY)
        }
        match fetch_null_float(false).unwrap()[0].values[0] {
            Val::Scalar(Scalar::Float(x)) => assert!(x.is_nan()),
            ref other => panic!("expected NaN, got {other:?}"),
        }
        let err = fetch_null_float(true).unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
    }

    #[test]
    fn fetch_bytes_blob() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let blobs = testdata::struct_id(&schema, "Blob");
        let data_fid = schema.struct_field(blobs, "data").unwrap().0;
        let maybe_fid = schema.struct_field(blobs, "maybe").unwrap().0;
        let rows = backend
            .fetch(blobs, &[data_fid, maybe_fid], &FetchConstraints::EMPTY)
            .unwrap();

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].values, vec![bytes([0, 1]), some(bytes([255])),]);
        assert_eq!(rows[1].values, vec![bytes([]), Val::None]);
    }

    #[test]
    fn fetch_scalars_positional() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(
                sid,
                &[
                    user_field(&schema, sid, "name"),
                    user_field(&schema, sid, "id"),
                    user_field(&schema, sid, "balance"),
                    user_field(&schema, sid, "created"),
                    user_field(&schema, sid, "elapsed"),
                    user_field(&schema, sid, "active"),
                ],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].rowid, Key(1));
        assert_eq!(
            rows[0].values,
            vec![
                str("Ada"),
                int(1),
                dec(dec!(30)),
                Val::Scalar(Scalar::Instant(
                    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
                )),
                Val::Scalar(Scalar::Duration(TimeDelta::seconds(90))),
                bool(true),
            ]
        );
        assert_eq!(
            rows[2].values,
            vec![
                str("Cyd"),
                int(3),
                dec(dec!(-5.25)),
                Val::Scalar(Scalar::Instant(
                    DateTime::from_timestamp(-1, 500_000_000).unwrap()
                )),
                Val::Scalar(Scalar::Duration(TimeDelta::milliseconds(-1500))),
                bool(false),
            ]
        );
    }

    #[test]
    fn fetch_empty_table_empty_fields() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let rows = backend.fetch(sid, &[], &FetchConstraints::EMPTY).unwrap();
        assert_eq!(rows.len(), 5);
        assert!(rows[0].values.is_empty());
        assert_eq!(rows[4].rowid, Key(5));
        backend.conn.execute("DELETE FROM users", []).unwrap();
        let rows = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "id")],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn fetch_reads_rowid_order() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        backend
            .conn
            .execute("DELETE FROM users WHERE rowid = 2", [])
            .unwrap();
        backend
            .conn
            .execute("INSERT INTO users(id) VALUES (9)", [])
            .unwrap();
        let sid = user_struct(&schema);
        let rows = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "id")],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.rowid).collect::<Vec<_>>(),
            vec![Key(1), Key(3), Key(4), Key(5), Key(6)]
        );
        assert_eq!(
            rows.iter().map(|r| r.values.clone()).collect::<Vec<_>>(),
            vec![
                vec![int(1)],
                vec![int(3)],
                vec![int(4)],
                vec![int(5)],
                vec![int(9)],
            ]
        );
    }

    #[test]
    fn fetch_ref_field_error() {
        use grove_schema::validated::{Field, Struct, Table, TableId};
        let schema = ValidatedSchema {
            tables: vec![Table {
                name: "t".to_string(),
                columns: vec![],
            }],
            structs: vec![Struct {
                name: "S".to_string(),
                table: TableId::new(0),
                fields: vec![Field::Ref {
                    name: "r".to_string(),
                    target: StructId::new(0),
                    optional: false,
                    is_list: false,
                    owning: false,
                }],
            }],
            ..Default::default()
        };
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(x INTEGER)").unwrap();
        let backend = SqliteBackend::new(conn, &schema).unwrap();
        let err = backend
            .fetch(
                StructId::new(0),
                &[FieldId::new(StructId::new(0), 0)],
                &FetchConstraints::EMPTY,
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Internal { .. }), "{err:?}");
        assert!(err.to_string().contains("Ref field `S.r`"), "{err}");
    }

    #[test]
    fn fetch_mismatched_types() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        let id = user_field(&schema, sid, "id");
        let name = user_field(&schema, sid, "name");
        let active = user_field(&schema, sid, "active");
        let balance = user_field(&schema, sid, "balance");
        for (setup, field, label) in [
            (
                "INSERT INTO users(id, name) VALUES (1, x'313233')",
                name,
                "User.name",
            ),
            (
                "INSERT INTO users(id, name) VALUES (1.5, 'x')",
                id,
                "User.id",
            ),
            (
                "INSERT INTO users(id, name, active) VALUES (1, 'x', 2)",
                active,
                "User.active",
            ),
            (
                "INSERT INTO users(id, name, balance) VALUES (1, 'x', x'6a756e6b')",
                balance,
                "User.balance",
            ),
        ] {
            backend.conn.execute_batch("DELETE FROM users").unwrap();
            backend.conn.execute_batch(setup).unwrap();
            let err = backend
                .fetch(sid, &[field], &FetchConstraints::EMPTY)
                .unwrap_err();
            assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
            assert!(err.to_string().contains(label), "{err}");
        }
    }

    #[test]
    fn fetch_bad_ids_error() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);
        assert!(matches!(
            backend
                .fetch(StructId::new(99), &[], &FetchConstraints::EMPTY)
                .unwrap_err(),
            ExecError::Internal { .. }
        ));
        assert!(matches!(
            backend
                .fetch(
                    sid,
                    &[FieldId::new(StructId::new(7), 0)],
                    &FetchConstraints::EMPTY
                )
                .unwrap_err(),
            ExecError::Internal { .. }
        ));
    }

    #[test]
    fn fetch_related_one_to_one() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let users = testdata::struct_id(&schema, "User");
        let profiles = testdata::struct_id(&schema, "Profile");
        let rel = rel_named(&schema, "Profile", "user", "User", "profile");
        let bio = testdata::field_id(&schema, profiles, "bio");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2), Key(3)],
                &[bio],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("AdaBio")], vec![], vec![str("CydBio")]]
        );
        let name = testdata::field_id(&schema, users, "name");
        let groups = backend
            .fetch_related(
                rel,
                true,
                &[Key(1), Key(2)],
                &[name],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(firsts(&groups), vec![vec![str("Ada")], vec![str("Cyd")]]);
    }

    #[test]
    fn fetch_related_many_to_one() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let users = testdata::struct_id(&schema, "User");
        let orders = testdata::struct_id(&schema, "Order");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");
        let name = testdata::field_id(&schema, users, "name");
        let groups = backend
            .fetch_related(
                rel,
                true,
                &[Key(1), Key(2), Key(3), Key(4)],
                &[name],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("Ada")], vec![str("Ada")], vec![], vec![str("Bob")]]
        );
        let amount = testdata::field_id(&schema, orders, "amount");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2), Key(3)],
                &[amount],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![int(100), int(200)], vec![int(400)], vec![]]
        );
    }

    #[test]
    fn fetch_related_constraints_filter_sort() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");
        let constraints = FetchConstraints {
            filters: vec![push_cmp(
                CmpOp::Gt,
                target_field(&schema, "Order", "amount"),
                PushExpr::Literal(Scalar::Int(150)),
            )],
            sort: vec![SortKey {
                field: amount,
                desc: true,
            }],
            tie_desc: false,
            offset: 0,
            limit: None,
        };
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2), Key(3)],
                &[amount],
                &constraints,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![int(200)], vec![int(400)], vec![]]
        );
    }

    #[test]
    fn fetch_related_tie_desc() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2)],
                &[amount],
                &FetchConstraints {
                    filters: vec![],
                    sort: vec![],
                    tie_desc: true,
                    offset: 0,
                    limit: None,
                },
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![int(200), int(100)], vec![int(400)]]
        );
    }

    #[test]
    fn fetch_related_limits_per_parent() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");

        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2), Key(3)],
                &[amount],
                &FetchConstraints {
                    filters: vec![],
                    sort: vec![],
                    tie_desc: false,
                    offset: 0,
                    limit: Some(1),
                },
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![int(100)], vec![int(400)], vec![]]
        );

        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2), Key(3)],
                &[amount],
                &FetchConstraints {
                    filters: vec![],
                    sort: vec![],
                    tie_desc: false,
                    offset: 1,
                    limit: Some(1),
                },
            )
            .unwrap();
        assert_eq!(firsts(&groups), vec![vec![int(200)], vec![], vec![]]);
    }

    #[test]
    fn fetch_related_duplicate_parents() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");

        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(1), Key(2)],
                &[amount],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![
                vec![int(100), int(200)],
                vec![int(100), int(200)],
                vec![int(400)],
            ]
        );
    }

    #[test]
    fn fetch_related_non_owning_constraints() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let name = testdata::field_id(&schema, testdata::struct_id(&schema, "Role"), "name");
        let rel = rel_named(&schema, "Role", "users", "User", "roles");
        let constraints = FetchConstraints {
            filters: vec![push_cmp(
                CmpOp::Eq,
                target_field(&schema, "Role", "name"),
                PushExpr::Literal(Scalar::String("editor".to_string())),
            )],
            sort: vec![],
            tie_desc: false,
            offset: 0,
            limit: Some(1),
        };
        let groups = backend
            .fetch_related(rel, false, &[Key(1), Key(3), Key(2)], &[name], &constraints)
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("editor")], vec![str("editor")], vec![]]
        );
    }

    #[test]
    fn fetch_related_offset_only() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
        let rel = rel_named(&schema, "Order", "user", "User", "orders");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(2)],
                &[amount],
                &FetchConstraints {
                    filters: vec![],
                    sort: vec![SortKey {
                        field: amount,
                        desc: false,
                    }],
                    tie_desc: false,
                    offset: 1,
                    limit: None,
                },
            )
            .unwrap();
        assert_eq!(firsts(&groups), vec![vec![int(200)], vec![]]);
    }

    #[test]
    fn fetch_related_one_to_many() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let images = testdata::struct_id(&schema, "Image");
        let messages = testdata::struct_id(&schema, "Message");
        let rel = rel_named(&schema, "Image", "messages", "Message", "image");
        let body = testdata::field_id(&schema, messages, "body");
        let groups = backend
            .fetch_related(
                rel,
                true,
                &[Key(1), Key(2)],
                &[body],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("nice"), str("wow")], vec![str("meh")]]
        );
        let title = testdata::field_id(&schema, images, "title");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(4)],
                &[title],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(firsts(&groups), vec![vec![str("Sunset")], vec![]]);
    }

    #[test]
    fn fetch_related_many_to_many() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let users = testdata::struct_id(&schema, "User");
        let roles = testdata::struct_id(&schema, "Role");
        let rel = rel_named(&schema, "Role", "users", "User", "roles");
        let rname = testdata::field_id(&schema, roles, "name");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(3), Key(2)],
                &[rname],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![
                vec![str("admin"), str("editor"), str("editor")],
                vec![str("editor")],
                vec![],
            ]
        );
        assert_eq!(
            groups[0].iter().map(|row| row.rowid).collect::<Vec<_>>(),
            vec![Key(1), Key(2), Key(2)]
        );
        let uname = testdata::field_id(&schema, users, "name");
        let groups = backend
            .fetch_related(
                rel,
                true,
                &[Key(2), Key(1)],
                &[uname],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("Ada"), str("Ada"), str("Cyd")], vec![str("Ada")]]
        );
        assert_eq!(
            groups[0].iter().map(|row| row.rowid).collect::<Vec<_>>(),
            vec![Key(1), Key(1), Key(3)]
        );
    }

    #[test]
    fn fetch_related_self_reference() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let users = testdata::struct_id(&schema, "User");
        let rel = rel_named(&schema, "User", "manager", "User", "subordinates");
        let name = testdata::field_id(&schema, users, "name");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(2), Key(1)],
                &[name],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("Ada"), str("Cyd")], vec![str("Dan")]]
        );
        let groups = backend
            .fetch_related(
                rel,
                true,
                &[Key(1), Key(4), Key(2)],
                &[name],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(
            firsts(&groups),
            vec![vec![str("Bob")], vec![str("Ada")], vec![]]
        );
    }

    #[test]
    fn fetch_related_with_array_fields() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let profiles = testdata::struct_id(&schema, "Profile");
        let rel = rel_named(&schema, "Profile", "user", "User", "profile");
        let groups = backend
            .fetch_related(
                rel,
                false,
                &[Key(1), Key(3)],
                &[
                    testdata::field_id(&schema, profiles, "bio"),
                    testdata::field_id(&schema, profiles, "notes"),
                ],
                &FetchConstraints::EMPTY,
            )
            .unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(
            groups[0][0].values,
            vec![str("AdaBio"), Val::list(vec![str("a-note")])]
        );
        assert_eq!(groups[1][0].values, vec![str("CydBio"), Val::list(vec![])]);
    }

    #[test]
    fn fetch_related_edge_cases() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let rel = rel_named(&schema, "Profile", "user", "User", "profile");
        // empty parents: no rows, fields arg irrelevant (never resolved)
        let groups = backend
            .fetch_related(rel, true, &[], &[], &FetchConstraints::EMPTY)
            .unwrap();
        assert!(groups.is_empty());
        let err = backend
            .fetch_related(
                RelationId::new(99),
                true,
                &[Key(1)],
                &[],
                &FetchConstraints::EMPTY,
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Internal { .. }), "{err:?}");
    }

    #[test]
    fn fetch_filter_comparisons() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);

        let filters = vec![push_cmp(CmpOp::Gt, push_field(&schema, "id"), push_int(2))];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Cyd", "Dan", "Eli"])
        );

        let filters = vec![push_cmp(
            CmpOp::Eq,
            push_field(&schema, "name"),
            push_str("Bob"),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Bob"])
        );

        let filters = vec![push_cmp(CmpOp::Ne, push_field(&schema, "id"), push_int(1))];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Bob", "Cyd", "Dan", "Eli"])
        );

        let filters = vec![push_cmp(
            CmpOp::Eq,
            push_field(&schema, "active"),
            PushExpr::Literal(Scalar::Bool(false)),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Cyd", "Eli"])
        );

        let filters = vec![PushExpr::In {
            lhs: Box::new(push_field(&schema, "id")),
            items: vec![push_int(1), push_int(3), push_int(5)],
        }];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Cyd", "Eli"])
        );

        let filters = vec![PushExpr::And(
            Box::new(push_field(&schema, "active")),
            Box::new(push_cmp(CmpOp::Gt, push_field(&schema, "id"), push_int(1))),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Bob", "Dan"])
        );

        let filters = vec![PushExpr::Or(
            Box::new(push_cmp(CmpOp::Eq, push_field(&schema, "id"), push_int(1))),
            Box::new(push_cmp(CmpOp::Eq, push_field(&schema, "id"), push_int(5))),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Eli"])
        );

        let filters = vec![PushExpr::Not(Box::new(push_field(&schema, "active")))];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Cyd", "Eli"])
        );
    }

    #[test]
    fn fetch_filter_string_operations() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);

        let string_op = |op: StringOp, needle: &str| {
            vec![PushExpr::StringOp {
                op,
                base: Box::new(push_field(&schema, "name")),
                needle: Box::new(push_str(needle)),
            }]
        };
        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::Contains, "a"),
                vec![],
                0,
                None
            ),
            names(&["Ada", "Dan"])
        );
        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::Contains, ""),
                vec![],
                0,
                None
            ),
            names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
        );

        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::StartsWith, "A"),
                vec![],
                0,
                None
            ),
            names(&["Ada"])
        );
        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::StartsWith, ""),
                vec![],
                0,
                None
            ),
            names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
        );

        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::EndsWith, "d"),
                vec![],
                0,
                None
            ),
            names(&["Cyd"])
        );
        assert_eq!(
            user_names(
                &backend,
                &schema,
                string_op(StringOp::EndsWith, ""),
                vec![],
                0,
                None
            ),
            names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
        );

        let filters = vec![push_cmp(
            CmpOp::Ge,
            PushExpr::StringLen(Box::new(push_field(&schema, "name"))),
            push_int(3),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
        );
    }

    #[test]
    fn fetch_filter_temporal_and_decimal() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);

        let filters = vec![push_cmp(
            CmpOp::Gt,
            push_field(&schema, "balance"),
            PushExpr::Literal(Scalar::Dec(dec!(30.00))),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Dan"])
        );

        let filters = vec![push_cmp(
            CmpOp::Ge,
            push_field(&schema, "balance"),
            PushExpr::Literal(Scalar::Dec(dec!(30.00))),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Dan"])
        );

        let filters = vec![push_cmp(
            CmpOp::Ge,
            push_field(&schema, "created"),
            PushExpr::Literal(Scalar::Instant(
                DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            )),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Dan", "Eli"])
        );

        let filters = vec![push_cmp(
            CmpOp::Gt,
            push_field(&schema, "elapsed"),
            PushExpr::Literal(Scalar::Duration(TimeDelta::zero())),
        )];
        assert_eq!(
            user_names(&backend, &schema, filters, vec![], 0, None),
            names(&["Ada", "Dan", "Eli"])
        );
    }

    #[test]
    fn fetch_sort_limit_offset() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);

        let sort = vec![SortKey {
            field: user_field(&schema, sid, "id"),
            desc: true,
        }];
        assert_eq!(
            user_names(&backend, &schema, vec![], sort, 0, None),
            names(&["Eli", "Dan", "Cyd", "Bob", "Ada"])
        );

        let sort = vec![
            SortKey {
                field: user_field(&schema, sid, "active"),
                desc: true,
            },
            SortKey {
                field: user_field(&schema, sid, "id"),
                desc: false,
            },
        ];
        assert_eq!(
            user_names(&backend, &schema, vec![], sort, 0, None),
            names(&["Ada", "Bob", "Dan", "Cyd", "Eli"])
        );

        let sort = vec![SortKey {
            field: user_field(&schema, sid, "active"),
            desc: false,
        }];
        assert_eq!(
            user_names(&backend, &schema, vec![], sort, 0, None),
            names(&["Cyd", "Eli", "Ada", "Bob", "Dan"])
        );

        assert_eq!(
            user_names(&backend, &schema, vec![], vec![], 0, Some(2)),
            names(&["Ada", "Bob"])
        );
        assert_eq!(
            user_names(&backend, &schema, vec![], vec![], 2, None),
            names(&["Cyd", "Dan", "Eli"])
        );
        assert_eq!(
            user_names(&backend, &schema, vec![], vec![], 3, Some(1)),
            names(&["Dan"])
        );
        assert_eq!(
            user_names(&backend, &schema, vec![], vec![], 0, Some(0)),
            Vec::<String>::new()
        );
        assert_eq!(
            user_names(&backend, &schema, vec![], vec![], 10, None),
            Vec::<String>::new()
        );

        let sort = vec![SortKey {
            field: user_field(&schema, sid, "name"),
            desc: true,
        }];
        assert_eq!(
            user_names(&backend, &schema, vec![], sort, 0, Some(2)),
            names(&["Eli", "Dan"])
        );

        assert_eq!(
            user_names_with_tie(&backend, &schema, vec![], vec![], true, 0, None),
            names(&["Eli", "Dan", "Cyd", "Bob", "Ada"])
        );

        let sort = vec![SortKey {
            field: user_field(&schema, sid, "active"),
            desc: true,
        }];
        assert_eq!(
            user_names_with_tie(&backend, &schema, vec![], sort, true, 0, None),
            names(&["Dan", "Bob", "Ada", "Eli", "Cyd"])
        );
    }

    #[test]
    fn fetch_constraints_rejected_fields() {
        let schema = testdata::schema();
        let backend = test_backend(&schema);
        let sid = user_struct(&schema);

        let order = testdata::struct_id(&schema, "Order");
        let amount = testdata::field_id(&schema, order, "amount");
        let err = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "name")],
                &FetchConstraints {
                    filters: vec![PushExpr::Field(amount)],
                    sort: vec![],
                    tie_desc: false,
                    offset: 0,
                    limit: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Internal { .. }), "{err:?}");

        let pos = user_field(&schema, sid, "pos");
        let err = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "name")],
                &FetchConstraints {
                    filters: vec![PushExpr::Field(pos)],
                    sort: vec![],
                    tie_desc: false,
                    offset: 0,
                    limit: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Internal { .. }), "{err:?}");

        let err = backend
            .fetch(
                sid,
                &[user_field(&schema, sid, "name")],
                &FetchConstraints {
                    filters: vec![],
                    sort: vec![SortKey {
                        field: pos,
                        desc: false,
                    }],
                    tie_desc: false,
                    offset: 0,
                    limit: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, ExecError::Internal { .. }), "{err:?}");
    }
}
