#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntArithmetic {
    Checked,
    Saturating,
    Wrapping,
}

impl TryFrom<&str> for IntArithmetic {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "checked" => Ok(Self::Checked),
            "saturating" => Ok(Self::Saturating),
            "wrapping" => Ok(Self::Wrapping),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecArithmetic {
    Checked,
    Saturating,
}

impl TryFrom<&str> for DecArithmetic {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "checked" => Ok(Self::Checked),
            "saturating" => Ok(Self::Saturating),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableId(u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StructId(u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RelationId(u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColumnId(u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FieldId(u32);

impl TableId {
    pub fn new(index: usize) -> Self {
        TableId(index as u16)
    }

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl StructId {
    pub fn new(index: usize) -> Self {
        StructId(index as u16)
    }

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl RelationId {
    pub fn new(index: usize) -> Self {
        RelationId(index as u16)
    }

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl ColumnId {
    pub fn new(table: TableId, local: usize) -> Self {
        ColumnId(((table.0 as u32) << 16) | ((local as u32) & 0xFFFF))
    }

    pub fn table_id(self) -> TableId {
        TableId((self.0 >> 16) as u16)
    }

    pub fn local(self) -> usize {
        (self.0 & 0xFFFF) as usize
    }
}

impl FieldId {
    pub fn new(struct_id: StructId, local: usize) -> Self {
        FieldId(((struct_id.0 as u32) << 16) | ((local as u32) & 0xFFFF))
    }

    pub fn struct_id(self) -> StructId {
        StructId((self.0 >> 16) as u16)
    }

    pub fn local(self) -> usize {
        (self.0 & 0xFFFF) as usize
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedSchema {
    pub config: Config,
    pub roots: Vec<Root>,
    pub tables: Vec<Table>,
    pub structs: Vec<Struct>,
    pub relations: Vec<Relation>,
}

impl ValidatedSchema {
    pub fn relation_of_field(&self, field: FieldId) -> Option<RelationId> {
        self.relations
            .iter()
            .position(|r| r.endpoints().contains(&field))
            .map(RelationId::new)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub int_arithmetic: IntArithmetic,
    pub float_checks: bool,
    pub dec_arithmetic: DecArithmetic,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Root {
    pub name: String,
    pub struct_id: StructId,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    // TODO: key/link column types
    pub ty: Option<ScalarType>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Struct {
    pub name: String,
    pub table: TableId,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    Value {
        name: String,
        ty: ValueType,
        columns: Vec<ColumnId>,
    },
    Array {
        name: String,
        element: ValueType,
        link: ColumnId,
        storage: StorageTable,
    },
    Ref {
        name: String,
        target: StructId,
        optional: bool,
        is_list: bool,
        owning: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ValueType {
    Scalar(ScalarType),
    Tuple(Vec<ValueType>),
    Optional(Box<ValueType>),
    Array {
        element: Box<ValueType>,
        storage: StorageTable,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarType {
    Int,
    Float,
    Dec,
    String,
    Bool,
    Instant,
    Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StorageTable {
    pub table: TableId,
    pub key: ColumnId,
    pub value: Vec<ColumnId>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Relation {
    OneToOne {
        child_ref: FieldId,
        parent_ref: FieldId,
        fk: ColumnId,
        pk: ColumnId,
    },
    ManyToOne {
        child_ref: FieldId,
        parent_ref: FieldId,
        fk: ColumnId,
        pk: ColumnId,
    },
    OneToMany {
        a_ref: FieldId,
        b_ref: FieldId,
        fk: ColumnId,
        pk: ColumnId,
    },
    ManyToMany {
        a_ref: FieldId,
        b_ref: FieldId,
        join_table: TableId,
        a_col: ColumnId,
        a_pk: ColumnId,
        b_col: ColumnId,
        b_pk: ColumnId,
    },
}

impl Relation {
    pub fn endpoints(&self) -> [FieldId; 2] {
        match self {
            Relation::OneToOne {
                child_ref,
                parent_ref,
                ..
            }
            | Relation::ManyToOne {
                child_ref,
                parent_ref,
                ..
            } => [*child_ref, *parent_ref],
            Relation::OneToMany { a_ref, b_ref, .. } => [*a_ref, *b_ref],
            Relation::ManyToMany { a_ref, b_ref, .. } => [*a_ref, *b_ref],
        }
    }
}
