use std::collections::HashMap;

use grove_schema::validated::{
    Field, FieldId, Relation, RelationId, ScalarType, StructId, ValueType,
};
use rusqlite::types::Value;
use rusqlite::{Transaction, TransactionBehavior};

use super::SqliteBackend;
use super::cache::{SqlCacheKey, fingerprint_fetch, fingerprint_fetch_related};
use super::decode::{
    ArraySlot, NodeDraft, Slot, ValueArraySlot, ValueSlot, split_roots, walk_element,
};
use super::types::{quote, saturating_i64, scalar_value};
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
        let fingerprint = fingerprint_fetch(struct_id, fields, constraints);
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
        let fingerprint =
            fingerprint_fetch_related(rel, fetch_from_first, fields, constraints, parents.len());
        let cached = self.cached_sql(
            fingerprint,
            |key| key.matches_related(rel, fetch_from_first, fields, constraints, parents.len()),
            || SqlCacheKey::Related {
                rel,
                fetch_from_first,
                fields: fields.to_vec(),
                constraints: constraints.clone(),
                parents: parents.len(),
            },
            || {
                let target_def = &schema.structs[target.index()];
                let fetched_table = quote(&schema.tables[target_def.table.index()].name);
                let known_table = quote(&schema.tables[parent_def.table.index()].name);
                let from = match relation {
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
                let placeholders = vec!["?"; parents.len()].join(", ");
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
                let mut where_clause = format!("g$k._rowid_ IN ({placeholders})");
                if !filters.is_empty() {
                    where_clause.push_str(" AND ");
                    where_clause.push_str(&filters.join(" AND "));
                }
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
                        "SELECT * FROM (SELECT {} FROM {from} WHERE {where_clause}) \
                     WHERE {} ORDER BY g$p, g$rn",
                        inner.join(", "),
                        outer_where.join(" AND ")
                    )
                } else {
                    select.push("g$k._rowid_".to_string());
                    format!(
                        "SELECT {} FROM {from} WHERE {where_clause} ORDER BY {}",
                        select.join(", "),
                        sort.join(", ")
                    )
                };
                Ok((sql, params))
            },
        )?;
        let mut bind: Vec<Value> = parents.iter().map(|key| Value::Integer(key.0)).collect();
        bind.extend(cached.params.iter().cloned());
        let mut stmt = self
            .conn
            .prepare_cached(&cached.sql)
            .map_err(|err| Self::storage_error(&ctx, err))?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(bind))
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
