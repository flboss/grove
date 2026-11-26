use std::collections::HashSet;

use grove_query::methods::MethodId;
use grove_schema::validated::{FieldId, StructId};
use grove_types::NodeId;

use crate::storage::{FetchConstraints, PushExpr, SortKey};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Need {
    Narrow,
    Whole,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ChainSummary {
    pub(super) site: NodeId,
    pub(super) struct_id: StructId,
}

#[derive(Debug, Default)]
pub(super) struct FieldPlan {
    pub(super) used: HashSet<FieldId>,
    pub(super) whole: bool,
}

#[derive(Debug, Default)]
pub(super) struct Order {
    pub(super) keys: Vec<SortKey>,
    pub(super) tie_desc: bool,
}

impl Order {
    pub(super) fn reverse(&mut self) {
        for key in &mut self.keys {
            key.desc = !key.desc;
        }
        self.tie_desc = !self.tie_desc;
    }

    pub(super) fn prepend(&mut self, keys: Vec<SortKey>) {
        self.keys.splice(0..0, keys);
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Window {
    pub(super) offset: u64,
    pub(super) end: Option<u64>,
}

impl Window {
    pub(super) fn take(&mut self, bound: u64) {
        let end = self.offset.saturating_add(bound);
        self.end = Some(self.end.map_or(end, |current| current.min(end)));
    }

    pub(super) fn skip(&mut self, bound: u64) {
        let offset = self.offset.saturating_add(bound);
        self.offset = self.end.map_or(offset, |end| offset.min(end));
    }

    pub(super) fn is_full(self) -> bool {
        self.offset == 0 && self.end.is_none()
    }

    pub(super) fn as_limit(self) -> Option<u64> {
        self.end.map(|end| end.saturating_sub(self.offset))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Barriers {
    pub(super) unpushed_filter: bool,
    pub(super) unpushed_sort: bool,
}

impl Barriers {
    pub(super) fn any(self) -> bool {
        self.unpushed_filter || self.unpushed_sort
    }

    pub(super) fn blocks_window(self) -> bool {
        self.any()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stage {
    #[default]
    Open,
    Front,
    Back {
        reverse_at: NodeId,
    },
    Closed {
        reverse_at: Option<NodeId>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SortOutcome {
    Pushed,
    Deferred,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackOutcome {
    Applied { previous_owner: Option<NodeId> },
    Closed,
}

#[derive(Debug, Default)]
pub(super) struct ChainState {
    pub(super) filters: Vec<PushExpr>,
    pub(super) order: Order,
    pub(super) window: Window,
    pub(super) stage: Stage,
    pub(super) barriers: Barriers,
}

impl ChainState {
    pub(super) fn into_constraints(self) -> FetchConstraints {
        FetchConstraints {
            filters: self.filters,
            sort: self.order.keys,
            tie_desc: self.order.tie_desc,
            offset: self.window.offset,
            limit: self.window.as_limit(),
        }
    }

    pub(super) fn close(&mut self) {
        let reverse_at = match self.stage {
            Stage::Back { reverse_at } => Some(reverse_at),
            Stage::Closed { reverse_at } => reverse_at,
            Stage::Open | Stage::Front => None,
        };
        self.stage = Stage::Closed { reverse_at };
    }

    pub(super) fn apply_filter(&mut self, predicate: Option<PushExpr>) -> bool {
        if !matches!(self.stage, Stage::Open) {
            self.close();
            return false;
        }
        match predicate {
            Some(predicate) => self.filters.push(predicate),
            None => self.barriers.unpushed_filter = true,
        }
        true
    }

    pub(super) fn apply_sort(&mut self, keys: Option<Vec<SortKey>>) -> SortOutcome {
        if !matches!(self.stage, Stage::Open) {
            self.close();
            return SortOutcome::Closed;
        }
        if self.barriers.unpushed_sort {
            return SortOutcome::Deferred;
        }
        match keys {
            Some(keys) => {
                self.order.prepend(keys);
                SortOutcome::Pushed
            }
            None => {
                self.barriers.unpushed_sort = true;
                SortOutcome::Deferred
            }
        }
    }

    pub(super) fn apply_front_take(&mut self, bound: Option<u64>) -> bool {
        if !matches!(self.stage, Stage::Open | Stage::Front) {
            self.close();
            return false;
        }
        let Some(bound) = bound else {
            self.close();
            return false;
        };
        if self.barriers.blocks_window() {
            self.close();
            return false;
        }
        self.window.take(bound);
        self.stage = Stage::Front;
        true
    }

    pub(super) fn apply_front_skip(&mut self, bound: Option<u64>) -> bool {
        if !matches!(self.stage, Stage::Open | Stage::Front) {
            self.close();
            return false;
        }
        let Some(bound) = bound else {
            self.close();
            return false;
        };
        if self.barriers.blocks_window() {
            self.close();
            return false;
        }
        self.window.skip(bound);
        self.stage = Stage::Front;
        true
    }

    pub(super) fn apply_front_nth(&mut self, index: Option<u64>) -> bool {
        if !matches!(self.stage, Stage::Open | Stage::Front) {
            self.close();
            return false;
        }
        let Some(index) = index else {
            self.close();
            return false;
        };
        if self.barriers.blocks_window() {
            self.close();
            return false;
        }
        self.window.skip(index);
        self.window.take(1);
        self.stage = Stage::Front;
        true
    }

    pub(super) fn apply_last(&mut self) -> bool {
        if !matches!(self.stage, Stage::Open) || self.barriers.blocks_window() {
            self.close();
            return false;
        }
        self.order.reverse();
        self.window.take(1);
        self.stage = Stage::Front;
        true
    }

    pub(super) fn apply_reverse(&mut self) -> bool {
        if !matches!(self.stage, Stage::Open) || self.barriers.any() {
            self.close();
            return false;
        }
        self.order.reverse();
        true
    }

    pub(super) fn apply_back(
        &mut self,
        node: NodeId,
        bound: Option<u64>,
        take: bool,
    ) -> BackOutcome {
        if !matches!(self.stage, Stage::Open | Stage::Back { .. }) || self.barriers.blocks_window()
        {
            self.close();
            return BackOutcome::Closed;
        }
        let Some(bound) = bound else {
            self.close();
            return BackOutcome::Closed;
        };
        let previous_owner = match self.stage {
            Stage::Back { reverse_at } => Some(reverse_at),
            _ => None,
        };
        if matches!(self.stage, Stage::Open) {
            debug_assert!(self.window.is_full());
            self.order.reverse();
        }
        if take {
            self.window.take(bound);
        } else {
            self.window.skip(bound);
        }
        self.stage = Stage::Back { reverse_at: node };
        BackOutcome::Applied { previous_owner }
    }

    pub(super) fn can_scan(&self, method: MethodId) -> bool {
        matches!(self.stage, Stage::Open)
            && !self.barriers.unpushed_filter
            && !(method == MethodId::ListFind && self.barriers.unpushed_sort)
    }

    pub(super) fn apply_scan(&mut self, predicate: PushExpr, method: MethodId) {
        match method {
            MethodId::ListAny | MethodId::ListFind => self.filters.push(predicate),
            MethodId::ListAll => self.filters.push(PushExpr::Not(Box::new(predicate))),
            _ => unreachable!("apply_scan called with a non-scan method"),
        }
        self.window.take(1);
        self.stage = Stage::Closed { reverse_at: None };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pushdown::RuntimeRewrite;
    use crate::pushdown::tests::*;
    use crate::storage::CmpOp;
    use crate::value::Scalar;

    #[test]
    fn take_skip_folding() {
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
}
