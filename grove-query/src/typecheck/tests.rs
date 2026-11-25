use super::*;
use grove_schema::validated::{FieldId, StructId};
use grove_types::NodeId;

const TEST_SCHEMA_SRC: &str = r#"
        config {
            int_arithmetic = "checked",
            float_checks = true,
            dec_arithmetic = "checked",
        }
        root users: User;
        struct User {
            name: String,
            age: Int,
            score: Float,
            balance: Dec,
            active: Bool,
            created: Instant,
            profile: ?Profile,
            maybe_pos: ?(Float, Float)@(mlat, mlng),
        }
        struct Profile {
            user: &User,
            username: String
        }
        rel Profile.user <-> User.profile (profiles.user_id -> users.id);
    "#;

fn test_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(TEST_SCHEMA_SRC)
            .0
            .expect("invalid test schema"),
    )
    .0
    .expect("invalid test schema")
}

const MUTATION_SCHEMA_SRC: &str = r#"
        root users: User;
        root images: Image;
        struct User {
            id: Int,
            name: String,
            orders: List<Order>,
        }
        struct Order {
            id: Int,
            total: Int,
            user: &User,
            image: &?Image,
        }
        struct Image {
            id: Int,
            title: String,
            orders: &List<Order>,
        }
        rel Order.user <<-> User.orders (orders.user_id -> users.id);
        rel Image.orders <->> Order.image (orders.image_id -> images.id);
    "#;

fn mutation_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(MUTATION_SCHEMA_SRC)
            .0
            .expect("invalid mutation schema"),
    )
    .0
    .expect("invalid mutation schema")
}

const ENUM_SCHEMA_SRC: &str = r#"
        root tickets: Ticket;
        enum Priority { Low => "low", Medium => "medium", High => "high" }
        enum Level { Low => 0, High => 2 }
        struct Ticket {
            id: Int,
            priority: Priority,
            level: Level,
            maybe_priority: ?Priority,
            priorities: List<Priority>@priorities_ref via ticket_priorities[ticket_key, priority],
            pair: (Priority, Level)@(pair_priority, pair_level),
        }
    "#;

fn enum_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(ENUM_SCHEMA_SRC)
            .0
            .expect("invalid enum test schema"),
    )
    .0
    .expect("invalid enum test schema")
}

fn error_codes(source: &str, schema: &ValidatedSchema) -> Vec<String> {
    let (file, parse_diags) = crate::parse_query(source);
    assert!(parse_diags.is_empty(), "parse diagnostics: {parse_diags:?}");
    let (typed, diags) = typecheck(file.unwrap(), schema);
    assert!(typed.is_none(), "expected type error for `{source}`");
    diags.iter().map(|d| d.code.to_string()).collect()
}

fn infer_ok(source: &str, schema: &ValidatedSchema) -> TypedExpr {
    infer_ok_with_row_ref(source, schema, false)
}

fn infer_ok_with_row_ref(
    source: &str,
    schema: &ValidatedSchema,
    allow_row_refs: bool,
) -> TypedExpr {
    let mut env = TypeEnv::new(schema);
    env.allow_row_refs = allow_row_refs;
    let (file, parse_diags) = crate::parse_query(source);
    assert!(parse_diags.is_empty(), "parse diagnostics: {parse_diags:?}");
    infer(file.unwrap().output().unwrap(), &mut env).unwrap()
}

fn infer_err_with_row_ref(
    source: &str,
    schema: &ValidatedSchema,
    allow_row_refs: bool,
) -> Vec<String> {
    let mut env = TypeEnv::new(schema);
    env.allow_row_refs = allow_row_refs;
    let (file, parse_diags) = crate::parse_query(source);
    assert!(parse_diags.is_empty(), "parse diagnostics: {parse_diags:?}");
    let err = infer(file.unwrap().output().unwrap(), &mut env).unwrap_err();
    vec![Diagnostic::from(err).code.to_string()]
}

fn typecheck_ok(source: &str, schema: &ValidatedSchema) -> TypedQueryFile {
    let (file, parse_diags) = crate::parse_query(source);
    assert!(parse_diags.is_empty(), "parse diagnostics: {parse_diags:?}");
    let (typed, diags) = typecheck(file.unwrap(), schema);
    assert!(diags.is_empty(), "type diagnostics: {diags:?}");
    typed.unwrap()
}

const ANNOTATION_SCHEMA_SRC: &str = r#"
        root items: Item;
        struct Item {
            a: ?String,
            b: (Int, Int)@(bx, by),
        }
    "#;

fn annotation_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(ANNOTATION_SCHEMA_SRC)
            .0
            .expect("invalid annotation test schema"),
    )
    .0
    .expect("invalid annotation test schema")
}

#[test]
fn infer_enum_variant() {
    let schema = enum_schema();
    let priority_id = schema.enum_by_name("Priority").unwrap().0;
    let result = infer_ok("Priority::Medium", &schema);
    assert_eq!(result.ty, QueryType::Enum(priority_id));
    assert!(matches!(
        result.kind,
        TypedExprKind::EnumVariant {
            enum_id,
            variant: 1
        } if enum_id == priority_id
    ));
}

#[test]
fn row_ref_type_equality_display() {
    let schema = test_schema();
    let user_id = schema.roots[0].struct_id;
    let ty = QueryType::RowRef(user_id);
    assert_eq!(ty.to_string(), format!("&Struct#{}", user_id.index()));
    assert!(!ty.is_unknown());
    assert!(ty.eq(&QueryType::RowRef(user_id), &schema));
    assert!(!ty.eq(&QueryType::Record(RecordSource::Schema(user_id)), &schema));
}

#[test]
fn row_ref_only_inside_mutation_args() {
    let schema = test_schema();
    assert_eq!(error_codes("&users", &schema), vec!["QT0040"]);
    assert_eq!(error_codes("let r = &users;", &schema), vec!["QT0040"]);
    assert_eq!(
        error_codes("users.filter(&users).len()", &schema),
        vec!["QT0040"]
    );
    assert_eq!(
        error_codes(
            "users.update({ active = &users.first().unwrap() });",
            &schema
        ),
        ["QT0024"]
    );
}

#[test]
fn row_ref_shapes() {
    let schema = nested_schema();
    let user = nested_struct(&schema, "User");
    let order = nested_struct(&schema, "Order");

    assert_eq!(
        infer_ok_with_row_ref("&users", &schema, true).ty,
        QueryType::List(Box::new(QueryType::RowRef(user)))
    );
    assert_eq!(
        infer_ok_with_row_ref("&users.first()", &schema, true).ty,
        QueryType::Optional(Box::new(QueryType::RowRef(user)))
    );
    assert_eq!(
        infer_ok_with_row_ref("(&users).first()", &schema, true).ty,
        QueryType::Optional(Box::new(QueryType::RowRef(user)))
    );
    assert_eq!(
        infer_ok_with_row_ref("&users.first().unwrap()", &schema, true).ty,
        QueryType::RowRef(user)
    );
    assert_eq!(
        infer_ok_with_row_ref("&users.subordinates", &schema, true).ty,
        QueryType::List(Box::new(QueryType::List(Box::new(QueryType::RowRef(user)))))
    );
    assert_eq!(
        infer_ok_with_row_ref("&users.orders.flatten()", &schema, true).ty,
        QueryType::List(Box::new(QueryType::RowRef(order)))
    );
    assert_eq!(
        infer_ok_with_row_ref("(&users.orders).flatten()", &schema, true).ty,
        QueryType::List(Box::new(QueryType::RowRef(order)))
    );
    assert!(matches!(
        infer_ok_with_row_ref("&users", &schema, true).kind,
        TypedExprKind::RowRef { .. }
    ));
}

#[test]
fn row_ref_is_opaque() {
    let schema = nested_schema();
    for source in [
        "(&users.first().unwrap()).name",
        "(&users).unique()",
        "(&users).contains(&users.first().unwrap())",
        "(&users).filter(true)",
        "(&users).map(true)",
        "(&users.first()).map(true)",
        "(&users).subordinates",
        "&users == &users",
    ] {
        assert_eq!(
            infer_err_with_row_ref(source, &schema, true),
            ["QT0042"],
            "{source}"
        );
    }
}

#[test]
fn row_ref_shape_preserving_operations_allowed() {
    let schema = nested_schema();
    for source in [
        "(&users).take(2)",
        "(&users).skip(1)",
        "(&users).take_back(1)",
        "(&users).skip_back(1)",
        "(&users).reverse()",
        "(&users).first()",
        "(&users).nth(0)",
        "(&users).last()",
        "(&users).len()",
        "(&users).extend(&users.take(1))",
        "&users.take(2)",
        "(&users.first()).unwrap()",
        "(&users.first()).is_some()",
        "(&users.first()).is_none()",
        "(&users.first()).unwrap_or(&users.first().unwrap())",
    ] {
        infer_ok_with_row_ref(source, &schema, true);
    }
}

#[test]
fn row_ref_untraceable_source() {
    let schema = nested_schema();
    for source in [
        r#"users.update({ active = &users { name } });"#,
        r#"let u = users; users.update({ active = &u });"#,
        r#"users.update({ active = & &users });"#,
        r#"users.update({ active = &1 });"#,
    ] {
        assert_eq!(error_codes(source, &schema), ["QT0041"], "{source}");
    }
}

#[test]
fn enum_field_type() {
    let schema = enum_schema();
    let priority_id = schema.enum_by_name("Priority").unwrap().0;
    assert_eq!(
        infer_ok("tickets.first().unwrap().priority", &schema).ty,
        QueryType::Enum(priority_id)
    );
}

#[test]
fn unknown_enum_rejected() {
    assert_eq!(error_codes("Missing::High", &enum_schema()), vec!["QT0030"]);
}

#[test]
fn unknown_enum_variant_rejected() {
    assert_eq!(
        error_codes("Priority::Urgent", &enum_schema()),
        vec!["QT0031"]
    );
}

#[test]
fn enum_equality_and_membership() {
    for source in [
        "Priority::High == Priority::Low",
        "Priority::High != Priority::Low",
        "Priority::High in (Priority::Low, Priority::High)",
        "[Priority::Low, Priority::High].contains(Priority::High)",
    ] {
        assert_eq!(
            infer_ok(source, &enum_schema()).ty,
            QueryType::Scalar(ScalarType::Bool),
            "source: {source}"
        );
    }
}

#[test]
fn enum_raw_value_comparison_rejected() {
    assert_eq!(
        error_codes(r#"Priority::High == "high""#, &enum_schema()),
        vec!["QT0007"]
    );
}

#[test]
fn different_enum_comparison_rejected() {
    assert_eq!(
        error_codes("Priority::Low == Level::Low", &enum_schema()),
        vec!["QT0007"]
    );
}

#[test]
fn enum_ordering_rejected() {
    let schema = enum_schema();
    assert_eq!(
        error_codes("Priority::High > Priority::Low", &schema),
        vec!["QT0007"]
    );
    assert_eq!(
        error_codes("[Priority::Low, Priority::High].sort()", &schema),
        vec!["QT0003"]
    );
}

#[test]
fn enum_cast_to_underlying_value() {
    let schema = enum_schema();

    let result = infer_ok("Priority::High as String", &schema);
    assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
    assert!(matches!(result.kind, TypedExprKind::Cast { .. }));

    assert_eq!(
        infer_ok("Level::High as Int", &schema).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn enum_cast_to_wrong_underlying_type_rejected() {
    let schema = enum_schema();
    assert_eq!(
        error_codes("Priority::High as Int", &schema),
        vec!["QT0013"]
    );
    assert_eq!(
        error_codes("Level::High as String", &schema),
        vec!["QT0013"]
    );
}

#[test]
fn enum_optional_and_projection() {
    let schema = enum_schema();
    let priority_id = schema.enum_by_name("Priority").unwrap().0;

    assert_eq!(
        infer_ok("some(Priority::High)", &schema).ty,
        QueryType::Optional(Box::new(QueryType::Enum(priority_id)))
    );

    let result = infer_ok("tickets { priority }", &schema);
    match result.ty {
        QueryType::List(inner) => match *inner {
            QueryType::Record(RecordSource::Projection(fields)) => {
                assert_eq!(fields[0].ty, QueryType::Enum(priority_id));
            }
            other => panic!("expected projection record, got {other:?}"),
        },
        other => panic!("expected list projection, got {other:?}"),
    }
}

#[test]
fn infer_int_literal() {
    assert_eq!(
        infer_ok("42", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn infer_float_literal() {
    assert_eq!(
        infer_ok("3.14f", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Float)
    );
}

#[test]
fn infer_dec_literal() {
    assert_eq!(
        infer_ok("3.14", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Dec)
    );
}

#[test]
fn infer_string_literal() {
    assert_eq!(
        infer_ok("\"hello\"", &test_schema()).ty,
        QueryType::Scalar(ScalarType::String)
    );
}

#[test]
fn infer_bytes_literal() {
    for source in [r#"hex"deadbeef""#, r#"b64"3q2+7w==""#] {
        assert_eq!(
            infer_ok(source, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Bytes),
            "source: {source}"
        );
    }
}

#[test]
fn bytes_len_method() {
    for source in [r#"hex"deadbeef".len()"#, r#"b64"3q2+7w==".len()"#] {
        assert_eq!(
            infer_ok(source, &test_schema()).ty,
            QueryType::Scalar(ScalarType::Int),
            "source: {source}"
        );
    }
}

#[test]
fn bytes_cast_from_string() {
    assert_eq!(
        infer_ok(r#""hello" as Bytes"#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bytes)
    );
}

#[test]
fn annotations_disambiguation_only() {
    for (source, expected) in [
        (
            "[] as List<Int>",
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int))),
        ),
        (
            "none as ?String",
            QueryType::Scalar(ScalarType::String).wrap_optional(),
        ),
        (
            "some(none) as ??Int",
            QueryType::Scalar(ScalarType::Int)
                .wrap_optional()
                .wrap_optional(),
        ),
        ("1 as Int", QueryType::Scalar(ScalarType::Int)),
    ] {
        let result = infer_ok(source, &test_schema());
        assert_eq!(result.ty, expected, "source: {source}");
        assert!(
            !matches!(&result.kind, TypedExprKind::Cast { .. }),
            "source should be a no-op: {source}"
        );
    }
}

#[test]
fn annotations_complex_and_named() {
    let schema = test_schema();
    let result = infer_ok(
        "[] as List<(?Int, List<Duration>, ??String, {a: Instant, d: Dec})>",
        &schema,
    );
    assert!(!result.ty.is_unknown());
    assert!(matches!(result.ty, QueryType::List(_)));
    assert!(!matches!(&result.kind, TypedExprKind::Cast { .. }));

    let user_id = schema.roots[0].struct_id;
    assert_eq!(
        infer_ok("none as ?User", &schema).ty,
        QueryType::Record(RecordSource::Schema(user_id)).wrap_optional()
    );

    let schema = enum_schema();
    let priority_id = schema.enum_by_name("Priority").unwrap().0;
    assert_eq!(
        infer_ok("none as ?Priority", &schema).ty,
        QueryType::Enum(priority_id).wrap_optional()
    );
}

#[test]
fn annotations_record_equality() {
    let schema = annotation_schema();
    for source in [
        "{a = none, b = (1, 2)} as Item",
        "{b = (1, 2), a = none} as {b: (Int, Int), a: ?String}",
        "items.first().unwrap() as {a: ?String, b: (Int, Int)}",
    ] {
        let result = infer_ok(source, &schema);
        assert!(!result.ty.is_unknown(), "source: {source}");
        assert!(
            !matches!(&result.kind, TypedExprKind::Cast { .. }),
            "source should be a no-op: {source}"
        );
    }
}

#[test]
fn annotations_ambiguous_and_invalid_casts() {
    let schema = test_schema();
    for source in ["none as Int", "[] as Int", "[none] as List<Int>"] {
        assert_eq!(
            error_codes(source, &schema),
            vec!["QT0034"],
            "source: {source}"
        );
    }
    assert_eq!(
        error_codes("{a = none} as Item", &annotation_schema()),
        vec!["QT0034"]
    );

    for source in [
        "none as ?String as Int",
        "[] as List<Int> as List<Float>",
        "1 as Bool",
        "1 as User",
    ] {
        assert_eq!(
            error_codes(source, &schema),
            vec!["QT0013"],
            "source: {source}"
        );
    }
}

#[test]
fn bytes_comparison() {
    let schema = test_schema();
    for source in [
        r#"hex"00" == hex"01""#,
        r#"hex"00" != hex"01""#,
        r#"hex"00" < hex"01""#,
        r#"hex"00" <= hex"01""#,
        r#"hex"00" > hex"01""#,
        r#"hex"00" >= hex"01""#,
        r#"hex"00" in (hex"00", hex"01")"#,
    ] {
        assert_eq!(
            infer_ok(source, &schema).ty,
            QueryType::Scalar(ScalarType::Bool),
            "source: {source}"
        );
    }
    assert_eq!(
        infer_ok(r#"[hex"02", hex"01"].sort()"#, &schema).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Bytes)))
    );
    assert_eq!(
        infer_ok(r#"[hex"02", hex"01"].min()"#, &schema).ty,
        QueryType::Scalar(ScalarType::Bytes).wrap_optional()
    );
    assert_eq!(
        infer_ok(r#"[hex"02", hex"01"].max()"#, &schema).ty,
        QueryType::Scalar(ScalarType::Bytes).wrap_optional()
    );
}

#[test]
fn infer_bool_literal() {
    assert_eq!(
        infer_ok("true", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn infer_instant_literal() {
    assert_eq!(
        infer_ok("@now", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Instant)
    );
}

#[test]
fn infer_duration_literal() {
    assert_eq!(
        infer_ok("#30d", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn none_without_context_unknown() {
    assert_eq!(
        infer_ok("none", &test_schema()).ty,
        QueryType::Unknown.wrap_optional()
    );
}

#[test]
fn root_lookup() {
    let schema = test_schema();
    let env = TypeEnv::new(&schema);
    let (entry, depth) = env.resolve("users").unwrap();
    assert!(matches!(entry.ty, QueryType::List(_)));
    assert_eq!(depth, 0);
}

#[test]
fn resolve_returns_depth() {
    let schema = test_schema();
    let user = schema.roots[0].struct_id;
    let mut env = TypeEnv::new(&schema);
    assert_eq!(env.resolve("users").unwrap().1, 0);
    env.push_scope();
    env.define(
        "name".to_string(),
        QueryType::Scalar(ScalarType::String),
        field_binding(&schema, user, "name"),
    );
    assert_eq!(env.resolve("name").unwrap().1, 0);
    assert_eq!(env.resolve("users").unwrap().1, 1);
    assert!(env.resolve("missing").is_none());
}

fn field_binding(schema: &ValidatedSchema, struct_id: StructId, field: &str) -> IdentBinding {
    let local = schema.structs[struct_id.index()]
        .fields
        .iter()
        .position(|f| f.name() == field)
        .unwrap();
    IdentBinding::Field(FieldId::new(struct_id, local))
}

#[test]
fn ident_binding_root() {
    let result = infer_ok("users", &test_schema());
    let TypedExprKind::Ident {
        name,
        binding,
        depth,
    } = &result.kind
    else {
        panic!("expected ident, got {:?}", result.kind);
    };
    assert_eq!(name.value, "users");
    assert_eq!(*binding, IdentBinding::Root { root_idx: 0 });
    assert_eq!(*depth, 0);
}

#[test]
fn ident_binding_scoped_field() {
    let schema = test_schema();
    let user = schema.roots[0].struct_id;
    let result = infer_ok("users[active]", &schema);
    let TypedExprKind::Method { args, .. } = &result.kind else {
        panic!("expected method, got {:?}", result.kind);
    };
    let TypedExprKind::Ident {
        name,
        binding,
        depth,
    } = &args[0].expr.kind
    else {
        panic!("expected ident, got {:?}", args[0].expr.kind);
    };
    assert_eq!(name.value, "active");
    assert_eq!(*binding, field_binding(&schema, user, "active"));
    assert_eq!(*depth, 0);
}

#[test]
fn ident_binding_scoped_ref_field() {
    let schema = test_schema();
    let user = schema.roots[0].struct_id;
    let result = infer_ok("users[profile.is_some()]", &schema);
    let TypedExprKind::Method { args, .. } = &result.kind else {
        panic!("expected method, got {:?}", result.kind);
    };
    let TypedExprKind::Method { base, .. } = &args[0].expr.kind else {
        panic!("expected method, got {:?}", args[0].expr.kind);
    };
    let TypedExprKind::Ident {
        name,
        binding,
        depth,
    } = &base.kind
    else {
        panic!("expected ident, got {:?}", base.kind);
    };
    assert_eq!(name.value, "profile");
    assert_eq!(*binding, field_binding(&schema, user, "profile"));
    assert_eq!(*depth, 0);
}

#[test]
fn ident_binding_projected_field() {
    let result = infer_ok("users { name } { name }", &test_schema());
    let TypedExprKind::Projection { items, .. } = &result.kind else {
        panic!("expected projection, got {:?}", result.kind);
    };
    let TypedExprKind::Ident {
        name,
        binding,
        depth,
    } = &items[0].value.kind
    else {
        panic!("expected ident, got {:?}", items[0].value.kind);
    };
    assert_eq!(name.value, "name");
    assert_eq!(
        *binding,
        IdentBinding::ProjectedField {
            name: "name".to_string()
        }
    );
    assert_eq!(*depth, 0);
}

#[test]
fn ident_binding_projected_scoped_field() {
    let result = infer_ok(r#"users { name }[name == "Alice"]"#, &test_schema());
    let TypedExprKind::Method { args, .. } = &result.kind else {
        panic!("expected method, got {:?}", result.kind);
    };
    let TypedExprKind::Binary { lhs, .. } = &args[0].expr.kind else {
        panic!("expected binary, got {:?}", args[0].expr.kind);
    };
    let TypedExprKind::Ident {
        name,
        binding,
        depth,
    } = &lhs.kind
    else {
        panic!("expected ident, got {:?}", lhs.kind);
    };
    assert_eq!(name.value, "name");
    assert_eq!(
        *binding,
        IdentBinding::ProjectedField {
            name: "name".to_string()
        }
    );
    assert_eq!(*depth, 0);
}

#[test]
fn let_statement_binds_value_type() {
    let (file, _) = crate::parse_query("let x = 42; x");
    let (typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    let typed = typed.unwrap();
    let TypedStatement::Let(let_stmt) = &typed.statements[0] else {
        panic!("expected let statement");
    };
    assert_eq!(let_stmt.name.value, "x");
    assert_eq!(let_stmt.value.ty, QueryType::Scalar(ScalarType::Int));
    let TypedExprKind::Ident { binding, depth, .. } = &typed.result.as_ref().unwrap().kind else {
        panic!("expected ident result");
    };
    assert_eq!(*binding, IdentBinding::Let);
    assert_eq!(*depth, 0);
    assert_eq!(
        typed.result.as_ref().unwrap().ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn statement_only_file_has_no_result() {
    let (file, _) = crate::parse_query("let x = 1;");
    let (typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    let typed = typed.unwrap();
    assert_eq!(typed.statements.len(), 1);
    assert!(typed.result.is_none());
}

#[test]
fn trailing_semicolon_result_valid() {
    let (file, _) = crate::parse_query("42;");
    let (typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    let typed = typed.unwrap();
    assert!(typed.statements.is_empty());
    assert_eq!(
        typed.result.as_ref().unwrap().ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn type_bindings_manage_entries() {
    let mut bindings = TypeBindings::new();
    assert!(bindings.is_empty());
    bindings.define("n", QueryType::Scalar(ScalarType::Int));
    bindings.define("s", QueryType::Scalar(ScalarType::String));
    assert_eq!(bindings.len(), 2);
    assert_eq!(bindings.get("n"), Some(&QueryType::Scalar(ScalarType::Int)));
    assert_eq!(bindings.iter().count(), 2);
    assert_eq!(
        bindings.remove("n"),
        Some(QueryType::Scalar(ScalarType::Int))
    );
    assert!(bindings.get("n").is_none());
    bindings.clear();
    assert!(bindings.is_empty());
}

#[test]
fn session_binding_resolves_at_depth_one() {
    let mut bindings = TypeBindings::new();
    bindings.define("min", QueryType::Scalar(ScalarType::Int));

    let (file, _) = crate::parse_query("min");
    let (typed, diags) = typecheck_with_bindings(file.unwrap(), &test_schema(), &bindings);
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    let typed = typed.unwrap();
    let output = typed.result.as_ref().unwrap();
    assert_eq!(output.ty, QueryType::Scalar(ScalarType::Int));
    let TypedExprKind::Ident {
        name,
        binding,
        depth,
    } = &output.kind
    else {
        panic!("expected ident output");
    };
    assert_eq!(name.value, "min");
    assert_eq!(*binding, IdentBinding::Let);
    assert_eq!(*depth, 1);
}

#[test]
fn session_binding_shadow_root() {
    let mut bindings = TypeBindings::new();
    bindings.define("users", QueryType::Scalar(ScalarType::Int));

    let (file, _) = crate::parse_query("users");
    let (typed, diags) = typecheck_with_bindings(file.unwrap(), &test_schema(), &bindings);
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    let typed = typed.unwrap();
    let output = typed.result.as_ref().unwrap();
    assert_eq!(output.ty, QueryType::Scalar(ScalarType::Int));
    let TypedExprKind::Ident {
        name,
        binding,
        depth,
    } = &output.kind
    else {
        panic!("expected ident output");
    };
    assert_eq!(name.value, "users");
    assert_eq!(*binding, IdentBinding::Let);
    assert_eq!(*depth, 1);
}

#[test]
fn turn_let_shadows_session_binding() {
    let mut bindings = TypeBindings::new();
    bindings.define("min", QueryType::Scalar(ScalarType::Int));

    let (file, _) = crate::parse_query("let min = 5; users[age > min]");
    let (typed, diags) = typecheck_with_bindings(file.unwrap(), &test_schema(), &bindings);
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    let typed = typed.unwrap();
    let output = typed.result.as_ref().unwrap();
    let TypedExprKind::Method { args, .. } = &output.kind else {
        panic!("expected filter method");
    };
    let TypedExprKind::Binary { rhs, .. } = &args[0].expr.kind else {
        panic!("expected binary argument");
    };
    let TypedExprKind::Ident {
        name,
        binding,
        depth,
    } = &rhs.kind
    else {
        panic!("expected ident");
    };
    assert_eq!(name.value, "min");
    assert_eq!(*binding, IdentBinding::Let);
    assert_eq!(*depth, 1);
}

#[test]
fn typed_file_exposes_output_type_and_lets() {
    let (file, _) = crate::parse_query("let x = 1; x");
    let (typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    let typed = typed.unwrap();
    assert_eq!(
        typed.output_type(),
        Some(&QueryType::Scalar(ScalarType::Int))
    );
    let lets: Vec<_> = typed
        .let_bindings()
        .map(|(name, ty)| (name.to_string(), ty.clone()))
        .collect();
    assert_eq!(
        lets,
        vec![("x".to_string(), QueryType::Scalar(ScalarType::Int))]
    );
}

#[test]
fn let_chain_scope_visible() {
    let (file, _) = crate::parse_query("let a = 1; let b = a; b");
    let (typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    assert_eq!(typed.unwrap().statements.len(), 2);
}

#[test]
fn let_collection_type() {
    let (file, _) = crate::parse_query("let names = users { name }; names");
    let (typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    let typed = typed.unwrap();
    let TypedStatement::Let(let_stmt) = &typed.statements[0] else {
        panic!("expected let statement");
    };
    assert!(matches!(let_stmt.value.ty, QueryType::List(_)));
    assert_eq!(typed.result.as_ref().unwrap().ty, let_stmt.value.ty);
}

#[test]
fn let_use_before_def() {
    let (file, _) = crate::parse_query("let x = y;");
    let (_, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.iter().any(|d| d.code.as_ref() == "QT0001"));
}

#[test]
fn let_self_reference_error() {
    let (file, _) = crate::parse_query("let x = x;");
    let (_, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.iter().any(|d| d.code.as_ref() == "QT0001"));
}

#[test]
fn let_shadows_root() {
    let (file, _) = crate::parse_query("let users = 1; users");
    let (typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    let typed = typed.unwrap();
    let TypedExprKind::Ident { binding, .. } = &typed.result.as_ref().unwrap().kind else {
        panic!("expected ident result");
    };
    assert_eq!(*binding, IdentBinding::Let);
}

#[test]
fn let_rebind_same_scope() {
    let (file, _) = crate::parse_query("let x = 1; let x = 2; x");
    let (typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    assert_eq!(
        typed.unwrap().result.as_ref().unwrap().ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn let_in_query() {
    let (file, _) = crate::parse_query("let limit = 30; users[age > limit]");
    let (typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    assert!(matches!(
        typed.unwrap().result.as_ref().unwrap().ty,
        QueryType::List(_)
    ));
}

#[test]
fn ident_binding_prev() {
    let schema = test_schema();
    let user = schema.roots[0].struct_id;
    let (file, _) = crate::parse_query(r#"users[age == 30].update({ age = prev.age + 1 });"#);
    let (typed, diags) = typecheck(file.unwrap(), &schema);
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    let TypedStatement::Mutation(TypedMutationStmt::Update { arg, .. }) =
        &typed.unwrap().statements[0]
    else {
        panic!("expected update statement");
    };
    let TypedExprKind::Struct { fields, .. } = &arg.kind else {
        panic!("expected struct, got {:?}", arg.kind);
    };
    let TypedExprKind::Binary { lhs, .. } = &fields[0].1.kind else {
        panic!("expected binary, got {:?}", fields[0].1.kind);
    };
    let TypedExprKind::Field { base, .. } = &lhs.kind else {
        panic!("expected field, got {:?}", lhs.kind);
    };
    let TypedExprKind::Ident {
        name,
        binding,
        depth,
    } = &base.kind
    else {
        panic!("expected ident, got {:?}", base.kind);
    };
    assert_eq!(name.value, "prev");
    assert_eq!(*binding, IdentBinding::Prev { struct_id: user });
    assert_eq!(*depth, 0);
}

#[test]
fn root_unknown_identifier() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("posts");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn field_access_struct() {
    assert_eq!(
        infer_ok("users.name", &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
    );
}

#[test]
fn field_access_unknown_field() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("users.unknown");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn optional_field_access() {
    assert!(
        matches!(infer_ok("users.profile", &test_schema()).ty, QueryType::List(inner) if matches!(*inner, QueryType::Optional(_)))
    );
}

#[test]
fn deep_optional_tuple_access() {
    assert_eq!(
        infer_ok("some(some((1, 2)))??.1", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
            .wrap_optional()
            .wrap_optional()
    );
}

#[test]
fn deep_optional_field_and_method() {
    assert_eq!(
        infer_ok(
            r#"some(some({name = "Ada"}))??.name??.to_upper()"#,
            &test_schema()
        )
        .ty,
        QueryType::Scalar(ScalarType::String)
            .wrap_optional()
            .wrap_optional()
    );
}

#[test]
fn deep_optional_list_method() {
    assert_eq!(
        infer_ok("some(some([1, 2]))??.map($ + 1)", &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
            .wrap_optional()
            .wrap_optional()
    );
}

#[test]
fn optional_map_types() {
    assert_eq!(
        infer_ok("some(1).map($ + 1)", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int).wrap_optional()
    );
    assert_eq!(
        infer_ok("some(some(1))?.map($ + 1)", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
            .wrap_optional()
            .wrap_optional()
    );
}

#[test]
fn optional_flatten_types() {
    assert_eq!(
        infer_ok("some(some(1)).flatten()", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int).wrap_optional()
    );
    assert_eq!(
        infer_ok("some(some(some(1))).flatten()", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
            .wrap_optional()
            .wrap_optional()
    );
}

#[test]
fn optional_list_methods() {
    assert_eq!(
        infer_ok("[some(1), none].compact()", &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
    );
    assert_eq!(
        infer_ok("[some(1), none].transpose()", &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int))).wrap_optional()
    );
}

#[test]
fn list_boundary_methods() {
    assert_eq!(
        infer_ok("[1, 2].last()", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int).wrap_optional()
    );
    assert_eq!(
        infer_ok("[1, 2].take_back(1)", &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
    );
}

#[test]
fn list_predicate_methods() {
    assert_eq!(
        infer_ok("[1, 2].any($ > 0)", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
    assert_eq!(
        infer_ok("[1, 2].find($ > 0)", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int).wrap_optional()
    );
}

#[test]
fn string_split_and_scalar_casts() {
    assert_eq!(
        infer_ok(r#""a,b".split(",")"#, &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
    );
    assert_eq!(
        infer_ok("42 as String", &test_schema()).ty,
        QueryType::Scalar(ScalarType::String)
    );
}

#[test]
fn optional_depth_mismatch() {
    let schema = test_schema();
    let (file, _diags) = crate::parse_query("1?.abs()");
    let (typed, diags) = typecheck(file.unwrap(), &schema);
    assert!(typed.is_none());
    assert_eq!(diags[0].code.as_ref(), "QT0032");
    let (file, _diags) = crate::parse_query("some(some(2))???.abs()");
    let (typed, diags) = typecheck(file.unwrap(), &schema);
    assert!(typed.is_none());
    assert_eq!(diags[0].code.as_ref(), "QT0032");
    let (file, _diags) = crate::parse_query("some(some(3))?.abs()");
    let (typed, diags) = typecheck(file.unwrap(), &schema);
    assert!(typed.is_none());
    assert_eq!(diags[0].code.as_ref(), "QT0003");
}

#[test]
fn optional_method_inner_base() {
    assert_eq!(
        infer_ok("some(some(some(1)))??.unwrap()", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
            .wrap_optional()
            .wrap_optional()
    );
}

#[test]
fn scalar_method() {
    assert_eq!(
        infer_ok(r#""xyz".starts_with("abc")"#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

fn collect_node_ids(expr: &TypedExpr, ids: &mut Vec<NodeId>) {
    ids.push(expr.id);
    match &expr.kind {
        TypedExprKind::Literal(_)
        | TypedExprKind::Ident { .. }
        | TypedExprKind::EnumVariant { .. }
        | TypedExprKind::TypeConstant { .. } => {}
        TypedExprKind::Some { value } => collect_node_ids(value, ids),
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            for element in elements {
                collect_node_ids(element, ids);
            }
        }
        TypedExprKind::Struct { fields } => {
            for (_, value) in fields {
                collect_node_ids(value, ids);
            }
        }
        TypedExprKind::Projection { base, items } => {
            collect_node_ids(base, ids);
            for item in items {
                collect_node_ids(&item.value, ids);
            }
        }
        TypedExprKind::Binary { lhs, rhs, .. } => {
            collect_node_ids(lhs, ids);
            collect_node_ids(rhs, ids);
        }
        TypedExprKind::Unary { expr, .. } | TypedExprKind::Cast { expr, .. } => {
            collect_node_ids(expr, ids);
        }
        TypedExprKind::RowRef { expr } => collect_node_ids(expr, ids),
        TypedExprKind::Method { base, args, .. } => {
            collect_node_ids(base, ids);
            for arg in args {
                collect_node_ids(&arg.expr, ids);
            }
        }
        TypedExprKind::If { arms, default } => {
            for (cond, value) in arms {
                collect_node_ids(cond, ids);
                collect_node_ids(value, ids);
            }
            collect_node_ids(default, ids);
        }
        TypedExprKind::Field { base, .. } | TypedExprKind::TupleAccess { base, .. } => {
            collect_node_ids(base, ids);
        }
    }
}

#[test]
fn node_ids_are_unique() {
    let result = infer_ok(
        r#"users { name, shout = name.to_upper() } { name }"#,
        &test_schema(),
    );
    let mut ids = Vec::new();
    collect_node_ids(&result, &mut ids);
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(ids.len(), sorted.len(), "duplicate node ids: {ids:?}");
    assert!(!ids.is_empty());
}

#[test]
fn list_method_len() {
    assert_eq!(
        infer_ok("users.len()", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn list_method_nth() {
    assert!(
        matches!(infer_ok("users.nth(0)", &test_schema()).ty, QueryType::Optional(inner)
            if matches!(*inner, QueryType::Record(_))
        )
    );
}

#[test]
fn method_unwrap_access() {
    assert_eq!(
        infer_ok("users.first().unwrap().name", &test_schema()).ty,
        QueryType::Scalar(ScalarType::String)
    );
}

#[test]
fn optional_method_is_some() {
    assert_eq!(
        infer_ok("users.first().is_some()", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn method_unknown() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("users.name.nonexistent()");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

const NESTED_SCHEMA_SRC: &str = r#"
        root users: User;
        struct User {
            name: String,
            active: Bool,
            manager: &User,
            subordinates: &List<User>,
            orders: List<Order>,
        }
        struct Order {
            total: Dec,
            user: &User,
            items: List<Item>,
        }
        struct Item {
            sku: String,
            order: &Order,
        }
        rel User.manager <<-> User.subordinates (users.manager_id -> users.id);
        rel Order.user <<-> User.orders (orders.user_id -> users.id);
        rel Item.order <<-> Order.items (items.order_id -> orders.id);
    "#;

fn nested_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(NESTED_SCHEMA_SRC)
            .0
            .expect("invalid test schema"),
    )
    .0
    .expect("invalid test schema")
}

fn nested_struct(schema: &ValidatedSchema, name: &str) -> StructId {
    StructId::new(
        schema
            .structs
            .iter()
            .position(|s| s.name == name)
            .unwrap_or_else(|| panic!("missing struct {name}")),
    )
}

#[test]
fn flatten_nested_collection() {
    let schema = nested_schema();
    assert_eq!(
        infer_ok("users.orders.flatten()", &schema).ty,
        QueryType::List(Box::new(QueryType::Record(RecordSource::Schema(
            nested_struct(&schema, "Order")
        ))))
    );
}

#[test]
fn flatten_compose_first() {
    let schema = nested_schema();
    assert_eq!(
        infer_ok("users.orders.flatten().first()", &schema).ty,
        QueryType::Record(RecordSource::Schema(nested_struct(&schema, "Order"))).wrap_optional()
    );
}

#[test]
fn flatten_compose_optional() {
    let schema = nested_schema();
    assert_eq!(
        infer_ok("users.orders.first()?.items?.flatten()", &schema).ty,
        QueryType::List(Box::new(QueryType::Record(RecordSource::Schema(
            nested_struct(&schema, "Item")
        ))))
        .wrap_optional()
    );
}

#[test]
fn flatten_flat_rejected() {
    let schema = nested_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("users.name.flatten()");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn flatten_scalar_rejected() {
    let schema = nested_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("1.flatten()");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn method_wrong_arg_count() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("users.name.contains(\"a\", \"b\")");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn method_arg_constraint_valid() {
    infer_ok("users.sum_over(age)", &test_schema());
}

#[test]
fn method_arg_constraint_invalid() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("users.sort_asc_over(profile)");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn sort_no_arg_over_ordered() {
    assert_eq!(
        infer_ok("[3, 1, 2].sort()", &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
    );
}

#[test]
fn sort_no_arg_over_tuple() {
    infer_ok("[(\"b\", 1), (\"a\", 2)].sort()", &test_schema());
}

#[test]
fn sort_no_arg_over_records_rejected() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("users.sort()");
    assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
}

#[test]
fn sort_over_single_and_multi_key() {
    let schema = test_schema();
    assert!(matches!(
        infer_ok("users.sort_over(age)", &schema).ty,
        QueryType::List(_)
    ));
    infer_ok("users.sort_desc_over(age, name)", &schema);
}

#[test]
fn tuple_access() {
    let schema = test_schema();
    assert_eq!(
        infer_ok("(\"a\", 1).0", &schema).ty,
        QueryType::Scalar(ScalarType::String)
    );
    assert_eq!(
        infer_ok("(\"a\", 1).1", &schema).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn tuple_access_chained() {
    assert_eq!(
        infer_ok("((\"a\", 1), 2).0.1", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn tuple_access_optional() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("(\"a\", 1)?.0");
    assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
}

#[test]
fn tuple_access_out_of_range() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    for query in ["(\"a\", 1).2", "(\"a\", 1).1000"] {
        let (file, _diags) = crate::parse_query(query);
        assert!(
            infer(file.unwrap().output().unwrap(), &mut env).is_err(),
            "{query}"
        );
    }
}

#[test]
fn tuple_access_on_non_tuple() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    for query in ["\"a\".0", "users.0", "[1, 2].0"] {
        let (file, _diags) = crate::parse_query(query);
        assert!(
            infer(file.unwrap().output().unwrap(), &mut env).is_err(),
            "{query}"
        );
    }
}

#[test]
fn tuple_access_optional_tuple() {
    assert_eq!(
        infer_ok("[(\"a\", 1)].first()?.0", &test_schema()).ty,
        QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::String)))
    );
}

#[test]
fn tuple_access_optional_schema_field() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    assert!(matches!(
        infer_ok("users { x = maybe_pos?.0 }", &schema).ty,
        QueryType::List(_)
    ));
    let (file, _diags) = crate::parse_query("users { x = maybe_pos.0 }");
    assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
}

#[test]
fn tuple_access_over_scope() {
    assert!(matches!(
        infer_ok("users { x = (\"a\", age).1 }", &test_schema()).ty,
        QueryType::List(_)
    ));
}

#[test]
fn dollar_sum_over() {
    assert_eq!(
        infer_ok("[1, 2, 3].sum_over($ + 1)", &test_schema()).ty,
        QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int)))
    );
}

#[test]
fn dollar_filter() {
    assert_eq!(
        infer_ok("[-1, -2, 2, 3][$ > 0]", &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
    );
}

#[test]
fn dollar_map_transformation() {
    let schema = test_schema();
    assert_eq!(
        infer_ok("[1, 2, 3].map($ - 1)", &schema).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
    );
    assert!(
        matches!(infer_ok("[1, 2].map(($, $))", &schema).ty, QueryType::List(inner)
            if matches!(inner.as_ref(), QueryType::Tuple(_)))
    );
}

#[test]
fn dollar_over_family() {
    for (query, is_list) in [
        ("[1, 2, 3].avg_over($ + 3)", false),
        ("[3, 1, 2].max_by($ + 2)", false),
        ("[3, 1, 2].sort_over($ + 1, -$)", true),
    ] {
        let result = infer_ok(query, &test_schema());
        if is_list {
            assert!(matches!(result.ty, QueryType::List(_)));
        } else {
            assert_eq!(
                result.ty,
                QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int)))
            );
        }
    }
}

#[test]
fn dollar_binds_element() {
    let schema = test_schema();
    let result = infer_ok("[1, 2].map($)", &schema);
    let TypedExprKind::Method { args, .. } = &result.kind else {
        panic!("expected method");
    };
    let TypedExprKind::Ident { name, binding, .. } = &args[0].expr.kind else {
        panic!("expected ident");
    };
    assert_eq!(name.value, "$");
    assert_eq!(*binding, IdentBinding::ListElement);
    assert_eq!(
        infer_ok("[[1, 2], [3]].map($.map($$ + 1))", &schema).ty,
        QueryType::List(Box::new(QueryType::List(Box::new(QueryType::Scalar(
            ScalarType::Int
        )))))
    );
}

#[test]
fn dollar_outer_access() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    assert_eq!(
        infer_ok("[[1, 2], [3]].map($.map($$ + $.len()))", &schema).ty,
        QueryType::List(Box::new(QueryType::List(Box::new(QueryType::Scalar(
            ScalarType::Int
        )))))
    );
    assert!(matches!(
        infer_ok(
            "[[[1]], [[2, 3]]].map($.map($$.map($$$ + $$.len() + $.len())))",
            &schema,
        )
        .ty,
        QueryType::List(_)
    ));
    let (file, _diags) = crate::parse_query("[[1, 2], [3]].map($ + $$.len())");
    assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
}

#[test]
fn dollar_through_record_scope() {
    assert!(matches!(
        infer_ok("[1, 2].map(users.filter(age > $))", &test_schema()).ty,
        QueryType::List(_)
    ));
}

#[test]
fn dollar_not_in_record_scope() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("users.filter($)");
    assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
    infer_ok("users.filter(age > 0)", &schema);
    let (file, _diags) = crate::parse_query("users.map(name)");
    assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
    infer_ok("users.map($)", &schema);
}

#[test]
fn dollar_outside_scope_rejected() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _diags) = crate::parse_query("$ + 1");
    assert!(infer(file.unwrap().output().unwrap(), &mut env).is_err());
    let (file, _diags) = crate::parse_query("[].map($)");
    let (typed, diags) = typecheck(file.unwrap(), &schema);
    assert!(typed.is_none() && !diags.is_empty());
}

#[test]
fn binary_int_add() {
    assert_eq!(
        infer_ok("1 + 2", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn binary_int_sub() {
    assert_eq!(
        infer_ok("5 - 3", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn binary_int_mul() {
    assert_eq!(
        infer_ok("4 * 3", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn binary_int_div() {
    assert_eq!(
        infer_ok("10 / 2", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn binary_int_rem() {
    assert_eq!(
        infer_ok("10 % 3", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn binary_int_mod() {
    assert_eq!(
        infer_ok("10 mod 3", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn binary_float_add() {
    assert_eq!(
        infer_ok("1.0f + 2.0f", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Float)
    );
}

#[test]
fn binary_dec_add() {
    assert_eq!(
        infer_ok("1.0 + 2.0", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Dec)
    );
}

#[test]
fn binary_int_float_mismatch() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("1 + 2.0f");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn binary_float_dec_mismatch() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("1.0f + 2.0");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn binary_duration_add() {
    assert_eq!(
        infer_ok("#30d + #5h", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn binary_duration_sub() {
    assert_eq!(
        infer_ok("#30d - #5h", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn binary_duration_mul_int() {
    assert_eq!(
        infer_ok("#30d * 3", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn binary_int_mul_duration() {
    assert_eq!(
        infer_ok("3 * #30d", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn binary_duration_mul_float() {
    assert_eq!(
        infer_ok("#30d * 1.5f", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn binary_duration_mul_dec() {
    assert_eq!(
        infer_ok("#30d * 1.5", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn binary_duration_div_int() {
    assert_eq!(
        infer_ok("#30d / 2", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn binary_duration_rem_int_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("#30d % 7");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn binary_duration_mod_int_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("#30d mod 7");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn binary_duration_div_duration() {
    assert_eq!(
        infer_ok("#90s / #30s", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Dec)
    );
}

#[test]
fn binary_duration_rem_duration() {
    assert_eq!(
        infer_ok("#90s % #7s", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn binary_duration_mod_duration() {
    assert_eq!(
        infer_ok("#90s mod #7s", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn binary_duration_add_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("#30d + 3");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn binary_duration_mul_duration_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("#30d * #5h");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn binary_comparison_eq() {
    assert_eq!(
        infer_ok("1 == 2", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_comparison_ne() {
    assert_eq!(
        infer_ok("1.0f != 2e5f", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_comparison_lt() {
    assert_eq!(
        infer_ok("1 < 2", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_comparison_gt() {
    assert_eq!(
        infer_ok("10.5 > 11.0", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_comparison_le() {
    assert_eq!(
        infer_ok("20 <= 40", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_comparison_ge() {
    assert_eq!(
        infer_ok("1001 >= 1000", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_string_eq() {
    assert_eq!(
        infer_ok(r#""a" == "b""#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_string_lt() {
    assert_eq!(
        infer_ok(r#""x" < "y""#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_comparison_type_mismatch() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query(r#"1 == "a""#);
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn binary_logical_and() {
    assert_eq!(
        infer_ok("true && false", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_logical_or() {
    assert_eq!(
        infer_ok("false || true", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_logical_and_non_bool_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("1 && 2");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn binary_in_string_tuple() {
    assert_eq!(
        infer_ok(r#""a" in ("a", "b", "c")"#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_in_non_tuple_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("1 in 2");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn binary_in_type_mismatch() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query(r#"1 in ("a", "b")"#);
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn optional_eq_optional() {
    assert_eq!(
        infer_ok(
            "users.first().unwrap().profile == users.first().unwrap().profile",
            &test_schema(),
        )
        .ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn optional_eq_non_optional_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("none != 1");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn optional_ordering_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("none < none");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn unary_neg_int() {
    assert_eq!(
        infer_ok("-1", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn unary_neg_float() {
    assert_eq!(
        infer_ok("-3e6f", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Float)
    );
}

#[test]
fn unary_neg_dec() {
    assert_eq!(
        infer_ok("-12.5", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Dec)
    );
}

#[test]
fn unary_neg_duration() {
    assert_eq!(
        infer_ok("-#30d", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn unary_neg_instant_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("-@now");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn unary_neg_string_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query(r#"-"hello""#);
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn unary_not_bool() {
    assert_eq!(
        infer_ok("!true", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn unary_not_type_mismatch() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("!1");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn binary_complex_arithmetic() {
    assert_eq!(
        infer_ok("1 + 2 * (3 / 2) % (7 - 2)", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn binary_arithmetic_comparison() {
    assert_eq!(
        infer_ok("(1 + 2 - 3) >= 3", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_field_arithmetic() {
    assert_eq!(
        infer_ok("users.first().unwrap().age + 1", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn binary_field_comparison() {
    assert_eq!(
        infer_ok("users.first().unwrap().age < 18", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn binary_duration_arithmetic() {
    assert_eq!(
        infer_ok("#30d + #24h - #7d + #7.8s", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Duration)
    );
}

#[test]
fn if_else_basic() {
    assert_eq!(
        infer_ok(r#"if true { 1 } else { 2 }"#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn if_else_bool_result() {
    assert_eq!(
        infer_ok(r#"if true { false } else { true }"#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn if_else_condition_type_mismatch() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query(r#"if 42 { 1 } else { 2 }"#);
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn if_else_branch_type_mismatch() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query(r#"if true { 1 } else { "two" }"#);
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn if_else_if_chain() {
    assert_eq!(
        infer_ok(
            r#"if true { "A" } else if false { "B" } else { "C" }"#,
            &test_schema(),
        )
        .ty,
        QueryType::Scalar(ScalarType::String)
    );
}

#[test]
fn if_else_with_comparison_condition() {
    assert_eq!(
        infer_ok(
            r#"if users.first().unwrap().age > 18 { "adult" } else { "minor" }"#,
            &test_schema(),
        )
        .ty,
        QueryType::Scalar(ScalarType::String)
    );
}

#[test]
fn cast_int_to_float() {
    assert_eq!(
        infer_ok("42 as Float", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Float)
    );
}

#[test]
fn cast_int_to_dec() {
    assert_eq!(
        infer_ok("42 as Dec", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Dec)
    );
}

#[test]
fn cast_float_to_int() {
    assert_eq!(
        infer_ok("1.0f as Int", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn cast_float_to_dec() {
    assert_eq!(
        infer_ok("1.0f as Dec", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Dec)
    );
}

#[test]
fn cast_dec_to_int() {
    assert_eq!(
        infer_ok("1.0 as Int", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn cast_dec_to_float() {
    assert_eq!(
        infer_ok("1.0 as Float", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Float)
    );
}

#[test]
fn cast_bool_to_int() {
    assert_eq!(
        infer_ok("true as Int", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn cast_disallowed_type() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query(r#""hello" as Int"#);
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn cast_complex_expr() {
    assert_eq!(
        infer_ok("users.first().unwrap().age as Float + 1.0f", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Float)
    );
}

#[test]
fn fallible_cast_optional_result() {
    assert_eq!(
        infer_ok(r#""42" as? Int"#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int).wrap_optional()
    );
    assert_eq!(
        infer_ok(r#""true" as? Bool"#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool).wrap_optional()
    );
}

#[test]
fn fallible_cast_strict_rejection() {
    for source in [
        "1 as? Int",
        "some(1) as? Float",
        "1 as? ?Int",
        "true as? Float",
    ] {
        assert_eq!(
            error_codes(source, &test_schema()),
            vec!["QT0013"],
            "source: {source}"
        );
    }
}

#[test]
fn fallible_enum_cast_matching_underlying_type_required() {
    let schema = enum_schema();
    assert_eq!(error_codes(r#""low" as? Level"#, &schema), vec!["QT0013"]);
    assert_eq!(error_codes("2 as? Priority", &schema), vec!["QT0013"]);
}

#[test]
fn some_basic() {
    assert_eq!(
        infer_ok(r#"some("hello")"#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::String).wrap_optional()
    );
}

#[test]
fn some_complex_expr() {
    assert_eq!(
        infer_ok("some(users.first().unwrap().age)", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int).wrap_optional()
    );
}

#[test]
fn some_none() {
    assert_eq!(
        infer_ok("none", &test_schema()).ty,
        QueryType::Unknown.wrap_optional()
    );
}

#[test]
fn some_nested_optional() {
    assert_eq!(
        infer_ok(r#"some(some("a"))"#, &test_schema()).ty,
        QueryType::Optional(Box::new(QueryType::Optional(Box::new(QueryType::Scalar(
            ScalarType::String
        )))))
    );
}

#[test]
fn infer_nested_optional() {
    assert_eq!(
        infer_ok(r#"some(some("a")) != some(none)"#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn array_int() {
    assert_eq!(
        infer_ok("[1, 2, 3]", &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
    );
}

#[test]
fn array_string() {
    assert_eq!(
        infer_ok(r#"["a", "b", "c"]"#, &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
    );
}

#[test]
fn array_type_mismatch() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query(r#"[1, "two", 3]"#);
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn array_types_inferred() {
    let result = infer_ok("[none, some(1), none]", &test_schema());
    assert_eq!(
        result.ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int).wrap_optional()))
    );
    match result.kind {
        TypedExprKind::Array { elements } => {
            assert_eq!(
                elements.into_iter().map(|e| e.ty).collect::<Vec<_>>(),
                vec![QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int))); 3]
            )
        }
        _ => panic!(),
    }
}

#[test]
fn array_empty() {
    assert_eq!(
        infer_ok("[]", &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Unknown))
    );
}

#[test]
fn tuple_mixed_types() {
    assert_eq!(
        infer_ok(r#"(1, "hello", false, @now, 1.7e-5)"#, &test_schema()).ty,
        QueryType::Tuple(vec![
            QueryType::Scalar(ScalarType::Int),
            QueryType::Scalar(ScalarType::String),
            QueryType::Scalar(ScalarType::Bool),
            QueryType::Scalar(ScalarType::Instant),
            QueryType::Scalar(ScalarType::Dec),
        ])
    );
}

#[test]
fn tuple_in_membership() {
    assert_eq!(
        infer_ok(r#"1 in (1, 2, 3)"#, &test_schema()).ty,
        QueryType::Scalar(ScalarType::Bool)
    );
}

#[test]
fn projection_basic() {
    assert!(matches!(
        infer_ok("users { name, age }", &test_schema()).ty,
        QueryType::List(inner)
        if matches!(
            *inner,
            QueryType::Record(RecordSource::Projection(ref fields))
            if fields.len() == 2
        )
    ));
}

#[test]
fn projection_single_field() {
    let result = infer_ok("users { name }", &test_schema());
    match &result.ty {
        QueryType::List(inner) => match inner.as_ref() {
            QueryType::Record(RecordSource::Projection(fields)) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].name, "name");
                assert_eq!(fields[0].ty, QueryType::Scalar(ScalarType::String));
            }
            other => panic!("expected Record, got {other:?}"),
        },
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn projection_computed_field() {
    let result = infer_ok(
        "users { name, adult = users.first().unwrap().age > 18 }",
        &test_schema(),
    );
    match &result.ty {
        QueryType::List(inner) => match inner.as_ref() {
            QueryType::Record(RecordSource::Projection(fields)) => {
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].name, "name");
                assert_eq!(fields[1].name, "adult");
                assert_eq!(fields[1].ty, QueryType::Scalar(ScalarType::Bool));
            }
            other => panic!("expected Record, got {other:?}"),
        },
        other => panic!("expected List, got {other:?}"),
    }
}

fn projected_fields(expr: TypedExpr) -> (Vec<String>, Vec<QueryType>) {
    let ty = match expr.ty {
        QueryType::List(inner) => *inner,
        ty => ty,
    };
    let QueryType::Record(RecordSource::Projection(fields)) = ty else {
        panic!("expected projection record, got {ty:?}");
    };
    fields
        .into_iter()
        .map(|field| (field.name, field.ty))
        .unzip()
}

#[test]
fn projection_spread_all_fields() {
    let result = infer_ok("users { ... }", &test_schema());
    assert_eq!(
        projected_fields(result).0,
        [
            "name",
            "age",
            "score",
            "balance",
            "active",
            "created",
            "profile",
            "maybe_pos",
        ]
    );
}

#[test]
fn projection_spread_existing_preserved() {
    let result = infer_ok("users { active, ... }", &test_schema());
    assert_eq!(
        projected_fields(result).0,
        [
            "active",
            "name",
            "age",
            "score",
            "balance",
            "created",
            "profile",
            "maybe_pos",
        ]
    );
}

#[test]
fn projection_spread_match_by_name() {
    let result = infer_ok("users { n = name, ... }", &test_schema());
    assert_eq!(
        projected_fields(result).0,
        [
            "n",
            "name",
            "age",
            "score",
            "balance",
            "active",
            "created",
            "profile",
            "maybe_pos",
        ]
    );

    let result = infer_ok("users { name = 1, ... }", &test_schema());
    let fields = projected_fields(result);
    assert_eq!(fields.0[0], "name");
    assert_eq!(fields.1[0], QueryType::Scalar(ScalarType::Int));
    assert_eq!(
        fields.0,
        [
            "name",
            "age",
            "score",
            "balance",
            "active",
            "created",
            "profile",
            "maybe_pos",
        ]
    );
}

#[test]
fn projection_spread_chained_projection() {
    let result = infer_ok("users { name, age } { age, ... }", &test_schema());
    assert_eq!(projected_fields(result).0, ["age", "name"]);
}

#[test]
fn projection_spread_literal() {
    let result = infer_ok("{ a = 1, b = 2 } { b, ... }", &test_schema());
    assert_eq!(projected_fields(result).0, ["b", "a"]);
}

#[test]
fn projection_spread_noop_all_fields_named() {
    let result = infer_ok(
        "users { name, age, score, balance, active, created, profile, maybe_pos, ... }",
        &test_schema(),
    );
    assert_eq!(projected_fields(result).0.len(), 8);
}

#[test]
fn projection_empty_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("users { }");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn projection_duplicate_field_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("users { name, name }");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn projection_on_non_record_error() {
    let schema = test_schema();
    let mut env = TypeEnv::new(&schema);
    let (file, _) = crate::parse_query("42 { name }");
    let result = infer(file.unwrap().output().unwrap(), &mut env);
    assert!(result.is_err());
}

#[test]
fn projection_derived_alias() {
    let result = infer_ok("users { name, profile?.username }", &test_schema());
    match &result.ty {
        QueryType::List(inner) => match inner.as_ref() {
            QueryType::Record(RecordSource::Projection(fields)) => {
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].name, "name");
                assert_eq!(fields[1].name, "username");
                assert_eq!(
                    fields[1].ty,
                    QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::String)))
                );
            }
            other => panic!("expected Record, got {other:?}"),
        },
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn projection_single_record_base() {
    assert!(matches!(
        infer_ok("users.first().unwrap() { name, age }", &test_schema()).ty,
        QueryType::Record(RecordSource::Projection(_))
    ));
}

#[test]
fn mutation_insert() {
    let (file, _diags) = crate::parse_query(
        r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now });"#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
}

#[test]
fn mutation_update() {
    let (file, _diags) = crate::parse_query(r#"users[age == 30].update({ age = 31 });"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
}

#[test]
fn mutation_update_with_prev() {
    let (file, _diags) = crate::parse_query(r#"users[age == 30].update({ age = prev.age + 1 });"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
}

#[test]
fn mutation_delete() {
    let (file, _diags) = crate::parse_query(r#"users[!active].delete();"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
}

#[test]
fn mutation_on_projection_error() {
    let (file, _diags) = crate::parse_query(r#"users { name }.insert({ name = "Alice" });"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn mutation_on_scalar_error() {
    let (file, _diags) = crate::parse_query(r#"42.insert({ name = "Alice" });"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn mutation_delete_arg_error() {
    let (file, _diags) = crate::parse_query(r#"users[!active].delete("abc");"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn mutation_extra_args_error() {
    let schema = test_schema();
    for source in [
        r#"users.insert({ name = "A" }, { name = "B" });"#,
        r#"users.update({ age = 1 }, { age = 2 });"#,
        r#"users.delete(1);"#,
    ] {
        assert_eq!(error_codes(source, &schema), ["QT0004"], "{source}");
    }
}

#[test]
fn mutation_missing_insert_arg_error() {
    assert_eq!(error_codes("users.insert();", &test_schema()), ["QT0004"]);
}

#[test]
fn mutation_optional_depth_preserved() {
    let typed = typecheck_ok("users.first()?.delete();", &test_schema());
    let TypedStatement::Mutation(TypedMutationStmt::Delete { opt_depth, .. }) =
        &typed.statements[0]
    else {
        panic!("expected delete mutation");
    };
    assert_eq!(*opt_depth, 1);
}

#[test]
fn mutation_record_targets() {
    let schema = test_schema();
    typecheck_ok("users.first().unwrap().delete();", &schema);
    typecheck_ok("users.first().unwrap().update({ age = 31 });", &schema);
    typecheck_ok("users.first()?.delete();", &schema);
    typecheck_ok("users.first()?.update({ age = 31 });", &schema);
}

#[test]
fn mutation_optional_target_errors() {
    let schema = test_schema();
    assert_eq!(error_codes("users.first().delete();", &schema), ["QT0035"]);
    assert_eq!(
        error_codes("users.first().update({ age = 31 });", &schema),
        ["QT0035"]
    );
    assert_eq!(
        error_codes("users.first()??.delete();", &schema),
        ["QT0032"]
    );
}

#[test]
fn mutation_let_targets_rejected() {
    let schema = mutation_schema();
    assert_eq!(
        error_codes("let u = users; u.delete();", &schema),
        ["QT0036"]
    );
    assert_eq!(
        error_codes(
            r#"let u = users; u.insert({ id = 1, name = "A", orders = [] });"#,
            &schema
        ),
        ["QT0036"]
    );
}

#[test]
fn mutation_insert_targets() {
    let schema = mutation_schema();
    typecheck_ok(
        r#"users.insert({ id = 1, name = "A", orders = [] });"#,
        &schema,
    );
    typecheck_ok(
        r#"users[id > 0].insert({ id = 1, name = "A", orders = [] });"#,
        &schema,
    );
    typecheck_ok(
        r#"users.first().unwrap().orders.insert({ id = 1, total = 10 });"#,
        &schema,
    );
    typecheck_ok(
        r#"users.first()?.orders?.insert({ id = 1, total = 10 });"#,
        &schema,
    );
    typecheck_ok(
        r#"images.first().unwrap().orders.insert(&users.orders.flatten());"#,
        &schema,
    );
    typecheck_ok(
        r#"images.first().unwrap().orders.insert(&users.orders.flatten().first().unwrap());"#,
        &schema,
    );

    assert_eq!(
        error_codes(
            r#"users.first().unwrap().insert({ id = 1, name = "A", orders = [] });"#,
            &schema
        ),
        ["QT0037"]
    );
    assert_eq!(
        error_codes(
            r#"users.orders.flatten().insert({ id = 1, total = 10 });"#,
            &schema
        ),
        ["QT0038"]
    );
    assert_eq!(
        error_codes(
            r#"users.extend(users.take(0)).insert({ id = 1, name = "A", orders = [] });"#,
            &schema
        ),
        ["QT0038"]
    );
    assert_eq!(
        error_codes(
            r#"images.first().unwrap().orders.insert({ id = 1, total = 10 });"#,
            &schema
        ),
        ["QT0047"]
    );
    assert_eq!(
        error_codes(
            r#"images.first().unwrap().orders.insert(users.orders.flatten());"#,
            &schema
        ),
        ["QT0047"]
    );
    assert_eq!(
        error_codes(
            r#"images.first().unwrap().orders.insert(&users.orders.flatten().first());"#,
            &schema
        ),
        ["QT0047"]
    );
    assert_eq!(
        error_codes(r#"images.first().unwrap().orders.insert(&users);"#, &schema),
        ["QT0045"]
    );
}

#[test]
fn mutation_nested_and_non_owning_delete_targets() {
    let schema = mutation_schema();
    typecheck_ok("users.orders.flatten().delete();", &schema);
    typecheck_ok("users.orders.flatten().update({ total = 1 });", &schema);
    typecheck_ok("users.first()?.orders?.delete();", &schema);
    typecheck_ok("images.first().unwrap().orders.delete();", &schema);
}

#[test]
fn mutation_non_owning_row_ref_fields() {
    let schema = nested_schema();
    for source in [
        "users.update({ manager = &users.first().unwrap() });",
        "users.update({ subordinates = &users });",
        "users.update({ subordinates = [] });",
    ] {
        typecheck_ok(source, &schema);
    }

    let schema = mutation_schema();
    for source in [
        "users.orders.flatten().update({ image = none });",
        "users.orders.flatten().update({ image = some(&images.first().unwrap()) });",
        "images.first().unwrap().update({ orders = &users.orders.flatten() });",
        "images.first().unwrap().update({ orders = [] });",
        "users.first().unwrap().orders.insert({ id = 1, total = 10, image = some(&images.first().unwrap()) });",
    ] {
        typecheck_ok(source, &schema);
    }
}

#[test]
fn mutation_non_owning_row_ref_shape_mismatch() {
    let schema = nested_schema();
    assert_eq!(
        error_codes(
            "users.update({ manager = users.first().unwrap() });",
            &schema
        ),
        ["QT0046"]
    );
    assert_eq!(
        error_codes("users.update({ manager = none });", &schema),
        ["QT0046"]
    );
    assert_eq!(
        error_codes(
            "users.update({ subordinates = &users.first().unwrap() });",
            &schema
        ),
        ["QT0046"]
    );
    assert_eq!(
        error_codes(
            "users.update({ manager = &users.orders.flatten().first().unwrap() });",
            &schema
        ),
        ["QT0045"]
    );

    let schema = mutation_schema();
    assert_eq!(
        error_codes(
            "users.orders.flatten().update({ image = some(&users.first().unwrap()) });",
            &schema
        ),
        ["QT0045"]
    );
}

#[test]
fn mutation_non_owning_backrefs_owning_relations_rejected() {
    let schema = mutation_schema();
    assert_eq!(
        error_codes(
            "users.orders.flatten().update({ user = &users.first().unwrap() });",
            &schema
        ),
        ["QT0043"]
    );
    assert_eq!(
        error_codes(
            "users.first().unwrap().orders.insert({ id = 1, total = 10, user = &users.first().unwrap() });",
            &schema
        ),
        ["QT0044"]
    );
}

#[test]
fn mutation_row_refs_inside_nested_owned_children() {
    let schema = mutation_schema();
    typecheck_ok(
        r#"users.insert({ id = 1, name = "A", orders = [{ id = 10, total = 1, image = some(&images.first().unwrap()) }] });"#,
        &schema,
    );
    typecheck_ok(
        r#"users.first().unwrap().update({ orders = [{ id = 10, total = 1, image = some(&images.first().unwrap()) }] });"#,
        &schema,
    );

    let schema = nested_schema();
    typecheck_ok(
        r#"users.insert({ name = "A", active = true, orders = [{ total = 1.0, items = [{ sku = "x" }] }] });"#,
        &schema,
    );
    typecheck_ok(
        r#"users.first().unwrap().update({ orders = [{ total = 1.0, items = [{ sku = "x" }] }] });"#,
        &schema,
    );
}

#[test]
fn mutation_nested_owned_child_row_ref_errors() {
    let schema = mutation_schema();
    assert_eq!(
        error_codes(
            r#"users.insert({ id = 1, name = "A", orders = [{ id = 10, total = 1, user = &users.first().unwrap() }] });"#,
            &schema
        ),
        ["QT0044"]
    );
    assert_eq!(
        error_codes(
            r#"users.first().unwrap().update({ orders = [{ id = 10, total = 1, user = &users.first().unwrap() }] });"#,
            &schema
        ),
        ["QT0043"]
    );
    assert_eq!(
        error_codes(
            r#"users.insert({ id = 1, name = "A", orders = [{ id = 10, total = 1, image = some(&users.first().unwrap()) }] });"#,
            &schema
        ),
        ["QT0045"]
    );
    assert_eq!(
        error_codes(
            r#"users.insert({ id = 1, name = "A", orders = [{ id = 10, total = 1, image = some(images.first().unwrap()) }] });"#,
            &schema
        ),
        ["QT0046"]
    );

    let schema = nested_schema();
    assert_eq!(
        error_codes(
            r#"users.insert({ name = "A", active = true, orders = [{ total = 1.0, items = [{ sku = "x", order = &users.orders.flatten().items.flatten().first().unwrap() }] }] });"#,
            &schema
        ),
        ["QT0044"]
    );
}

#[test]
fn mutation_projected_and_untraceable_targets() {
    let schema = mutation_schema();
    assert_eq!(
        error_codes(
            r#"users { id }.insert({ id = 1, name = "A", orders = [] });"#,
            &schema
        ),
        ["QT0019"]
    );
    assert_eq!(error_codes("users { id }.delete();", &schema), ["QT0019"]);
    assert_eq!(
        error_codes(
            r#"users.map($).insert({ id = 1, name = "A", orders = [] });"#,
            &schema
        ),
        ["QT0036"]
    );
    assert_eq!(error_codes("users.map($).delete();", &schema), ["QT0036"]);
}

#[test]
fn mutation_target_provenance_tracked() {
    let schema = test_schema();
    let user = schema.roots[0].struct_id;
    let profile_field = schema.struct_field(user, "profile").unwrap().0;

    assert_eq!(
        infer_ok("users", &schema).provenance,
        TargetProvenance::root(0)
    );
    assert_eq!(
        infer_ok("users.filter(active)", &schema).provenance,
        TargetProvenance::root(0)
    );
    assert_eq!(
        infer_ok("users.first().unwrap()", &schema).provenance,
        TargetProvenance {
            rows: true,
            collection: None,
        }
    );
    assert_eq!(
        infer_ok("users.first().unwrap().profile", &schema).provenance,
        TargetProvenance::ref_collection(profile_field)
    );
    assert_eq!(
        infer_ok("users.profile", &schema).provenance,
        TargetProvenance::ref_collection(profile_field)
    );
    assert_eq!(
        infer_ok("users.name", &schema).provenance,
        TargetProvenance::NONE
    );

    assert_eq!(
        infer_ok("some(users)", &schema).provenance,
        TargetProvenance::root(0)
    );
    assert_eq!(
        infer_ok("if true { users } else { users }", &schema).provenance,
        TargetProvenance::root(0)
    );
    assert_eq!(
        infer_ok("users.extend(users.take(0))", &schema).provenance,
        TargetProvenance {
            rows: true,
            collection: None,
        }
    );
    assert_eq!(
        infer_ok("users.map($)", &schema).provenance,
        TargetProvenance::NONE
    );
    assert_eq!(
        infer_ok("users { name }", &schema).provenance,
        TargetProvenance::NONE
    );
}

#[test]
fn mutation_target_provenance_not_propagated_through_let() {
    let typed = typecheck_ok("let u = users; u", &test_schema());
    assert_eq!(typed.result.unwrap().provenance, TargetProvenance::NONE);
}

#[test]
fn type_constant_int() {
    assert_eq!(
        infer_ok("Int::MAX", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int)
    );
}

#[test]
fn type_constant_float() {
    assert_eq!(
        infer_ok("Float::MIN", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Float)
    );
}

#[test]
fn type_constant_dec() {
    assert_eq!(
        infer_ok("Dec::MAX", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Dec)
    );
}

#[test]
fn mutation_insert_missing_required() {
    let (file, _diags) = crate::parse_query(r#"users.insert({ name = "Alice" });"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn mutation_insert_unknown_field() {
    let (file, _diags) = crate::parse_query(
        r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now, bogus = 1 });"#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn mutation_insert_wrong_type() {
    let (file, _diags) = crate::parse_query(
        r#"users.insert({ name = 42, age = 30, score = 1.0f, balance = 0.0, active = true, created = @now });"#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn mutation_update_unknown_field() {
    let (file, _diags) = crate::parse_query(r#"users[name == "Bob"].update({ bogus = 2 });"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn mutation_update_subset() {
    let (file, _diags) =
        crate::parse_query(r#"users[name == "Bob"].update({ age = 30, score = 2.5f });"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
}

#[test]
fn mutation_duplicate_field() {
    let (file, _diags) = crate::parse_query(
        r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, name = "Bob", created = @now });"#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn mutation_insert_optional_field_omitted() {
    let (file, _diags) = crate::parse_query(
        r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now });"#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
}

#[test]
fn mutation_insert_with_ref_field() {
    let (file, _diags) = crate::parse_query(
        r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now, profile = none });"#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty());
}

#[test]
fn file_with_mutations_result_expr() {
    let (file, _diags) = crate::parse_query(
        r#"
            users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now });
            users.insert({ name = "Bob", age = 25, score = 2.0f, balance = 0.0, active = true, created = @now });
            users[!(name in ("Bob", "Alice"))].delete();
            users[name == "Bob"].update({ score = prev.score + 0.4f });
            users { name, age }
            "#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
}

#[test]
fn batch_insert() {
    let (file, _diags) = crate::parse_query(
        r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = "Bob", age = 25, score = 2.0f, balance = 2.0, active = false, created = @today },
            ]);"#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(diags.is_empty(), "expected no errors, got {diags:?}");
}

#[test]
fn batch_insert_element_missing_required() {
    let (file, _diags) = crate::parse_query(
        r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = "Bob", score = 2.0f, balance = 0.0, active = true, created = @now },
            ]);"#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn batch_insert_element_mismatched_type() {
    let (file, _diags) = crate::parse_query(
        r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = 42, age = 25, score = 2.0f, balance = 0.0, active = true, created = @now },
            ]);"#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn batch_insert_element_unknown_field() {
    let (file, _diags) = crate::parse_query(
        r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = "Bob", age = 25, score = 2.0f, balance = 0.0, active = true, created = @now, foo = 1 },
            ]);"#,
    );
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn typecheck_if_else_optional_list() {
    assert_eq!(
        infer_ok(
            "if true { none }
                else {
                    if false { some([]) }
                    else { some([1, 2, 3]) }
                }",
            &test_schema(),
        )
        .ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int))).wrap_optional()
    );
}

#[test]
fn typecheck_if_else_ambiguous() {
    let (file, _) = crate::parse_query("if true { none } else { none }");
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn typecheck_array_optional() {
    assert_eq!(
        infer_ok("[none, some(3), none]", &test_schema()).ty,
        QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int).wrap_optional()))
    );
}

#[test]
fn typecheck_array_empty() {
    let (file, _) = crate::parse_query("[]");
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn typecheck_array_mismatched_types() {
    let (file, _) = crate::parse_query(r#"[1, "two", 3]"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn typecheck_some_none_if_else() {
    assert_eq!(
        infer_ok("if true { some(42) } else { none }", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int).wrap_optional()
    );
}

#[test]
fn typecheck_method_chain() {
    assert_eq!(
        infer_ok("users.first().unwrap().profile?.username", &test_schema()).ty,
        QueryType::Scalar(ScalarType::String).wrap_optional()
    );
}

#[test]
fn typecheck_filter_project() {
    assert!(matches!(
        infer_ok("users[age > 18] { name, profile?.username }", &test_schema()).ty,
        QueryType::List(inner) if matches!(*inner, QueryType::Record(RecordSource::Projection(_)))
    ));
}

#[test]
fn typecheck_optional_comparison_error() {
    let (file, _) = crate::parse_query("none == 1");
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn typecheck_optional_ambiguous() {
    let (file, _) = crate::parse_query("none == none");
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}

#[test]
fn typecheck_filter_method_optional() {
    assert_eq!(
        infer_ok("users[age > 18].first()?.age", &test_schema()).ty,
        QueryType::Scalar(ScalarType::Int).wrap_optional()
    );
}

#[test]
fn typecheck_tuple() {
    assert_eq!(
        infer_ok("if true { (1, 2) } else { (3, 4) }", &test_schema()).ty,
        QueryType::Tuple(vec![
            QueryType::Scalar(ScalarType::Int),
            QueryType::Scalar(ScalarType::Int),
        ])
    );
}

#[test]
fn typecheck_tuple_mismatch() {
    let (file, _) = crate::parse_query(r#"if true { (1, 2, 4) } else { (4, "abc") }"#);
    let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
    assert!(!diags.is_empty());
}
