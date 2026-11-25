use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema, ValueType};

use crate::ast::MutationKind;
use crate::typecheck::compat::types_compatible;
use crate::typecheck::error::TypeError;
use crate::typecheck::mutation::{
    expect_constructed_record, relation_has_owning_endpoint, row_ref_field_type, row_ref_target_of,
    same_row_ref_shape,
};
use crate::typecheck::types::{ProjectionField, QueryType, RecordSource, TypedExpr};
use grove_types::Span;

pub(super) fn validate_insert_arg_non_owning(
    field: FieldId,
    struct_id: StructId,
    typed_arg: &mut TypedExpr,
    schema: &ValidatedSchema,
) -> Result<(), TypeError> {
    let Some(Field::Ref { name, .. }) = schema.field(field) else {
        return Err(TypeError::MutationTargetNotTraceable {
            got: typed_arg.ty.to_string(),
            span: typed_arg.span,
        });
    };
    let struct_name = &schema.structs[field.struct_id().index()].name;

    let mut single = QueryType::RowRef(struct_id);
    if types_compatible(&mut single, &mut typed_arg.ty, schema) {
        return Ok(());
    }

    let mut expected_list = QueryType::List(Box::new(QueryType::RowRef(struct_id)));
    if types_compatible(&mut expected_list, &mut typed_arg.ty, schema) {
        return Ok(());
    }

    if row_ref_target_of(&typed_arg.ty).is_some_and(|target| target != struct_id) {
        return Err(TypeError::RowRefTargetMismatch {
            field: name.clone(),
            struct_name: struct_name.clone(),
            expected: single.to_string(),
            got: typed_arg.ty.to_string(),
            span: typed_arg.span,
        });
    }

    Err(TypeError::InsertTargetRowRefRequired {
        field: name.clone(),
        struct_name: struct_name.clone(),
        expected: single.to_string(),
        got: typed_arg.ty.to_string(),
        span: typed_arg.span,
    })
}

/// Validate a non-owning mutation field against its required RowRef shape
pub(super) fn validate_row_ref_field(
    field: &str,
    mut expected: QueryType,
    struct_name: &str,
    proj_field: &mut ProjectionField,
    schema: &ValidatedSchema,
    span: Span,
) -> Result<(), TypeError> {
    if types_compatible(&mut expected, &mut proj_field.ty, schema) {
        return Ok(());
    }

    let got = proj_field.ty.to_string();
    if same_row_ref_shape(&expected, &proj_field.ty) {
        return Err(TypeError::RowRefTargetMismatch {
            field: field.to_string(),
            struct_name: struct_name.to_string(),
            expected: expected.to_string(),
            got,
            span,
        });
    }

    Err(TypeError::RowRefRequired {
        field: field.to_string(),
        struct_name: struct_name.to_string(),
        expected: expected.to_string(),
        got,
        span,
    })
}

/// Validate a supplied mutation field according to its ownership and value kind
pub(super) fn validate_mutation_field(
    struct_id: StructId,
    proj_field: &mut ProjectionField,
    schema: &ValidatedSchema,
    kind: MutationKind,
    span: Span,
) -> Result<(), TypeError> {
    let struct_name = &schema.structs[struct_id.index()].name;
    let Some((field_id, schema_field)) = schema.struct_field(struct_id, &proj_field.name) else {
        return Err(TypeError::StructFieldNotInSchema {
            field: proj_field.name.clone(),
            struct_name: struct_name.to_string(),
            span,
        });
    };
    match schema_field {
        Field::Ref {
            name,
            owning: false,
            ..
        } => {
            if relation_has_owning_endpoint(schema, field_id) {
                return Err(match kind {
                    MutationKind::Insert => TypeError::InsertOwningBackref {
                        field: name.clone(),
                        struct_name: struct_name.to_string(),
                        span,
                    },
                    MutationKind::Update | MutationKind::Delete => TypeError::UpdateOwningBackref {
                        field: name.clone(),
                        struct_name: struct_name.to_string(),
                        span,
                    },
                });
            }
            let expected =
                row_ref_field_type(schema_field).expect("non-owning ref has a row ref type");
            validate_row_ref_field(name, expected, struct_name, proj_field, schema, span)
        }
        Field::Ref {
            name, owning: true, ..
        } => {
            let expected = QueryType::from(schema_field);
            validate_owning_ref_type(
                name,
                struct_name,
                &expected,
                &mut proj_field.ty,
                schema,
                kind,
                span,
            )
        }
        Field::Value { .. } | Field::Array { .. } => {
            let mut expected = QueryType::from(schema_field);
            if types_compatible(&mut expected, &mut proj_field.ty, schema) {
                Ok(())
            } else {
                Err(TypeError::StructFieldTypeMismatch {
                    field: proj_field.name.clone(),
                    struct_name: struct_name.to_string(),
                    expected: expected.to_string(),
                    got: proj_field.ty.to_string(),
                    span,
                })
            }
        }
    }
}

/// Recursively validate constructed owned-child values against the target schema
pub(super) fn validate_owning_ref_type(
    field: &str,
    struct_name: &str,
    expected: &QueryType,
    actual: &mut QueryType,
    schema: &ValidatedSchema,
    kind: MutationKind,
    span: Span,
) -> Result<(), TypeError> {
    match (expected, actual) {
        (QueryType::Optional(expected), QueryType::Optional(actual))
        | (QueryType::List(expected), QueryType::List(actual)) => {
            validate_owning_ref_type(field, struct_name, expected, actual, schema, kind, span)
        }
        (
            QueryType::Record(RecordSource::Schema(target)),
            QueryType::Record(RecordSource::Projection(fields)),
        ) => validate_mutation_fields(*target, fields, schema, kind, span),
        (
            QueryType::Record(RecordSource::Schema(target)),
            QueryType::Record(RecordSource::Schema(actual)),
        ) if target == actual => Ok(()),
        (expected, actual) => {
            let mut expected = expected.clone();
            if types_compatible(&mut expected, actual, schema) {
                Ok(())
            } else {
                Err(TypeError::StructFieldTypeMismatch {
                    field: field.to_string(),
                    struct_name: struct_name.to_string(),
                    expected: expected.to_string(),
                    got: actual.to_string(),
                    span,
                })
            }
        }
    }
}

/// Validate supplied record fields, recursing through owned children
pub(super) fn validate_mutation_fields(
    struct_id: StructId,
    proj_fields: &mut [ProjectionField],
    schema: &ValidatedSchema,
    kind: MutationKind,
    span: Span,
) -> Result<(), TypeError> {
    for proj_field in proj_fields {
        validate_mutation_field(struct_id, proj_field, schema, kind, span)?;
    }
    Ok(())
}

/// Validate an insert argument for a root or owning collection target
pub(super) fn validate_insert_arg_owning(
    sid: StructId,
    typed_arg: &mut TypedExpr,
    schema: &ValidatedSchema,
) -> Result<(), TypeError> {
    let record_ty = match &mut typed_arg.ty {
        QueryType::List(inner) => inner.as_mut(),
        ty => ty,
    };
    let proj_fields = expect_constructed_record(record_ty, typed_arg.span, MutationKind::Insert)?;

    let struct_fields = &schema.structs[sid.index()].fields;
    let struct_name = &schema.structs[sid.index()].name;
    for (local, schema_field) in struct_fields.iter().enumerate() {
        let field_id = FieldId::new(sid, local);
        let (expected_name, is_optional) = match &schema_field {
            Field::Value {
                name,
                ty: ValueType::Optional(_),
                ..
            } => (name, true),
            Field::Value { name, .. } | Field::Array { name, .. } => (name, false),
            Field::Ref {
                name,
                owning: false,
                ..
            } => {
                if relation_has_owning_endpoint(schema, field_id)
                    && proj_fields.iter().any(|f| &f.name == name)
                {
                    return Err(TypeError::InsertOwningBackref {
                        field: name.clone(),
                        struct_name: struct_name.clone(),
                        span: typed_arg.span,
                    });
                }
                continue;
            }
            Field::Ref { name, optional, .. } => (name, *optional),
        };

        if proj_fields.iter().any(|f| &f.name == expected_name) {
            continue;
        }

        if is_optional {
            proj_fields.push(ProjectionField {
                name: expected_name.clone(),
                ty: QueryType::from(schema_field),
            });
        } else {
            return Err(TypeError::InsertMissingRequiredField {
                field: expected_name.clone(),
                struct_name: struct_name.clone(),
                span: typed_arg.span,
            });
        }
    }

    validate_mutation_fields(
        sid,
        proj_fields,
        schema,
        MutationKind::Insert,
        typed_arg.span,
    )
}

/// Validate an update argument against the target struct
pub(super) fn validate_update_arg(
    sid: StructId,
    typed_arg: &mut TypedExpr,
    schema: &ValidatedSchema,
) -> Result<(), TypeError> {
    let proj_fields =
        expect_constructed_record(&mut typed_arg.ty, typed_arg.span, MutationKind::Update)?;

    validate_mutation_fields(
        sid,
        proj_fields,
        schema,
        MutationKind::Update,
        typed_arg.span,
    )
}
