pub mod builder;
mod lower;
pub mod model;
use model::CompiledQuery;

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
    let (_builder, _columns) = lower::lower_query(&mut ctx, &file.result);
    todo!()
}
