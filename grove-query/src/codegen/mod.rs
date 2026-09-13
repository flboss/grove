pub mod builder;
mod lower;
pub mod model;
pub mod render;

use model::{
    ColumnRole, CompiledQuery, OutputColumn, PlannedStatement, ResultShape, StatementShape,
};

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
    let (builder, columns) = lower::lower_query(&mut ctx, &file.result);
    let mut sql = String::new();
    let mut params = Vec::new();
    let mut stack = Vec::new();
    render::render_select(&builder, &mut stack, &mut sql, &mut params);
    let shape = StatementShape {
        columns: columns
            .iter()
            .map(|name| OutputColumn {
                name: name.clone(),
                role: ColumnRole::Value,
            })
            .collect(),
    };
    CompiledQuery {
        statements: vec![PlannedStatement { sql, params, shape }],
        result: ResultShape::Rows { columns },
    }
}
