
use std::collections::HashMap;

use grove_query::codegen::model::{
    ChildRef, CompiledQuery, ParentRef, PlannedStatement, ResultShape, RowCount, StatementKind,
};

use super::Runtime;
use super::decode::decode_value;
use super::error::RuntimeError;
use super::params::encode_params;
use super::temp;
use super::tvf;
use super::value::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub values: Vec<(String, Value)>,
}

impl Row {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.values.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatementOutput {
    pub rows: Vec<Row>,
}

impl Runtime {
    pub fn execute(&mut self, query: &CompiledQuery) -> Result<Vec<StatementOutput>, RuntimeError> {
        validate_literals(query)?;
        self.register_lits(query)?;
        self.swap_lit_grids(query);
        let temps: Vec<String> = query
            .statements
            .iter()
            .filter_map(|stmt| match &stmt.kind {
                StatementKind::Temp { name } => Some(name.clone()),
                StatementKind::Query => None,
            })
            .collect();
        temp::preamble(self.conn(), &temps)?;
        let _guard = temp::TempGuard::new(self.conn(), temps);
        let parents = collect_parents(&query.result);
        let mut outputs: Vec<StatementOutput> = Vec::with_capacity(query.statements.len());
        for (index, stmt) in query.statements.iter().enumerate() {
            self.fill(index, parents.get(&index), &outputs)?;
            outputs.push(self.run(index, stmt)?);
        }
        Ok(outputs)
    }

    fn fill(
        &self,
        index: usize,
        refs: Option<&Vec<ParentRef>>,
        outputs: &[StatementOutput],
    ) -> Result<(), RuntimeError> {
        let mut slot = self.bulk.lock().unwrap();
        let Some(refs) = refs else {
            slot.clear();
            return Ok(());
        };
        let mut keys = Vec::new();
        for pref in refs {
            let parent = outputs.get(pref.stmt).ok_or_else(|| {
                RuntimeError::Contract(format!(
                    "statement {index}: parent output {} missing",
                    pref.stmt
                ))
            })?;
            for row in &parent.rows {
                let value = row.get(&pref.parent_key).ok_or_else(|| {
                    RuntimeError::Contract(format!(
                        "statement {index}: parent key {} missing",
                        pref.parent_key
                    ))
                })?;
                match value {
                    Value::Null => {}
                    Value::Int(n) => keys.push(*n),
                    other => {
                        return Err(RuntimeError::Contract(format!(
                            "statement {index}: key column holds {other:?}, not an id"
                        )));
                    }
                }
            }
        }
        *slot = keys;
        Ok(())
    }

    fn run(&self, index: usize, stmt: &PlannedStatement) -> Result<StatementOutput, RuntimeError> {
        let mut prepared = self
            .conn()
            .prepare(&stmt.sql)
            .map_err(|err| RuntimeError::Prepare {
                index,
                message: err.to_string(),
            })?;
        let values = encode_params(&stmt.params)?;
        let refs: Vec<&dyn rusqlite::ToSql> =
            values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        match &stmt.kind {
            StatementKind::Query => {
                let rows = collect(index, &mut prepared, &refs, &stmt.shape.columns)?;
                check_rows(index, &stmt.shape.rows, &rows);
                Ok(StatementOutput { rows })
            }
            StatementKind::Temp { name } => {
                prepared
                    .execute(refs.as_slice())
                    .map_err(|err| RuntimeError::step_error(index, err))?;
                let mut readback =
                    self.conn()
                        .prepare(&temp::readback_sql(name))
                        .map_err(|err| RuntimeError::Step {
                            index,
                            message: format!("temp readback for {name}: {err}"),
                        })?;
                let rows = collect(index, &mut readback, &[], &stmt.shape.columns)?;
                check_rows(index, &stmt.shape.rows, &rows);
                Ok(StatementOutput { rows })
            }
        }
    }

    fn register_lits(&mut self, query: &CompiledQuery) -> Result<(), RuntimeError> {
        let mut needed = Vec::new();
        for stmt in &query.statements {
            for module in tvf::lit_modules_in(&stmt.sql) {
                if !self.lit_modules.contains(&module) && !needed.contains(&module) {
                    needed.push(module);
                }
            }
        }
        if needed.is_empty() {
            return Ok(());
        }
        tvf::register_lit_modules(self.conn(), &needed, self.lit.clone())
            .map_err(|err| RuntimeError::Contract(format!("literal module registration: {err}")))?;
        self.lit_modules.extend(needed);
        Ok(())
    }

    fn swap_lit_grids(&self, query: &CompiledQuery) {
        let mut state = self.lit.lock().unwrap();
        state.clear();
        for (id, grid) in &query.literals {
            state.insert(
                id.clone(),
                grid.iter()
                    .map(|row| row.iter().map(super::decode::encode_param).collect())
                    .collect(),
            );
        }
    }
}

fn collect_parents(result: &ResultShape) -> HashMap<usize, Vec<ParentRef>> {
    fn walk(shape: &ResultShape, out: &mut HashMap<usize, Vec<ParentRef>>) {
        match shape {
            ResultShape::Column(_) => {}
            ResultShape::Tuple(elements) => {
                for e in elements {
                    walk(e, out);
                }
            }
            ResultShape::Record(fields) => {
                for (_, e) in fields {
                    walk(e, out);
                }
            }
            ResultShape::List { element, child } | ResultShape::Single { element, child } => {
                walk(element, out);
                record_child(child, out);
            }
        }
    }
    fn record_child(child: &ChildRef, out: &mut HashMap<usize, Vec<ParentRef>>) {
        if let Some(parent) = child.parent.clone() {
            out.entry(child.stmt).or_default().push(parent);
        }
    }
    let mut out = HashMap::new();
    walk(result, &mut out);
    out
}

fn validate_literals(query: &CompiledQuery) -> Result<(), RuntimeError> {
    use grove_query::codegen::model::ParamValue;
    let contract = |msg: String| RuntimeError::Contract(msg);
    for stmt in &query.statements {
        for (module, id) in tvf::lit_calls_in(&stmt.sql) {
            let Some(grid) = query.literals.get(&id) else {
                return Err(contract(format!(
                    "unknown literal id {id} (module {module})"
                )));
            };
            let Some((arity, sig)) = tvf::module_shape(&module) else {
                return Err(contract(format!("bad literal module {module}")));
            };
            for (ri, row) in grid.iter().enumerate() {
                if row.len() != arity {
                    return Err(contract(format!(
                        "literal {id} row {ri}: {} values for arity-{arity} {module}",
                        row.len()
                    )));
                }
                for (value, letter) in row.iter().zip(sig.iter()) {
                    let actual = match value {
                        ParamValue::Int(_) | ParamValue::Bool(_) => b'I',
                        ParamValue::Float(_) => b'F',
                        ParamValue::String(_) => b'T',
                        ParamValue::Dec(_) | ParamValue::Instant(_) | ParamValue::Duration(_) => {
                            b'B'
                        }
                        ParamValue::Null => continue,
                    };
                    if actual != *letter {
                        return Err(contract(format!(
                            "literal {id} row {ri}: {value:?} under sig letter {}",
                            *letter as char
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

fn collect(
    index: usize,
    stmt: &mut rusqlite::Statement<'_>,
    params: &[&dyn rusqlite::ToSql],
    columns: &[grove_query::codegen::model::OutputColumn],
) -> Result<Vec<Row>, RuntimeError> {
    let _width = stmt.column_count();
    let mut rows = stmt.query(params).map_err(|err| RuntimeError::Bind {
        index,
        message: err.to_string(),
    })?;
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|err| RuntimeError::step_error(index, err))?
    {
        debug_assert!(
            _width >= columns.len(),
            "statement {index}: fewer columns than the shape needs"
        );
        let mut values = Vec::with_capacity(columns.len());
        for col in columns {
            let raw = row.get_ref(col.name.as_str()).map_err(|_| {
                RuntimeError::Contract(format!(
                    "statement {index}: shape column {} missing from query output",
                    col.name
                ))
            })?;
            values.push((
                col.name.clone(),
                decode_value(raw, &col.name, col.ty)?,
            ));
        }
        out.push(Row { values });
    }
    Ok(out)
}

fn check_rows(index: usize, expected: &RowCount, rows: &[Row]) {
    match expected {
        RowCount::One => debug_assert_eq!(
            rows.len(),
            1,
            "statement {index}: One row expected, got {}",
            rows.len()
        ),
        RowCount::ZeroOrOne => debug_assert!(
            rows.len() <= 1,
            "statement {index}: at most one row expected, got {}",
            rows.len()
        ),
        RowCount::Many => {}
    }
}
