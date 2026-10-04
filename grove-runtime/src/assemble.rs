
use std::collections::HashMap;
use std::hash::Hash;

use chrono::{DateTime, TimeDelta, Utc};
use grove_query::codegen::model::{
    CROSS_PAIR_TAG, Cardinality, ChildRef, CompiledQuery, ResultShape, StatementKind,
};
use rust_decimal::Decimal;

use super::Runtime;
use super::error::RuntimeError;
use super::exec::{Row, StatementOutput};
use super::value::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Int(i64),
    Float(u64),
    Bool(bool),
    Str(String),
    Dec(Decimal),
    Instant(DateTime<Utc>),
    Duration(TimeDelta),
}

fn key_of(value: &Value) -> Result<Option<Key>, RuntimeError> {
    match value {
        Value::Null => Ok(None),
        Value::Int(n) => Ok(Some(Key::Int(*n))),
        Value::Float(f) => Ok(Some(Key::Float(f.to_bits()))),
        Value::Bool(b) => Ok(Some(Key::Bool(*b))),
        Value::String(s) => Ok(Some(Key::Str(s.clone()))),
        Value::Dec(d) => Ok(Some(Key::Dec(*d))),
        Value::Instant(t) => Ok(Some(Key::Instant(*t))),
        Value::Duration(d) => Ok(Some(Key::Duration(*d))),
        other => Err(RuntimeError::Contract(format!(
            "non-scalar key value: {other:?}"
        ))),
    }
}

impl Runtime {
    pub fn execute_value(&mut self, query: &CompiledQuery) -> Result<Value, RuntimeError> {
        let outputs = self.execute(query)?;
        assemble(query, &outputs)
    }
}

pub fn assemble(query: &CompiledQuery, outputs: &[StatementOutput]) -> Result<Value, RuntimeError> {
    let root_idx = query
        .statements
        .iter()
        .take_while(|s| matches!(s.kind, StatementKind::Temp { .. }))
        .count();
    let root_rows = outputs.get(root_idx).ok_or_else(|| {
        RuntimeError::Contract(format!("root statement {root_idx} has no output"))
    })?;
    match &query.result {
        ResultShape::Column(name) => {
            if outputs.len() != 1 || root_rows.rows.len() != 1 {
                return Err(RuntimeError::Contract(format!(
                    "scalar root yielded {} rows in {} statements",
                    root_rows.rows.len(),
                    outputs.len()
                )));
            }
            root_rows.rows[0]
                .get(name)
                .cloned()
                .ok_or_else(|| RuntimeError::Contract(format!("scalar root column {name} missing")))
        }
        ResultShape::Tuple(_) | ResultShape::Record(_) => {
            if root_rows.rows.len() != 1 {
                return Err(RuntimeError::Contract(format!(
                    "composite root yielded {} rows",
                    root_rows.rows.len()
                )));
            }
            assemble_shape(&query.result, outputs, Some(&root_rows.rows[0]))
        }
        shape @ (ResultShape::List { .. } | ResultShape::Single { .. }) => {
            assemble_shape(shape, outputs, None)
        }
    }
}

fn assemble_shape(
    shape: &ResultShape,
    outputs: &[StatementOutput],
    parent: Option<&Row>,
) -> Result<Value, RuntimeError> {
    match shape {
        ResultShape::Column(name) => parent
            .and_then(|row| row.get(name))
            .cloned()
            .ok_or_else(|| RuntimeError::Contract(format!("column {name} has no parent row"))),
        ResultShape::Tuple(elements) => elements
            .iter()
            .map(|e| assemble_shape(e, outputs, parent))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Tuple),
        ResultShape::Record(fields) => fields
            .iter()
            .map(|(name, e)| assemble_shape(e, outputs, parent).map(|v| (name.clone(), v)))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Record),
        ResultShape::List { element, child } => assemble_list(element, child, outputs, parent),
        ResultShape::Single { element, child } => assemble_single(element, child, outputs, parent),
    }
}

type GroupedRows = (HashMap<Key, Vec<usize>>, Vec<usize>);

fn group_by(rows: &[Row], key_col: &str, child_stmt: usize) -> Result<GroupedRows, RuntimeError> {
    let mut groups: HashMap<Key, Vec<usize>> = HashMap::new();
    let mut broadcast: Vec<usize> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        match row.get(CROSS_PAIR_TAG) {
            Some(&Value::Int(1)) => {
                broadcast.push(i);
                continue;
            }
            Some(&Value::Int(0)) | None => {}
            Some(other) => {
                return Err(RuntimeError::Contract(format!(
                    "statement {child_stmt}: pair tag holds {other:?}, want 0 or 1"
                )));
            }
        }
        let value = row.get(key_col).ok_or_else(|| {
            RuntimeError::Contract(format!(
                "statement {child_stmt}: key column {key_col} missing"
            ))
        })?;
        if let Some(key) = key_of(value)? {
            groups.entry(key).or_default().push(i);
        }
    }
    Ok((groups, broadcast))
}

fn check_tag(child: &ChildRef) -> Result<(), RuntimeError> {
    if let Some(tag) = &child.tag {
        return Err(RuntimeError::Contract(format!(
            "branch tag {tag} has no lane yet"
        )));
    }
    Ok(())
}

fn child_rows<'o>(
    outputs: &'o [StatementOutput],
    child: &ChildRef,
) -> Result<&'o [Row], RuntimeError> {
    outputs
        .get(child.stmt)
        .map(|o| o.rows.as_slice())
        .ok_or_else(|| {
            RuntimeError::Contract(format!("child statement {} has no output", child.stmt))
        })
}

fn assemble_list(
    element: &ResultShape,
    child: &ChildRef,
    outputs: &[StatementOutput],
    parent: Option<&Row>,
) -> Result<Value, RuntimeError> {
    check_tag(child)?;
    let rows = child_rows(outputs, child)?;
    match &child.parent {
        None => rows
            .iter()
            .map(|row| assemble_shape(element, outputs, Some(row)))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        Some(pref) => {
            let (groups, broadcast) = group_by(rows, &pref.child_key, child.stmt)?;
            let one = matches!(child.cardinality, Cardinality::One);
            let matched = |key: &Value| -> Result<Vec<Value>, RuntimeError> {
                let mut hits = if pref.negated {
                    let own = key_of(key)?;
                    let mut hits = Vec::new();
                    for (i, row) in rows.iter().enumerate() {
                        if row.get(CROSS_PAIR_TAG) == Some(&Value::Int(1)) {
                            continue;
                        }
                        let value = row.get(&pref.child_key).ok_or_else(|| {
                            RuntimeError::Contract(format!(
                                "statement {}: key column {} missing",
                                child.stmt, pref.child_key
                            ))
                        })?;
                        match key_of(value)? {
                            Some(k) => {
                                if Some(&k) != own.as_ref() {
                                    hits.push(i);
                                }
                            }
                            None => hits.push(i),
                        }
                    }
                    hits
                } else {
                    match key_of(key)? {
                        Some(key) => groups.get(&key).cloned().unwrap_or_default(),
                        None => Vec::new(),
                    }
                };
                hits.extend(broadcast.iter().cloned());
                if one {
                    hits.truncate(1);
                }
                hits.iter()
                    .map(|&i| assemble_shape(element, outputs, Some(&rows[i])))
                    .collect()
            };
            match parent {
                Some(prow) => {
                    let key = prow.get(&pref.parent_key).ok_or_else(|| {
                        RuntimeError::Contract(format!("parent key {} missing", pref.parent_key))
                    })?;
                    matched(key).map(Value::List)
                }
                None => {
                    let parents = outputs
                        .get(pref.stmt)
                        .map(|o| o.rows.as_slice())
                        .ok_or_else(|| {
                            RuntimeError::Contract(format!(
                                "parent statement {} has no output",
                                pref.stmt
                            ))
                        })?;
                    if matches!(
                        element,
                        ResultShape::List { .. } | ResultShape::Single { .. }
                    ) {
                        return parents
                            .iter()
                            .map(|prow| assemble_shape(element, outputs, Some(prow)))
                            .collect::<Result<Vec<_>, _>>()
                            .map(Value::List);
                    }
                    let mut all = Vec::new();
                    for prow in parents {
                        let key = prow.get(&pref.parent_key).ok_or_else(|| {
                            RuntimeError::Contract(format!(
                                "parent key {} missing",
                                pref.parent_key
                            ))
                        })?;
                        all.extend(matched(key)?);
                    }
                    Ok(Value::List(all))
                }
            }
        }
    }
}

fn assemble_single(
    element: &ResultShape,
    child: &ChildRef,
    outputs: &[StatementOutput],
    parent: Option<&Row>,
) -> Result<Value, RuntimeError> {
    check_tag(child)?;
    let rows = child_rows(outputs, child)?;
    let first = |i: usize| assemble_shape(element, outputs, Some(&rows[i]));
    match &child.parent {
        None => match rows.first() {
            Some(_) => first(0),
            None => Ok(Value::Null),
        },
        Some(pref) => {
            let (groups, broadcast) = group_by(rows, &pref.child_key, child.stmt)?;
            let lookup = |key: &Value| -> Result<Value, RuntimeError> {
                if pref.negated {
                    let own = key_of(key)?;
                    for (i, row) in rows.iter().enumerate() {
                        if row.get(CROSS_PAIR_TAG) == Some(&Value::Int(1)) {
                            continue;
                        }
                        let value = row.get(&pref.child_key).ok_or_else(|| {
                            RuntimeError::Contract(format!(
                                "statement {}: key column {} missing",
                                child.stmt, pref.child_key
                            ))
                        })?;
                        let keep = match key_of(value)? {
                            Some(k) => Some(&k) != own.as_ref(),
                            None => true,
                        };
                        if keep {
                            return first(i);
                        }
                    }
                    return match broadcast.first() {
                        Some(&i) => first(i),
                        None => Ok(Value::Null),
                    };
                }
                let keyed = match key_of(key)? {
                    Some(key) => groups.get(&key).and_then(|hits| hits.first()),
                    None => None,
                };
                match keyed.or_else(|| broadcast.first()) {
                    Some(&i) => first(i),
                    None => Ok(Value::Null),
                }
            };
            match parent {
                Some(prow) => {
                    let key = prow.get(&pref.parent_key).ok_or_else(|| {
                        RuntimeError::Contract(format!("parent key {} missing", pref.parent_key))
                    })?;
                    lookup(key)
                }
                None => {
                    let parents = outputs
                        .get(pref.stmt)
                        .map(|o| o.rows.as_slice())
                        .ok_or_else(|| {
                            RuntimeError::Contract(format!(
                                "parent statement {} has no output",
                                pref.stmt
                            ))
                        })?;
                    for prow in parents {
                        let key = prow.get(&pref.parent_key).ok_or_else(|| {
                            RuntimeError::Contract(format!(
                                "parent key {} missing",
                                pref.parent_key
                            ))
                        })?;
                        let value = lookup(key)?;
                        if !value.is_null() {
                            return Ok(value);
                        }
                    }
                    Ok(Value::Null)
                }
            }
        }
    }
}
