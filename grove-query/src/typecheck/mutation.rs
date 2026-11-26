use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema};

use crate::ast::MutationKind;
use crate::typecheck::error::TypeError;
use crate::typecheck::types::{
    CollectionOriginKind, ProjectionField, QueryType, RecordSource, TypedExpr,
};
use grove_types::Span;

/// Unwrap an optional mutation target, classify it, and require traceable rows
fn traceable_mutation_target<T>(
    typed_base: &TypedExpr,
    opt_depth: usize,
    classify: impl FnOnce(&QueryType) -> Result<T, TypeError>,
) -> Result<T, TypeError> {
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
    let target = classify(ty)?;
    if !typed_base.provenance.rows {
        return Err(TypeError::MutationTargetNotTraceable {
            got: typed_base.ty.to_string(),
            span: typed_base.span,
        });
    }
    Ok(target)
}

/// Validate an update/delete base and returns the target struct
pub(super) fn check_mutation_base(
    typed_base: &TypedExpr,
    opt_depth: usize,
) -> Result<StructId, TypeError> {
    traceable_mutation_target(typed_base, opt_depth, |ty| {
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
        Ok(struct_id)
    })
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
    let struct_id = traceable_mutation_target(typed_base, opt_depth, |ty| {
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
        Ok(struct_id)
    })?;

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

#[cfg(test)]
mod tests {
    use crate::typecheck::tests::{
        error_codes, infer_ok, mutation_schema, test_schema, typecheck_ok,
    };
    use crate::typecheck::{
        IdentBinding, TargetProvenance, TypedExprKind, TypedMutationStmt, TypedStatement, typecheck,
    };

    #[test]
    fn ident_binding_prev() {
        let schema = test_schema();
        let user = schema.roots[0].struct_id;
        let (file, _) = crate::parse_query(r#"users[age == 30].update({ age = prev.age + 1 });"#);
        let (typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let TypedStatement::Mutation(TypedMutationStmt::Update { arg, .. }) =
            &typed.unwrap().statements[0]
        else {
            panic!("expected update statement");
        };
        let TypedExprKind::Struct { fields, .. } = &arg.kind else {
            panic!("expected struct, got {:?}", arg.kind);
        };
        let TypedExprKind::Binary { lhs, .. } = &fields[0].1.kind else {
            panic!("expected binary, got {:?}", fields[0].1.kind);
        };
        let TypedExprKind::Field { base, .. } = &lhs.kind else {
            panic!("expected field, got {:?}", lhs.kind);
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &base.kind
        else {
            panic!("expected ident, got {:?}", base.kind);
        };
        assert_eq!(name.value, "prev");
        assert_eq!(*binding, IdentBinding::Prev { struct_id: user });
        assert_eq!(*depth, 0);
    }

    #[test]
    fn mutation_delete() {
        let (file, _diags) = crate::parse_query(r#"users[!active].delete();"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_on_projection_error() {
        let (file, _diags) = crate::parse_query(r#"users { name }.insert({ name = "Alice" });"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_on_scalar_error() {
        let (file, _diags) = crate::parse_query(r#"42.insert({ name = "Alice" });"#);
        let (_typed, diags) = typecheck(file.unwrap(), &test_schema());
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_optional_depth_preserved() {
        let typed = typecheck_ok("users.first()?.delete();", &test_schema());
        let TypedStatement::Mutation(TypedMutationStmt::Delete { opt_depth, .. }) =
            &typed.statements[0]
        else {
            panic!("expected delete mutation");
        };
        assert_eq!(*opt_depth, 1);
    }

    #[test]
    fn mutation_record_targets() {
        let schema = test_schema();
        typecheck_ok("users.first().unwrap().delete();", &schema);
        typecheck_ok("users.first().unwrap().update({ age = 31 });", &schema);
        typecheck_ok("users.first()?.delete();", &schema);
        typecheck_ok("users.first()?.update({ age = 31 });", &schema);
    }

    #[test]
    fn mutation_optional_target_errors() {
        let schema = test_schema();
        assert_eq!(error_codes("users.first().delete();", &schema), ["QT0035"]);
        assert_eq!(
            error_codes("users.first().update({ age = 31 });", &schema),
            ["QT0035"]
        );
        assert_eq!(
            error_codes("users.first()??.delete();", &schema),
            ["QT0032"]
        );
    }

    #[test]
    fn mutation_let_targets_rejected() {
        let schema = mutation_schema();
        assert_eq!(
            error_codes("let u = users; u.delete();", &schema),
            ["QT0036"]
        );
        assert_eq!(
            error_codes(
                r#"let u = users; u.insert({ id = 1, name = "A", orders = [] });"#,
                &schema
            ),
            ["QT0036"]
        );
    }

    #[test]
    fn mutation_insert_targets() {
        let schema = mutation_schema();
        typecheck_ok(
            r#"users.insert({ id = 1, name = "A", orders = [] });"#,
            &schema,
        );
        typecheck_ok(
            r#"users[id > 0].insert({ id = 1, name = "A", orders = [] });"#,
            &schema,
        );
        typecheck_ok(
            r#"users.first().unwrap().orders.insert({ id = 1, total = 10 });"#,
            &schema,
        );
        typecheck_ok(
            r#"users.first()?.orders?.insert({ id = 1, total = 10 });"#,
            &schema,
        );
        typecheck_ok(
            r#"images.first().unwrap().orders.insert(&users.orders.flatten());"#,
            &schema,
        );
        typecheck_ok(
            r#"images.first().unwrap().orders.insert(&users.orders.flatten().first().unwrap());"#,
            &schema,
        );

        assert_eq!(
            error_codes(
                r#"users.first().unwrap().insert({ id = 1, name = "A", orders = [] });"#,
                &schema
            ),
            ["QT0037"]
        );
        assert_eq!(
            error_codes(
                r#"users.orders.flatten().insert({ id = 1, total = 10 });"#,
                &schema
            ),
            ["QT0038"]
        );
        assert_eq!(
            error_codes(
                r#"users.extend(users.take(0)).insert({ id = 1, name = "A", orders = [] });"#,
                &schema
            ),
            ["QT0038"]
        );
        assert_eq!(
            error_codes(
                r#"images.first().unwrap().orders.insert({ id = 1, total = 10 });"#,
                &schema
            ),
            ["QT0047"]
        );
        assert_eq!(
            error_codes(
                r#"images.first().unwrap().orders.insert(users.orders.flatten());"#,
                &schema
            ),
            ["QT0047"]
        );
        assert_eq!(
            error_codes(
                r#"images.first().unwrap().orders.insert(&users.orders.flatten().first());"#,
                &schema
            ),
            ["QT0047"]
        );
        assert_eq!(
            error_codes(r#"images.first().unwrap().orders.insert(&users);"#, &schema),
            ["QT0045"]
        );
    }

    #[test]
    fn mutation_nested_and_non_owning_delete_targets() {
        let schema = mutation_schema();
        typecheck_ok("users.orders.flatten().delete();", &schema);
        typecheck_ok("users.orders.flatten().update({ total = 1 });", &schema);
        typecheck_ok("users.first()?.orders?.delete();", &schema);
        typecheck_ok("images.first().unwrap().orders.delete();", &schema);
    }

    #[test]
    fn mutation_projected_and_untraceable_targets() {
        let schema = mutation_schema();
        assert_eq!(
            error_codes(
                r#"users { id }.insert({ id = 1, name = "A", orders = [] });"#,
                &schema
            ),
            ["QT0019"]
        );
        assert_eq!(error_codes("users { id }.delete();", &schema), ["QT0019"]);
        assert_eq!(
            error_codes(
                r#"users.map($).insert({ id = 1, name = "A", orders = [] });"#,
                &schema
            ),
            ["QT0036"]
        );
        assert_eq!(error_codes("users.map($).delete();", &schema), ["QT0036"]);
    }

    #[test]
    fn mutation_target_provenance_tracked() {
        let schema = test_schema();
        let user = schema.roots[0].struct_id;
        let profile_field = schema.struct_field(user, "profile").unwrap().0;

        assert_eq!(
            infer_ok("users", &schema).provenance,
            TargetProvenance::root(0)
        );
        assert_eq!(
            infer_ok("users.filter(active)", &schema).provenance,
            TargetProvenance::root(0)
        );
        assert_eq!(
            infer_ok("users.first().unwrap()", &schema).provenance,
            TargetProvenance {
                rows: true,
                collection: None,
            }
        );
        assert_eq!(
            infer_ok("users.first().unwrap().profile", &schema).provenance,
            TargetProvenance::ref_collection(profile_field)
        );
        assert_eq!(
            infer_ok("users.profile", &schema).provenance,
            TargetProvenance::ref_collection(profile_field)
        );
        assert_eq!(
            infer_ok("users.name", &schema).provenance,
            TargetProvenance::NONE
        );

        assert_eq!(
            infer_ok("some(users)", &schema).provenance,
            TargetProvenance::root(0)
        );
        assert_eq!(
            infer_ok("if true { users } else { users }", &schema).provenance,
            TargetProvenance::root(0)
        );
        assert_eq!(
            infer_ok("users.extend(users.take(0))", &schema).provenance,
            TargetProvenance {
                rows: true,
                collection: None,
            }
        );
        assert_eq!(
            infer_ok("users.map($)", &schema).provenance,
            TargetProvenance::NONE
        );
        assert_eq!(
            infer_ok("users { name }", &schema).provenance,
            TargetProvenance::NONE
        );
    }

    #[test]
    fn mutation_target_provenance_not_propagated_through_let() {
        let typed = typecheck_ok("let u = users; u", &test_schema());
        assert_eq!(typed.result.unwrap().provenance, TargetProvenance::NONE);
    }
}
