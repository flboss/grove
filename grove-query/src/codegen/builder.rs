use grove_schema::validated::ScalarType;

use super::model::ParamValue;

#[derive(Debug, Clone, PartialEq)]
pub struct SelectBuilder {
    pub select: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub where_: Vec<SqlExpr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<SqlExpr>,
    pub offset: Option<SqlExpr>,
    pub unions: Vec<SelectBuilder>,
}

impl SelectBuilder {
    pub fn new() -> Self {
        SelectBuilder {
            select: Vec::new(),
            from: None,
            where_: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            unions: Vec::new(),
        }
    }
}

impl Default for SelectBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    AllFrom {
        depth: usize,
    },
    Column {
        depth: usize,
        column: String,
        output: String,
        ty: Option<ScalarType>,
    },
    Expr {
        expr: SqlExpr,
        output: String,
        ty: Option<ScalarType>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct NamedColumn {
    pub name: String,
    pub ty: Option<ScalarType>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FromClause {
    Table {
        table: String,
        alias: String,
    },
    Subquery {
        query: Box<SelectBuilder>,
        alias: String,
    },
    TableFunction {
        name: String,
    },
    Temp {
        name: String,
        columns: Vec<NamedColumn>,
    },
    Literal {
        module: String,
        id: String,
        columns: Vec<NamedColumn>,
        middle_parts: Option<Vec<MiddlePart>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum MiddlePart {
    Value {
        name: Option<String>,
        col: String,
    },
    Nested {
        name: Option<String>,
    },
    Tuple(Vec<MiddlePart>),
    Record {
        name: Option<String>,
        fields: Vec<MiddlePart>,
    },
}

impl FromClause {
    pub fn alias(&self) -> &str {
        match self {
            FromClause::Table { alias, .. } | FromClause::Subquery { alias, .. } => alias,
            FromClause::TableFunction { name } => name,
            FromClause::Temp { name, .. } => name,
            FromClause::Literal { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SqlExpr {
    Column {
        depth: usize,
        column: String,
    },
    Param(ParamValue),
    Binary {
        op: SqlBinOp,
        lhs: Box<SqlExpr>,
        rhs: Box<SqlExpr>,
    },
    Not(Box<SqlExpr>),
    Func {
        name: String,
        args: Vec<SqlExpr>,
    },
    Case {
        arms: Vec<(SqlExpr, SqlExpr)>,
        else_: Box<SqlExpr>,
    },
    In {
        expr: Box<SqlExpr>,
        items: Vec<SqlExpr>,
    },
    Coalesce(Vec<SqlExpr>),
    InSubquery {
        expr: Box<SqlExpr>,
        query: Box<SelectBuilder>,
    },
    Subquery {
        query: Box<SelectBuilder>,
    },
    Exists(Box<SelectBuilder>),
    Cast {
        expr: Box<SqlExpr>,
        target: &'static str,
    },
    Window {
        func: String,
        partition: Vec<SqlExpr>,
        order: Vec<OrderItem>,
    },
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlBinOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    Is,
    IsNot,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: SqlExpr,
    pub dir: OrderDir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDir {
    Asc,
    Desc,
}

#[derive(Debug, Default)]
pub struct AliasGen {
    next: usize,
}

impl AliasGen {
    fn generate(&mut self, base: &str) -> String {
        let name = format!("$${base}{}", self.next);
        self.next += 1;
        name
    }

    pub fn generic(&mut self) -> String {
        self.generate("val")
    }

    pub fn table(&mut self, table: &str) -> String {
        self.generate(table)
    }

    pub fn subquery(&mut self) -> String {
        self.generate("subq")
    }

    pub fn temp(&mut self) -> String {
        self.generate("split")
    }

    pub fn literal(&mut self) -> String {
        let id = format!("lit{}", self.next);
        self.next += 1;
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_shared_counter() {
        let mut aliases = AliasGen::default();
        assert_eq!(aliases.table("users"), "$$users0");
        assert_eq!(aliases.subquery(), "$$subq1");
        assert_eq!(aliases.table("users"), "$$users2");
        assert_eq!(aliases.generic(), "$$val3");
    }
}
