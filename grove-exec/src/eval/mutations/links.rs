use std::collections::HashSet;

use grove_schema::validated::{Field, FieldId, ValidatedSchema};

use crate::error::ExecError;
use crate::value::{RowOrigin, Val};

pub(super) fn link_targets(
    schema: &ValidatedSchema,
    field_id: FieldId,
    value: Val,
) -> Result<Vec<RowOrigin>, ExecError> {
    let mut targets = Vec::new();
    collect_link_targets(schema, field_id, value, &mut targets)?;
    let mut seen = HashSet::new();
    targets.retain(|origin| seen.insert(*origin));
    Ok(targets)
}

fn collect_link_targets(
    schema: &ValidatedSchema,
    field_id: FieldId,
    value: Val,
    targets: &mut Vec<RowOrigin>,
) -> Result<(), ExecError> {
    let Some(Field::Ref { name, target, .. }) = schema.field(field_id) else {
        return Err(ExecError::Internal {
            message: "link field is not a ref".to_string(),
        });
    };
    match value {
        Val::RowRef(origin) => {
            if origin.struct_id != *target {
                return Err(ExecError::Internal {
                    message: format!("row reference target mismatch for field `{name}`"),
                });
            }
            targets.push(origin);
            Ok(())
        }
        Val::None => Ok(()),
        Val::Some(inner) => collect_link_targets(schema, field_id, *inner, targets),
        Val::List(list) => {
            for item in list.items {
                collect_link_targets(schema, field_id, item, targets)?;
            }
            Ok(())
        }
        other => Err(ExecError::Internal {
            message: format!("link field contains non-RowRef value {other}"),
        }),
    }
}
