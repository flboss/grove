use std::collections::HashSet;

use crate::{
    validated::{Relation, StructId},
    validation_error::SchemaValidationError,
};

use super::Builder;

impl<'s> Builder<'s> {
    pub(super) fn stage_ownership(&mut self) {
        let owned: HashSet<StructId> = self
            .relations
            .iter()
            .filter_map(|r| match r {
                Relation::OneToOne {
                    child_ref,
                    parent_ref,
                    ..
                }
                | Relation::ManyToOne {
                    child_ref,
                    parent_ref,
                    ..
                } if child_ref.struct_id() != parent_ref.struct_id() => Some(child_ref.struct_id()),
                _ => None,
            })
            .collect();

        for root in &self.schema.roots {
            let Some(&sid) = self.struct_index.get(root.struct_name.as_str()) else {
                continue;
            };
            if owned.contains(&sid) {
                self.emit_error(SchemaValidationError::OwnedAsRoot {
                    span: root.struct_name.span,
                    root: root.name.value.clone(),
                    struct_name: root.struct_name.value.clone(),
                });
            }
        }

        self.roots.retain(|root| !owned.contains(&root.struct_id));
    }
}

#[cfg(test)]
mod tests {
    use grove_types::Diagnostic;

    use super::*;
    use crate::parse_schema;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    fn stage_ownership(source: &str, f: impl FnOnce(&Builder<'_>)) {
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
        builder.stage_ownership();
        f(&builder);
    }

    #[test]
    fn one_to_one_child_root_error() {
        stage_ownership(
            "root profiles: Profile; root users: User; \
             struct Profile { user: &User, bio: String } struct User { profile: Profile } \
             rel Profile.user <-> User.profile (profiles.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0020"]);
                assert_eq!(b.roots.len(), 1);
                assert_eq!(b.roots[0].name, "users");
            },
        );
    }

    #[test]
    fn many_to_one_child_root_error() {
        stage_ownership(
            "root users: User; root orders: Order; \
             struct User { orders: List<Order> } struct Order { user: &User } \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0020"]);
                assert_eq!(b.roots.len(), 1);
                assert_eq!(b.roots[0].name, "users");
            },
        );
    }

    #[test]
    fn non_owning_sides_may_be_roots() {
        stage_ownership(
            "root users: User; root messages: Message; \
             struct User { messages: &List<Message> } struct Message { user: &?User } \
             rel User.messages <->> Message.user (messages.user_id -> users.id);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.roots.len(), 2);
            },
        );
    }

    #[test]
    fn self_ref_root_not_owned() {
        stage_ownership(
            "root users: User; struct User { manager: &?User, subordinates: &List<User> } \
             rel User.manager <<-> User.subordinates (users.manager_id -> users.id);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.roots.len(), 1);
            },
        );
    }
}
