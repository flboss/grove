use std::collections::{HashMap, HashSet};

use crate::{
    validated::{Relation, StructId},
    validation_error::SchemaValidationError,
};

use super::Builder;

impl<'s> Builder<'s> {
    pub(super) fn stage_ownership(&mut self) {
        let mut owners: HashMap<StructId, Vec<StructId>> = HashMap::new();
        for relation in &self.relations {
            let (child, parent) = match relation {
                Relation::OneToOne {
                    child_ref,
                    parent_ref,
                    ..
                }
                | Relation::ManyToOne {
                    child_ref,
                    parent_ref,
                    ..
                } => (child_ref.struct_id(), parent_ref.struct_id()),
                _ => continue,
            };
            if child != parent {
                owners.entry(child).or_default().push(parent);
            }
        }

        for root in &self.schema.roots {
            let Some(&sid) = self.struct_index.get(root.struct_name.as_str()) else {
                continue;
            };
            if owners.contains_key(&sid) {
                self.emit_error(SchemaValidationError::OwnedAsRoot {
                    span: root.struct_name.span,
                    root: root.name.value.clone(),
                    struct_name: root.struct_name.value.clone(),
                });
            }
        }

        let declared_roots: HashSet<StructId> =
            self.roots.iter().map(|root| root.struct_id).collect();
        let mut terminated: HashMap<StructId, bool> = HashMap::new();
        let mut starts: Vec<StructId> = owners.keys().copied().collect();
        starts.sort_by_key(|sid| sid.index());
        for start in starts {
            if !reaches_root(
                start,
                &owners,
                &declared_roots,
                &mut terminated,
                &mut Vec::new(),
            ) {
                let struct_name = self.structs[start.index()].name.clone();
                let span = self
                    .schema
                    .structs
                    .iter()
                    .find(|def| def.name.value == struct_name)
                    .map(|def| def.name.span)
                    .unwrap_or_default();
                self.emit_error(SchemaValidationError::UnrootedOwnership { span, struct_name });
            }
        }

        self.roots
            .retain(|root| !owners.contains_key(&root.struct_id));
    }
}

fn reaches_root(
    sid: StructId,
    owners: &HashMap<StructId, Vec<StructId>>,
    roots: &HashSet<StructId>,
    terminated: &mut HashMap<StructId, bool>,
    visiting: &mut Vec<StructId>,
) -> bool {
    if roots.contains(&sid) {
        return true;
    }
    if let Some(&done) = terminated.get(&sid) {
        return done;
    }
    if visiting.contains(&sid) {
        return false;
    }
    let Some(parents) = owners.get(&sid) else {
        return false;
    };
    visiting.push(sid);
    let done = parents
        .iter()
        .all(|parent| reaches_root(*parent, owners, roots, terminated, visiting));
    visiting.pop();
    terminated.insert(sid, done);
    done
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

    #[test]
    fn linear_ownership_chain_rooted() {
        stage_ownership(
            "root users: User; \
             struct User { orders: List<Order> } \
             struct Order { user: &User, items: List<Item> } \
             struct Item { order: &Order } \
             rel Order.user <<-> User.orders (orders.user_id -> users.id); \
             rel Item.order <<-> Order.items (items.order_id -> orders.id);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.roots.len(), 1);
            },
        );
    }

    #[test]
    fn circular_ownership_unrooted_error() {
        stage_ownership(
            "struct A { as_child: &B, as_parent: List<B> } \
             struct B { as_child: &A, as_parent: List<A> } \
             rel A.as_child <<-> B.as_parent (a.fk_to_b -> b.id); \
             rel B.as_child <<-> A.as_parent (b.fk_to_a -> a.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0022", "SV0022"]);
                assert!(b.roots.is_empty());
            },
        );
    }

    #[test]
    fn ownership_chain_unrooted_error() {
        stage_ownership(
            "struct Middle { items: List<Leaf>, top: &Top } \
             struct Leaf { middle: &Middle } \
             struct Top { middles: List<Middle> } \
             rel Leaf.middle <<-> Middle.items (leaf.middle_id -> middle.id); \
             rel Middle.top <<-> Top.middles (middle.top_id -> top.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0022", "SV0022"]);
            },
        );
    }
}
