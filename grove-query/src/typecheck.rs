pub mod bindings;
pub(crate) mod error;
pub mod types;

mod compat;
mod env;
mod infer;
mod mutation;
mod statements;
mod validate;

pub use bindings::TypeBindings;
pub use types::{
    CollectionOriginKind, IdentBinding, ProjectionField, QueryType, RecordSource, TargetProvenance,
    TypedExpr, TypedExprKind, TypedLetStmt, TypedMethodArg, TypedMutationStmt, TypedProjectionItem,
    TypedQueryFile, TypedStatement,
};

pub(crate) use env::TypeEnv;
pub(crate) use infer::infer;

use crate::ast::QueryFile;
use crate::typecheck::compat::check;
use crate::typecheck::statements::typecheck_stmt;
use grove_schema::validated::ValidatedSchema;
use grove_types::{Diagnostic, Severity};

pub fn typecheck(
    file: QueryFile,
    schema: &ValidatedSchema,
) -> (Option<TypedQueryFile>, Vec<Diagnostic>) {
    typecheck_with_bindings(file, schema, &TypeBindings::new())
}

pub fn typecheck_with_bindings(
    file: QueryFile,
    schema: &ValidatedSchema,
    bindings: &TypeBindings,
) -> (Option<TypedQueryFile>, Vec<Diagnostic>) {
    let (statements_src, result_src) = file.into_statements_and_output();
    let mut env = TypeEnv::new(schema);
    for (name, ty) in bindings.iter() {
        env.define(name.to_owned(), ty.clone(), IdentBinding::Let);
    }
    // repl turn scope always pushed, executor matching
    env.push_scope();

    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut statements = Vec::new();

    for stmt in &statements_src {
        match typecheck_stmt(stmt, &mut env) {
            Ok(typed) => statements.push(typed),
            Err(err) => diags.push(err.into()),
        }
    }

    let mut result = None;
    if let Some(expr) = result_src {
        match infer(&expr, &mut env) {
            Ok(mut typed) => {
                let mut ty = typed.ty.clone();
                if let Err(err) = check(&mut typed, &mut ty, &mut env) {
                    diags.push(err.into());
                }
                result = Some(typed);
            }
            Err(err) => {
                diags.push(err.into());
            }
        }
    }

    if diags.iter().any(|d| d.severity == Severity::Error) {
        (None, diags)
    } else {
        (Some(TypedQueryFile { statements, result }), diags)
    }
}

#[cfg(test)]
mod tests {
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

    pub(super) fn test_schema() -> ValidatedSchema {
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

    pub(super) fn mutation_schema() -> ValidatedSchema {
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

    pub(super) fn enum_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(ENUM_SCHEMA_SRC)
                .0
                .expect("invalid enum test schema"),
        )
        .0
        .expect("invalid enum test schema")
    }

    pub(super) fn error_codes(source: &str, schema: &ValidatedSchema) -> Vec<String> {
        let (file, parse_diags) = crate::parse_query(source);
        assert!(parse_diags.is_empty(), "parse diagnostics: {parse_diags:?}");
        let (typed, diags) = typecheck(file.unwrap(), schema);
        assert!(typed.is_none(), "expected type error for `{source}`");
        diags.iter().map(|d| d.code.to_string()).collect()
    }

    pub(super) fn infer_ok(source: &str, schema: &ValidatedSchema) -> TypedExpr {
        infer_ok_with_row_ref(source, schema, false)
    }

    pub(super) fn infer_ok_with_row_ref(
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

    pub(super) fn infer_err_with_row_ref(
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

    pub(super) fn typecheck_ok(source: &str, schema: &ValidatedSchema) -> TypedQueryFile {
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

    pub(super) fn annotation_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(ANNOTATION_SCHEMA_SRC)
                .0
                .expect("invalid annotation test schema"),
        )
        .0
        .expect("invalid annotation test schema")
    }

    pub(super) fn field_binding(
        schema: &ValidatedSchema,
        struct_id: StructId,
        field: &str,
    ) -> IdentBinding {
        let local = schema.structs[struct_id.index()]
            .fields
            .iter()
            .position(|f| f.name() == field)
            .unwrap();
        IdentBinding::Field(FieldId::new(struct_id, local))
    }

    pub(super) fn collect_node_ids(expr: &TypedExpr, ids: &mut Vec<NodeId>) {
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
    fn node_ids_unique() {
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

    pub(super) fn nested_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(NESTED_SCHEMA_SRC)
                .0
                .expect("invalid test schema"),
        )
        .0
        .expect("invalid test schema")
    }

    pub(super) fn nested_struct(schema: &ValidatedSchema, name: &str) -> StructId {
        StructId::new(
            schema
                .structs
                .iter()
                .position(|s| s.name == name)
                .unwrap_or_else(|| panic!("missing struct {name}")),
        )
    }

    pub(super) fn projected_fields(expr: TypedExpr) -> (Vec<String>, Vec<QueryType>) {
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
}
