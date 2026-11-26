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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::harness::{FIXED_NOW, NoStorage, int, run_src, str};
    use crate::eval::{EvalCtx, eval};
    use crate::pushdown::PushdownPlan;
    use crate::value::Val;
    use grove_query::{IdentBinding, QueryType, TargetProvenance, TypedExpr, TypedExprKind};
    use grove_schema::validated::{ScalarType, ValidatedSchema};
    use grove_types::{NodeId, Span, Spanned};

    #[test]
    fn eval_let_binding() {
        assert_eq!(run_src("let x = 1; x"), Ok(int(1)));
        assert_eq!(run_src("let x = 1; let x = 2; x"), Ok(int(2)));
        assert_eq!(
            run_src("let x = true; let x = \"hello\"; x"),
            Ok(str("hello"))
        );
        assert_eq!(run_src("let a = 1; let b = a; b"), Ok(int(1)));
    }

    #[test]
    fn eval_env_depth_collections() {
        let mut env = Env::new();
        env.define(Rc::from("xs"), Val::list(vec![int(1), Val::None]));
        let schema = ValidatedSchema::default();
        let plan = PushdownPlan::disabled(&schema);
        let mut ctx = EvalCtx::new(&mut env, FIXED_NOW, &NoStorage, &schema, &plan);
        let span = Span::new(0, 0);
        let expr = TypedExpr {
            kind: TypedExprKind::Ident {
                name: Spanned::new(span, "xs".to_string()),
                binding: IdentBinding::Let,
                depth: 0,
            },
            ty: QueryType::List(Box::new(QueryType::Optional(Box::new(QueryType::Scalar(
                ScalarType::Int,
            ))))),
            provenance: TargetProvenance::NONE,
            span,
            id: NodeId(0),
        };
        assert_eq!(
            eval(&expr, &mut ctx),
            Ok(Val::list(vec![int(1), Val::None]))
        );
    }
}
