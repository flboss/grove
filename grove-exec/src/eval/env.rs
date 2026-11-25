use std::collections::HashMap;
use std::rc::Rc;

use crate::value::Val;

pub(crate) struct Env {
    scopes: Vec<HashMap<Rc<str>, Val>>,
    pub(crate) element_depth: usize,
}

impl Env {
    pub fn new() -> Self {
        Env {
            scopes: vec![HashMap::new()],
            element_depth: 0,
        }
    }

    pub fn define(&mut self, name: impl Into<Rc<str>>, value: Val) {
        self.scopes
            .last_mut()
            .expect("env always has a scope")
            .insert(name.into(), value);
    }

    pub fn get(&self, name: &str) -> Option<&Val> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    pub fn remove(&mut self, name: &str) -> Option<Val> {
        self.scopes
            .iter_mut()
            .rev()
            .find_map(|scope| scope.remove(name))
    }

    pub fn clear(&mut self) {
        self.scopes.clear();
        self.scopes.push(HashMap::new());
        self.element_depth = 0;
    }

    /// Committed bindings in the base scope
    pub fn bindings(&self) -> impl Iterator<Item = (&str, &Val)> {
        self.scopes
            .first()
            .into_iter()
            .flat_map(|scope| scope.iter().map(|(name, value)| (name.as_ref(), value)))
    }

    /// `depth` counts scopes skipped to reach the identifier
    pub(crate) fn lookup(&self, name: &str, depth: usize) -> Option<&Val> {
        self.scopes.iter().rev().nth(depth)?.get(name)
    }

    pub(crate) fn push_scope(&mut self, scope: HashMap<Rc<str>, Val>) {
        self.scopes.push(scope);
    }

    pub(crate) fn pop_scope(&mut self) {
        if !self.scopes.is_empty() {
            self.scopes.pop();
        }
    }

    pub(crate) fn begin_turn(&mut self) {
        debug_assert_eq!(self.scopes.len(), 1, "turn must start at the base scope");
        debug_assert_eq!(self.element_depth, 0, "turn leaked an element scope");
        self.push_scope(HashMap::new());
    }

    pub(crate) fn commit_turn(&mut self) {
        debug_assert_eq!(
            self.scopes.len(),
            2,
            "successful turn left temporary scopes behind"
        );
        if self.scopes.len() > 1 {
            let turn = self.scopes.pop().expect("turn scope exists");
            if let Some(base) = self.scopes.last_mut() {
                base.extend(turn);
            }
        }
        self.element_depth = 0;
    }

    pub(crate) fn rollback_turn(&mut self) {
        self.scopes.truncate(1);
        self.element_depth = 0;
    }
}
