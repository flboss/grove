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
    ColumnId, Config, FieldId, Relation, Root, ScalarType, Struct, StructId, Table, TableId,
    ValidatedSchema,
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
    field_index: HashMap<(&'s str, &'s str), FieldId>,
    relations: Vec<Relation>,
    diags: Vec<Diagnostic>,
}

pub fn validate(schema: Schema) -> (Option<ValidatedSchema>, Vec<Diagnostic>) {
    let mut builder = Builder::new(&schema);
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
            field_index: HashMap::new(),
            relations: Vec::new(),
            diags: Vec::new(),
        }
    }

    fn emit_error(&mut self, error: SchemaValidationError) {
        self.diags.push(error.into());
    }
}

fn field_kind(ty: &TypeExpr) -> FieldKind<'_> {
    match ty {
        TypeExpr::Primitive(_) | TypeExpr::Tuple { .. } => FieldKind::Value,
        TypeExpr::Struct(_) => FieldKind::Ref,
        TypeExpr::Optional { inner, .. } if matches!(inner.as_ref(), TypeExpr::Struct(_)) => {
            FieldKind::Ref
        }
        TypeExpr::Optional { .. } => FieldKind::Value,
        TypeExpr::List { element, .. } if matches!(element.as_ref(), TypeExpr::Struct(_)) => {
            FieldKind::Ref
        }
        TypeExpr::List {
            element,
            via: Some(via),
            ..
        } => FieldKind::Array { element, via },
        TypeExpr::List { .. } => FieldKind::ListNoVia,
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

fn value_scalar_leaves(ty: &TypeExpr) -> Vec<ScalarType> {
    match ty {
        TypeExpr::Primitive(spanned) => vec![builtin_scalar(spanned)],
        TypeExpr::Optional { inner, .. } => value_scalar_leaves(inner),
        TypeExpr::Tuple { elements, .. } => elements.iter().flat_map(value_scalar_leaves).collect(),
        TypeExpr::List { element, .. } => value_scalar_leaves(element),
        TypeExpr::Struct(_) => Vec::new(),
    }
}

fn storage_value_types(ty: &TypeExpr) -> Vec<Option<ScalarType>> {
    match ty {
        TypeExpr::Primitive(spanned) => vec![Some(builtin_scalar(spanned))],
        TypeExpr::Optional { inner, .. } => storage_value_types(inner),
        TypeExpr::Tuple { elements, .. } => elements.iter().flat_map(storage_value_types).collect(),
        TypeExpr::List { .. } => vec![None],
        TypeExpr::Struct(_) => Vec::new(),
    }
}

fn builtin_scalar(builtin: &BuiltinType) -> ScalarType {
    match builtin {
        BuiltinType::Int => ScalarType::Int,
        BuiltinType::Float => ScalarType::Float,
        BuiltinType::Dec => ScalarType::Dec,
        BuiltinType::String => ScalarType::String,
        BuiltinType::Bool => ScalarType::Bool,
        BuiltinType::Instant => ScalarType::Instant,
        BuiltinType::Duration => ScalarType::Duration,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_schema;
    use crate::validated::{DecArithmetic, Field, IntArithmetic};

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
