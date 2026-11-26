use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use grove_query::{TypedMutationStmt, TypedQueryFile, TypedStatement};
use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema};
use grove_types::NodeId;

use crate::storage::FetchConstraints;

mod builder;
mod chain;
mod translate;

use builder::PlanBuilder;
use chain::Need;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeRewrite {
    /// Operation already folded into fetch constraints
    Skip,
    /// nth/find already folded; evaluate as `first`
    First,
    /// Reverse the fetched window (at final list-producing operation)
    ReverseOutput,
    /// any() folded to filtered LIMIT 1 fetch
    NonEmpty,
    /// all() folded to NOT-filtered LIMIT 1 fetch
    Empty,
}

/// Runtime rewrite annotation, with the fetch trigger which applied it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RewritePlan {
    pub rewrite: RuntimeRewrite,
    pub trigger: NodeId,
}

/// Annotation map for pushdown
#[derive(Debug, Clone, Default)]
pub struct PushdownPlan {
    fetches: HashMap<NodeId, FetchConstraints>,
    fields: HashMap<StructId, Vec<FieldId>>,
    rewrites: HashMap<NodeId, RewritePlan>,
    prefetch_skip: HashSet<FieldId>,
}

impl PushdownPlan {
    /// Analyze a typed file and build the pushdown plan
    pub fn analyze(
        file: &TypedQueryFile,
        schema: &ValidatedSchema,
        now: DateTime<Utc>,
        materialize_lets: bool,
    ) -> PushdownPlan {
        let mut builder = PlanBuilder {
            schema,
            now,
            chains: HashMap::new(),
            ref_sites: HashMap::new(),
            usage: HashMap::new(),
            rewrites: HashMap::new(),
        };
        for statement in &file.statements {
            match statement {
                TypedStatement::Let(let_stmt) => {
                    let need = if materialize_lets {
                        Need::Whole
                    } else {
                        Need::Narrow
                    };
                    builder.visit(&let_stmt.value, need);
                }
                TypedStatement::Expr(_) => {}
                TypedStatement::Mutation(mutation) => match mutation {
                    TypedMutationStmt::Insert { base, arg, .. }
                    | TypedMutationStmt::Update { base, arg, .. } => {
                        builder.visit(base, Need::Narrow);
                        builder.visit(arg, Need::Narrow);
                    }
                    TypedMutationStmt::Delete { base, .. } => {
                        builder.visit(base, Need::Narrow);
                    }
                },
            }
        }
        if let Some(result) = &file.result {
            builder.visit(result, Need::Whole);
        }
        builder.finish()
    }

    pub fn disabled(schema: &ValidatedSchema) -> PushdownPlan {
        PushdownPlan {
            fetches: HashMap::new(),
            fields: all_struct_fields(schema),
            rewrites: HashMap::new(),
            prefetch_skip: HashSet::new(),
        }
    }

    pub fn constraints_for(&self, node: NodeId) -> Option<&FetchConstraints> {
        self.fetches.get(&node)
    }

    pub fn fields_for(&self, struct_id: StructId) -> &[FieldId] {
        self.fields
            .get(&struct_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    #[cfg(test)]
    fn runtime_rewrite(&self, node: NodeId) -> Option<RuntimeRewrite> {
        self.rewrites.get(&node).map(|n| n.rewrite)
    }

    pub fn rewrite_plan(&self, node: NodeId) -> Option<RewritePlan> {
        self.rewrites.get(&node).copied()
    }

    pub fn prefetch_skip(&self) -> &HashSet<FieldId> {
        &self.prefetch_skip
    }
}

/// Non-Ref fields of every struct in declaration order
fn all_struct_fields(schema: &ValidatedSchema) -> HashMap<StructId, Vec<FieldId>> {
    schema
        .structs
        .iter()
        .enumerate()
        .map(|(index, struct_def)| {
            let struct_id = StructId::new(index);
            let fields = struct_def
                .fields
                .iter()
                .enumerate()
                .filter(|(_, field)| !matches!(field, Field::Ref { .. }))
                .map(|(local, _)| FieldId::new(struct_id, local))
                .collect();
            (struct_id, fields)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdata;

    use grove_query::{IdentBinding, TypedExpr, TypedExprKind, TypedQueryFile};
    use grove_types::Severity;

    pub(super) const FIXED_NOW: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

    pub(super) fn typed(src: &str, schema: &ValidatedSchema) -> TypedQueryFile {
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

    pub(super) fn typed_and_plan(src: &str) -> (TypedQueryFile, PushdownPlan, ValidatedSchema) {
        let schema = testdata::schema();
        let file = typed(src, &schema);
        let plan = PushdownPlan::analyze(&file, &schema, FIXED_NOW, false);
        (file, plan, schema)
    }

    pub(super) fn walk(expr: &TypedExpr, f: &mut dyn FnMut(&TypedExpr)) {
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

    pub(super) fn walk_file(file: &TypedQueryFile, f: &mut dyn FnMut(&TypedExpr)) {
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

    pub(super) fn root_nodes(file: &TypedQueryFile) -> Vec<NodeId> {
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

    pub(super) fn method_nodes(file: &TypedQueryFile, name: &str) -> Vec<NodeId> {
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

    pub(super) fn field_ident_nodes(file: &TypedQueryFile, field: FieldId) -> Vec<NodeId> {
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

    pub(super) fn root_constraints<'a>(
        plan: &'a PushdownPlan,
        file: &TypedQueryFile,
    ) -> &'a FetchConstraints {
        let roots = root_nodes(file);
        assert_eq!(roots.len(), 1, "expected exactly one root");
        plan.constraints_for(roots[0])
            .expect("root trigger has a constraints entry")
    }

    pub(super) fn field(schema: &ValidatedSchema, struct_name: &str, name: &str) -> FieldId {
        testdata::field_id(schema, testdata::struct_id(schema, struct_name), name)
    }

    pub(super) fn non_ref_field_names<'a>(
        schema: &'a ValidatedSchema,
        struct_name: &str,
    ) -> Vec<&'a str> {
        let struct_id = testdata::struct_id(schema, struct_name);
        schema.structs[struct_id.index()]
            .fields
            .iter()
            .filter(|field| !matches!(field, Field::Ref { .. }))
            .map(|field| field.name())
            .collect()
    }

    pub(super) fn assert_fields(
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
    fn discarded_expressions_ignored() {
        let (file, plan, schema) = typed_and_plan("users[id > 2]; users { name }");
        let roots = root_nodes(&file);
        assert_eq!(roots.len(), 2);
        assert!(plan.constraints_for(roots[0]).is_none());
        assert!(plan.constraints_for(roots[1]).is_some());
        assert_eq!(plan.fetches.len(), 1);
        assert_fields(&plan, &schema, "User", &["name"]);
    }

    #[test]
    fn materialized_lets_entire_records_fetched() {
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
}
