use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema};

use crate::ast::MutationKind;
use crate::typecheck::error::TypeError;
use crate::typecheck::types::{
    CollectionOriginKind, ProjectionField, QueryType, RecordSource, TypedExpr,
};
use grove_types::Span;

pub(super) fn check_mutation_base(
    typed_base: &TypedExpr,
    opt_depth: usize,
) -> Result<StructId, TypeError> {
    let Some(ty) = typed_base.ty.unwrap_optional_depth(opt_depth) else {
        return Err(TypeError::OptionalDepthMismatch {
            depth: opt_depth,
            ty: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    };
    if ty.is_optional() {
        return Err(TypeError::MutationOnOptional {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    }

    let struct_id = match ty {
        QueryType::Record(RecordSource::Schema(id)) => *id,
        QueryType::List(inner) => match inner.as_ref() {
            QueryType::Record(RecordSource::Schema(id)) => *id,
            QueryType::Record(RecordSource::Projection(_)) => {
                return Err(TypeError::MutationOnProjection {
                    span: typed_base.span,
                });
            }
            _ => {
                return Err(TypeError::MutationOnNonList {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            }
        },
        QueryType::Record(RecordSource::Projection(_)) => {
            return Err(TypeError::MutationOnProjection {
                span: typed_base.span,
            });
        }
        _ => {
            return Err(TypeError::MutationOnNonList {
                got: typed_base.ty.to_string(),
                span: typed_base.span,
            });
        }
    };

    if !typed_base.provenance.rows {
        return Err(TypeError::MutationTargetNotTraceable {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    }
    Ok(struct_id)
}

/// Classifies insert target collection ownership
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InsertTargetSource {
    Root,
    OwningRef,
    NonOwningRef { field: FieldId },
}

/// Check an insert target and return its element struct and ownership source
pub(super) fn check_insert_base(
    typed_base: &TypedExpr,
    opt_depth: usize,
    schema: &ValidatedSchema,
) -> Result<(StructId, InsertTargetSource), TypeError> {
    let Some(ty) = typed_base.ty.unwrap_optional_depth(opt_depth) else {
        return Err(TypeError::OptionalDepthMismatch {
            depth: opt_depth,
            ty: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    };
    if ty.is_optional() {
        return Err(TypeError::MutationOnOptional {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    }

    let struct_id = match ty {
        QueryType::List(inner) => match inner.as_ref() {
            QueryType::Record(RecordSource::Schema(id)) => *id,
            QueryType::Record(RecordSource::Projection(_)) => {
                return Err(TypeError::MutationOnProjection {
                    span: typed_base.span,
                });
            }
            _ => {
                return Err(TypeError::MutationOnNonList {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            }
        },
        QueryType::Record(RecordSource::Schema(_))
        | QueryType::Record(RecordSource::Projection(_)) => {
            return Err(TypeError::InsertOnRecord {
                got: typed_base.ty.to_string(),
                span: typed_base.span,
            });
        }
        _ => {
            return Err(TypeError::MutationOnNonList {
                got: typed_base.ty.to_string(),
                span: typed_base.span,
            });
        }
    };

    if !typed_base.provenance.rows {
        return Err(TypeError::MutationTargetNotTraceable {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    }

    let Some(collection) = typed_base.provenance.collection else {
        return Err(TypeError::InsertTargetNotCollection {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    };
    match collection {
        CollectionOriginKind::Root(root_idx) => {
            if let Some(root) = schema.roots.get(root_idx)
                && root.struct_id == struct_id
            {
            } else {
                return Err(TypeError::MutationTargetNotTraceable {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            };
            Ok((struct_id, InsertTargetSource::Root))
        }
        CollectionOriginKind::Ref(field_id) => {
            let Some(Field::Ref {
                name,
                target,
                is_list,
                owning,
                ..
            }) = schema.field(field_id)
            else {
                return Err(TypeError::MutationTargetNotTraceable {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            };
            if *target != struct_id {
                return Err(TypeError::MutationTargetNotTraceable {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            }
            if !*is_list {
                return Err(TypeError::InsertOnRecord {
                    got: typed_base.ty.to_string(),
                    span: typed_base.span,
                });
            }
            if *owning {
                Ok((struct_id, InsertTargetSource::OwningRef))
            } else if relation_has_owning_endpoint(schema, field_id) {
                let struct_name = schema.structs[field_id.struct_id().index()].name.clone();
                Err(TypeError::InsertOwningBackref {
                    field: name.clone(),
                    struct_name,
                    span: typed_base.span,
                })
            } else {
                Ok((
                    struct_id,
                    InsertTargetSource::NonOwningRef { field: field_id },
                ))
            }
        }
    }
}

pub(super) fn expect_constructed_record(
    ty: &mut QueryType,
    span: Span,
    mutation_kind: MutationKind,
) -> Result<&mut Vec<ProjectionField>, TypeError> {
    match ty {
        QueryType::Record(RecordSource::Projection(p)) => Ok(p),
        ty => Err(TypeError::ArgTypeMismatch {
            method: mutation_kind.to_string(),
            expected: "constructed record".into(),
            got: ty.to_string(),
            span,
        }),
    }
}

pub(super) fn relation_has_owning_endpoint(schema: &ValidatedSchema, field: FieldId) -> bool {
    let Some(relation) = schema.relation_of_field(field) else {
        return false;
    };
    schema.relation_has_owning_endpoint(&schema.relations[relation.index()])
}

/// returns the required RowRef shape for a non-owning field
pub(super) fn row_ref_field_type(field: &Field) -> Option<QueryType> {
    let Field::Ref {
        target,
        optional,
        is_list,
        owning: false,
        ..
    } = field
    else {
        return None;
    };
    let ref_ty = QueryType::RowRef(*target);
    Some(if *is_list {
        QueryType::List(Box::new(ref_ty))
    } else if *optional {
        QueryType::Optional(Box::new(ref_ty))
    } else {
        ref_ty
    })
}

pub(super) fn same_row_ref_shape(expected: &QueryType, got: &QueryType) -> bool {
    match (expected, got) {
        (QueryType::RowRef(_), QueryType::RowRef(_)) => true,
        (QueryType::Optional(expected), QueryType::Optional(got))
        | (QueryType::List(expected), QueryType::List(got)) => same_row_ref_shape(expected, got),
        _ => false,
    }
}

/// Returns the RowRef target struct nested under optional/list
pub(super) fn row_ref_target_of(ty: &QueryType) -> Option<StructId> {
    match ty {
        QueryType::RowRef(target) => Some(*target),
        QueryType::Optional(inner) | QueryType::List(inner) => row_ref_target_of(inner),
        _ => None,
    }
}
