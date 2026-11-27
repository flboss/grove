use std::collections::{HashMap, HashSet};

use grove_schema::validated::{
    ColumnId, Field, FieldId, Relation, RelationId, StorageTable, StructId, TableId,
    ValidatedSchema, ValueType,
};
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction};

use super::SqliteBackend;
use super::types::{encode_value, quote, value_write_width};
use crate::error::ExecError;
use crate::storage::{LinkMode, WriteTxn};
use crate::value::{Key, RowOrigin, Val};

const PARAM_CHUNK: usize = 500;

fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

pub(super) struct SqliteWriteTxn<'conn, 'schema> {
    pub(super) txn: Option<Transaction<'conn>>,
    pub(super) schema: &'schema ValidatedSchema,
}

impl SqliteWriteTxn<'_, '_> {
    fn table_name(&self, struct_id: StructId) -> Result<String, ExecError> {
        let struct_def =
            self.schema
                .structs
                .get(struct_id.index())
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown struct id {}", struct_id.index()),
                })?;
        Ok(quote(&self.schema.tables[struct_def.table.index()].name))
    }

    /// Encode field values into quoted columns and parameters,
    /// writing array rows to storage
    fn encode_row_write(
        &mut self,
        struct_id: StructId,
        values: &[(FieldId, Val)],
    ) -> Result<(Vec<String>, Vec<Value>), ExecError> {
        let struct_def =
            self.schema
                .structs
                .get(struct_id.index())
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown struct id {}", struct_id.index()),
                })?;
        let mut seen = HashSet::with_capacity(values.len());
        let mut columns = Vec::new();
        let mut params = Vec::new();
        for (field_id, value) in values {
            if field_id.struct_id() != struct_id {
                return Err(ExecError::Internal {
                    message: format!("write field {field_id:?} belongs to another struct"),
                });
            }
            if !seen.insert(field_id.local()) {
                return Err(ExecError::Internal {
                    message: format!("write field {field_id:?} specified more than once"),
                });
            }
            let field =
                struct_def
                    .fields
                    .get(field_id.local())
                    .ok_or_else(|| ExecError::Internal {
                        message: format!(
                            "unknown field {field_id:?} on struct `{}`",
                            struct_def.name
                        ),
                    })?;
            match field {
                Field::Value {
                    ty,
                    columns: field_columns,
                    ..
                } => {
                    let mut encoded = Vec::new();
                    self.encode_value_write(ty, value, &mut encoded)?;
                    if encoded.len() != field_columns.len() {
                        return Err(ExecError::Internal {
                            message: format!(
                                "field {field_id:?} encoded to {} values for {} columns",
                                encoded.len(),
                                field_columns.len()
                            ),
                        });
                    }
                    for (column, value) in field_columns.iter().zip(encoded) {
                        columns.push(quote(self.schema.column_name(*column)));
                        params.push(value);
                    }
                }
                Field::Array {
                    link,
                    element,
                    storage,
                    ..
                } => {
                    let Val::List(list) = value else {
                        return Err(ExecError::Internal {
                            message: format!("expected list value, got {value}"),
                        });
                    };
                    columns.push(quote(self.schema.column_name(*link)));
                    if list.is_empty() {
                        params.push(Value::Null);
                    } else {
                        let key = self.next_link_key(storage)?;
                        params.push(Value::Integer(key));
                        self.write_array_rows(storage, key, element, list.iter().cloned())?;
                    }
                }
                Field::Ref { name, .. } => {
                    return Err(ExecError::Internal {
                        message: format!(
                            "attempted to write directly to referennce field `{}.{name}`",
                            struct_def.name
                        ),
                    });
                }
            }
        }
        Ok((columns, params))
    }

    /// Recursively encode a value for insertion,
    /// allocating and writing array rows to storage
    fn encode_value_write(
        &mut self,
        ty: &ValueType,
        value: &Val,
        out: &mut Vec<Value>,
    ) -> Result<(), ExecError> {
        match ty {
            ValueType::Scalar(_) | ValueType::Enum(_) => encode_value(ty, value, self.schema, out),
            ValueType::Optional(inner) => match value {
                Val::None => {
                    for _ in 0..value_write_width(inner) {
                        out.push(Value::Null);
                    }
                    Ok(())
                }
                Val::Some(value) => self.encode_value_write(inner, value, out),
                other => Err(ExecError::Internal {
                    message: format!("expected optional value, got {other}"),
                }),
            },
            ValueType::Tuple(elements) => {
                let Val::Tuple(values) = value else {
                    return Err(ExecError::Internal {
                        message: format!("expected tuple value, got {value}"),
                    });
                };
                if values.len() != elements.len() {
                    return Err(ExecError::Internal {
                        message: format!(
                            "tuple has {} values for {} element types",
                            values.len(),
                            elements.len()
                        ),
                    });
                }
                for (element_ty, value) in elements.iter().zip(values) {
                    self.encode_value_write(element_ty, value, out)?;
                }
                Ok(())
            }
            ValueType::Array { element, storage } => {
                let Val::List(list) = value else {
                    return Err(ExecError::Internal {
                        message: format!("expected list value, got {value}"),
                    });
                };
                if list.is_empty() {
                    out.push(Value::Null);
                } else {
                    let key = self.next_link_key(storage)?;
                    out.push(Value::Integer(key));
                    self.write_array_rows(storage, key, element, list.iter().cloned())?;
                }
                Ok(())
            }
        }
    }

    /// Allocate the next integer link key in a storage table
    fn next_link_key(&self, storage: &StorageTable) -> Result<i64, ExecError> {
        let table = quote(&self.schema.tables[storage.table.index()].name);
        let key = quote(self.schema.column_name(storage.key));
        let sql = format!("SELECT COALESCE(MAX({key}), 0) + 1 FROM {table}");
        self.txn()?
            .query_row(&sql, [], |row| row.get(0))
            .map_err(|err| SqliteBackend::storage_error("allocate array link key", err))
    }

    /// Write all element rows for one array link key
    fn write_array_rows(
        &mut self,
        storage: &StorageTable,
        key: i64,
        element: &ValueType,
        values: impl Iterator<Item = Val>,
    ) -> Result<(), ExecError> {
        let table = quote(&self.schema.tables[storage.table.index()].name);
        let mut columns = Vec::with_capacity(storage.value.len() + 1);
        columns.push(quote(self.schema.column_name(storage.key)));
        columns.extend(
            storage
                .value
                .iter()
                .map(|column| quote(self.schema.column_name(*column))),
        );
        let mut rows = Vec::new();
        for value in values {
            let mut cells = Vec::new();
            self.encode_value_write(element, &value, &mut cells)?;
            if cells.len() != storage.value.len() {
                return Err(ExecError::Internal {
                    message: format!(
                        "array element encoded to {} values for {} storage columns",
                        cells.len(),
                        storage.value.len()
                    ),
                });
            }
            let mut row = Vec::with_capacity(cells.len() + 1);
            row.push(Value::Integer(key));
            row.append(&mut cells);
            rows.push(row);
        }
        self.insert_encoded_rows(&table, &columns, &rows)?;
        Ok(())
    }

    fn insert_rows_individually(
        &mut self,
        struct_id: StructId,
        rows: &[Vec<(FieldId, Val)>],
    ) -> Result<Vec<Key>, ExecError> {
        rows.iter()
            .map(|row| self.insert_row(struct_id, row))
            .collect()
    }

    /// Insert one encoded row and returns its rowid
    fn insert_encoded_row(
        &mut self,
        table: &str,
        columns: &[String],
        params: &[Value],
    ) -> Result<Key, ExecError> {
        let sql = if columns.is_empty() {
            format!("INSERT INTO {table} DEFAULT VALUES")
        } else {
            let placeholders = placeholders(columns.len());
            format!(
                "INSERT INTO {table} ({}) VALUES ({placeholders})",
                columns.join(", ")
            )
        };
        let txn = self.txn()?;
        let mut stmt = txn
            .prepare_cached(&sql)
            .map_err(|err| SqliteBackend::storage_error("insert row (prepare)", err))?;
        stmt.execute(rusqlite::params_from_iter(params))
            .map_err(|err| SqliteBackend::storage_error("insert row (execute)", err))?;
        Ok(Key(txn.last_insert_rowid()))
    }

    /// Insert encoded rows in parameter-chunked batches and return their rowids
    fn insert_encoded_rows(
        &mut self,
        table: &str,
        columns: &[String],
        encoded_rows: &[Vec<Value>],
    ) -> Result<Vec<Key>, ExecError> {
        let width = columns.len();
        let rows_per_chunk = (500 / width).max(1);
        let row_placeholders = format!("({})", placeholders(width));
        let txn = self.txn()?;
        let mut keys = Vec::with_capacity(encoded_rows.len());
        for chunk in encoded_rows.chunks(rows_per_chunk) {
            let values = vec![row_placeholders.as_str(); chunk.len()].join(", ");
            let sql = format!(
                "INSERT INTO {table} ({}) VALUES {values} RETURNING _rowid_",
                columns.join(", ")
            );
            let mut params = Vec::with_capacity(chunk.len() * width);
            for row in chunk {
                params.extend(row.iter().cloned());
            }
            let mut stmt = txn
                .prepare_cached(&sql)
                .map_err(|err| SqliteBackend::storage_error("insert rows (prepare)", err))?;
            let mut returned = stmt
                .query(rusqlite::params_from_iter(params))
                .map_err(|err| SqliteBackend::storage_error("insert rows (execute)", err))?;
            while let Some(row) = returned
                .next()
                .map_err(|err| SqliteBackend::storage_error("insert rows (returning)", err))?
            {
                let rowid = row
                    .get(0)
                    .map_err(|err| SqliteBackend::storage_error("insert rows (rowid)", err))?;
                keys.push(Key(rowid));
            }
        }
        Ok(keys)
    }

    /// Return the child FK and parent PK columns for a writable direct relation
    fn relation_key_columns(&self, rel: RelationId) -> Result<(ColumnId, ColumnId), ExecError> {
        match self.schema.relations.get(rel.index()) {
            Some(Relation::OneToOne { fk, pk, .. } | Relation::ManyToOne { fk, pk, .. }) => {
                Ok((*fk, *pk))
            }
            Some(Relation::OneToMany { .. } | Relation::ManyToMany { .. }) => {
                Err(ExecError::Unsupported {
                    message: "inserting through non-owning relations is not implemented"
                        .to_string(),
                })
            }
            None => Err(ExecError::Internal {
                message: format!("unknown relation id {}", rel.index()),
            }),
        }
    }

    /// Read and validate a parent row's join-key value
    fn fetch_parent_key(&self, pk: ColumnId, parent: Key) -> Result<Value, ExecError> {
        self.fetch_row_key(pk, parent, "insert parent")
    }

    fn fetch_row_key(&self, column: ColumnId, row: Key, role: &str) -> Result<Value, ExecError> {
        let table = quote(&self.schema.tables[column.table_id().index()].name);
        let column_name = quote(self.schema.column_name(column));
        let sql = format!("SELECT {column_name} FROM {table} WHERE _rowid_ = ?");
        let txn = self.txn()?;
        let value: Option<Value> = txn
            .query_row(&sql, [row.0], |row| row.get(0))
            .optional()
            .map_err(|err| SqliteBackend::storage_error("fetch row key", err))?;
        match value {
            Some(Value::Null) | None => Err(ExecError::Integrity {
                message: format!(
                    "{role} row {} has no join key in column `{}`",
                    row.0,
                    self.schema.column_name(column)
                ),
            }),
            Some(value) => Ok(value),
        }
    }

    fn fetch_row_keys(
        &self,
        column: ColumnId,
        rows: &[RowOrigin],
        role: &str,
    ) -> Result<Vec<Value>, ExecError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let table = quote(&self.schema.tables[column.table_id().index()].name);
        let column_name = quote(self.schema.column_name(column));
        let txn = self.txn()?;
        let mut found = HashMap::with_capacity(rows.len());
        for chunk in rows.chunks(PARAM_CHUNK) {
            let placeholders = placeholders(chunk.len());
            let sql = format!(
                "SELECT _rowid_, {column_name} FROM {table} WHERE _rowid_ IN ({placeholders})"
            );
            let params = chunk.iter().map(|origin| Value::Integer(origin.key.0));
            let mut stmt = txn
                .prepare_cached(&sql)
                .map_err(|err| SqliteBackend::storage_error("fetch row keys (prepare)", err))?;
            let mut query = stmt
                .query(rusqlite::params_from_iter(params))
                .map_err(|err| SqliteBackend::storage_error("fetch row keys (query)", err))?;
            while let Some(row) = query
                .next()
                .map_err(|err| SqliteBackend::storage_error("fetch row keys (row)", err))?
            {
                let rowid: i64 = row
                    .get(0)
                    .map_err(|err| SqliteBackend::storage_error("fetch row keys (rowid)", err))?;
                let value: Value = row
                    .get(1)
                    .map_err(|err| SqliteBackend::storage_error("fetch row keys (value)", err))?;
                found.insert(rowid, value);
            }
        }

        let mut values = Vec::with_capacity(rows.len());
        for origin in rows {
            let Some(value) = found.remove(&origin.key.0) else {
                return Err(ExecError::Integrity {
                    message: format!("{role} row {} was not found", origin.key.0),
                });
            };
            if matches!(value, Value::Null) {
                return Err(ExecError::Integrity {
                    message: format!(
                        "{role} row {} has no join key in column `{}`",
                        origin.key.0,
                        self.schema.column_name(column)
                    ),
                });
            }
            values.push(value);
        }
        Ok(values)
    }

    fn fetch_join_values(
        &self,
        join_table: TableId,
        owner_col: ColumnId,
        target_col: ColumnId,
        owner: &Value,
    ) -> Result<Vec<Value>, ExecError> {
        let table = quote(&self.schema.tables[join_table.index()].name);
        let owner_col = quote(self.schema.column_name(owner_col));
        let target_col = quote(self.schema.column_name(target_col));
        let sql = format!("SELECT {target_col} FROM {table} WHERE {owner_col} = ?");
        let txn = self.txn()?;
        let mut stmt = txn
            .prepare_cached(&sql)
            .map_err(|err| SqliteBackend::storage_error("fetch join values (prepare)", err))?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter([owner]))
            .map_err(|err| SqliteBackend::storage_error("fetch join values (query)", err))?;
        let mut values = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| SqliteBackend::storage_error("fetch join values (row)", err))?
        {
            values.push(
                row.get(0).map_err(|err| {
                    SqliteBackend::storage_error("fetch join values (value)", err)
                })?,
            );
        }
        Ok(values)
    }

    fn insert_join_rows(
        &mut self,
        join_table: TableId,
        owner_col: ColumnId,
        target_col: ColumnId,
        owner: &Value,
        targets: &[Value],
    ) -> Result<(), ExecError> {
        if targets.is_empty() {
            return Ok(());
        }
        let table = quote(&self.schema.tables[join_table.index()].name);
        let owner_col = quote(self.schema.column_name(owner_col));
        let target_col = quote(self.schema.column_name(target_col));
        let txn = self.txn()?;
        for chunk in targets.chunks(PARAM_CHUNK) {
            let values = vec!["(?, ?)"; chunk.len()].join(", ");
            let sql = format!("INSERT INTO {table} ({owner_col}, {target_col}) VALUES {values}");
            let mut params = Vec::with_capacity(chunk.len() * 2);
            for target in chunk {
                params.push(owner.clone());
                params.push(target.clone());
            }
            let mut stmt = txn
                .prepare_cached(&sql)
                .map_err(|err| SqliteBackend::storage_error("insert join rows (prepare)", err))?;
            stmt.execute(rusqlite::params_from_iter(params.iter()))
                .map_err(|err| SqliteBackend::storage_error("insert join rows (execute)", err))?;
        }
        Ok(())
    }

    fn link_many_to_many(
        &mut self,
        rel: RelationId,
        field: FieldId,
        owner: Key,
        targets: &[RowOrigin],
        mode: LinkMode,
    ) -> Result<(), ExecError> {
        let relation = self
            .schema
            .relations
            .get(rel.index())
            .cloned()
            .ok_or_else(|| ExecError::Internal {
                message: format!("unknown relation id {}", rel.index()),
            })?;
        let Relation::ManyToMany {
            join_table,
            a_col,
            a_pk,
            b_col,
            b_pk,
            a_ref,
            b_ref,
        } = relation
        else {
            return Err(ExecError::Internal {
                message: "many-to-many link requires a many-to-many relation".to_string(),
            });
        };
        let Some(Field::Ref {
            target, is_list, ..
        }) = self.schema.field(field)
        else {
            return Err(ExecError::Internal {
                message: "link field is not a ref".to_string(),
            });
        };
        if !*is_list {
            return Err(ExecError::Internal {
                message: "many-to-many link field must be a list".to_string(),
            });
        }
        if targets.iter().any(|origin| origin.struct_id != *target) {
            return Err(ExecError::Internal {
                message: "link target struct mismatch".to_string(),
            });
        }

        let owner_is_first = field == a_ref;
        let (owner_col, owner_pk, target_col, target_pk) = if owner_is_first {
            (a_col, a_pk, b_col, b_pk)
        } else if field == b_ref {
            (b_col, b_pk, a_col, a_pk)
        } else {
            return Err(ExecError::Internal {
                message: "link field is not an endpoint of its relation".to_string(),
            });
        };

        let owner_pk = self.fetch_row_key(owner_pk, owner, "link owner")?;
        if mode == LinkMode::Replace {
            self.delete_join_rows(rel, owner_is_first, &[owner])?;
        }
        if targets.is_empty() {
            return Ok(());
        }

        let mut target_keys = self.fetch_row_keys(target_pk, targets, "link target")?;
        if mode == LinkMode::Append {
            let existing = self.fetch_join_values(join_table, owner_col, target_col, &owner_pk)?;
            target_keys.retain(|value| !existing.iter().any(|existing| existing == value));
        }
        self.insert_join_rows(join_table, owner_col, target_col, &owner_pk, &target_keys)
    }

    fn link_direct(
        &mut self,
        rel: RelationId,
        field: FieldId,
        owner: Key,
        targets: &[RowOrigin],
        mode: LinkMode,
    ) -> Result<(), ExecError> {
        let relation =
            self.schema
                .relations
                .get(rel.index())
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown relation id {}", rel.index()),
                })?;
        if self.schema.relation_has_owning_endpoint(relation) {
            return Err(ExecError::Internal {
                message: "link_row_refs requires a non-owning relation".to_string(),
            });
        }
        let (fk, pk) = match relation {
            Relation::OneToOne { fk, pk, .. }
            | Relation::ManyToOne { fk, pk, .. }
            | Relation::OneToMany { fk, pk, .. } => (*fk, *pk),
            Relation::ManyToMany { .. } => {
                return Err(ExecError::Unsupported {
                    message: "many-to-many links are not implemented yet".to_string(),
                });
            }
        };
        let Some(Field::Ref {
            target, is_list, ..
        }) = self.schema.field(field)
        else {
            return Err(ExecError::Internal {
                message: "link field is not a ref".to_string(),
            });
        };
        if targets.iter().any(|origin| origin.struct_id != *target) {
            return Err(ExecError::Internal {
                message: "link target struct mismatch".to_string(),
            });
        }

        if *is_list {
            let owner_pk = self.fetch_row_key(pk, owner, "link owner")?;
            if mode == LinkMode::Replace {
                self.clear_foreign_keys(rel, &[owner])?;
            }
            if targets.is_empty() {
                return Ok(());
            }
            let table = quote(&self.schema.tables[fk.table_id().index()].name);
            let fk_column = quote(self.schema.column_name(fk));
            let txn = self.txn()?;
            for chunk in targets.chunks(PARAM_CHUNK) {
                let placeholders = placeholders(chunk.len());
                let sql =
                    format!("UPDATE {table} SET {fk_column} = ? WHERE _rowid_ IN ({placeholders})");
                let mut params = Vec::with_capacity(chunk.len() + 1);
                params.push(owner_pk.clone());
                params.extend(chunk.iter().map(|target| Value::Integer(target.key.0)));
                let mut stmt = txn
                    .prepare_cached(&sql)
                    .map_err(|err| SqliteBackend::storage_error("link rows (prepare)", err))?;
                let changed = stmt
                    .execute(rusqlite::params_from_iter(params.iter()))
                    .map_err(|err| SqliteBackend::storage_error("link rows (execute)", err))?;
                if changed != chunk.len() {
                    return Err(ExecError::Integrity {
                        message: format!(
                            "link target row was not found ({changed}/{})",
                            chunk.len()
                        ),
                    });
                }
            }
            Ok(())
        } else {
            let table = quote(&self.schema.tables[fk.table_id().index()].name);
            let fk_column = quote(self.schema.column_name(fk));
            match targets {
                [] if mode == LinkMode::Replace => {
                    let sql = format!("UPDATE {table} SET {fk_column} = NULL WHERE _rowid_ = ?");
                    let txn = self.txn()?;
                    let mut stmt = txn.prepare_cached(&sql).map_err(|err| {
                        SqliteBackend::storage_error("clear link row (prepare)", err)
                    })?;
                    let changed = stmt.execute([owner.0]).map_err(|err| {
                        SqliteBackend::storage_error("clear link row (execute)", err)
                    })?;
                    if changed != 1 {
                        return Err(ExecError::Integrity {
                            message: "link owner row was not found".to_string(),
                        });
                    }
                }
                [] => {}
                [target] => {
                    let target_pk = self.fetch_row_key(pk, target.key, "link target")?;
                    let sql = format!("UPDATE {table} SET {fk_column} = ? WHERE _rowid_ = ?");
                    let txn = self.txn()?;
                    let mut stmt = txn
                        .prepare_cached(&sql)
                        .map_err(|err| SqliteBackend::storage_error("link row (prepare)", err))?;
                    let changed = stmt
                        .execute(rusqlite::params_from_iter([
                            target_pk,
                            Value::Integer(owner.0),
                        ]))
                        .map_err(|err| SqliteBackend::storage_error("link row (execute)", err))?;
                    if changed != 1 {
                        return Err(ExecError::Integrity {
                            message: "link owner row was not found".to_string(),
                        });
                    }
                }
                _ => {
                    return Err(ExecError::Internal {
                        message: "single link field received multiple targets".to_string(),
                    });
                }
            };
            Ok(())
        }
    }

    fn delete_array_tree(
        &self,
        storage: &StorageTable,
        element: &ValueType,
        parent_select: &str,
        params: &[Value],
    ) -> Result<(), ExecError> {
        let mut nodes = Vec::new();
        let mut pos = 0;
        collect_top_level_arrays(element, &mut pos, &mut nodes);
        let parent_table = quote(&self.schema.tables[storage.table.index()].name);
        let parent_key = quote(self.schema.column_name(storage.key));
        for (child_storage, child_element, child_pos) in nodes {
            let parent_link = storage
                .value
                .get(child_pos)
                .ok_or_else(|| ExecError::Internal {
                    message: "nested array position has no storage column".to_string(),
                })?;
            let parent_link = quote(self.schema.column_name(*parent_link));
            let nested_select = format!(
                "SELECT p.{parent_link} FROM {parent_table} AS p \
                 WHERE p.{parent_key} IN ({parent_select})"
            );
            self.delete_array_tree(child_storage, child_element, &nested_select, params)?;
        }
        let table = quote(&self.schema.tables[storage.table.index()].name);
        let key = quote(self.schema.column_name(storage.key));
        let sql = format!("DELETE FROM {table} WHERE {key} IN ({parent_select})");
        let txn = self.txn()?;
        let mut stmt = txn
            .prepare_cached(&sql)
            .map_err(|err| SqliteBackend::storage_error("delete array rows (prepare)", err))?;
        stmt.execute(rusqlite::params_from_iter(params.iter()))
            .map_err(|err| SqliteBackend::storage_error("delete array rows (execute)", err))?;
        Ok(())
    }

    fn txn(&self) -> Result<&Transaction<'_>, ExecError> {
        self.txn.as_ref().ok_or_else(|| ExecError::Internal {
            message: "write transaction is already finished".to_string(),
        })
    }
}

fn collect_top_level_arrays<'a>(
    ty: &'a ValueType,
    pos: &mut usize,
    out: &mut Vec<(&'a StorageTable, &'a ValueType, usize)>,
) {
    match ty {
        ValueType::Scalar(_) | ValueType::Enum(_) => *pos += 1,
        ValueType::Tuple(elements) => {
            for element in elements {
                collect_top_level_arrays(element, pos, out);
            }
        }
        ValueType::Optional(inner) => collect_top_level_arrays(inner, pos, out),
        ValueType::Array { element, storage } => {
            out.push((storage, element, *pos));
            *pos += 1;
        }
    }
}
impl<'conn, 'schema> WriteTxn for SqliteWriteTxn<'conn, 'schema> {
    fn insert_row(
        &mut self,
        struct_id: StructId,
        values: &[(FieldId, Val)],
    ) -> Result<Key, ExecError> {
        let table = self.table_name(struct_id)?;
        let (columns, params) = self.encode_row_write(struct_id, values)?;
        self.insert_encoded_row(&table, &columns, &params)
    }

    fn insert_rows(
        &mut self,
        struct_id: StructId,
        rows: &[Vec<(FieldId, Val)>],
    ) -> Result<Vec<Key>, ExecError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let table = self.table_name(struct_id)?;
        let mut columns: Option<Vec<String>> = None;
        let mut encoded_rows = Vec::with_capacity(rows.len());
        for row in rows {
            let (row_columns, params) = self.encode_row_write(struct_id, row)?;
            if let Some(existing) = &columns {
                if existing != &row_columns {
                    return self.insert_rows_individually(struct_id, rows);
                }
            } else {
                columns = Some(row_columns);
            }
            encoded_rows.push(params);
        }
        let columns = columns.expect("rows is non-empty");
        if columns.is_empty() {
            return self.insert_rows_individually(struct_id, rows);
        }
        self.insert_encoded_rows(&table, &columns, &encoded_rows)
    }

    fn insert_row_with_parent(
        &mut self,
        struct_id: StructId,
        values: &[(FieldId, Val)],
        rel: RelationId,
        parent: Key,
        _parent_is_first: bool,
    ) -> Result<Key, ExecError> {
        let (fk, pk) = self.relation_key_columns(rel)?;
        let parent_key = self.fetch_parent_key(pk, parent)?;
        let table = self.table_name(struct_id)?;
        let (mut columns, mut params) = self.encode_row_write(struct_id, values)?;
        columns.push(quote(self.schema.column_name(fk)));
        params.push(parent_key);
        self.insert_encoded_row(&table, &columns, &params)
    }

    fn insert_rows_with_parent(
        &mut self,
        struct_id: StructId,
        rows: &[Vec<(FieldId, Val)>],
        rel: RelationId,
        parent: Key,
        parent_is_first: bool,
    ) -> Result<Vec<Key>, ExecError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let (fk, pk) = self.relation_key_columns(rel)?;
        let parent_key = self.fetch_parent_key(pk, parent)?;
        let table = self.table_name(struct_id)?;
        let fk_column = quote(self.schema.column_name(fk));
        let mut columns: Option<Vec<String>> = None;
        let mut encoded_rows = Vec::with_capacity(rows.len());
        for row in rows {
            let (mut row_columns, mut params) = self.encode_row_write(struct_id, row)?;
            row_columns.push(fk_column.clone());
            params.push(parent_key.clone());
            if let Some(existing) = &columns {
                if existing != &row_columns {
                    return rows
                        .iter()
                        .map(|row| {
                            self.insert_row_with_parent(
                                struct_id,
                                row,
                                rel,
                                parent,
                                parent_is_first,
                            )
                        })
                        .collect();
                }
            } else {
                columns = Some(row_columns);
            }
            encoded_rows.push(params);
        }
        let columns = columns.expect("rows is non-empty");
        self.insert_encoded_rows(&table, &columns, &encoded_rows)
    }

    fn link_row_refs(
        &mut self,
        field: FieldId,
        owner: Key,
        targets: &[RowOrigin],
        mode: LinkMode,
    ) -> Result<(), ExecError> {
        let relation = self
            .schema
            .relation_of_field(field)
            .ok_or_else(|| ExecError::Internal {
                message: "link field has no relation".to_string(),
            })?;
        let mut unique_targets = Vec::with_capacity(targets.len());
        for target in targets {
            if !unique_targets.contains(target) {
                unique_targets.push(*target);
            }
        }
        if matches!(
            self.schema.relations.get(relation.index()),
            Some(Relation::ManyToMany { .. })
        ) {
            self.link_many_to_many(relation, field, owner, &unique_targets, mode)
        } else {
            self.link_direct(relation, field, owner, &unique_targets, mode)
        }
    }

    fn update_row(
        &mut self,
        struct_id: StructId,
        key: Key,
        values: &[(FieldId, Val)],
    ) -> Result<(), ExecError> {
        let table = self.table_name(struct_id)?;
        let (columns, mut params) = self.encode_row_write(struct_id, values)?;
        if columns.is_empty() {
            return Ok(());
        }
        let assignments = columns
            .iter()
            .map(|column| format!("{column} = ?"))
            .collect::<Vec<_>>()
            .join(", ");
        params.push(Value::Integer(key.0));
        let sql = format!("UPDATE {table} SET {assignments} WHERE _rowid_ = ?");
        let txn = self.txn()?;
        let mut stmt = txn
            .prepare_cached(&sql)
            .map_err(|err| SqliteBackend::storage_error("update row (prepare)", err))?;
        stmt.execute(rusqlite::params_from_iter(params.iter()))
            .map_err(|err| SqliteBackend::storage_error("update row (execute)", err))?;
        Ok(())
    }

    fn delete_rows(&mut self, struct_id: StructId, keys: &[Key]) -> Result<(), ExecError> {
        if keys.is_empty() {
            return Ok(());
        }
        let table = self.table_name(struct_id)?;
        let txn = self.txn()?;
        for chunk in keys.chunks(PARAM_CHUNK) {
            let placeholders = placeholders(chunk.len());
            let sql = format!("DELETE FROM {table} WHERE _rowid_ IN ({placeholders})");
            let params = chunk.iter().map(|key| Value::Integer(key.0));
            let mut stmt = txn
                .prepare_cached(&sql)
                .map_err(|err| SqliteBackend::storage_error("delete rows (prepare)", err))?;
            stmt.execute(rusqlite::params_from_iter(params))
                .map_err(|err| SqliteBackend::storage_error("delete rows (execute)", err))?;
        }
        Ok(())
    }

    fn clear_foreign_keys(&mut self, rel: RelationId, referenced: &[Key]) -> Result<(), ExecError> {
        if referenced.is_empty() {
            return Ok(());
        }
        let relation =
            self.schema
                .relations
                .get(rel.index())
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown relation id {}", rel.index()),
                })?;
        let (fk, pk) = match relation {
            Relation::OneToOne { fk, pk, .. }
            | Relation::ManyToOne { fk, pk, .. }
            | Relation::OneToMany { fk, pk, .. } => (*fk, *pk),
            Relation::ManyToMany { .. } => {
                return Err(ExecError::Internal {
                    message: "many-to-many relations do not use FK cleanup".to_string(),
                });
            }
        };
        let referencing_table = quote(&self.schema.tables[fk.table_id().index()].name);
        let fk = quote(self.schema.column_name(fk));
        let referenced_table = quote(&self.schema.tables[pk.table_id().index()].name);
        let pk = quote(self.schema.column_name(pk));
        let txn = self.txn()?;
        for chunk in referenced.chunks(PARAM_CHUNK) {
            let placeholders = placeholders(chunk.len());
            let sql = format!(
                "UPDATE {referencing_table} SET {fk} = NULL WHERE {fk} IN \
                 (SELECT {pk} FROM {referenced_table} WHERE _rowid_ IN ({placeholders}))"
            );
            let params = chunk.iter().map(|key| Value::Integer(key.0));
            let mut stmt = txn
                .prepare_cached(&sql)
                .map_err(|err| SqliteBackend::storage_error("clear FK (prepare)", err))?;
            stmt.execute(rusqlite::params_from_iter(params))
                .map_err(|err| SqliteBackend::storage_error("clear FK (execute)", err))?;
        }
        Ok(())
    }

    fn delete_join_rows(
        &mut self,
        rel: RelationId,
        endpoint_is_first: bool,
        keys: &[Key],
    ) -> Result<(), ExecError> {
        if keys.is_empty() {
            return Ok(());
        }
        let relation =
            self.schema
                .relations
                .get(rel.index())
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown relation id {}", rel.index()),
                })?;
        let Relation::ManyToMany {
            join_table,
            a_col,
            a_pk,
            b_col,
            b_pk,
            ..
        } = relation
        else {
            return Err(ExecError::Internal {
                message: "join cleanup requires a many-to-many relation".to_string(),
            });
        };
        let (join_col, pk) = if endpoint_is_first {
            (*a_col, *a_pk)
        } else {
            (*b_col, *b_pk)
        };
        let join_table = quote(&self.schema.tables[join_table.index()].name);
        let join_col = quote(self.schema.column_name(join_col));
        let pk_table = quote(&self.schema.tables[pk.table_id().index()].name);
        let pk = quote(self.schema.column_name(pk));
        let txn = self.txn()?;
        for chunk in keys.chunks(PARAM_CHUNK) {
            let placeholders = placeholders(chunk.len());
            let sql = format!(
                "DELETE FROM {join_table} WHERE {join_col} IN \
                 (SELECT {pk} FROM {pk_table} WHERE _rowid_ IN ({placeholders}))"
            );
            let params = chunk.iter().map(|key| Value::Integer(key.0));
            let mut stmt = txn
                .prepare_cached(&sql)
                .map_err(|err| SqliteBackend::storage_error("delete join rows (prepare)", err))?;
            stmt.execute(rusqlite::params_from_iter(params))
                .map_err(|err| SqliteBackend::storage_error("delete join rows (execute)", err))?;
        }
        Ok(())
    }

    fn delete_array_field(&mut self, field_id: FieldId, keys: &[Key]) -> Result<(), ExecError> {
        if keys.is_empty() {
            return Ok(());
        }
        let struct_id = field_id.struct_id();
        let struct_def =
            self.schema
                .structs
                .get(struct_id.index())
                .ok_or_else(|| ExecError::Internal {
                    message: format!("unknown struct id {}", struct_id.index()),
                })?;
        let field = struct_def
            .fields
            .get(field_id.local())
            .ok_or_else(|| ExecError::Internal {
                message: format!("unknown field {field_id:?}"),
            })?;
        let table = quote(&self.schema.tables[struct_def.table.index()].name);
        match field {
            Field::Array {
                link,
                element,
                storage,
                ..
            } => {
                for chunk in keys.chunks(PARAM_CHUNK) {
                    let placeholders = placeholders(chunk.len());
                    let link = quote(self.schema.column_name(*link));
                    let parent_select = format!(
                        "SELECT o.{link} FROM {table} AS o WHERE o._rowid_ IN ({placeholders})"
                    );
                    let params: Vec<Value> =
                        chunk.iter().map(|key| Value::Integer(key.0)).collect();
                    self.delete_array_tree(storage, element, &parent_select, &params)?;
                }
            }
            Field::Value { ty, columns, .. } if ty.has_array() => {
                let mut nodes = Vec::new();
                let mut pos = 0;
                collect_top_level_arrays(ty, &mut pos, &mut nodes);
                for chunk in keys.chunks(PARAM_CHUNK) {
                    let placeholders = placeholders(chunk.len());
                    let params: Vec<Value> =
                        chunk.iter().map(|key| Value::Integer(key.0)).collect();
                    for (storage, element, node_pos) in &nodes {
                        let link = columns.get(*node_pos).ok_or_else(|| ExecError::Internal {
                            message: "array position has no owner column".to_string(),
                        })?;
                        let link = quote(self.schema.column_name(*link));
                        let parent_select = format!(
                            "SELECT o.{link} FROM {table} AS o WHERE o._rowid_ IN ({placeholders})"
                        );
                        self.delete_array_tree(storage, element, &parent_select, &params)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<(), ExecError> {
        let txn = self.txn.take().ok_or_else(|| ExecError::Internal {
            message: "write transaction is already finished".to_string(),
        })?;
        txn.commit()
            .map_err(|err| SqliteBackend::storage_error("commit write transaction", err))
    }

    fn rollback(&mut self) -> Result<(), ExecError> {
        let txn = self.txn.take().ok_or_else(|| ExecError::Internal {
            message: "write transaction is already finished".to_string(),
        })?;
        txn.rollback()
            .map_err(|err| SqliteBackend::storage_error("rollback write transaction", err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::tests::*;
    use crate::storage::{FetchConstraints, Storage};
    use rusqlite::Connection;

    fn write_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(
                r#"
                    root items: Item;
                    struct Item {
                        id: Int,
                        name: String,
                        maybe: ?Int,
                    }
                    "#,
            )
            .0
            .expect("invalid write schema"),
        )
        .0
        .expect("invalid write schema")
    }

    fn write_item_fields(schema: &ValidatedSchema) -> (StructId, FieldId, FieldId, FieldId) {
        let item = schema.roots[0].struct_id;
        (
            item,
            schema.struct_field(item, "id").unwrap().0,
            schema.struct_field(item, "name").unwrap().0,
            schema.struct_field(item, "maybe").unwrap().0,
        )
    }

    fn write_test_backend(schema: &ValidatedSchema) -> Result<SqliteBackend<'_>, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("CREATE TABLE items(id INTEGER, name TEXT, maybe INTEGER)")?;
        Ok(SqliteBackend::new(conn, schema).expect("write test backend should pass verification"))
    }

    #[test]
    fn write_txn_insert_commit_read() {
        let schema = write_schema();
        let backend = write_test_backend(&schema).unwrap();
        let (item, id, name, maybe) = write_item_fields(&schema);

        let mut txn = backend.begin_write().unwrap();
        let key = txn
            .insert_row(
                item,
                &[(id, int(1)), (name, str("alpha")), (maybe, Val::None)],
            )
            .unwrap();
        txn.commit().unwrap();

        let rows = backend
            .fetch(item, &[id, name, maybe], &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rowid, key);
        assert_eq!(rows[0].values, vec![int(1), str("alpha"), Val::None]);
    }

    #[test]
    fn write_txn_drop_rollback() {
        let schema = write_schema();
        let backend = write_test_backend(&schema).unwrap();
        let (item, id, name, maybe) = write_item_fields(&schema);

        let mut txn = backend.begin_write().unwrap();
        txn.insert_row(
            item,
            &[(id, int(1)), (name, str("alpha")), (maybe, Val::None)],
        )
        .unwrap();
        drop(txn);

        let rows = backend
            .fetch(item, &[id, name, maybe], &FetchConstraints::EMPTY)
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn write_txn_update_delete_batch_insert() {
        let schema = write_schema();
        let backend = write_test_backend(&schema).unwrap();
        let (item, id, name, maybe) = write_item_fields(&schema);

        let mut txn = backend.begin_write().unwrap();
        let keys = txn
            .insert_rows(
                item,
                &[
                    vec![(id, int(1)), (name, str("alpha")), (maybe, Val::None)],
                    vec![(id, int(2)), (name, str("beta")), (maybe, Val::None)],
                ],
            )
            .unwrap();
        assert_eq!(keys.len(), 2);

        txn.update_row(item, keys[0], &[(name, str("gamma"))])
            .unwrap();
        txn.commit().unwrap();

        let rows = backend
            .fetch(item, &[id, name, maybe], &FetchConstraints::EMPTY)
            .unwrap();
        assert_eq!(rows[0].values, vec![int(1), str("gamma"), Val::None]);
        assert_eq!(rows[1].values, vec![int(2), str("beta"), Val::None]);

        let mut txn = backend.begin_write().unwrap();
        txn.delete_rows(item, &keys).unwrap();
        txn.commit().unwrap();

        let rows = backend
            .fetch(item, &[id, name, maybe], &FetchConstraints::EMPTY)
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn write_txn_readonly_connection_rejected() {
        let schema = write_schema();
        let path = std::env::temp_dir().join(format!(
            "grove-write-txn-readonly-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE items(id INTEGER, name TEXT, maybe INTEGER)")
                .unwrap();
        }
        let conn =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let backend = SqliteBackend::new(conn, &schema).unwrap();
        assert!(matches!(
            backend.begin_write(),
            Err(ExecError::ReadOnly { .. })
        ));
        drop(backend);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_txn_delete_array_field_recursive() {
        let schema = grove_schema::validate(
            grove_schema::parse_schema(
                "root rows: Row;
                struct Row {
                    id: Int,
                    tags: List<String>@tags_ref via row_tags[row_key, tag],
                    nums: List<Int>@nums_ref via row_nums[row_key, num],
                    matrix: List<List<Int> via inner_nums[inner_key, val]>@matrix_ref
                        via outer_nums[outer_key, inner_key],
                    extra: (Int, List<String> via extra_tags[ekey, eval])@(eid, elink),
                }",
            )
            .0
            .unwrap(),
        )
        .0
        .unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE rows(id INTEGER, tags_ref INTEGER, nums_ref INTEGER,
                matrix_ref INTEGER, eid INTEGER, elink INTEGER);
            CREATE TABLE row_tags(row_key INTEGER, tag TEXT);
            CREATE TABLE row_nums(row_key INTEGER, num INTEGER);
            CREATE TABLE outer_nums(outer_key INTEGER, inner_key INTEGER);
            CREATE TABLE inner_nums(inner_key INTEGER, val INTEGER);
            CREATE TABLE extra_tags(ekey INTEGER, eval TEXT);
            INSERT INTO rows VALUES (1, 100, 110, 200, 7, 300);
            INSERT INTO row_tags VALUES (100, 'a'), (100, 'b');
            INSERT INTO row_nums VALUES (110, 1), (110, 2);
            INSERT INTO outer_nums VALUES (200, 20);
            INSERT INTO inner_nums VALUES (20, 1), (20, 2);
            INSERT INTO extra_tags VALUES (300, 'x');",
        )
        .unwrap();
        let backend = SqliteBackend::new(conn, &schema).unwrap();
        let row = schema.roots[0].struct_id;
        let tags = schema.struct_field(row, "tags").unwrap().0;
        let nums = schema.struct_field(row, "nums").unwrap().0;
        let matrix = schema.struct_field(row, "matrix").unwrap().0;
        let extra = schema.struct_field(row, "extra").unwrap().0;

        let mut txn = backend.begin_write().unwrap();
        for field in [tags, nums, matrix, extra] {
            txn.delete_array_field(field, &[Key(1)]).unwrap();
        }
        txn.commit().unwrap();

        for table in [
            "row_tags",
            "row_nums",
            "outer_nums",
            "inner_nums",
            "extra_tags",
        ] {
            let count: i64 = backend
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "{table}");
        }
    }
}
