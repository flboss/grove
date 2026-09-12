use super::model::ParamValue;

#[derive(Debug, Clone, PartialEq)]
pub struct SelectBuilder {
    pub select: Vec<SelectItem>,
    pub from: Option<FromClause>,
    pub where_: Vec<SqlExpr>,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<SqlExpr>,
    pub offset: Option<SqlExpr>,
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
    },
    Expr {
        expr: SqlExpr,
        output: String,
    },
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
}

impl FromClause {
    pub fn alias(&self) -> &str {
        match self {
            FromClause::Table { alias, .. } | FromClause::Subquery { alias, .. } => alias,
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
        self.generate("sub")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_shared_counter() {
        let mut aliases = AliasGen::default();
        assert_eq!(aliases.table("users"), "$$users0");
        assert_eq!(aliases.subquery(), "$$sub1");
        assert_eq!(aliases.table("users"), "$$users2");
        assert_eq!(aliases.generic(), "$$val3");
    }
}
