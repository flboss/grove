use crate::testdata;
use crate::value::Scalar;

use super::*;
use crate::storage::{CmpOp, PushExpr, SortKey, StringOp};
use grove_query::{IdentBinding, TypedExpr, TypedExprKind, TypedQueryFile};
use grove_types::Severity;

const FIXED_NOW: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

fn typed(src: &str, schema: &ValidatedSchema) -> TypedQueryFile {
    let (file, parse_diags) = grove_query::parse_query(src);
    assert!(
        parse_diags
            .iter()
            .all(|diag| diag.severity == Severity::Warning),
        "{src}: {parse_diags:?}"
    );
    let (typed, diags) = grove_query::typecheck::typecheck(file.expect("parse"), schema);
    assert!(diags.is_empty(), "{src}: {diags:?}");
    typed.expect("typecheck")
}

fn typed_and_plan(src: &str) -> (TypedQueryFile, PushdownPlan, ValidatedSchema) {
    let schema = testdata::schema();
    let file = typed(src, &schema);
    let plan = PushdownPlan::analyze(&file, &schema, FIXED_NOW, false);
    (file, plan, schema)
}

fn walk(expr: &TypedExpr, f: &mut dyn FnMut(&TypedExpr)) {
    f(expr);
    match &expr.kind {
        TypedExprKind::Literal(_)
        | TypedExprKind::TypeConstant { .. }
        | TypedExprKind::EnumVariant { .. } => {}
        TypedExprKind::Ident { .. } => {}
        TypedExprKind::Field { base, .. } | TypedExprKind::TupleAccess { base, .. } => {
            walk(base, f);
        }
        TypedExprKind::Method { base, args, .. } => {
            walk(base, f);
            for arg in args {
                walk(&arg.expr, f);
            }
        }
        TypedExprKind::Binary { lhs, rhs, .. } => {
            walk(lhs, f);
            walk(rhs, f);
        }
        TypedExprKind::Unary { expr: inner, .. }
        | TypedExprKind::Cast { expr: inner, .. }
        | TypedExprKind::RowRef { expr: inner }
        | TypedExprKind::Some { value: inner } => walk(inner, f),
        TypedExprKind::If { arms, default } => {
            for (condition, value) in arms {
                walk(condition, f);
                walk(value, f);
            }
            walk(default, f);
        }
        TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
            for element in elements {
                walk(element, f);
            }
        }
        TypedExprKind::Struct { fields } => {
            for (_, value) in fields {
                walk(value, f);
            }
        }
        TypedExprKind::Projection { base, items } => {
            walk(base, f);
            for item in items {
                walk(&item.value, f);
            }
        }
    }
}

fn walk_file(file: &TypedQueryFile, f: &mut dyn FnMut(&TypedExpr)) {
    for statement in &file.statements {
        match statement {
            TypedStatement::Let(let_stmt) => walk(&let_stmt.value, f),
            TypedStatement::Expr(expr) => walk(expr, f),
            TypedStatement::Mutation(_) => {}
        }
    }
    if let Some(result) = &file.result {
        walk(result, f);
    }
}

fn root_nodes(file: &TypedQueryFile) -> Vec<NodeId> {
    let mut ids = Vec::new();
    walk_file(file, &mut |expr| {
        if matches!(
            expr.kind,
            TypedExprKind::Ident {
                binding: IdentBinding::Root { .. },
                ..
            }
        ) {
            ids.push(expr.id);
        }
    });
    ids
}

fn method_nodes(file: &TypedQueryFile, name: &str) -> Vec<NodeId> {
    let mut ids = Vec::new();
    walk_file(file, &mut |expr| {
        if let TypedExprKind::Method {
            name: method_name, ..
        } = &expr.kind
            && method_name.value == name
        {
            ids.push(expr.id);
        }
    });
    ids
}

fn field_ident_nodes(file: &TypedQueryFile, field: FieldId) -> Vec<NodeId> {
    let mut ids = Vec::new();
    walk_file(file, &mut |expr| {
        if let TypedExprKind::Ident {
            binding: IdentBinding::Field(bound),
            ..
        } = &expr.kind
            && *bound == field
        {
            ids.push(expr.id);
        }
    });
    ids
}

fn root_constraints<'a>(plan: &'a PushdownPlan, file: &TypedQueryFile) -> &'a FetchConstraints {
    let roots = root_nodes(file);
    assert_eq!(roots.len(), 1, "expected exactly one root");
    plan.constraints_for(roots[0])
        .expect("root trigger has a constraints entry")
}

fn field(schema: &ValidatedSchema, struct_name: &str, name: &str) -> FieldId {
    testdata::field_id(schema, testdata::struct_id(schema, struct_name), name)
}

fn non_ref_field_names<'a>(schema: &'a ValidatedSchema, struct_name: &str) -> Vec<&'a str> {
    let struct_id = testdata::struct_id(schema, struct_name);
    schema.structs[struct_id.index()]
        .fields
        .iter()
        .filter(|field| !matches!(field, Field::Ref { .. }))
        .map(|field| field.name())
        .collect()
}

fn assert_fields(
    plan: &PushdownPlan,
    schema: &ValidatedSchema,
    struct_name: &str,
    expected: &[&str],
) {
    let struct_id = testdata::struct_id(schema, struct_name);
    let mut actual: Vec<&str> = plan
        .fields_for(struct_id)
        .iter()
        .map(|field| schema.structs[struct_id.index()].fields[field.local()].name())
        .collect();
    actual.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    assert_eq!(actual, expected, "fields for {struct_name}");
}

#[test]
fn matcher_bytes_predicates() {
    let (file, plan, schema) = typed_and_plan(r#"blobs[data == hex"0001"]"#);
    assert_eq!(
        root_constraints(&plan, &file).filters,
        vec![PushExpr::Cmp {
            op: CmpOp::Eq,
            lhs: Box::new(PushExpr::Field(field(&schema, "Blob", "data"))),
            rhs: Box::new(PushExpr::Literal(Scalar::Bytes(vec![0x00, 0x01]))),
        }]
    );

    let (file, plan, schema) = typed_and_plan(r#"blobs[data > hex"00"]"#);
    assert_eq!(
        root_constraints(&plan, &file).filters,
        vec![PushExpr::Cmp {
            op: CmpOp::Gt,
            lhs: Box::new(PushExpr::Field(field(&schema, "Blob", "data"))),
            rhs: Box::new(PushExpr::Literal(Scalar::Bytes(vec![0x00]))),
        }]
    );

    let (file, plan, schema) = typed_and_plan(r#"blobs[data in (hex"0001", hex"00ff")]"#);
    assert_eq!(
        root_constraints(&plan, &file).filters,
        vec![PushExpr::In {
            lhs: Box::new(PushExpr::Field(field(&schema, "Blob", "data"))),
            items: vec![
                PushExpr::Literal(Scalar::Bytes(vec![0x00, 0x01])),
                PushExpr::Literal(Scalar::Bytes(vec![0x00, 0xff])),
            ],
        }]
    );
}

#[test]
fn bytes_sort_and_unpushed() {
    let (file, plan, schema) = typed_and_plan("blobs.sort_over(data)");
    assert_eq!(
        root_constraints(&plan, &file).sort,
        vec![SortKey {
            field: field(&schema, "Blob", "data"),
            desc: false,
        }]
    );

    for source in [
        r#"blobs[data.len() > 1]"#,
        r#"blobs[("x" as Bytes) == data]"#,
    ] {
        let (file, plan, _schema) = typed_and_plan(source);
        let constraints = root_constraints(&plan, &file);
        assert!(
            constraints.filters.is_empty(),
            "{source} should not push: {constraints:?}"
        );
    }
}

#[test]
fn matcher_simple_predicates() {
    let sources = [
        "users[id > 2]",
        "users[name == \"Ada\"]",
        "users[active]",
        "users[active == true]",
        "users[balance > 30.00]",
        "users[created > @now]",
        "users[name.contains(\"A\")]",
        "users[name.starts_with(\"A\")]",
        "users[name.ends_with(\"a\")]",
        "users[name.len() > 3]",
        "users[id in (1, 2, 3)]",
        "users[active && id > 1]",
        "users[!(active)]",
    ];
    for source in sources {
        let (file, plan, _schema) = typed_and_plan(source);
        let constraints = root_constraints(&plan, &file);
        assert_eq!(constraints.filters.len(), 1, "{source}: {constraints:?}");
        assert!(constraints.sort.is_empty(), "{source}");
        assert_eq!(constraints.offset, 0, "{source}");
        assert_eq!(constraints.limit, None, "{source}");
    }
}

#[test]
fn matcher_predicate_translation() {
    let (file, plan, schema) = typed_and_plan("users[active]");
    assert_eq!(
        root_constraints(&plan, &file).filters,
        vec![PushExpr::Field(field(&schema, "User", "active"))]
    );

    let (file, plan, schema) = typed_and_plan("users[id in (1, 2, 3)]");
    assert_eq!(
        root_constraints(&plan, &file).filters,
        vec![PushExpr::In {
            lhs: Box::new(PushExpr::Field(field(&schema, "User", "id"))),
            items: vec![
                PushExpr::Literal(Scalar::Int(1)),
                PushExpr::Literal(Scalar::Int(2)),
                PushExpr::Literal(Scalar::Int(3)),
            ],
        }]
    );

    let (file, plan, schema) = typed_and_plan("users[created > @now]");
    assert_eq!(
        root_constraints(&plan, &file).filters,
        vec![PushExpr::Cmp {
            op: CmpOp::Gt,
            lhs: Box::new(PushExpr::Field(field(&schema, "User", "created"))),
            rhs: Box::new(PushExpr::Literal(Scalar::Instant(FIXED_NOW))),
        }]
    );

    let (file, plan, schema) = typed_and_plan("users[name.contains(\"A\")]");
    assert_eq!(
        root_constraints(&plan, &file).filters,
        vec![PushExpr::StringOp {
            op: StringOp::Contains,
            base: Box::new(PushExpr::Field(field(&schema, "User", "name"))),
            needle: Box::new(PushExpr::Literal(Scalar::String("A".to_string()))),
        }]
    );
}

#[test]
fn matcher_enum_predicates() {
    let (file, plan, schema) = typed_and_plan("settings[priority == Priority::High]");
    assert_eq!(
        root_constraints(&plan, &file).filters,
        vec![PushExpr::Cmp {
            op: CmpOp::Eq,
            lhs: Box::new(PushExpr::Field(field(&schema, "Setting", "priority"))),
            rhs: Box::new(PushExpr::Literal(Scalar::String("high".to_string()))),
        }]
    );

    let (file, plan, schema) = typed_and_plan("settings[level in (Level::High, Level::Low)]");
    assert_eq!(
        root_constraints(&plan, &file).filters,
        vec![PushExpr::In {
            lhs: Box::new(PushExpr::Field(field(&schema, "Setting", "level"))),
            items: vec![
                PushExpr::Literal(Scalar::Int(2)),
                PushExpr::Literal(Scalar::Int(-1)),
            ],
        }]
    );

    let (file, plan, schema) = typed_and_plan("settings[priority == Priority::High] { name }");
    assert_eq!(root_constraints(&plan, &file).filters.len(), 1);
    assert_fields(&plan, &schema, "Setting", &["name", "priority"]);
}

#[test]
fn optional_method_chains_are_not_pushed() {
    let (file, plan, _schema) = typed_and_plan("users.first()?.orders?.take(1)");
    let takes = method_nodes(&file, "take");
    assert_eq!(takes.len(), 1);
    assert_eq!(plan.runtime_rewrite(takes[0]), None);
}

#[test]
fn matcher_unsupported_predicates() {
    let sources = [
        "users[nickname == some(\"A\")]",
        "users[profile?.bio?.len() == some(6)]",
        "users[pos.0 > 1.5f]",
        "users[orders.len() > 1]",
        "users[id + 1 > 2]",
        "users[name.to_upper() == \"ADA\"]",
    ];
    for source in sources {
        let (file, plan, _schema) = typed_and_plan(source);
        let constraints = root_constraints(&plan, &file);
        assert!(
            constraints.filters.is_empty(),
            "{source} should not push: {constraints:?}"
        );
    }
}

#[test]
fn filters_composition_across_unpushed() {
    let (file, plan, schema) = typed_and_plan("users[id > 2][orders.len() > 1][name == \"Ada\"]");
    let constraints = root_constraints(&plan, &file);
    assert_eq!(
        constraints.filters,
        vec![
            PushExpr::Cmp {
                op: CmpOp::Gt,
                lhs: Box::new(PushExpr::Field(field(&schema, "User", "id"))),
                rhs: Box::new(PushExpr::Literal(Scalar::Int(2))),
            },
            PushExpr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(PushExpr::Field(field(&schema, "User", "name"))),
                rhs: Box::new(PushExpr::Literal(Scalar::String("Ada".to_string()))),
            },
        ]
    );

    let (file, plan, _schema) = typed_and_plan("users[orders.len() > 1].take(3)");
    let constraints = root_constraints(&plan, &file);
    assert!(constraints.filters.is_empty());
    assert_eq!(constraints.limit, None);
    let take = method_nodes(&file, "take");
    assert_eq!(take.len(), 1);
    assert_eq!(plan.runtime_rewrite(take[0]), None);
}

#[test]
fn sort_keys() {
    let (file, plan, schema) = typed_and_plan("users.sort_over(name, desc id)");
    assert_eq!(
        root_constraints(&plan, &file).sort,
        vec![
            SortKey {
                field: field(&schema, "User", "name"),
                desc: false,
            },
            SortKey {
                field: field(&schema, "User", "id"),
                desc: true,
            },
        ]
    );

    let (file, plan, schema) = typed_and_plan("users.sort_desc_over(id)");
    assert_eq!(
        root_constraints(&plan, &file).sort,
        vec![SortKey {
            field: field(&schema, "User", "id"),
            desc: true,
        }]
    );
}

#[test]
fn unpushed_sort_composition() {
    let (file, plan, schema) =
        typed_and_plan("users.sort_over(orders.len()).filter(id > 2).sort_over(name)");
    let constraints = root_constraints(&plan, &file);
    assert!(constraints.sort.is_empty());
    assert_eq!(
        constraints.filters,
        vec![PushExpr::Cmp {
            op: CmpOp::Gt,
            lhs: Box::new(PushExpr::Field(field(&schema, "User", "id"))),
            rhs: Box::new(PushExpr::Literal(Scalar::Int(2))),
        }]
    );
}

#[test]
fn cap_folding() {
    let cases: &[(&str, u64, Option<u64>)] = &[
        ("users.take(10)", 0, Some(10)),
        ("users.skip(10)", 10, None),
        ("users.take(30).skip(10)", 10, Some(20)),
        ("users.skip(10).take(30)", 10, Some(30)),
        ("users.take(10).take(5)", 0, Some(5)),
        ("users.take(5).take(10)", 0, Some(5)),
        ("users.skip(10).skip(20)", 30, None),
        ("users.skip(30).take(10).skip(5)", 35, Some(5)),
    ];
    for &(source, offset, limit) in cases {
        let (file, plan, _schema) = typed_and_plan(source);
        let constraints = root_constraints(&plan, &file);
        assert_eq!(
            (constraints.offset, constraints.limit),
            (offset, limit),
            "{source}"
        );
        for name in ["take", "skip"] {
            for node in method_nodes(&file, name) {
                assert_eq!(
                    plan.runtime_rewrite(node),
                    Some(RuntimeRewrite::Skip),
                    "{source}: {name} should be folded"
                );
            }
        }
    }
}

#[test]
fn first_nth() {
    let (file, plan, _schema) = typed_and_plan("users.first()");
    let constraints = root_constraints(&plan, &file);
    assert_eq!((constraints.offset, constraints.limit), (0, Some(1)));
    for node in method_nodes(&file, "first") {
        assert_eq!(plan.runtime_rewrite(node), None);
    }

    let (file, plan, _schema) = typed_and_plan("users.skip(2).nth(3)");
    let constraints = root_constraints(&plan, &file);
    assert_eq!((constraints.offset, constraints.limit), (5, Some(1)));
    for node in method_nodes(&file, "skip") {
        assert_eq!(plan.runtime_rewrite(node), Some(RuntimeRewrite::Skip));
    }
    for node in method_nodes(&file, "nth") {
        assert_eq!(plan.runtime_rewrite(node), Some(RuntimeRewrite::First));
    }

    let (file, plan, _schema) = typed_and_plan("users.take(10).nth(3)");
    let constraints = root_constraints(&plan, &file);
    assert_eq!((constraints.offset, constraints.limit), (3, Some(1)));

    let (file, plan, _schema) = typed_and_plan("users.nth(-1)");
    let constraints = root_constraints(&plan, &file);
    assert_eq!((constraints.offset, constraints.limit), (0, None));
    for node in method_nodes(&file, "nth") {
        assert_eq!(plan.runtime_rewrite(node), None);
    }
}

#[test]
fn rewrites_composition() {
    let (file, plan, _schema) = typed_and_plan("users[id > 2].sort_over(name).take(3)");
    let constraints = root_constraints(&plan, &file);
    assert_eq!(constraints.filters.len(), 1);
    assert_eq!(constraints.sort.len(), 1);
    assert_eq!((constraints.offset, constraints.limit), (0, Some(3)));
    let roots = root_nodes(&file);
    assert_eq!(roots.len(), 1);
    for take in method_nodes(&file, "take") {
        assert_eq!(
            plan.rewrite_plan(take).expect("take planned").trigger,
            roots[0]
        );
    }

    let (file, plan, _schema) = typed_and_plan("users[id > 2].take(3).sort_over(name)");
    let constraints = root_constraints(&plan, &file);
    assert_eq!(constraints.filters.len(), 1);
    assert!(constraints.sort.is_empty());
    assert_eq!((constraints.offset, constraints.limit), (0, Some(3)));

    let (file, plan, _schema) = typed_and_plan("users.take(10).sort_over(id)");
    let constraints = root_constraints(&plan, &file);
    assert!(constraints.sort.is_empty());
    assert_eq!((constraints.offset, constraints.limit), (0, Some(10)));

    let (file, plan, _schema) = typed_and_plan("users.take(10)[id > 2]");
    let constraints = root_constraints(&plan, &file);
    assert!(constraints.filters.is_empty());
    assert_eq!((constraints.offset, constraints.limit), (0, Some(10)));
}

#[test]
fn pushdown_window_and_predicate_methods() {
    let (file, plan, _schema) = typed_and_plan("users.last()");
    let constraints = root_constraints(&plan, &file);
    assert!(constraints.sort.is_empty());
    assert!(constraints.tie_desc);
    assert_eq!(constraints.limit, Some(1));
    assert_eq!(plan.runtime_rewrite(method_nodes(&file, "last")[0]), None);

    let (file, plan, schema) = typed_and_plan("users.sort_over(id).reverse()");
    let constraints = root_constraints(&plan, &file);
    assert_eq!(
        constraints.sort,
        vec![SortKey {
            field: field(&schema, "User", "id"),
            desc: true,
        }]
    );
    assert!(constraints.tie_desc);
    assert_eq!(
        plan.runtime_rewrite(method_nodes(&file, "sort_over")[0]),
        Some(RuntimeRewrite::Skip)
    );
    assert_eq!(
        plan.runtime_rewrite(method_nodes(&file, "reverse")[0]),
        Some(RuntimeRewrite::Skip)
    );

    let (file, plan, _) = typed_and_plan("users.take_back(2)");
    let constraints = root_constraints(&plan, &file);
    assert!(constraints.tie_desc);
    assert_eq!(constraints.limit, Some(2));
    assert_eq!(
        plan.runtime_rewrite(method_nodes(&file, "take_back")[0]),
        Some(RuntimeRewrite::ReverseOutput)
    );

    let (file, plan, _) = typed_and_plan("users.skip_back(2)");
    let constraints = root_constraints(&plan, &file);
    assert!(constraints.tie_desc);
    assert_eq!((constraints.offset, constraints.limit), (2, None));
    assert_eq!(
        plan.runtime_rewrite(method_nodes(&file, "skip_back")[0]),
        Some(RuntimeRewrite::ReverseOutput)
    );

    let (file, plan, _) = typed_and_plan("users.take_back(3).take_back(2)");
    let constraints = root_constraints(&plan, &file);
    assert_eq!(constraints.limit, Some(2));
    let nodes = method_nodes(&file, "take_back");
    assert_eq!(
        plan.runtime_rewrite(nodes[0]),
        Some(RuntimeRewrite::ReverseOutput)
    );
    assert_eq!(plan.runtime_rewrite(nodes[1]), Some(RuntimeRewrite::Skip));

    let (file, plan, _) = typed_and_plan("users.take_back(5).skip_back(1)");
    let constraints = root_constraints(&plan, &file);
    assert_eq!((constraints.offset, constraints.limit), (1, Some(4)));
    let take_back = method_nodes(&file, "take_back");
    let skip_back = method_nodes(&file, "skip_back");
    assert_eq!(
        plan.runtime_rewrite(take_back[0]),
        Some(RuntimeRewrite::Skip)
    );
    assert_eq!(
        plan.runtime_rewrite(skip_back[0]),
        Some(RuntimeRewrite::ReverseOutput)
    );

    let (file, plan, schema) = typed_and_plan("users.any(active)");
    let constraints = root_constraints(&plan, &file);
    assert_eq!(
        constraints.filters,
        vec![PushExpr::Field(field(&schema, "User", "active"))]
    );
    assert_eq!(constraints.limit, Some(1));
    assert_eq!(
        plan.runtime_rewrite(method_nodes(&file, "any")[0]),
        Some(RuntimeRewrite::NonEmpty)
    );

    let (file, plan, schema) = typed_and_plan("users.all(active)");
    let constraints = root_constraints(&plan, &file);
    assert_eq!(
        constraints.filters,
        vec![PushExpr::Not(Box::new(PushExpr::Field(field(
            &schema, "User", "active"
        ))))]
    );
    assert_eq!(constraints.limit, Some(1));
    assert_eq!(
        plan.runtime_rewrite(method_nodes(&file, "all")[0]),
        Some(RuntimeRewrite::Empty)
    );

    let (file, plan, schema) = typed_and_plan("users.find(name == \"Dan\")");
    let constraints = root_constraints(&plan, &file);
    assert_eq!(
        constraints.filters,
        vec![PushExpr::Cmp {
            op: CmpOp::Eq,
            lhs: Box::new(PushExpr::Field(field(&schema, "User", "name"))),
            rhs: Box::new(PushExpr::Literal(Scalar::String("Dan".to_string()))),
        }]
    );
    assert_eq!(constraints.limit, Some(1));
    assert_eq!(
        plan.runtime_rewrite(method_nodes(&file, "find")[0]),
        Some(RuntimeRewrite::First)
    );

    let (file, plan, _) = typed_and_plan("users.take(1).any(name == \"Bob\")");
    let constraints = root_constraints(&plan, &file);
    assert!(constraints.filters.is_empty());
    assert_eq!(constraints.limit, Some(1));
    assert_eq!(plan.runtime_rewrite(method_nodes(&file, "any")[0]), None);

    let (file, plan, _) = typed_and_plan("users.take(0).any(active)");
    let constraints = root_constraints(&plan, &file);
    assert!(constraints.filters.is_empty());
    assert_eq!(constraints.limit, Some(0));
    assert_eq!(plan.runtime_rewrite(method_nodes(&file, "any")[0]), None);
}

#[test]
fn pushdown_chain_termination_on_projection() {
    let (file, plan, schema) = typed_and_plan("users[id > 2].take(3) { name }");
    let constraints = root_constraints(&plan, &file);
    assert_eq!(constraints.filters.len(), 1);
    assert_eq!((constraints.offset, constraints.limit), (0, Some(3)));
    assert_fields(&plan, &schema, "User", &["id", "name"]);

    let (file, plan, schema) = typed_and_plan("users { o = orders.take(2) }");
    let constraints = root_constraints(&plan, &file);
    assert_eq!((constraints.offset, constraints.limit), (0, None));
    let orders = testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "orders");
    let orders_sites = field_ident_nodes(&file, orders);
    assert_eq!(orders_sites.len(), 1);
    assert_eq!(
        plan.constraints_for(orders_sites[0]),
        Some(&FetchConstraints {
            filters: vec![],
            sort: vec![],
            tie_desc: false,
            offset: 0,
            limit: Some(2),
        })
    );
    let take = method_nodes(&file, "take");
    assert_eq!(take.len(), 1);
    assert_eq!(
        plan.rewrite_plan(take[0]),
        Some(RewritePlan {
            rewrite: RuntimeRewrite::Skip,
            trigger: orders_sites[0],
        })
    );
    assert!(plan.prefetch_skip.contains(&orders));
}

#[test]
fn related_ident_chains() {
    let (file, plan, schema) =
        typed_and_plan("users { o = orders[amount > 100].take(1), r = roles.take(1) }");
    let user = testdata::struct_id(&schema, "User");
    let orders = testdata::field_id(&schema, user, "orders");
    let roles = testdata::field_id(&schema, user, "roles");

    let orders_sites = field_ident_nodes(&file, orders);
    assert_eq!(orders_sites.len(), 1);
    let orders_plan = plan
        .constraints_for(orders_sites[0])
        .expect("orders trigger constraints");
    assert_eq!(orders_plan.filters.len(), 1);
    assert_eq!(orders_plan.limit, Some(1));

    let roles_sites = field_ident_nodes(&file, roles);
    assert_eq!(roles_sites.len(), 1);
    let roles_plan = plan
        .constraints_for(roles_sites[0])
        .expect("roles trigger constraints");
    assert!(roles_plan.filters.is_empty());
    assert_eq!(roles_plan.limit, Some(1));

    assert!(plan.prefetch_skip.contains(&orders));
    assert!(plan.prefetch_skip.contains(&roles));
    for take in method_nodes(&file, "take") {
        let rewrite = plan.rewrite_plan(take).expect("take planned");
        assert_eq!(rewrite.rewrite, RuntimeRewrite::Skip);
    }
}

#[test]
fn related_empty_chain() {
    let (file, plan, schema) = typed_and_plan("users { o = orders.len() }");
    let orders = testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "orders");
    let sites = field_ident_nodes(&file, orders);
    assert_eq!(sites.len(), 1);
    let constraints = plan
        .constraints_for(sites[0])
        .expect("related chain entry exists");
    assert!(constraints.is_empty());
    assert!(!plan.prefetch_skip.contains(&orders));
    let len = method_nodes(&file, "len");
    assert_eq!(len.len(), 1);
    assert_eq!(plan.rewrite_plan(len[0]), None);
}

#[test]
fn related_field_chains() {
    let (file, plan, schema) = typed_and_plan("users.map($.orders.take(2).len())");
    let orders = testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "orders");
    let take = method_nodes(&file, "take");
    assert_eq!(take.len(), 1);
    let rewrite = plan.rewrite_plan(take[0]).expect("take planned");
    assert_eq!(rewrite.rewrite, RuntimeRewrite::Skip);
    let constraints = plan
        .constraints_for(rewrite.trigger)
        .expect("field trigger constraints");
    assert_eq!(constraints.limit, Some(2));
    assert!(plan.prefetch_skip.contains(&orders));

    let (file, plan, _schema) = typed_and_plan("users.map($.roles.skip(1).len())");
    let skip = method_nodes(&file, "skip");
    assert_eq!(skip.len(), 1);
    let rewrite = plan.rewrite_plan(skip[0]).expect("take planned");
    assert_eq!(
        plan.constraints_for(rewrite.trigger)
            .expect("field trigger constraints")
            .offset,
        1
    );
    let roles = testdata::field_id(&schema, testdata::struct_id(&schema, "User"), "roles");
    assert!(plan.prefetch_skip.contains(&roles));
}

#[test]
fn list_base_field_access_is_not_pushdown_trigger() {
    let (file, plan, _schema) = typed_and_plan("users.orders.take(2).len()");
    let take = method_nodes(&file, "take");
    assert_eq!(take.len(), 1);
    assert_eq!(plan.rewrite_plan(take[0]), None);
    assert!(plan.prefetch_skip.is_empty());
}

#[test]
fn let_pushdown_chain() {
    let (file, plan, _schema) = typed_and_plan("let u = users[id > 2].take(3); u");
    let constraints = root_constraints(&plan, &file);
    assert_eq!(constraints.filters.len(), 1);
    assert_eq!((constraints.offset, constraints.limit), (0, Some(3)));
    for node in method_nodes(&file, "take") {
        assert_eq!(plan.runtime_rewrite(node), Some(RuntimeRewrite::Skip));
    }
}

#[test]
fn discarded_statements_ignored() {
    let (file, plan, schema) = typed_and_plan("users[id > 2]; users { name }");
    let roots = root_nodes(&file);
    assert_eq!(roots.len(), 2);
    assert!(plan.constraints_for(roots[0]).is_none());
    assert!(plan.constraints_for(roots[1]).is_some());
    assert_eq!(plan.fetches.len(), 1);
    assert_fields(&plan, &schema, "User", &["name"]);
}

#[test]
fn liveness_projection_prunes_fields() {
    let (_file, plan, schema) = typed_and_plan("users { name }");
    assert_fields(&plan, &schema, "User", &["name"]);
    assert_fields(&plan, &schema, "Order", &[]);
    assert_fields(&plan, &schema, "Profile", &[]);
}

#[test]
fn liveness_projection_spread_all_fields_kept() {
    let (_file, plan, schema) = typed_and_plan("users { name, ... }");
    assert_fields(
        &plan,
        &schema,
        "User",
        &non_ref_field_names(&schema, "User"),
    );
}

#[test]
fn liveness_whole_record() {
    let (_file, plan, schema) = typed_and_plan("users");
    assert_fields(
        &plan,
        &schema,
        "User",
        &non_ref_field_names(&schema, "User"),
    );
    assert_fields(
        &plan,
        &schema,
        "Profile",
        &non_ref_field_names(&schema, "Profile"),
    );
    assert_fields(
        &plan,
        &schema,
        "Order",
        &non_ref_field_names(&schema, "Order"),
    );
    assert_fields(&plan, &schema, "Role", &[]);

    let (_file, plan, schema) = typed_and_plan("users { m = manager }");
    assert_fields(
        &plan,
        &schema,
        "User",
        &non_ref_field_names(&schema, "User"),
    );

    let (_file, plan, schema) = typed_and_plan("let u = users; u");
    assert_fields(
        &plan,
        &schema,
        "User",
        &non_ref_field_names(&schema, "User"),
    );

    let (_file, plan, schema) = typed_and_plan("roles");
    assert_fields(
        &plan,
        &schema,
        "Role",
        &non_ref_field_names(&schema, "Role"),
    );
    assert_fields(&plan, &schema, "User", &[]);
}

#[test]
fn liveness_partial() {
    let (_file, plan, schema) = typed_and_plan("users { o = orders { amount } }");
    assert_fields(&plan, &schema, "User", &[]);
    assert_fields(&plan, &schema, "Order", &["amount"]);

    let (_file, plan, schema) = typed_and_plan("users { p = profile?.bio }");
    assert_fields(&plan, &schema, "User", &[]);
    assert_fields(&plan, &schema, "Profile", &["bio"]);

    let (_file, plan, schema) = typed_and_plan("users { p = profile }");
    assert_fields(&plan, &schema, "User", &[]);
    assert_fields(
        &plan,
        &schema,
        "Profile",
        &non_ref_field_names(&schema, "Profile"),
    );

    let (_file, plan, schema) = typed_and_plan("users[orders.len() > 1]");
    assert_fields(
        &plan,
        &schema,
        "User",
        &non_ref_field_names(&schema, "User"),
    );
    assert_fields(
        &plan,
        &schema,
        "Order",
        &non_ref_field_names(&schema, "Order"),
    );

    let (_file, plan, schema) = typed_and_plan("users.len()");
    assert_fields(&plan, &schema, "User", &[]);

    let (_file, plan, schema) = typed_and_plan("users { c = orders.len() }");
    assert_fields(&plan, &schema, "User", &[]);
    assert_fields(&plan, &schema, "Order", &[]);

    let (_file, plan, schema) = typed_and_plan("users[active].sort_over(name) { id }");
    assert_fields(&plan, &schema, "User", &["id", "active", "name"]);

    let (_file, plan, schema) = typed_and_plan("users { e = extra.0 }");
    assert_fields(&plan, &schema, "User", &["extra"]);

    let (_file, plan, schema) = typed_and_plan("users.map($.name)");
    assert_fields(&plan, &schema, "User", &["name"]);
}

#[test]
fn materialized_lets_whole_records_fetched() {
    let schema = testdata::schema();
    let file = typed("let a = users.first();", &schema);
    let narrow = PushdownPlan::analyze(&file, &schema, FIXED_NOW, false);
    let whole = PushdownPlan::analyze(&file, &schema, FIXED_NOW, true);
    let user = testdata::struct_id(&schema, "User");
    let expected: Vec<&str> = schema.structs[user.index()]
        .fields
        .iter()
        .filter(|field| !matches!(field, Field::Ref { .. }))
        .map(|field| field.name())
        .collect();

    assert_fields(&whole, &schema, "User", &expected);
    assert!(narrow.fields_for(user).len() < whole.fields_for(user).len());
}

#[test]
fn disabled_pushdown_plan() {
    let schema = testdata::schema();
    let plan = PushdownPlan::disabled(&schema);
    assert!(plan.fetches.is_empty());
    assert!(plan.rewrites.is_empty());
    assert!(plan.prefetch_skip.is_empty());
    for struct_def in &schema.structs {
        let struct_id = testdata::struct_id(&schema, &struct_def.name);
        let expected: Vec<&str> = struct_def
            .fields
            .iter()
            .filter(|field| !matches!(field, Field::Ref { .. }))
            .map(|field| field.name())
            .collect();
        assert_fields(&plan, &schema, &struct_def.name, &expected);
        assert_eq!(
            plan.fields_for(struct_id).len(),
            expected.len(),
            "{}",
            struct_def.name
        );
    }
}
