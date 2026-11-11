use std::collections::HashMap;

use grove_types::Span;

use crate::{ast::TypeExpr, validation_error::SchemaValidationError};

use super::Builder;

impl<'s> Builder<'s> {
    pub(super) fn stage_identity(&mut self) {
        self.declared_structs
            .extend(self.schema.structs.iter().map(|s| s.name.as_str()));

        let mut unknown_refs: HashMap<&'s str, Vec<Span>> = HashMap::new();
        let mut enum_as_struct: HashMap<&'s str, Vec<Span>> = HashMap::new();

        for root in &self.schema.roots {
            let name = root.struct_name.as_str();
            if !self.declared_structs.contains(name) {
                let spans = if self.enum_index.contains_key(name) {
                    &mut enum_as_struct
                } else {
                    &mut unknown_refs
                };
                spans.entry(name).or_default().push(root.struct_name.span);
            }
        }

        for (index, relation) in self.schema.relations.iter().enumerate() {
            for endpoint in [&relation.child, &relation.parent] {
                let name = endpoint.struct_name.as_str();
                if !self.declared_structs.contains(name) {
                    let spans = if self.enum_index.contains_key(name) {
                        &mut enum_as_struct
                    } else {
                        &mut unknown_refs
                    };
                    spans
                        .entry(name)
                        .or_default()
                        .push(endpoint.struct_name.span);
                } else {
                    self.relation_endpoint_fields
                        .entry((name, endpoint.field_name.as_str()))
                        .or_insert(index);
                }
            }
        }

        let mut targets = Vec::new();
        for def in &self.schema.structs {
            for field in &def.fields {
                collect_named_types(&field.exposed_type, &mut targets);
            }
        }
        for (name, span) in targets {
            if !self.declared_structs.contains(name) && !self.enum_index.contains_key(name) {
                unknown_refs.entry(name).or_default().push(span);
            }
        }

        for (name, mut spans) in unknown_refs {
            spans.sort_unstable();
            self.emit_error(SchemaValidationError::UnknownTypeRef {
                name: name.to_string(),
                occurrences: spans,
            });
        }

        for (name, mut spans) in enum_as_struct {
            spans.sort_unstable();
            self.emit_error(SchemaValidationError::EnumAsStruct {
                name: name.to_string(),
                occurrences: spans,
            });
        }
    }
}

fn collect_named_types<'a>(ty: &'a TypeExpr, out: &mut Vec<(&'a str, Span)>) {
    match ty {
        TypeExpr::Primitive(_) => {}
        TypeExpr::Named(ident) => out.push((ident.as_str(), ident.span)),
        TypeExpr::Optional { inner, .. } | TypeExpr::List { element: inner, .. } => {
            collect_named_types(inner, out)
        }
        TypeExpr::Tuple { elements, .. } => {
            for element in elements {
                collect_named_types(element, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_schema;
    use grove_types::Diagnostic;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    fn stage_identity(source: &str, f: impl FnOnce(&Builder<'_>)) {
        let (schema, parse_diags) = parse_schema(source);
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let schema = schema.expect("source should parse");
        let mut builder = Builder::new(&schema);
        builder.stage_enums();
        builder.stage_identity();
        f(&builder);
    }

    #[test]
    fn enum_field_type() {
        stage_identity(
            r#"enum Priority { Low => "low" } struct User { priority: Priority }"#,
            |b| {
                assert!(b.diags.is_empty());
            },
        );
    }

    #[test]
    fn enum_as_root_rejected() {
        stage_identity(
            r#"enum Priority { Low => "low" } root priorities: Priority;"#,
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0025"]);
                assert_eq!(b.diags[0].labels.len(), 1);
            },
        );
    }

    #[test]
    fn enum_as_relation_endpoint_rejected() {
        stage_identity(
            "enum P { A => 1 } struct S {} rel P.x <-> S.y (a.a -> b.b);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0025"]);
            },
        );
    }

    #[test]
    fn unknown_root_struct() {
        stage_identity("root users: Missing;", |b| {
            assert_eq!(codes(&b.diags), vec!["SV0001"]);
            assert_eq!(b.diags[0].labels.len(), 1);
        });
    }

    #[test]
    fn unknown_relation_endpoint() {
        stage_identity("rel Profile.user <-> User.profile (a.a -> b.b);", |b| {
            assert_eq!(codes(&b.diags), vec!["SV0001", "SV0001"]);
            assert!(b.relation_endpoint_fields.is_empty());
        });
    }

    #[test]
    fn one_to_many_claims_both_endpoints() {
        stage_identity(
            "struct A {} struct B {} rel A.x <->> B.y (a.a -> b.b);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.relation_endpoint_fields.len(), 2);
                assert_eq!(b.relation_endpoint_fields.get(&("A", "x")), Some(&0));
                assert_eq!(b.relation_endpoint_fields.get(&("B", "y")), Some(&0));
            },
        );
    }

    #[test]
    fn unknown_type_in_field_type() {
        stage_identity("struct User { manager: Missing }", |b| {
            assert_eq!(codes(&b.diags), vec!["SV0001"]);
            assert_eq!(b.diags[0].labels.len(), 1);
        });
    }

    #[test]
    fn unknown_type_labels_all_occurrences() {
        stage_identity(
            "root users: Missing; \
             struct User { manager: Missing, orders: List<Missing> } \
             rel Missing.x <<-> User.y (a.a -> b.b);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0001"]);
                assert_eq!(b.diags[0].labels.len(), 4);
            },
        );
    }

    #[test]
    fn unknown_structs_separate_diagnostics() {
        stage_identity("root a: Missing; root b: Missing; root c: Other;", |b| {
            assert_eq!(codes(&b.diags), vec!["SV0001", "SV0001"]);
            assert_eq!(b.diags.iter().map(|d| d.labels.len()).sum::<usize>(), 3);
        });
    }

    #[test]
    fn relation_claims_both_endpoints() {
        stage_identity(
            "struct User { orders: List<Order> } struct Order {} \
             rel Order.user <<-> User.orders (a.a -> b.b);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.relation_endpoint_fields.len(), 2);
                assert_eq!(
                    b.relation_endpoint_fields.get(&("User", "orders")),
                    Some(&0)
                );
                assert_eq!(b.relation_endpoint_fields.get(&("Order", "user")), Some(&0));
            },
        );
    }

    #[test]
    fn self_ref_claims_both_endpoints() {
        stage_identity(
            "struct User { manager: ?User } \
             rel User.manager <<-> User.subordinates (a.a -> b.b);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.relation_endpoint_fields.len(), 2);
                assert_eq!(
                    b.relation_endpoint_fields.get(&("User", "manager")),
                    Some(&0)
                );
                assert_eq!(
                    b.relation_endpoint_fields.get(&("User", "subordinates")),
                    Some(&0)
                );
            },
        );
    }

    #[test]
    fn many_to_many_claims_both_endpoints() {
        stage_identity(
            "struct User {} struct Role {} \
             rel Role.users <<->> User.roles via jt[role_id -> roles.id, user_id -> users.id];",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.relation_endpoint_fields.len(), 2);
                assert_eq!(b.relation_endpoint_fields.get(&("Role", "users")), Some(&0));
                assert_eq!(b.relation_endpoint_fields.get(&("User", "roles")), Some(&0));
            },
        );
    }
}
