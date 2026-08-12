use std::collections::{HashMap, HashSet};

use crate::ast::{RelationArrow, Schema, TypeExpr};
use crate::validated::ValidatedSchema;
use crate::validation_error::SchemaValidationError;
use grove_types::{Diagnostic, Span};

pub struct Builder<'s> {
    schema: &'s Schema,
    declared_structs: HashSet<&'s str>,
    forward_ref_relations: HashMap<(&'s str, &'s str), usize>,
    excluded_relations: HashSet<usize>,
    diags: Vec<Diagnostic>,
}

pub fn validate(schema: Schema) -> (Option<ValidatedSchema>, Vec<Diagnostic>) {
    let mut builder = Builder::new(&schema);
    builder.stage_identity();

    // TODO: `ValidatedSchema` assembly
    (None, builder.diags)
}

impl<'s> Builder<'s> {
    fn new(schema: &'s Schema) -> Builder<'s> {
        Builder {
            schema,
            declared_structs: HashSet::new(),
            forward_ref_relations: HashMap::new(),
            excluded_relations: HashSet::new(),
            diags: Vec::new(),
        }
    }

    fn emit_error(&mut self, error: SchemaValidationError) {
        self.diags.push(error.into());
    }

    fn stage_identity(&mut self) {
        self.declared_structs
            .extend(self.schema.structs.iter().map(|s| s.name.as_str()));

        let mut unknown_refs: HashMap<&'s str, Vec<Span>> = HashMap::new();

        for root in &self.schema.roots {
            let name = root.struct_name.as_str();
            if !self.declared_structs.contains(name) {
                unknown_refs
                    .entry(name)
                    .or_default()
                    .push(root.struct_name.span);
            }
        }

        for (index, relation) in self.schema.relations.iter().enumerate() {
            for endpoint in [&relation.child, &relation.parent] {
                let name = endpoint.struct_name.as_str();
                if !self.declared_structs.contains(name) {
                    unknown_refs
                        .entry(name)
                        .or_default()
                        .push(endpoint.struct_name.span);
                }
            }

            match relation.arrow.value {
                RelationArrow::OneToMany => {
                    self.excluded_relations.insert(index);
                    self.emit_error(SchemaValidationError::InvalidArrow {
                        span: relation.arrow.span,
                    });
                }
                RelationArrow::ManyToMany => {}
                RelationArrow::OneToOne | RelationArrow::ManyToOne => {
                    let child = relation.child.struct_name.value.as_str();
                    let parent = relation.parent.struct_name.value.as_str();
                    if self.declared_structs.contains(child)
                        && self.declared_structs.contains(parent)
                    {
                        let (owner, field) = if child == parent {
                            (
                                relation.child.struct_name.value.as_str(),
                                relation.child.field_name.value.as_str(),
                            )
                        } else {
                            (
                                relation.parent.struct_name.value.as_str(),
                                relation.parent.field_name.value.as_str(),
                            )
                        };
                        self.forward_ref_relations.insert((owner, field), index);
                    }
                }
            }
        }

        let mut targets = Vec::new();
        for def in &self.schema.structs {
            for field in &def.fields {
                collect_struct_types(&field.exposed_type, &mut targets);
            }
        }
        for (name, span) in targets {
            if !self.declared_structs.contains(name) {
                unknown_refs.entry(name).or_default().push(span);
            }
        }

        for (name, spans) in unknown_refs {
            self.emit_error(SchemaValidationError::UnknownStructRef {
                name: name.to_string(),
                occurrences: spans,
            });
        }
    }
}

fn collect_struct_types<'a>(ty: &'a TypeExpr, out: &mut Vec<(&'a str, Span)>) {
    match ty {
        TypeExpr::Primitive(_) => {}
        TypeExpr::Struct(ident) => out.push((ident.as_str(), ident.span)),
        TypeExpr::Optional { inner, .. } | TypeExpr::List { element: inner, .. } => {
            collect_struct_types(inner, out)
        }
        TypeExpr::Tuple { elements, .. } => {
            for element in elements {
                collect_struct_types(element, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_schema;

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
        builder.stage_identity();
        f(&builder);
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
            assert!(b.forward_ref_relations.is_empty());
        });
    }

    #[test]
    fn invalid_arrow_is_excluded() {
        stage_identity(
            "struct A {} struct B {} rel A.x <->> B.y (a.a -> b.b);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0002"]);
                assert!(b.excluded_relations.contains(&0));
                assert!(!b.forward_ref_relations.contains_key(&("A", "x")));
            },
        );
    }

    #[test]
    fn unknown_struct_in_field_type() {
        stage_identity("struct User { manager: Missing }", |b| {
            assert_eq!(codes(&b.diags), vec!["SV0001"]);
            assert_eq!(b.diags[0].labels.len(), 1);
        });
    }

    #[test]
    fn unknown_struct_labels_all_occurrences() {
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
    fn forward_endpoint_claims_parent_field() {
        stage_identity(
            "struct User { orders: List<Order> } struct Order {} \
             rel Order.user <<-> User.orders (a.a -> b.b);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.forward_ref_relations.len(), 1);
                assert_eq!(b.forward_ref_relations.get(&("User", "orders")), Some(&0));
            },
        );
    }

    #[test]
    fn forward_endpoint_self_ref_claims_child_field() {
        stage_identity(
            "struct User { manager: ?User } \
             rel User.manager <<-> User.subordinates (a.a -> b.b);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.forward_ref_relations.len(), 1);
                assert_eq!(b.forward_ref_relations.get(&("User", "manager")), Some(&0));
            },
        );
    }

    #[test]
    fn many_to_many_claims_no_forward() {
        stage_identity(
            "struct User {} struct Role {} \
             rel Role.users <<->> User.roles via jt[role_id -> roles.id, user_id -> users.id];",
            |b| {
                assert!(b.diags.is_empty());
                assert!(b.forward_ref_relations.is_empty());
                assert!(b.excluded_relations.is_empty());
            },
        );
    }
}
