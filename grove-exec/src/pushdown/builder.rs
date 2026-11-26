use std::collections::HashMap;

use chrono::{DateTime, Utc};
use grove_query::ast::Literal;
use grove_query::methods::MethodId;
use grove_query::{
    IdentBinding, QueryType, RecordSource, TypedExpr, TypedExprKind, TypedMethodArg,
};
use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema};
use grove_types::NodeId;

use super::chain::{BackOutcome, ChainState, ChainSummary, FieldPlan, SortOutcome};
use super::{PushdownPlan, RewritePlan, RuntimeRewrite};
use crate::method_meta::{self, BaseNeed};
use crate::pushdown::Need;
use crate::storage::FetchConstraints;

pub(super) struct PlanBuilder<'a> {
    pub(super) schema: &'a ValidatedSchema,
    pub(super) now: DateTime<Utc>,
    pub(super) chains: HashMap<NodeId, ChainState>,
    pub(super) ref_sites: HashMap<NodeId, FieldId>,
    pub(super) usage: HashMap<StructId, FieldPlan>,
    pub(super) rewrites: HashMap<NodeId, RewritePlan>,
}

impl<'a> PlanBuilder<'a> {
    pub(super) fn finish(self) -> PushdownPlan {
        let PlanBuilder {
            schema,
            chains,
            ref_sites,
            mut usage,
            rewrites,
            ..
        } = self;
        expand_whole(schema, &mut usage);
        let fields = schema
            .structs
            .iter()
            .enumerate()
            .map(|(index, struct_def)| {
                let struct_id = StructId::new(index);
                let plan = usage.get(&struct_id);
                let whole = plan.is_some_and(|plan| plan.whole);
                let subset = struct_def
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| !matches!(field, Field::Ref { .. }))
                    .map(|(local, _)| FieldId::new(struct_id, local))
                    .filter(|fid| whole || plan.is_some_and(|plan| plan.used.contains(fid)))
                    .collect();
                (struct_id, subset)
            })
            .collect();
        let fetches: HashMap<NodeId, FetchConstraints> = chains
            .into_iter()
            .map(|(node, state)| (node, state.into_constraints()))
            .collect();
        let prefetch_skip = ref_sites
            .into_iter()
            .filter_map(|(node, field)| {
                let constraints = fetches.get(&node)?;
                (!constraints.is_empty()).then_some(field)
            })
            .collect();
        PushdownPlan {
            fetches,
            fields,
            rewrites,
            prefetch_skip,
        }
    }

    /// Walk expression tree for column liveness (downward)
    /// and fetch pushdown chain recognition (upward)
    pub(super) fn visit(&mut self, expr: &TypedExpr, need: Need) -> Option<ChainSummary> {
        if need == Need::Whole {
            self.mark_used_whole(&expr.ty);
        }
        match &expr.kind {
            TypedExprKind::Literal(_)
            | TypedExprKind::TypeConstant { .. }
            | TypedExprKind::EnumVariant { .. } => None,
            TypedExprKind::Ident { binding, .. } => self.visit_ident(expr, binding),
            TypedExprKind::Field { base, name, .. } => {
                self.visit_field_access(base, name);
                let trigger = base
                    .ty
                    .single_schema_record()
                    .and_then(|struct_id| self.schema.struct_field(struct_id, name))
                    .and_then(|(field, _)| {
                        self.list_ref_target(field).map(|target| (field, target))
                    });
                trigger.map(|(field, target)| self.start_ref_chain(expr.id, field, target))
            }
            TypedExprKind::TupleAccess { base, .. } => {
                self.visit(base, Need::Narrow);
                None
            }
            TypedExprKind::Method {
                method,
                base,
                args,
                opt_depth,
                ..
            } => {
                let base_need = method_base_need(*method, need);
                let summary = self.visit(base, base_need);
                for arg in args {
                    self.visit(&arg.expr, Need::Whole);
                }
                self.extend_chain(expr.id, *method, *opt_depth, args, summary)
            }
            TypedExprKind::Binary { lhs, rhs, .. } => {
                self.visit(lhs, Need::Whole);
                self.visit(rhs, Need::Whole);
                None
            }
            TypedExprKind::Unary { expr: inner, .. } | TypedExprKind::Cast { expr: inner, .. } => {
                self.visit(inner, Need::Whole);
                None
            }
            TypedExprKind::RowRef { expr: inner } => {
                self.visit(inner, Need::Narrow);
                None
            }
            TypedExprKind::Some { value } => {
                self.visit(value, need);
                None
            }
            TypedExprKind::If { arms, default } => {
                for (condition, value) in arms {
                    self.visit(condition, Need::Whole);
                    self.visit(value, need);
                }
                self.visit(default, need);
                None
            }
            TypedExprKind::Tuple { elements } | TypedExprKind::Array { elements } => {
                for element in elements {
                    self.visit(element, Need::Whole);
                }
                None
            }
            TypedExprKind::Struct { fields } => {
                for (_, value) in fields {
                    self.visit(value, Need::Whole);
                }
                None
            }
            TypedExprKind::Projection { base, items } => {
                self.visit(base, Need::Narrow);
                for item in items {
                    self.visit(&item.value, Need::Whole);
                }
                None
            }
        }
    }

    fn visit_ident(&mut self, expr: &TypedExpr, binding: &IdentBinding) -> Option<ChainSummary> {
        match binding {
            IdentBinding::Root { root_idx } => {
                let root = self.schema.roots.get(*root_idx)?;
                self.chains.entry(expr.id).or_default();
                Some(ChainSummary {
                    site: expr.id,
                    struct_id: root.struct_id,
                })
            }
            IdentBinding::Field(field) => {
                self.mark_used_field(*field);
                let target = self.list_ref_target(*field)?;
                Some(self.start_ref_chain(expr.id, *field, target))
            }
            IdentBinding::ProjectedField { .. }
            | IdentBinding::Prev { .. }
            | IdentBinding::ListElement
            | IdentBinding::Let => None,
        }
    }

    fn list_ref_target(&self, field: FieldId) -> Option<StructId> {
        match self.schema.field(field)? {
            Field::Ref {
                target,
                is_list: true,
                ..
            } => Some(*target),
            _ => None,
        }
    }

    fn start_ref_chain(&mut self, site: NodeId, field: FieldId, target: StructId) -> ChainSummary {
        self.ref_sites.insert(site, field);
        self.chains.entry(site).or_default();
        ChainSummary {
            site,
            struct_id: target,
        }
    }

    fn visit_field_access(&mut self, base: &TypedExpr, name: &str) {
        if let Some(struct_id) = base.ty.schema_record()
            && let Some((field, _)) = self.schema.struct_field(struct_id, name)
        {
            self.mark_used_field(field);
        }
        self.visit(base, Need::Narrow);
    }

    fn mark_used_field(&mut self, field: FieldId) {
        if matches!(
            self.schema.field(field),
            Some(Field::Value { .. } | Field::Array { .. })
        ) {
            self.usage
                .entry(field.struct_id())
                .or_default()
                .used
                .insert(field);
        }
    }

    fn mark_used_whole(&mut self, ty: &QueryType) {
        match ty {
            QueryType::Record(RecordSource::Schema(struct_id)) => {
                self.usage.entry(*struct_id).or_default().whole = true;
            }
            QueryType::Optional(inner) | QueryType::List(inner) => self.mark_used_whole(inner),
            QueryType::Tuple(elements) => {
                for element in elements {
                    self.mark_used_whole(element);
                }
            }
            QueryType::Record(RecordSource::Projection(fields)) => {
                for field in fields {
                    self.mark_used_whole(&field.ty);
                }
            }
            QueryType::RowRef(_) => {}
            QueryType::Scalar(_) | QueryType::Enum(_) | QueryType::Void | QueryType::Unknown => {}
        }
    }

    /// Attempt to extend chain by a method; decide whether the chain ends
    fn extend_chain(
        &mut self,
        node: NodeId,
        method: MethodId,
        opt_depth: usize,
        args: &[TypedMethodArg],
        base: Option<ChainSummary>,
    ) -> Option<ChainSummary> {
        let chain = base?;
        if opt_depth > 0 {
            return None;
        }
        match method {
            MethodId::ListFilter => {
                let [arg] = args else {
                    return None;
                };
                let predicate = self.translate_expr(&arg.expr, chain.struct_id);
                let state = self.chains.get_mut(&chain.site)?;
                if !state.apply_filter(predicate) {
                    return None;
                }
                Some(chain)
            }
            MethodId::ListSortAsc
            | MethodId::ListSortDesc
            | MethodId::ListSortAscOver
            | MethodId::ListSortDescOver => {
                let keyed = matches!(
                    method,
                    MethodId::ListSortAscOver | MethodId::ListSortDescOver
                );
                let default_desc =
                    matches!(method, MethodId::ListSortDesc | MethodId::ListSortDescOver);
                let keys = if keyed {
                    self.translate_sort(args, chain.struct_id, default_desc)
                } else {
                    None
                };
                let outcome = self.chains.get_mut(&chain.site)?.apply_sort(keys);
                match outcome {
                    SortOutcome::Pushed => {
                        self.rewrites.insert(
                            node,
                            RewritePlan {
                                rewrite: RuntimeRewrite::Skip,
                                trigger: chain.site,
                            },
                        );
                        Some(chain)
                    }
                    SortOutcome::Deferred => Some(chain),
                    SortOutcome::Closed => None,
                }
            }
            MethodId::ListTake | MethodId::ListSkip => {
                let bound = const_int_arg(args);
                let applied = {
                    let state = self.chains.get_mut(&chain.site)?;
                    if method == MethodId::ListTake {
                        state.apply_front_take(bound)
                    } else {
                        state.apply_front_skip(bound)
                    }
                };
                if !applied {
                    return None;
                }
                self.rewrites.insert(
                    node,
                    RewritePlan {
                        rewrite: RuntimeRewrite::Skip,
                        trigger: chain.site,
                    },
                );
                Some(chain)
            }
            MethodId::ListFirst => {
                if !args.is_empty() {
                    self.chains.get_mut(&chain.site)?.close();
                    return None;
                }
                let state = self.chains.get_mut(&chain.site)?;
                state.apply_front_take(Some(1)).then_some(chain)
            }
            MethodId::ListNth => {
                let index = const_int_arg(args);
                if !self.chains.get_mut(&chain.site)?.apply_front_nth(index) {
                    return None;
                }
                self.rewrites.insert(
                    node,
                    RewritePlan {
                        rewrite: RuntimeRewrite::First,
                        trigger: chain.site,
                    },
                );
                Some(chain)
            }
            MethodId::ListLast => {
                if !args.is_empty() {
                    self.chains.get_mut(&chain.site)?.close();
                    return None;
                }
                let state = self.chains.get_mut(&chain.site)?;
                state.apply_last().then_some(chain)
            }
            MethodId::ListReverse => {
                if !self.chains.get_mut(&chain.site)?.apply_reverse() {
                    return None;
                }
                self.rewrites.insert(
                    node,
                    RewritePlan {
                        rewrite: RuntimeRewrite::Skip,
                        trigger: chain.site,
                    },
                );
                Some(chain)
            }
            MethodId::ListTakeBack | MethodId::ListSkipBack => {
                let bound = const_int_arg(args);
                let outcome = {
                    let state = self.chains.get_mut(&chain.site)?;
                    state.apply_back(node, bound, method == MethodId::ListTakeBack)
                };
                let BackOutcome::Applied { previous_owner } = outcome else {
                    return None;
                };
                if let Some(old) = previous_owner {
                    self.rewrites.insert(
                        old,
                        RewritePlan {
                            rewrite: RuntimeRewrite::Skip,
                            trigger: chain.site,
                        },
                    );
                }
                self.rewrites.insert(
                    node,
                    RewritePlan {
                        rewrite: RuntimeRewrite::ReverseOutput,
                        trigger: chain.site,
                    },
                );
                Some(chain)
            }
            MethodId::ListAny | MethodId::ListAll | MethodId::ListFind => {
                if !self.chains.get(&chain.site)?.can_scan(method) {
                    return None;
                }
                let [arg] = args else {
                    return None;
                };
                let predicate = self.translate_expr(&arg.expr, chain.struct_id)?;
                self.chains
                    .get_mut(&chain.site)?
                    .apply_scan(predicate, method);
                let rewrite = match method {
                    MethodId::ListAny => RuntimeRewrite::NonEmpty,
                    MethodId::ListAll => RuntimeRewrite::Empty,
                    MethodId::ListFind => RuntimeRewrite::First,
                    _ => unreachable!(),
                };
                self.rewrites.insert(
                    node,
                    RewritePlan {
                        rewrite,
                        trigger: chain.site,
                    },
                );
                None
            }
            _ => None,
        }
    }
}

fn expand_whole(schema: &ValidatedSchema, usage: &mut HashMap<StructId, FieldPlan>) {
    let mut queue: Vec<StructId> = usage
        .iter()
        .filter(|(_, plan)| plan.whole)
        .map(|(struct_id, _)| *struct_id)
        .collect();
    while let Some(struct_id) = queue.pop() {
        let Some(struct_def) = schema.structs.get(struct_id.index()) else {
            continue;
        };
        for field in &struct_def.fields {
            if let Field::Ref {
                owning: true,
                target,
                ..
            } = field
            {
                let plan = usage.entry(*target).or_default();
                if !plan.whole {
                    plan.whole = true;
                    queue.push(*target);
                }
            }
        }
    }
}

fn method_base_need(method: MethodId, output: Need) -> Need {
    match method_meta::base_need(method) {
        BaseNeed::Narrow => Need::Narrow,
        BaseNeed::Whole => Need::Whole,
        BaseNeed::Output => output,
    }
}

fn const_int_arg(args: &[TypedMethodArg]) -> Option<u64> {
    let [arg] = args else {
        return None;
    };
    match &arg.expr.kind {
        TypedExprKind::Literal(literal) => match literal.value {
            Literal::Int(value) if value >= 0 => Some(value as u64),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pushdown::tests::*;
    use crate::storage::{CmpOp, PushExpr};
    use crate::testdata;
    use crate::value::Scalar;

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
    fn optional_method_chains_not_pushed() {
        let (file, plan, _schema) = typed_and_plan("users.first()?.orders?.take(1)");
        let takes = method_nodes(&file, "take");
        assert_eq!(takes.len(), 1);
        assert_eq!(plan.runtime_rewrite(takes[0]), None);
    }

    #[test]
    fn filters_composition_across_unpushed() {
        let (file, plan, schema) =
            typed_and_plan("users[id > 2][orders.len() > 1][name == \"Ada\"]");
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
    fn list_base_field_access_not_pushdown_trigger() {
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
    fn liveness_projection_fields_pruned() {
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
}
