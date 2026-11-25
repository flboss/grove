use std::collections::HashMap;
use std::rc::Rc;

use super::super::EvalCtx;
use crate::value::Val;

fn push_dollar_scope(ctx: &mut EvalCtx<'_>, item: Val) {
    ctx.env.element_depth += 1;
    let scope = [(ctx.interner.intern("$".repeat(ctx.env.element_depth)), item)].into();
    ctx.env.push_scope(scope);
}

pub(crate) fn push_element_scope(ctx: &mut EvalCtx<'_>, item: &Val) -> bool {
    match item {
        Val::Record(record) => {
            let scope: HashMap<Rc<str>, Val> = record
                .iter()
                .map(|(name, value)| (ctx.interner.intern(name.as_ref()), value.clone()))
                .collect();
            ctx.env.push_scope(scope);
            false
        }
        _ => {
            push_dollar_scope(ctx, item.clone());
            true
        }
    }
}

pub(crate) fn pop_element_scope(ctx: &mut EvalCtx<'_>, map_element: bool) {
    ctx.env.pop_scope();
    if map_element {
        ctx.env.element_depth -= 1;
    }
}

pub(crate) fn push_map_scope(ctx: &mut EvalCtx<'_>, item: Val) {
    push_dollar_scope(ctx, item);
}

pub(crate) fn pop_map_scope(ctx: &mut EvalCtx<'_>) {
    pop_element_scope(ctx, true);
}
