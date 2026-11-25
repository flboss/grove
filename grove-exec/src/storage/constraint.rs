use grove_schema::validated::FieldId;

use crate::value::Scalar;

/// Backend-agnostic constraint set attached to a storage read
#[derive(Debug, Clone, PartialEq, Default, Hash)]
pub struct FetchConstraints {
    /// WHERE predicates connected by AND
    pub filters: Vec<PushExpr>,
    /// ORDER BY keys in priority order
    pub sort: Vec<SortKey>,
    /// Reverse the implicit stable-order tie-breaker (rowid in SQLite)
    pub tie_desc: bool,
    pub offset: u64,
    pub limit: Option<u64>,
}

impl FetchConstraints {
    pub const EMPTY: FetchConstraints = FetchConstraints {
        filters: Vec::new(),
        sort: Vec::new(),
        tie_desc: false,
        offset: 0,
        limit: None,
    };

    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
            && self.sort.is_empty()
            && !self.tie_desc
            && self.offset == 0
            && self.limit.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SortKey {
    pub field: FieldId,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq, Hash)]
pub enum PushExpr {
    /// Non-optional single-column scalar
    Field(FieldId),
    /// Literal evaluated at plan-build time
    Literal(Scalar),
    Cmp {
        op: CmpOp,
        lhs: Box<PushExpr>,
        rhs: Box<PushExpr>,
    },
    And(Box<PushExpr>, Box<PushExpr>),
    Or(Box<PushExpr>, Box<PushExpr>),
    Not(Box<PushExpr>),
    In {
        lhs: Box<PushExpr>,
        items: Vec<PushExpr>,
    },
    StringOp {
        op: StringOp,
        base: Box<PushExpr>,
        needle: Box<PushExpr>,
    },
    StringLen(Box<PushExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StringOp {
    Contains,
    StartsWith,
    EndsWith,
}
