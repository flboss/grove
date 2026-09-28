pub mod builder;
mod lower;
pub mod model;
pub mod render;

use model::{CompiledQuery, Param, PlannedStatement, ResultShape};

use grove_schema::validated::ValidatedSchema;

use crate::typecheck::types::{TypedQueryFile, TypedStatement};

use lower::Context;

pub fn codegen(file: &TypedQueryFile, schema: &ValidatedSchema) -> CompiledQuery {
    let mut ctx = Context::new(schema);
    for stmt in &file.statements {
        if let TypedStatement::Mutation(_) = stmt {
            todo!("mutation statements");
        }
    }
    let root = lower::lower_query(&mut ctx, &file.result);
    let root_stmt = render_one(&root.builder);
    let mut rendered = vec![(root_stmt.0, root_stmt.1, root.shape)];
    for sunk in &ctx.statements {
        let (sql, params) = render_one(&sunk.builder);
        rendered.push((sql, params, sunk.shape.clone()));
    }
    let n_children = ctx.statements.len();
    let mut statements = Vec::with_capacity(rendered.len());
    let (sql, params, shape) = rendered.remove(0);
    statements.push(PlannedStatement { sql, params, shape });
    for (sql, params, shape) in rendered.into_iter().rev() {
        statements.push(PlannedStatement { sql, params, shape });
    }
    let mut result = root.result;
    remap_shape(&mut result, n_children);
    CompiledQuery {
        statements,
        result,
        literals: ctx.literals,
    }
}

fn render_one(builder: &builder::SelectBuilder) -> (String, Vec<Param>) {
    let mut sql = String::new();
    let mut params = Vec::new();
    let mut stack = Vec::new();
    render::render_select(builder, &mut stack, &mut sql, &mut params);
    (sql, params)
}

fn remap_idx(i: usize, n_children: usize) -> usize {
    if i == 0 { 0 } else { n_children - i + 1 }
}

fn remap_shape(shape: &mut ResultShape, n_children: usize) {
    match shape {
        ResultShape::Column(_) => {}
        ResultShape::Tuple(elements) => {
            for e in elements {
                remap_shape(e, n_children);
            }
        }
        ResultShape::Record(fields) => {
            for (_, e) in fields {
                remap_shape(e, n_children);
            }
        }
        ResultShape::List { element, child } | ResultShape::Single { element, child } => {
            remap_shape(element, n_children);
            child.stmt = remap_idx(child.stmt, n_children);
            if let Some(parent) = child.parent.as_mut() {
                parent.stmt = remap_idx(parent.stmt, n_children);
            }
        }
    }
}
