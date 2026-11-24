use grove_types::{Diagnostic, Span};

#[derive(Debug, Clone)]
pub(crate) enum SchemaValidationError {
    UnknownTypeRef {
        name: String,
        occurrences: Vec<Span>,
    },
    EnumAsStruct {
        name: String,
        occurrences: Vec<Span>,
    },
    TableMismatch {
        span: Span,
        name: String,
        tables: Vec<String>,
    },
    StructNoTable {
        span: Span,
        name: String,
    },
    DuplicateTable {
        span: Span,
        name: String,
        previous: Span,
    },
    DuplicateColumn {
        table: String,
        name: String,
        span: Span,
        previous: Span,
    },
    NestedStructRef {
        span: Span,
        struct_name: String,
        field: String,
        nested: Span,
    },
    ViaOnStructList {
        span: Span,
        struct_name: String,
        field: String,
    },
    TupleArityMismatch {
        span: Span,
        columns: Span,
        struct_name: String,
        field: String,
        declared: usize,
        expected: usize,
    },
    ViaArityMismatch {
        span: Span,
        columns: Span,
        struct_name: String,
        field: String,
        declared: usize,
        expected: usize,
    },
    NoViaScalarList {
        span: Span,
        struct_name: String,
        field: String,
    },
    UnmatchedForwardRef {
        span: Span,
        struct_name: String,
        field: String,
    },
    RelationEndpointMissing {
        span: Span,
        struct_name: String,
        field: String,
    },
    EndpointShapeMismatch {
        span: Span,
        struct_name: String,
        field: String,
    },
    EndpointTargetMismatch {
        span: Span,
        struct_name: String,
        field: String,
        actual: String,
        expected: String,
    },
    ViaOnOwning {
        span: Span,
    },
    DirectOnManyToMany {
        span: Span,
    },
    DuplicateEndpoint {
        span: Span,
        struct_name: String,
        field: String,
    },
    OwnedAsRoot {
        span: Span,
        root: String,
        struct_name: String,
    },
    UnrootedOwnership {
        span: Span,
        struct_name: String,
    },
    MultipleOwners {
        span: Span,
        struct_name: String,
        parents: Vec<String>,
    },
    UnownedNonRoot {
        span: Span,
        struct_name: String,
    },
    NonOwningNonRef {
        span: Span,
        struct_name: String,
        field: String,
    },
}

impl From<SchemaValidationError> for Diagnostic {
    fn from(err: SchemaValidationError) -> Self {
        use SchemaValidationError::*;

        match err {
            UnknownTypeRef { name, occurrences } => {
                let (first, rest) = occurrences
                    .split_first()
                    .expect("an unknown type reference always has at least one occurrence");
                let mut diag = Diagnostic::error(
                    "SV0001",
                    format!("unknown type reference `{name}`"),
                    *first,
                    "unknown type",
                );
                for span in rest {
                    diag = diag.with_secondary_label(*span, "also referenced here");
                }
                diag
            }
            EnumAsStruct { name, occurrences } => {
                let (first, rest) = occurrences
                    .split_first()
                    .expect("an invalid reference always has at least one occurrence");
                let mut diag = Diagnostic::error(
                    "SV0025",
                    format!("enum `{name}` cannot be used as a struct"),
                    *first,
                    "expected a struct type",
                )
                .with_help("roots and relation endpoints must name structs, not enums");
                for span in rest {
                    diag = diag.with_secondary_label(*span, "also used as a struct here");
                }
                diag
            }
            TableMismatch { span, name, tables } => Diagnostic::error(
                "SV0003",
                format!("struct `{name}` maps to multiple tables"),
                span,
                "conflicting table mappings",
            )
            .with_note(format!("candidate tables: {}", tables.join(", "))),
            StructNoTable { span, name } => Diagnostic::error(
                "SV0004",
                format!("struct `{name}` has no underlying table"),
                span,
                "no underlying table",
            )
            .with_help("add a `root` for this struct or a relation referencing it"),
            DuplicateTable {
                span,
                name,
                previous,
            } => Diagnostic::error(
                "SV0005",
                format!("duplicate table `{name}`"),
                span,
                "table already declared",
            )
            .with_secondary_label(previous, "first declared here"),
            DuplicateColumn {
                table,
                name,
                span,
                previous,
            } => Diagnostic::error(
                "SV0006",
                format!("duplicate column `{table}.{name}`"),
                span,
                "column already declared",
            )
            .with_secondary_label(previous, "first declared here"),
            NestedStructRef {
                span,
                struct_name,
                field,
                nested,
            } => Diagnostic::error(
                "SV0007",
                format!(
                    "field `{struct_name}.{field}` nests a struct reference inside a value type"
                ),
                span,
                "struct in a value position",
            )
            .with_secondary_label(nested, "nested struct reference")
            .with_help(
                "struct references are only valid as a standalone `S`, `?S`, or `List<S>` field",
            ),
            ViaOnStructList {
                span,
                struct_name,
                field,
            } => Diagnostic::error(
                "SV0008",
                format!(
                    "field `{struct_name}.{field}` attaches a `via` storage table to a list of \
                     structs"
                ),
                span,
                "`via` on a struct list",
            )
            .with_help("struct collections are defined by `rel` statements, not a `via` clause"),
            TupleArityMismatch {
                span,
                columns,
                struct_name,
                field,
                declared,
                expected,
            } => Diagnostic::error(
                "SV0009",
                format!(
                    "field `{struct_name}.{field}` maps {declared} column(s) but its type needs \
                     {expected}"
                ),
                span,
                format!("type needs {expected} column(s)"),
            )
            .with_secondary_label(columns, format!("{declared} column(s) provided")),
            ViaArityMismatch {
                span,
                columns,
                struct_name,
                field,
                declared,
                expected,
            } => Diagnostic::error(
                "SV0010",
                format!(
                    "field `{struct_name}.{field}` lists {declared} value column(s) in its \
                     storage table but the element type needs {expected}"
                ),
                span,
                format!("type needs {expected} column(s)"),
            )
            .with_secondary_label(columns, format!("{declared} column(s) provided")),
            NoViaScalarList {
                span,
                struct_name,
                field,
            } => Diagnostic::error(
                "SV0011",
                format!(
                    "field `{struct_name}.{field}` is a list of values without a `via` storage \
                     table"
                ),
                span,
                "missing `via` storage",
            )
            .with_help("a list of scalar values must be backed by a `via` storage table"),
            UnmatchedForwardRef {
                span,
                struct_name,
                field,
            } => Diagnostic::error(
                "SV0012",
                format!(
                    "field `{struct_name}.{field}` is a struct reference that cannot be \
                     materialized"
                ),
                span,
                "unrepresentable reference",
            )
            .with_help("struct references must be matched by a `rel` forward reference"),
            RelationEndpointMissing {
                span,
                struct_name,
                field,
            } => Diagnostic::error(
                "SV0013",
                format!("relation endpoint `{struct_name}.{field}` is not declared as a field"),
                span,
                "undeclared endpoint",
            )
            .with_help("every relation endpoint must be a declared struct field"),
            EndpointShapeMismatch {
                span,
                struct_name,
                field,
            } => Diagnostic::error(
                "SV0014",
                format!(
                    "endpoint field `{struct_name}.{field}` doesn't match this relation's shape \
                     or `&` marker"
                ),
                span,
                "wrong endpoint shape",
            )
            .with_note(
                "endpoint fields must carry the `&` marker and shape the relation's type requires",
            ),
            EndpointTargetMismatch {
                span,
                struct_name,
                field,
                actual,
                expected,
            } => Diagnostic::error(
                "SV0016",
                format!(
                    "endpoint field `{struct_name}.{field}` points at `{actual}`, but this \
                     relation expects `{expected}`"
                ),
                span,
                "wrong reference target",
            ),
            ViaOnOwning { span } => Diagnostic::error(
                "SV0017",
                "`via` mapping on owning relation",
                span,
                "relation type doesn't support `via`",
            )
            .with_note("an owning or OneToMany relation requires a direct mapping, not `via`"),
            DirectOnManyToMany { span } => Diagnostic::error(
                "SV0018",
                "direct mapping on M:N relation",
                span,
                "relation type requires `via`",
            )
            .with_note("a ManyToMany relation requires a `via` join table, not a direct mapping"),
            DuplicateEndpoint {
                span,
                struct_name,
                field,
            } => Diagnostic::error(
                "SV0019",
                format!("field `{struct_name}.{field}` is already an endpoint of another relation"),
                span,
                "already a relation endpoint",
            ),
            OwnedAsRoot {
                span,
                root,
                struct_name,
            } => Diagnostic::error(
                "SV0020",
                format!(
                    "root `{root}` exposes `{struct_name}`, which is already owned by a relation"
                ),
                span,
                "owned struct as root",
            )
            .with_note(
                "every struct must either be owned by exactly one other struct \
                 through a relation or be declared as a root",
            ),
            NonOwningNonRef {
                span,
                struct_name,
                field,
            } => Diagnostic::error(
                "SV0021",
                format!(
                    "field `{struct_name}.{field}` is marked `&` but is not a struct reference"
                ),
                span,
                "`&` on a non-reference",
            )
            .with_note("`&` is only valid on a struct reference: `S`, `?S`, or `List<S>`"),
            UnrootedOwnership { span, struct_name } => Diagnostic::error(
                "SV0022",
                format!(
                    "struct `{struct_name}` is owned \
                     but its ownership chain contains circular ownership"
                ),
                span,
                "ownership without a root",
            )
            .with_note(
                "every struct must either be owned by exactly one other struct \
                 through a relation or be declared as a root",
            ),
            MultipleOwners {
                span,
                struct_name,
                parents,
            } => Diagnostic::error(
                "SV0023",
                format!(
                    "struct `{struct_name}` is owned by more than one struct ({})",
                    parents.join(", ")
                ),
                span,
                "multiple owners",
            )
            .with_note(
                "every struct must either be owned by exactly one other struct \
                 through a relation or be declared as a root",
            ),
            UnownedNonRoot { span, struct_name } => Diagnostic::error(
                "SV0024",
                format!("struct `{struct_name}` has no owner and is not declared as a root"),
                span,
                "floating struct",
            )
            .with_help(format!(
                "declare `{struct_name}` as a root if it is has no owner"
            ))
            .with_note(
                "every struct must either be owned by exactly one other struct \
                 through a relation or be declared as a root",
            ),
        }
    }
}
