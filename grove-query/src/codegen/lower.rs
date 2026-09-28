
use super::builder::{AliasGen, SelectBuilder};
use crate::typecheck::types::TypedExpr;
use grove_schema::validated::ValidatedSchema;

#[allow(dead_code)]
pub struct Context<'s> {
    pub schema: &'s ValidatedSchema,
    pub aliases: AliasGen,
}

impl<'s> Context<'s> {
    pub fn new(schema: &'s ValidatedSchema) -> Self {
        Context {
            schema,
            aliases: AliasGen::default(),
        }
    }
}

pub fn lower_query(ctx: &mut Context, result: &TypedExpr) -> (SelectBuilder, Vec<String>) {
    let _ = (ctx, result);
    todo!("codegen rewrite: scaffold — lowering lands in milestone M1");
}
