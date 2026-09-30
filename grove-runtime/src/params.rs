
use grove_query::codegen::model::Param;
use rusqlite::types::Value as SqlValue;

use super::decode::encode_param;
use super::error::RuntimeError;

pub fn encode_params(params: &[Param]) -> Result<Vec<SqlValue>, RuntimeError> {
    params
        .iter()
        .map(|param| match param {
            Param::Value(value) => Ok(encode_param(value)),
            Param::FromPrior {
                stmt: prior,
                column,
            } => Err(RuntimeError::Contract(format!(
                "reserved param: FromPrior {{ stmt: {prior}, column: {column} }} \
                 (scalar from a single-row prior statement) has no lane yet"
            ))),
            Param::FromPriorKeys {
                stmt: prior,
                column,
            } => Err(RuntimeError::Contract(format!(
                "reserved param: FromPriorKeys {{ stmt: {prior}, column: {column} }} \
                 (bulk-fill from a prior column) has no lane yet"
            ))),
        })
        .collect()
}
