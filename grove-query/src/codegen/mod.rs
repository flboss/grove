pub mod builder;
#[cfg(test)]
mod corpus;
mod lower;
pub mod model;
pub mod render;

use model::{CompiledQuery, Param, PlannedStatement, ResultShape, StatementKind};

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
    debug_assert!(
        !lower::has_pending_parent(&root.result),
        "pending grandchild pairing leaked to emission"
    );
    let root_rendered = render_one(&root.builder);
    let mut rendered: Vec<PlannedStatement> = Vec::with_capacity(ctx.statements.len());
    for sunk in &ctx.statements {
        let (mut sql, params) = render_one(&sunk.builder);
        if let StatementKind::Temp { name } = &sunk.kind {
            sql = format!("CREATE TEMP TABLE {} AS {sql}", render::quote(name));
        }
        rendered.push(PlannedStatement {
            sql,
            params,
            shape: sunk.shape.clone(),
            kind: sunk.kind.clone(),
        });
    }
    let mut order: Vec<usize> = Vec::with_capacity(rendered.len() + 1);
    for (i, stmt) in rendered.iter().enumerate() {
        if matches!(stmt.kind, StatementKind::Temp { .. }) {
            order.push(i + 1);
        }
    }
    order.push(0);
    for (i, stmt) in rendered.iter().enumerate().rev() {
        if matches!(stmt.kind, StatementKind::Query) {
            order.push(i + 1);
        }
    }
    let mut pos = vec![0usize; rendered.len() + 1];
    for (final_idx, creation_idx) in order.iter().enumerate() {
        pos[*creation_idx] = final_idx;
    }
    let mut statements = Vec::with_capacity(order.len());
    for creation_idx in order {
        if creation_idx == 0 {
            statements.push(PlannedStatement {
                sql: root_rendered.0.clone(),
                params: root_rendered.1.clone(),
                shape: root.shape.clone(),
                kind: StatementKind::Query,
            });
        } else {
            statements.push(rendered[creation_idx - 1].clone());
        }
    }
    let mut result = root.result;
    remap_shape(&mut result, &pos);
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

fn remap_shape(shape: &mut ResultShape, pos: &[usize]) {
    match shape {
        ResultShape::Column(_) => {}
        ResultShape::Tuple(elements) => {
            for e in elements {
                remap_shape(e, pos);
            }
        }
        ResultShape::Record(fields) => {
            for (_, e) in fields {
                remap_shape(e, pos);
            }
        }
        ResultShape::List { element, child } | ResultShape::Single { element, child } => {
            remap_shape(element, pos);
            child.stmt = pos[child.stmt];
            if let Some(parent) = child.parent.as_mut() {
                parent.stmt = pos[parent.stmt];
            }
        }
    }
}
