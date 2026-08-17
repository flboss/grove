use crate::{
    ast::{FkMapping, RelationArrow},
    validated::{ColumnId, Field, FieldId, Relation, StructId, TableId},
    validation_error::SchemaValidationError,
};

use super::Builder;

impl<'s> Builder<'s> {
    pub(super) fn stage_relations(&mut self) {
        for rel in &self.schema.relations {
            let Some(&child) = self.struct_index.get(rel.child.struct_name.as_str()) else {
                continue;
            };
            let Some(&parent) = self.struct_index.get(rel.parent.struct_name.as_str()) else {
                continue;
            };
            let is_self = child == parent;

            let child_fid = self
                .field_index
                .get(&(
                    rel.child.struct_name.as_str(),
                    rel.child.field_name.as_str(),
                ))
                .copied();
            let parent_fid = self
                .field_index
                .get(&(
                    rel.parent.struct_name.as_str(),
                    rel.parent.field_name.as_str(),
                ))
                .copied();

            let (Some(child_fid), Some(parent_fid)) = (child_fid, parent_fid) else {
                for (fid, endpoint) in [(child_fid, &rel.child), (parent_fid, &rel.parent)] {
                    if fid.is_none() {
                        self.emit_error(SchemaValidationError::RelationEndpointMissing {
                            span: endpoint.field_name.span,
                            struct_name: endpoint.struct_name.value.clone(),
                            field: endpoint.field_name.value.clone(),
                        });
                    }
                }
                continue;
            };

            if child_fid == parent_fid {
                self.emit_error(SchemaValidationError::DuplicateEndpoint {
                    span: rel.child.field_name.span,
                    struct_name: rel.child.struct_name.value.clone(),
                    field: rel.child.field_name.value.clone(),
                });
                continue;
            }
            if self.field_is_relation_endpoint(child_fid) {
                self.emit_error(SchemaValidationError::DuplicateEndpoint {
                    span: rel.child.field_name.span,
                    struct_name: rel.child.struct_name.value.clone(),
                    field: rel.child.field_name.value.clone(),
                });
                continue;
            }
            if self.field_is_relation_endpoint(parent_fid) {
                self.emit_error(SchemaValidationError::DuplicateEndpoint {
                    span: rel.parent.field_name.span,
                    struct_name: rel.parent.struct_name.value.clone(),
                    field: rel.parent.field_name.value.clone(),
                });
                continue;
            }

            let child_field =
                self.structs[child_fid.struct_id().index()].fields[child_fid.local()].clone();
            let parent_field =
                self.structs[parent_fid.struct_id().index()].fields[parent_fid.local()].clone();

            let child_shape = match &child_field {
                Field::Ref {
                    owning, is_list, ..
                } => Some(EndpointShape {
                    owning: *owning,
                    is_list: *is_list,
                }),
                _ => {
                    self.emit_error(SchemaValidationError::EndpointShapeMismatch {
                        span: rel.child.field_name.span,
                        struct_name: rel.child.struct_name.value.clone(),
                        field: rel.child.field_name.value.clone(),
                    });
                    None
                }
            };
            let parent_shape = match &parent_field {
                Field::Ref {
                    owning, is_list, ..
                } => Some(EndpointShape {
                    owning: *owning,
                    is_list: *is_list,
                }),
                _ => {
                    self.emit_error(SchemaValidationError::EndpointShapeMismatch {
                        span: rel.parent.field_name.span,
                        struct_name: rel.parent.struct_name.value.clone(),
                        field: rel.parent.field_name.value.clone(),
                    });
                    None
                }
            };

            if let (Some(child_shape), Some(parent_shape)) = (child_shape, parent_shape) {
                let (expect_child, expect_parent) =
                    expected_endpoint_shapes(rel.arrow.value, is_self);
                if child_shape != expect_child {
                    self.emit_error(SchemaValidationError::EndpointShapeMismatch {
                        span: rel.child.field_name.span,
                        struct_name: rel.child.struct_name.value.clone(),
                        field: rel.child.field_name.value.clone(),
                    });
                }
                if parent_shape != expect_parent {
                    self.emit_error(SchemaValidationError::EndpointShapeMismatch {
                        span: rel.parent.field_name.span,
                        struct_name: rel.parent.struct_name.value.clone(),
                        field: rel.parent.field_name.value.clone(),
                    });
                }
                if matches!(rel.arrow.value, RelationArrow::OneToMany)
                    && !matches!(&parent_field, Field::Ref { optional: true, .. })
                {
                    self.emit_error(SchemaValidationError::EndpointShapeMismatch {
                        span: rel.parent.field_name.span,
                        struct_name: rel.parent.struct_name.value.clone(),
                        field: rel.parent.field_name.value.clone(),
                    });
                }
            }

            if let (
                Field::Ref {
                    target: child_target,
                    ..
                },
                Field::Ref {
                    target: parent_target,
                    ..
                },
            ) = (&child_field, &parent_field)
            {
                if *child_target != parent {
                    self.emit_error(SchemaValidationError::EndpointTargetMismatch {
                        span: rel.child.field_name.span,
                        struct_name: rel.child.struct_name.value.clone(),
                        field: rel.child.field_name.value.clone(),
                        actual: self.structs[child_target.index()].name.clone(),
                        expected: self.structs[parent.index()].name.clone(),
                    });
                }
                if *parent_target != child {
                    self.emit_error(SchemaValidationError::EndpointTargetMismatch {
                        span: rel.parent.field_name.span,
                        struct_name: rel.parent.struct_name.value.clone(),
                        field: rel.parent.field_name.value.clone(),
                        actual: self.structs[parent_target.index()].name.clone(),
                        expected: self.structs[child.index()].name.clone(),
                    });
                }
            }

            let columns = match self.resolve_relation_columns(rel, child, parent) {
                Ok(columns) => columns,
                Err(err) => {
                    self.emit_error(err);
                    continue;
                }
            };

            let relation = match rel.arrow.value {
                RelationArrow::OneToOne => {
                    let RelationColumns::Direct { fk, pk } = columns else {
                        unreachable!("OneToOne uses a direct FK mapping")
                    };
                    Relation::OneToOne {
                        child_ref: child_fid,
                        parent_ref: parent_fid,
                        fk,
                        pk,
                    }
                }
                RelationArrow::ManyToOne => {
                    let RelationColumns::Direct { fk, pk } = columns else {
                        unreachable!("ManyToOne uses a direct FK mapping")
                    };
                    Relation::ManyToOne {
                        child_ref: child_fid,
                        parent_ref: parent_fid,
                        fk,
                        pk,
                    }
                }
                RelationArrow::OneToMany => {
                    let RelationColumns::Direct { fk, pk } = columns else {
                        unreachable!("OneToMany uses a direct FK mapping")
                    };
                    Relation::OneToMany {
                        a_ref: child_fid,
                        b_ref: parent_fid,
                        fk,
                        pk,
                    }
                }
                RelationArrow::ManyToMany => {
                    let RelationColumns::Indirect {
                        join_table,
                        a_col,
                        a_pk,
                        b_col,
                        b_pk,
                    } = columns
                    else {
                        unreachable!("ManyToMany uses a join-table mapping")
                    };
                    Relation::ManyToMany {
                        a_ref: child_fid,
                        b_ref: parent_fid,
                        join_table,
                        a_col,
                        a_pk,
                        b_col,
                        b_pk,
                    }
                }
            };
            self.relations.push(relation);
        }
    }

    fn field_is_relation_endpoint(&self, fid: FieldId) -> bool {
        self.relations.iter().any(|r| r.endpoints().contains(&fid))
    }

    fn resolve_relation_columns(
        &self,
        rel: &'s crate::ast::Relation,
        child: StructId,
        parent: StructId,
    ) -> Result<RelationColumns, SchemaValidationError> {
        let child_tid = self.struct_table[child.index()];
        let parent_tid = self.struct_table[parent.index()];
        let is_one_to_many = matches!(rel.arrow.value, RelationArrow::OneToMany);
        let is_many_to_many = matches!(rel.arrow.value, RelationArrow::ManyToMany);
        match &rel.fk {
            FkMapping::Direct { child, parent } => {
                if is_many_to_many {
                    return Err(SchemaValidationError::DirectOnManyToMany {
                        span: rel.arrow.span,
                    });
                }
                let (fk_tid, pk_tid) = if is_one_to_many {
                    (parent_tid, child_tid)
                } else {
                    (child_tid, parent_tid)
                };
                let fk = self
                    .column_index
                    .get(&(fk_tid, child.col.as_str()))
                    .copied()
                    .expect("relation FK column claimed in physical stage");
                let pk = self
                    .column_index
                    .get(&(pk_tid, parent.col.as_str()))
                    .copied()
                    .expect("relation PK column claimed in physical stage");
                Ok(RelationColumns::Direct { fk, pk })
            }
            FkMapping::Indirect {
                join_table, a, b, ..
            } => {
                if !is_many_to_many {
                    return Err(SchemaValidationError::ViaOnOwning {
                        span: rel.arrow.span,
                    });
                }
                let j_tid = self.table_index[join_table.as_str()];
                let a_col = self
                    .column_index
                    .get(&(j_tid, a.join_col.as_str()))
                    .copied()
                    .expect("join column claimed in stage II");
                let a_pk = self
                    .column_index
                    .get(&(child_tid, a.target.col.as_str()))
                    .copied()
                    .expect("join target column claimed in stage II");
                let b_col = self
                    .column_index
                    .get(&(j_tid, b.join_col.as_str()))
                    .copied()
                    .expect("join column claimed in stage II");
                let b_pk = self
                    .column_index
                    .get(&(parent_tid, b.target.col.as_str()))
                    .copied()
                    .expect("join target column claimed in stage II");
                Ok(RelationColumns::Indirect {
                    join_table: j_tid,
                    a_col,
                    a_pk,
                    b_col,
                    b_pk,
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EndpointShape {
    owning: bool,
    is_list: bool,
}

fn expected_endpoint_shapes(arrow: RelationArrow, is_self: bool) -> (EndpointShape, EndpointShape) {
    match arrow {
        RelationArrow::OneToOne => (
            EndpointShape {
                owning: false,
                is_list: false,
            },
            if is_self {
                EndpointShape {
                    owning: false,
                    is_list: false,
                }
            } else {
                EndpointShape {
                    owning: true,
                    is_list: false,
                }
            },
        ),
        RelationArrow::ManyToOne => (
            EndpointShape {
                owning: false,
                is_list: false,
            },
            if is_self {
                EndpointShape {
                    owning: false,
                    is_list: true,
                }
            } else {
                EndpointShape {
                    owning: true,
                    is_list: true,
                }
            },
        ),
        RelationArrow::OneToMany => (
            EndpointShape {
                owning: false,
                is_list: true,
            },
            EndpointShape {
                owning: false,
                is_list: false,
            },
        ),
        RelationArrow::ManyToMany => (
            EndpointShape {
                owning: false,
                is_list: true,
            },
            EndpointShape {
                owning: false,
                is_list: true,
            },
        ),
    }
}

enum RelationColumns {
    Direct {
        fk: ColumnId,
        pk: ColumnId,
    },
    Indirect {
        join_table: TableId,
        a_col: ColumnId,
        a_pk: ColumnId,
        b_col: ColumnId,
        b_pk: ColumnId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_schema;
    use grove_types::Diagnostic;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    fn stage_relations(source: &str, f: impl FnOnce(&Builder<'_>)) {
        let (schema, parse_diags) = parse_schema(source);
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let schema = schema.expect("source should parse");
        let mut builder = Builder::new(&schema);
        builder.stage_identity();
        builder.stage_physical();
        builder.stage_structs();
        builder.stage_relations();
        f(&builder);
    }

    #[test]
    fn relations_one_to_one() {
        stage_relations(
            "root profiles: Profile; root users: User; \
             struct Profile { bio: String, user: &User } struct User { profile: Profile } \
             rel Profile.user <-> User.profile (profiles.user_id -> users.id);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.relations.len(), 1);
                let profile = *b.struct_index.get("Profile").unwrap();
                let user = *b.struct_index.get("User").unwrap();
                let Relation::OneToOne {
                    child_ref,
                    parent_ref,
                    fk,
                    pk,
                } = &b.relations[0]
                else {
                    panic!("expected OneToOne");
                };
                assert_eq!(*child_ref, FieldId::new(profile, 1));
                assert_eq!(*parent_ref, FieldId::new(user, 0));
                assert_eq!(
                    *fk,
                    b.column_index[&(b.struct_table[profile.index()], "user_id")]
                );
                assert_eq!(*pk, b.column_index[&(b.struct_table[user.index()], "id")]);
                assert!(matches!(
                    b.structs[profile.index()].fields[1],
                    Field::Ref {
                        name: ref n,
                        target,
                        is_list: false,
                        optional: false,
                        owning: false,
                    } if n == "user" && target == user
                ));
            },
        );
    }

    #[test]
    fn relations_many_to_one() {
        stage_relations(
            "root users: User; struct User { orders: List<Order> } \
             struct Order { total: Dec, user: &User } \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert!(b.diags.is_empty());
                let user = *b.struct_index.get("User").unwrap();
                let order = *b.struct_index.get("Order").unwrap();
                let Relation::ManyToOne {
                    child_ref,
                    parent_ref,
                    fk,
                    pk,
                } = &b.relations[0]
                else {
                    panic!("expected ManyToOne");
                };
                assert_eq!(*child_ref, FieldId::new(order, 1));
                assert_eq!(*parent_ref, FieldId::new(user, 0));
                assert_eq!(
                    *fk,
                    b.column_index[&(b.struct_table[order.index()], "user_id")]
                );
                assert_eq!(*pk, b.column_index[&(b.struct_table[user.index()], "id")]);
            },
        );
    }

    #[test]
    fn relations_self_reference() {
        stage_relations(
            "root users: User; struct User { manager: &?User, subordinates: &List<User> } \
             rel User.manager <<-> User.subordinates (users.manager_id -> users.id);",
            |b| {
                assert!(b.diags.is_empty());
                let user = *b.struct_index.get("User").unwrap();
                let Relation::ManyToOne {
                    child_ref,
                    parent_ref,
                    fk,
                    pk,
                } = &b.relations[0]
                else {
                    panic!("expected ManyToOne");
                };
                assert_eq!(*child_ref, FieldId::new(user, 0));
                assert_eq!(*parent_ref, FieldId::new(user, 1));
                assert_eq!(
                    *fk,
                    b.column_index[&(b.struct_table[user.index()], "manager_id")]
                );
                assert_eq!(*pk, b.column_index[&(b.struct_table[user.index()], "id")]);
                assert!(matches!(
                    b.structs[user.index()].fields[1],
                    Field::Ref {
                        name: ref n,
                        target,
                        is_list: true,
                        optional: false,
                        owning: false,
                    } if n == "subordinates" && target == user
                ));
            },
        );
    }

    #[test]
    fn relations_one_to_many() {
        stage_relations(
            "root images: Image; root messages: Message; \
             struct Image { uploaded: Instant, messages: &List<Message> } \
             struct Message { text: String, image: &?Image } \
             rel Image.messages <->> Message.image (messages.image_id -> images.id);",
            |b| {
                assert!(b.diags.is_empty());
                let image = *b.struct_index.get("Image").unwrap();
                let message = *b.struct_index.get("Message").unwrap();
                let Relation::OneToMany {
                    a_ref,
                    b_ref,
                    fk,
                    pk,
                } = &b.relations[0]
                else {
                    panic!("expected OneToMany");
                };
                assert_eq!(*a_ref, FieldId::new(image, 1));
                assert_eq!(*b_ref, FieldId::new(message, 1));
                assert_eq!(
                    *fk,
                    b.column_index[&(b.struct_table[message.index()], "image_id")]
                );
                assert_eq!(*pk, b.column_index[&(b.struct_table[image.index()], "id")]);
                assert!(matches!(
                    b.structs[image.index()].fields[1],
                    Field::Ref {
                        name: ref n,
                        target,
                        is_list: true,
                        owning: false,
                        ..
                    } if n == "messages" && target == message
                ));
                assert!(matches!(
                    b.structs[message.index()].fields[1],
                    Field::Ref {
                        name: ref n,
                        target,
                        is_list: false,
                        optional: true,
                        owning: false,
                    } if n == "image" && target == image
                ));
            },
        );
    }

    #[test]
    fn relations_many_to_many() {
        stage_relations(
            "root roles: Role; root users: User; struct Role { name: String, users: &List<User> } \
             struct User { name: String, roles: &List<Role> } \
             rel Role.users <<->> User.roles via user_roles[role_id -> roles.id, user_id -> users.id];",
            |b| {
                assert!(b.diags.is_empty());
                let role = *b.struct_index.get("Role").unwrap();
                let user = *b.struct_index.get("User").unwrap();
                let Relation::ManyToMany {
                    a_ref,
                    b_ref,
                    join_table,
                    a_col,
                    a_pk,
                    b_col,
                    b_pk,
                } = &b.relations[0]
                else {
                    panic!("expected ManyToMany");
                };
                assert_eq!(*a_ref, FieldId::new(role, 1));
                assert_eq!(*b_ref, FieldId::new(user, 1));
                assert_eq!(b.tables[join_table.index()].name, "user_roles");
                assert_eq!(*a_col, b.column_index[&(*join_table, "role_id")]);
                assert_eq!(*a_pk, b.column_index[&(b.struct_table[role.index()], "id")]);
                assert_eq!(*b_col, b.column_index[&(*join_table, "user_id")]);
                assert_eq!(*b_pk, b.column_index[&(b.struct_table[user.index()], "id")]);
                assert!(matches!(
                    b.structs[role.index()].fields[1],
                    Field::Ref {
                        name: ref n,
                        is_list: true,
                        owning: false,
                        ..
                    } if n == "users"
                ));
                assert!(matches!(
                    b.structs[user.index()].fields[1],
                    Field::Ref {
                        name: ref n,
                        is_list: true,
                        owning: false,
                        ..
                    } if n == "roles"
                ));
            },
        );
    }

    #[test]
    fn relation_endpoint_undeclared() {
        stage_relations(
            "root users: User; struct User {} struct Order {} \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0013", "SV0013"]);
                assert!(b.relations.is_empty());
            },
        );
    }

    #[test]
    fn endpoint_shape_mismatch() {
        stage_relations(
            "root users: User; struct User { orders: Order } struct Order { user: &User } \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0014"]);
                assert_eq!(b.relations.len(), 1);
            },
        );
    }

    #[test]
    fn endpoint_target_mismatch() {
        stage_relations(
            "root users: User; root roles: Role; \
             struct User { orders: List<Role> } struct Order { user: &Role } struct Role {} \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0016", "SV0016"]);
                assert_eq!(b.relations.len(), 1);
            },
        );
    }

    #[test]
    fn value_endpoint_reported() {
        stage_relations(
            "root profiles: Profile; root users: User; \
             struct Profile { user: String, bio: String } struct User { profile: Profile } \
             rel Profile.user <-> User.profile (profiles.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0014"]);
                assert_eq!(b.relations.len(), 1);
            },
        );
    }

    #[test]
    fn duplicate_endpoint_excluded() {
        stage_relations(
            "root users: User; root orders: Order; \
             struct User { a: List<Order>, b: List<Order> } struct Order { user: &User } \
             rel Order.user <<-> User.a (orders.user_id -> users.id); \
             rel Order.user <<-> User.b (orders.user_id2 -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0019"]);
                assert_eq!(b.relations.len(), 1);
            },
        );
    }

    #[test]
    fn relation_self_endpoint_excluded() {
        stage_relations(
            "root users: User; struct User { profile: &User } \
             rel User.profile <-> User.profile (users.a_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0019"]);
                assert!(b.relations.is_empty());
            },
        );
    }

    #[test]
    fn column_mismatch_via_on_owning() {
        stage_relations(
            "root users: User; struct User { orders: List<Order> } struct Order { user: &User } \
             rel Order.user <<-> User.orders via user_roles[order_id -> orders.id, user_id -> users.id];",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0017"]);
                assert!(b.relations.is_empty());
            },
        );
    }

    #[test]
    fn column_mismatch_n_m_direct() {
        stage_relations(
            "root users: User; root roles: Role; \
             struct User { roles: &List<Role> } struct Role { users: &List<User> } \
             rel Role.users <<->> User.roles (roles.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0018"]);
                assert!(b.relations.is_empty());
            },
        );
    }
}
