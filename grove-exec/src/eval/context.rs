use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use grove_schema::validated::ValidatedSchema;
use grove_types::NodeId;

use super::env::Env;
use crate::pushdown::PushdownPlan;
use crate::storage::Storage;
use crate::value::{Interner, Val};

pub(crate) struct EvalCtx<'a> {
    pub(super) env: &'a mut Env,
    pub(super) now: DateTime<Utc>,
    pub(crate) interner: Interner,
    pub(crate) storage: &'a dyn Storage,
    pub(crate) schema: &'a ValidatedSchema,
    pub(crate) plan: &'a PushdownPlan,
    pub(crate) applied_triggers: HashSet<NodeId>,
}

impl<'a> EvalCtx<'a> {
    pub fn new(
        env: &'a mut Env,
        now: DateTime<Utc>,
        storage: &'a dyn Storage,
        schema: &'a ValidatedSchema,
        plan: &'a PushdownPlan,
    ) -> Self {
        EvalCtx {
            env,
            now,
            interner: Interner::default(),
            storage,
            schema,
            plan,
            applied_triggers: HashSet::new(),
        }
    }
}

impl EvalCtx<'_> {
    pub(crate) fn push_binding_scope(&mut self, name: &str, value: Val) {
        let interned = self.interner.intern(name);
        let mut scope = HashMap::new();
        scope.insert(interned, value);
        self.env.push_scope(scope);
    }
}
