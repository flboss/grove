use grove_types::Spanned;

#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    pub roots: Vec<RootCollection>,
    pub structs: Vec<StructDef>,
    pub config: Option<ConfigBlock>,
    pub relations: Vec<Relation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigBlock {
    pub int_arithmetic: Option<Spanned<IntArithmetic>>,
    pub float_checks: Option<Spanned<bool>>,
    pub dec_arithmetic: Option<Spanned<DecArithmetic>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntArithmetic {
    Checked,
    Saturating,
    Wrapping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecArithmetic {
    Checked,
    Saturating,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RootCollection {
    pub name: Spanned<String>,
    pub table: Option<Spanned<String>>,
    pub struct_name: Spanned<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StructDef {
    pub name: Spanned<String>,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: Spanned<String>,
    pub exposed_type: TypeExpr,
    pub column: Option<ColumnMapping>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    Named(Spanned<BuiltinType>),
    Struct(Spanned<String>),
    Optional(Box<TypeExpr>),
    List {
        element: Box<TypeExpr>,
        via: Option<ListStorage>,
    },
    Tuple(Vec<TypeExpr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinType {
    Int,
    Float,
    Dec,
    String,
    Bool,
    Instant,
    Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnMapping {
    Single(Spanned<String>),
    Multi(Vec<Spanned<String>>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListStorage {
    pub table: Spanned<String>,
    pub key_col: Spanned<String>,
    pub value: ColumnMapping,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub child: RelationEndpoint,
    pub arrow: Spanned<RelationArrow>,
    pub parent: RelationEndpoint,
    pub fk: FkMapping,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelationEndpoint {
    pub struct_name: Spanned<String>,
    pub field_name: Spanned<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationArrow {
    OneToOne,
    ManyToOne,
    OneToMany,
    ManyToMany,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnRef {
    pub table: Spanned<String>,
    pub col: Spanned<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JoinColumn {
    pub join_col: Spanned<String>,
    pub target: ColumnRef,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FkMapping {
    Direct {
        child: ColumnRef,
        parent: ColumnRef,
    },
    Indirect {
        join_table: Spanned<String>,
        a: JoinColumn,
        b: JoinColumn,
    },
}
