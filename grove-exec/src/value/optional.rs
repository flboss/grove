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

#[cfg(test)]
mod tests {
    use crate::eval::harness::{int, some};

    use super::*;

    #[test]
    fn option_display() {
        assert_eq!(Val::None.to_string(), "none");
        assert_eq!(some(int(3)).to_string(), "some(3)");
        assert_eq!(some(some(int(3))).to_string(), "some(some(3))");
    }

    #[test]
    fn optional_depth_helpers() {
        let value = some(some(int(1)));
        assert_eq!(unwrap_optional_depth(value, 2).unwrap(), (int(1), 2));
        assert_eq!(
            unwrap_optional_depth(some(Val::None), 2).unwrap(),
            (Val::None, 1)
        );
        assert_eq!(rewrap_optional(2, int(1)), some(some(int(1))));
        assert_eq!(rewrap_optional(1, Val::None), some(Val::None));
    }
}
