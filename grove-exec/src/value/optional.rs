use crate::error::ExecError;
use crate::value::Val;

pub(crate) fn rewrap_optional(depth: usize, mut value: Val) -> Val {
    for _ in 0..depth {
        value = Val::Some(Box::new(value));
    }
    value
}

pub(crate) fn unwrap_optional_depth(
    mut value: Val,
    depth: usize,
) -> Result<(Val, usize), ExecError> {
    let mut peeled = 0;
    while peeled < depth {
        match value {
            Val::Some(inner) => {
                value = *inner;
                peeled += 1;
            }
            Val::None => return Ok((Val::None, peeled)),
            other => {
                return Err(ExecError::Internal {
                    message: format!("optional access expected {depth} layer(s), found {other}"),
                });
            }
        }
    }
    Ok((value, peeled))
}
