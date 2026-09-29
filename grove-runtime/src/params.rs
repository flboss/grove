
use grove_query::codegen::model::Param;

use super::decode::encode_param;
use super::error::RuntimeError;

pub fn bind_param(
    stmt: &mut rusqlite::Statement<'_>,
    index: usize,
    position: usize,
    param: &Param,
) -> Result<(), RuntimeError> {
    match param {
        Param::Value(value) => stmt
            .raw_bind_parameter(position, encode_param(value))
            .map_err(|err| RuntimeError::Bind {
                index,
                message: err.to_string(),
            }),
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
    }
}
