mod enums;
mod identity;
mod ownership;
mod physical;
mod relations;
mod structs;

use std::collections::{HashMap, HashSet};

use crate::ast::{
    BuiltinType, ColumnMapping, ConfigBlock, DecArithmetic, Field as FieldDecl, IntArithmetic,
    ListStorage, Schema, TypeExpr,
};
use crate::validated::{
    ColumnId, Config, Enum, EnumId, FieldId, Relation, Root, ScalarType, Struct, StructId, Table,
    TableId, ValidatedSchema,
};
use crate::validation_error::SchemaValidationError;
use grove_types::{Diagnostic, Span};

pub struct Builder<'s> {
    schema: &'s Schema,
    declared_structs: HashSet<&'s str>,
    relation_endpoint_fields: HashMap<(&'s str, &'s str), usize>,
    tables: Vec<Table>,
    table_index: HashMap<&'s str, TableId>,
    column_index: HashMap<(TableId, &'s str), ColumnId>,
    struct_table: Vec<TableId>,
    struct_index: HashMap<&'s str, StructId>,
    roots: Vec<Root>,
    structs: Vec<Struct>,
    enums: Vec<Enum>,
    enum_index: HashMap<&'s str, EnumId>,
    field_index: HashMap<(&'s str, &'s str), FieldId>,
    relations: Vec<Relation>,
    diags: Vec<Diagnostic>,
}

pub fn validate(schema: Schema) -> (Option<ValidatedSchema>, Vec<Diagnostic>) {
    let mut builder = Builder::new(&schema);
    builder.stage_enums();
    builder.stage_identity();
    builder.stage_physical();
    builder.stage_structs();
    builder.stage_relations();
    builder.stage_ownership();
    assemble(builder)
}

fn assemble(builder: Builder) -> (Option<ValidatedSchema>, Vec<Diagnostic>) {
    let diags = builder.diags;
    let validated = diags.is_empty().then(|| ValidatedSchema {
        config: resolve_config(builder.schema.config.as_deref()),
        roots: builder.roots,
        tables: builder.tables,
        structs: builder.structs,
        enums: builder.enums,
        relations: builder.relations,
    });
    (validated, diags)
}

fn resolve_config(config: Option<&ConfigBlock>) -> Config {
    Config {
        int_arithmetic: config
            .and_then(|block| block.int_arithmetic.as_ref())
            .map_or(IntArithmetic::Checked, |v| **v),
        float_checks: config
            .and_then(|block| block.float_checks.as_ref())
            .is_none_or(|v| **v),
        dec_arithmetic: config
            .and_then(|block| block.dec_arithmetic.as_ref())
            .map_or(DecArithmetic::Checked, |v| **v),
    }
}

impl<'s> Builder<'s> {
    fn new(schema: &'s Schema) -> Builder<'s> {
        Builder {
            schema,
            declared_structs: HashSet::new(),
            relation_endpoint_fields: HashMap::new(),
            tables: Vec::new(),
            table_index: HashMap::new(),
            column_index: HashMap::new(),
            struct_table: Vec::new(),
            struct_index: HashMap::new(),
            roots: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            enum_index: HashMap::new(),
            field_index: HashMap::new(),
            relations: Vec::new(),
            diags: Vec::new(),
        }
    }

    fn emit_error(&mut self, error: SchemaValidationError) {
        self.diags.push(error.into());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamedKind {
    Struct,
    Enum(EnumId),
    Unknown,
}

impl<'s> Builder<'s> {
    fn named_kind(&self, name: &str) -> NamedKind {
        if let Some(&id) = self.enum_index.get(name) {
            NamedKind::Enum(id)
        } else if self.declared_structs.contains(name) {
            NamedKind::Struct
        } else {
            NamedKind::Unknown
        }
    }

    fn enum_scalar(&self, id: EnumId) -> ScalarType {
        self.enums[id.index()].underlying.scalar_type()
    }

    fn contains_unknown_named(&self, ty: &TypeExpr) -> bool {
        match ty {
            TypeExpr::Primitive(_) => false,
            TypeExpr::Named(name) => self.named_kind(name.as_str()) == NamedKind::Unknown,
            TypeExpr::Optional { inner, .. } | TypeExpr::List { element: inner, .. } => {
                self.contains_unknown_named(inner)
            }
            TypeExpr::Tuple { elements, .. } => {
                elements.iter().any(|e| self.contains_unknown_named(e))
            }
        }
    }

    fn field_kind<'a>(&self, ty: &'a TypeExpr) -> Option<FieldKind<'a>> {
        if self.contains_unknown_named(ty) {
            return None;
        }

        Some(match ty {
            TypeExpr::Primitive(_) | TypeExpr::Tuple { .. } => FieldKind::Value,
            TypeExpr::Named(name) => match self.named_kind(name.as_str()) {
                NamedKind::Enum(_) => FieldKind::Value,
                NamedKind::Struct => FieldKind::Ref,
                NamedKind::Unknown => unreachable!("unknown named type was filtered"),
            },
            TypeExpr::Optional { inner, .. } => match inner.as_ref() {
                TypeExpr::Named(name) if self.named_kind(name.as_str()) == NamedKind::Struct => {
                    FieldKind::Ref
                }
                _ => FieldKind::Value,
            },
            TypeExpr::List { element, via, .. } => match element.as_ref() {
                TypeExpr::Named(name) if self.named_kind(name.as_str()) == NamedKind::Struct => {
                    FieldKind::Ref
                }
                _ => match via {
                    Some(via) => FieldKind::Array { element, via },
                    None => FieldKind::ListNoVia,
                },
            },
        })
    }

    /// columns representing the field in its owning struct's table
    fn field_column_types(&self, ty: &TypeExpr) -> Vec<Option<ScalarType>> {
        match ty {
            TypeExpr::Primitive(spanned) => vec![Some(builtin_scalar(spanned))],
            TypeExpr::Named(name) => match self.named_kind(name.as_str()) {
                NamedKind::Enum(id) => vec![Some(self.enum_scalar(id))],
                NamedKind::Struct | NamedKind::Unknown => Vec::new(),
            },
            TypeExpr::Optional { inner, .. } => self.field_column_types(inner),
            TypeExpr::Tuple { elements, .. } => elements
                .iter()
                .flat_map(|e| self.field_column_types(e))
                .collect(),
            TypeExpr::List { via: Some(_), .. } => vec![None],
            TypeExpr::List { element, .. } => self.field_column_types(element),
        }
    }

    /// value columns stored in a via table for an array element
    fn via_value_column_types(&self, ty: &TypeExpr) -> Vec<Option<ScalarType>> {
        match ty {
            TypeExpr::Primitive(spanned) => vec![Some(builtin_scalar(spanned))],
            TypeExpr::Named(name) => match self.named_kind(name.as_str()) {
                NamedKind::Enum(id) => vec![Some(self.enum_scalar(id))],
                NamedKind::Struct | NamedKind::Unknown => Vec::new(),
            },
            TypeExpr::Optional { inner, .. } => self.via_value_column_types(inner),
            TypeExpr::Tuple { elements, .. } => elements
                .iter()
                .flat_map(|e| self.via_value_column_types(e))
                .collect(),
            TypeExpr::List { .. } => vec![None],
        }
    }
}

enum FieldKind<'a> {
    Value,
    Array {
        element: &'a TypeExpr,
        via: &'a ListStorage,
    },
    ListNoVia,
    Ref,
}

fn value_column_refs(field: &FieldDecl) -> Vec<(&str, Span)> {
    match &field.column {
        None => vec![(field.name.as_str(), field.name.span)],
        Some(ColumnMapping::Single(col)) => vec![(col.as_str(), col.span)],
        Some(ColumnMapping::Multi(cols)) => cols.iter().map(|c| (c.as_str(), c.span)).collect(),
    }
}

fn array_link_column(field: &FieldDecl) -> (&str, Span) {
    match &field.column {
        None => (field.name.as_str(), field.name.span),
        Some(ColumnMapping::Single(col)) => (col.as_str(), col.span),
        Some(ColumnMapping::Multi(cols)) => (cols[0].as_str(), cols[0].span),
    }
}

fn builtin_scalar(builtin: &BuiltinType) -> ScalarType {
    match builtin {
        BuiltinType::Int => ScalarType::Int,
        BuiltinType::Float => ScalarType::Float,
        BuiltinType::Dec => ScalarType::Dec,
        BuiltinType::String => ScalarType::String,
        BuiltinType::Bytes => ScalarType::Bytes,
        BuiltinType::Bool => ScalarType::Bool,
        BuiltinType::Instant => ScalarType::Instant,
        BuiltinType::Duration => ScalarType::Duration,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_schema;
    use crate::validated::{
        DecArithmetic, EnumId, EnumUnderlying, Field, IntArithmetic, ValueType,
    };

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    const TEST_GSCHEMA: &str = "\
        root users: User;\n\
        root roles: Role;\n\
        root discount_codes: DiscountCode;\n\
        struct User { name: String, manager: &?User, subordinates: &List<User>, \
        profile: Profile, orders: List<Order>, roles: &List<Role>, \
        tags: List<String>@id via user_tags[id, tag_value] }\n\
        struct Order { total: Dec@order_total, user: &User, discount: &?DiscountCode }\n\
        struct Profile { bio: String, user: &User }\n\
        struct DiscountCode { percent: Dec, orders: &List<Order> }\n\
        struct Role { users: &List<User> }\n\
        rel Order.user <<-> User.orders (orders.user_id -> users.id);\n\
        rel User.manager <<-> User.subordinates (users.manager_id -> users.id);\n\
        rel Profile.user <-> User.profile (profiles.user_id -> users.id);\n\
        rel DiscountCode.orders <->> Order.discount (orders.discount_id -> discount_codes.id);\n\
        rel Role.users <<->> User.roles via user_roles[role_id -> roles.id, user_id -> users.id];\n";

    #[test]
    fn validate_assembles_clean_schema() {
        let (schema, parse_diags) = parse_schema(TEST_GSCHEMA);
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let (validated, diags) = validate(schema.expect("source should parse"));
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let validated = validated.expect("clean schema should assemble");

        assert_eq!(validated.config.int_arithmetic, IntArithmetic::Checked);
        assert!(validated.config.float_checks);
        assert_eq!(validated.config.dec_arithmetic, DecArithmetic::Checked);

        let root_names: Vec<&str> = validated.roots.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(root_names, vec!["users", "roles", "discount_codes"]);

        assert_eq!(validated.tables.len(), 7);
        assert!(validated.tables.iter().any(|t| t.name == "user_roles"));
        assert!(validated.tables.iter().any(|t| t.name == "user_tags"));

        let user = validated
            .structs
            .iter()
            .find(|s| s.name == "User")
            .expect("User struct");
        let order = validated
            .structs
            .iter()
            .find(|s| s.name == "Order")
            .expect("Order struct");
        assert_eq!(user.fields.len(), 7);
        let orders = user
            .fields
            .iter()
            .find(|f| matches!(f, Field::Ref { name, .. } if name == "orders"))
            .expect("orders field");
        assert!(matches!(
            orders,
            Field::Ref {
                owning: true,
                is_list: true,
                ..
            }
        ));
        let manager = user
            .fields
            .iter()
            .find(|f| matches!(f, Field::Ref { name, .. } if name == "manager"))
            .expect("manager field");
        assert!(matches!(
            manager,
            Field::Ref {
                owning: false,
                optional: true,
                ..
            }
        ));
        let order_user = order
            .fields
            .iter()
            .find(|f| matches!(f, Field::Ref { name, .. } if name == "user"))
            .expect("order.user field");
        assert!(matches!(
            order_user,
            Field::Ref {
                owning: false,
                is_list: false,
                optional: false,
                ..
            }
        ));

        let relation_counts = |v: &ValidatedSchema| {
            let one_to_one = v
                .relations
                .iter()
                .filter(|r| matches!(r, Relation::OneToOne { .. }))
                .count();
            let many_to_one = v
                .relations
                .iter()
                .filter(|r| matches!(r, Relation::ManyToOne { .. }))
                .count();
            let one_to_many = v
                .relations
                .iter()
                .filter(|r| matches!(r, Relation::OneToMany { .. }))
                .count();
            let many_to_many = v
                .relations
                .iter()
                .filter(|r| matches!(r, Relation::ManyToMany { .. }))
                .count();
            (one_to_one, many_to_one, one_to_many, many_to_many)
        };
        assert_eq!(relation_counts(&validated), (1, 2, 1, 1));

        let order_sid = StructId::new(
            validated
                .structs
                .iter()
                .position(|s| s.name == "Order")
                .expect("Order position"),
        );
        let order_user_local = order
            .fields
            .iter()
            .position(|f| matches!(f, Field::Ref { name, .. } if name == "user"))
            .expect("order.user position");
        let order_user_fid = FieldId::new(order_sid, order_user_local);
        assert!(
            validated.relation_of_field(order_user_fid).is_some(),
            "order.user should be part of a relation"
        );
    }

    #[test]
    fn validate_returns_none_on_diagnostics() {
        let (schema, parse_diags) = parse_schema(
            "root profiles: Profile; root users: User; \
             struct Profile { user: &User, bio: String } struct User { profile: Profile } \
             rel Profile.user <-> User.profile (profiles.user_id -> users.id);",
        );
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let (validated, diags) = validate(schema.expect("source should parse"));
        assert_eq!(codes(&diags), vec!["SV0020"]);
        assert!(
            validated.is_none(),
            "a schema with diagnostics must not assemble"
        );
    }

    #[test]
    fn unknown_field_type_no_cascade() {
        let (schema, parse_diags) = parse_schema("root users: User; struct User { x: Missing }");
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let (validated, diags) = validate(schema.expect("source should parse"));
        assert_eq!(codes(&diags), vec!["SV0001"]);
        assert!(validated.is_none());
    }

    #[test]
    fn validate_enums_and_enum_fields() {
        let (schema, parse_diags) = parse_schema(
            r#"root tickets: Ticket;
               enum Priority { Low => "low", High => "high" }
               enum Level { Low => 0, High => 2 }
               struct Ticket {
                   priority: Priority,
                   fallback: ?Priority,
                   pair: Tuple<Priority, Level>@(p, l),
                   tags: List<Priority>@tags_ref via ticket_tags[ticket_key, tag],
               }"#,
        );
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let (validated, diags) = validate(schema.expect("source should parse"));
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let validated = validated.expect("clean schema should assemble");

        assert_eq!(validated.enums.len(), 2);
        let (priority_id, priority) = validated.enum_by_name("Priority").expect("Priority");
        assert_eq!(priority_id, EnumId::new(0));
        assert_eq!(priority.name, "Priority");
        assert_eq!(priority.underlying, EnumUnderlying::String);
        let (level_id, level) = validated.enum_by_name("Level").expect("Level");
        assert_eq!(level_id, EnumId::new(1));
        assert_eq!(level.underlying, EnumUnderlying::Int);

        let ticket = validated
            .structs
            .iter()
            .find(|s| s.name == "Ticket")
            .expect("Ticket");
        assert!(matches!(
            &ticket.fields[0],
            Field::Value { ty: ValueType::Enum(id), .. } if *id == priority_id
        ));
        assert!(matches!(
            &ticket.fields[1],
            Field::Value { ty: ValueType::Optional(inner), .. }
                if matches!(inner.as_ref(), ValueType::Enum(id) if *id == priority_id)
        ));
        assert!(matches!(
            &ticket.fields[2],
            Field::Value { ty: ValueType::Tuple(items), .. }
                if items == &vec![ValueType::Enum(priority_id), ValueType::Enum(level_id)]
        ));
        assert!(matches!(
            &ticket.fields[3],
            Field::Array { element: ValueType::Enum(id), .. } if *id == priority_id
        ));

        let column_ty = |name: &str| {
            validated.tables[ticket.table.index()]
                .columns
                .iter()
                .find(|c| c.name == name)
                .and_then(|c| c.ty)
        };
        assert_eq!(column_ty("priority"), Some(ScalarType::String));
        assert_eq!(column_ty("fallback"), Some(ScalarType::String));
        assert_eq!(column_ty("p"), Some(ScalarType::String));
        assert_eq!(column_ty("l"), Some(ScalarType::Int));
        assert_eq!(column_ty("tags_ref"), None);
    }

    #[test]
    fn config_defaults_when_omitted() {
        let (schema, parse_diags) = parse_schema("root users: User; struct User { name: String }");
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let (validated, diags) = validate(schema.expect("source should parse"));
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let validated = validated.expect("clean schema should assemble");
        assert_eq!(validated.config.int_arithmetic, IntArithmetic::Checked);
        assert!(validated.config.float_checks);
        assert_eq!(validated.config.dec_arithmetic, DecArithmetic::Checked);
    }

    #[test]
    fn config_overrides_partial_and_full() {
        let (schema, parse_diags) = parse_schema(
            "config { float_checks = false } root users: User; struct User { name: String }",
        );
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let (validated, diags) = validate(schema.expect("source should parse"));
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let validated = validated.expect("clean schema should assemble");
        assert_eq!(validated.config.int_arithmetic, IntArithmetic::Checked);
        assert!(!validated.config.float_checks);
        assert_eq!(validated.config.dec_arithmetic, DecArithmetic::Checked);

        let (schema, parse_diags) = parse_schema(
            "config { int_arithmetic = \"wrapping\", float_checks = false, \
             dec_arithmetic = \"saturating\" } \
             root users: User; struct User { name: String }",
        );
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let (validated, diags) = validate(schema.expect("source should parse"));
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let validated = validated.expect("clean schema should assemble");
        assert_eq!(validated.config.int_arithmetic, IntArithmetic::Wrapping);
        assert!(!validated.config.float_checks);
        assert_eq!(validated.config.dec_arithmetic, DecArithmetic::Saturating);
    }
}
