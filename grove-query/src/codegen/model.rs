#[derive(Debug, Clone, PartialEq)]
pub struct CompiledQuery {
    pub statements: Vec<PlannedStatement>,
    pub result: ResultShape,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedStatement {
    pub sql: String,
    pub params: Vec<Param>,
    pub shape: StatementShape,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Value(ParamValue),
    FromPrior { stmt: usize, column: String },
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputColumn {
    pub name: String,
    pub role: ColumnRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnRole {
    Key,
    Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResultShape {
    Rows { columns: Vec<String> },
}
