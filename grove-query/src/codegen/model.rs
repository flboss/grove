
use std::collections::HashMap;

pub use grove_schema::validated::ScalarType;

use super::builder::SelectBuilder;

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledQuery {
    pub statements: Vec<PlannedStatement>,
    pub result: ResultShape,
    pub literals: HashMap<String, Vec<Vec<ParamValue>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedStatement {
    pub sql: String,
    pub params: Vec<Param>,
    pub shape: StatementShape,
    pub kind: StatementKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatementKind {
    Query,
    Temp { name: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SunkStatement {
    pub builder: SelectBuilder,
    pub shape: StatementShape,
    pub kind: StatementKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Value(ParamValue),
    FromPrior { stmt: usize, column: String },
    FromPriorKeys { stmt: usize, column: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    Int(i64),
    Float(f64),
    Dec(rust_decimal::Decimal),
    String(String),
    Bool(bool),
    Instant(chrono::DateTime<chrono::Utc>),
    Duration(chrono::TimeDelta),
    Null,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatementShape {
    pub columns: Vec<OutputColumn>,
    pub rows: RowCount,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputColumn {
    pub name: String,
    pub role: ColumnRole,
    pub ty: Option<ScalarType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnRole {
    Key,
    Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowCount {
    One,
    ZeroOrOne,
    Many,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResultShape {
    Column(String),
    Tuple(Vec<ResultShape>),
    Record(Vec<(String, ResultShape)>),
    List {
        element: Box<ResultShape>,
        child: ChildRef,
    },
    Single {
        element: Box<ResultShape>,
        child: ChildRef,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChildRef {
    pub stmt: usize,
    pub parent: Option<ParentRef>,
    pub cardinality: Cardinality,
    pub tag: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParentRef {
    pub stmt: usize,
    pub parent_key: String,
    pub child_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    One,
    Many,
}

pub const CROSS_PAIR_TAG: &str = "$$ptag";
