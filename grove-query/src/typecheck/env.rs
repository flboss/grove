use std::collections::HashMap;

use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema};
use grove_types::{NodeId, Span};

use crate::typecheck::types::{
    IdentBinding, QueryType, RecordSource, TargetProvenance, TypedExpr, TypedExprKind,
};

pub(super) struct ScopeEntry {
    pub(super) ty: QueryType,
    pub(super) binding: IdentBinding,
    pub(super) provenance: TargetProvenance,
}

pub(crate) struct TypeEnv<'s> {
    pub(super) scopes: Vec<HashMap<String, ScopeEntry>>,
    pub(super) schema: &'s ValidatedSchema,
    pub(super) element_depth: usize,
    pub(super) next_id: u64,
    pub(super) allow_row_refs: bool,
}

impl<'s> TypeEnv<'s> {
    pub(super) fn new(schema: &'s ValidatedSchema) -> Self {
        let mut env = TypeEnv {
            scopes: vec![HashMap::new()],
            schema,
            element_depth: 0,
            next_id: 0,
            allow_row_refs: false,
        };

        for (root_idx, root) in schema.roots.iter().enumerate() {
            let record_ty = QueryType::Record(RecordSource::Schema(root.struct_id));
            let list_ty = QueryType::List(Box::new(record_ty));
            env.define_with_provenance(
                root.name.clone(),
                list_ty,
                IdentBinding::Root { root_idx },
                TargetProvenance::root(root_idx),
            );
        }

        env
    }

    pub(super) fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    pub(super) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    pub(super) fn push_element(&mut self, ty: QueryType) {
        self.element_depth += 1;
        let name = "$".repeat(self.element_depth);
        self.define(name, ty, IdentBinding::ListElement);
    }

    pub(super) fn define(&mut self, name: String, ty: QueryType, binding: IdentBinding) {
        self.define_with_provenance(name, ty, binding, TargetProvenance::NONE);
    }

    pub(super) fn define_with_provenance(
        &mut self,
        name: String,
        ty: QueryType,
        binding: IdentBinding,
        provenance: TargetProvenance,
    ) {
        self.scopes.last_mut().unwrap().insert(
            name,
            ScopeEntry {
                ty,
                binding,
                provenance,
            },
        );
    }

    pub(super) fn resolve(&self, name: &str) -> Option<(&ScopeEntry, usize)> {
        for (skipped, scope) in self.scopes.iter().rev().enumerate() {
            if let Some(entry) = scope.get(name) {
                return Some((entry, skipped));
            }
        }
        None
    }

    pub(super) fn schema(&self) -> &'s ValidatedSchema {
        self.schema
    }

    pub(super) fn typed(&mut self, kind: TypedExprKind, ty: QueryType, span: Span) -> TypedExpr {
        self.typed_with_provenance(kind, ty, TargetProvenance::NONE, span)
    }

    pub(super) fn typed_with_provenance(
        &mut self,
        kind: TypedExprKind,
        ty: QueryType,
        provenance: TargetProvenance,
        span: Span,
    ) -> TypedExpr {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        TypedExpr {
            kind,
            ty,
            provenance,
            span,
            id,
        }
    }

    pub(super) fn push_record_fields(&mut self, ty: &QueryType) -> bool {
        match ty {
            QueryType::Record(RecordSource::Schema(struct_id)) => {
                self.push_scope();
                self.push_schema_struct_fields(*struct_id);
                true
            }
            QueryType::Record(RecordSource::Projection(fields)) => {
                self.push_scope();
                for field in fields {
                    self.define(
                        field.name.clone(),
                        field.ty.clone(),
                        IdentBinding::ProjectedField {
                            name: field.name.clone(),
                        },
                    );
                }
                true
            }
            QueryType::List(inner) => self.push_record_fields(inner),
            _ => false,
        }
    }

    pub(super) fn push_schema_struct_fields(&mut self, struct_id: StructId) {
        let struct_ = &self.schema.structs[struct_id.index()];
        for (local_idx, field) in struct_.fields.iter().enumerate() {
            let field_id = FieldId::new(struct_id, local_idx);
            let binding = IdentBinding::Field(field_id);
            let provenance = match field {
                Field::Ref { .. } => TargetProvenance::ref_collection(field_id),
                _ => TargetProvenance::NONE,
            };
            let name = field.name().to_owned();
            let ty = QueryType::from(field);
            self.define_with_provenance(name, ty, binding, provenance);
        }
    }
}

pub(super) fn record_field_by_name(
    record: &QueryType,
    name: &str,
    schema: &ValidatedSchema,
) -> Option<QueryType> {
    match record {
        QueryType::Record(RecordSource::Schema(struct_id)) => schema
            .struct_field(*struct_id, name)
            .map(|(_, field)| QueryType::from(field)),
        QueryType::Record(RecordSource::Projection(fields)) => {
            fields.iter().find(|f| f.name == name).map(|f| f.ty.clone())
        }
        _ => None,
    }
}

pub(super) fn record_field_provenance(
    record: &QueryType,
    name: &str,
    schema: &ValidatedSchema,
) -> TargetProvenance {
    let QueryType::Record(RecordSource::Schema(struct_id)) = record else {
        return TargetProvenance::NONE;
    };
    let Some((field_id, Field::Ref { .. })) = schema.struct_field(*struct_id, name) else {
        return TargetProvenance::NONE;
    };
    TargetProvenance::ref_collection(field_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typecheck::infer::infer;
    use crate::typecheck::tests::{field_binding, infer_ok, test_schema};
    use crate::typecheck::{TypeBindings, typecheck_with_bindings};
    use grove_schema::validated::ScalarType;

    #[test]
    fn root_lookup() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let (entry, depth) = env.resolve("users").unwrap();
        assert!(matches!(entry.ty, QueryType::List(_)));
        assert_eq!(depth, 0);
    }

    #[test]
    fn resolve_depth() {
        let schema = test_schema();
        let user = schema.roots[0].struct_id;
        let mut env = TypeEnv::new(&schema);
        assert_eq!(env.resolve("users").unwrap().1, 0);
        env.push_scope();
        env.define(
            "name".to_string(),
            QueryType::Scalar(ScalarType::String),
            field_binding(&schema, user, "name"),
        );
        assert_eq!(env.resolve("name").unwrap().1, 0);
        assert_eq!(env.resolve("users").unwrap().1, 1);
        assert!(env.resolve("missing").is_none());
    }

    #[test]
    fn ident_binding_root() {
        let result = infer_ok("users", &test_schema());
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &result.kind
        else {
            panic!("expected ident, got {:?}", result.kind);
        };
        assert_eq!(name.value, "users");
        assert_eq!(*binding, IdentBinding::Root { root_idx: 0 });
        assert_eq!(*depth, 0);
    }

    #[test]
    fn ident_binding_scoped_field() {
        let schema = test_schema();
        let user = schema.roots[0].struct_id;
        let result = infer_ok("users[active]", &schema);
        let TypedExprKind::Method { args, .. } = &result.kind else {
            panic!("expected method, got {:?}", result.kind);
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &args[0].expr.kind
        else {
            panic!("expected ident, got {:?}", args[0].expr.kind);
        };
        assert_eq!(name.value, "active");
        assert_eq!(*binding, field_binding(&schema, user, "active"));
        assert_eq!(*depth, 0);
    }

    #[test]
    fn ident_binding_scoped_ref_field() {
        let schema = test_schema();
        let user = schema.roots[0].struct_id;
        let result = infer_ok("users[profile.is_some()]", &schema);
        let TypedExprKind::Method { args, .. } = &result.kind else {
            panic!("expected method, got {:?}", result.kind);
        };
        let TypedExprKind::Method { base, .. } = &args[0].expr.kind else {
            panic!("expected method, got {:?}", args[0].expr.kind);
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &base.kind
        else {
            panic!("expected ident, got {:?}", base.kind);
        };
        assert_eq!(name.value, "profile");
        assert_eq!(*binding, field_binding(&schema, user, "profile"));
        assert_eq!(*depth, 0);
    }

    #[test]
    fn ident_binding_projected_field() {
        let result = infer_ok("users { name } { name }", &test_schema());
        let TypedExprKind::Projection { items, .. } = &result.kind else {
            panic!("expected projection, got {:?}", result.kind);
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &items[0].value.kind
        else {
            panic!("expected ident, got {:?}", items[0].value.kind);
        };
        assert_eq!(name.value, "name");
        assert_eq!(
            *binding,
            IdentBinding::ProjectedField {
                name: "name".to_string()
            }
        );
        assert_eq!(*depth, 0);
    }

    #[test]
    fn ident_binding_projected_scoped_field() {
        let result = infer_ok(r#"users { name }[name == "Alice"]"#, &test_schema());
        let TypedExprKind::Method { args, .. } = &result.kind else {
            panic!("expected method, got {:?}", result.kind);
        };
        let TypedExprKind::Binary { lhs, .. } = &args[0].expr.kind else {
            panic!("expected binary, got {:?}", args[0].expr.kind);
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &lhs.kind
        else {
            panic!("expected ident, got {:?}", lhs.kind);
        };
        assert_eq!(name.value, "name");
        assert_eq!(
            *binding,
            IdentBinding::ProjectedField {
                name: "name".to_string()
            }
        );
        assert_eq!(*depth, 0);
    }

    #[test]
    fn type_bindings_manage_entries() {
        let mut bindings = TypeBindings::new();
        assert!(bindings.is_empty());
        bindings.define("n", QueryType::Scalar(ScalarType::Int));
        bindings.define("s", QueryType::Scalar(ScalarType::String));
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings.get("n"), Some(&QueryType::Scalar(ScalarType::Int)));
        assert_eq!(bindings.iter().count(), 2);
        assert_eq!(
            bindings.remove("n"),
            Some(QueryType::Scalar(ScalarType::Int))
        );
        assert!(bindings.get("n").is_none());
        bindings.clear();
        assert!(bindings.is_empty());
    }

    #[test]
    fn session_binding_depth_one() {
        let mut bindings = TypeBindings::new();
        bindings.define("min", QueryType::Scalar(ScalarType::Int));

        let (file, _) = crate::parse_query("min");
        let (typed, diags) = typecheck_with_bindings(file.unwrap(), &test_schema(), &bindings);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let output = typed.result.as_ref().unwrap();
        assert_eq!(output.ty, QueryType::Scalar(ScalarType::Int));
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &output.kind
        else {
            panic!("expected ident output");
        };
        assert_eq!(name.value, "min");
        assert_eq!(*binding, IdentBinding::Let);
        assert_eq!(*depth, 1);
    }

    #[test]
    fn session_binding_shadow_root() {
        let mut bindings = TypeBindings::new();
        bindings.define("users", QueryType::Scalar(ScalarType::Int));

        let (file, _) = crate::parse_query("users");
        let (typed, diags) = typecheck_with_bindings(file.unwrap(), &test_schema(), &bindings);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let output = typed.result.as_ref().unwrap();
        assert_eq!(output.ty, QueryType::Scalar(ScalarType::Int));
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &output.kind
        else {
            panic!("expected ident output");
        };
        assert_eq!(name.value, "users");
        assert_eq!(*binding, IdentBinding::Let);
        assert_eq!(*depth, 1);
    }

    #[test]
    fn turn_let_shadow_session_binding() {
        let mut bindings = TypeBindings::new();
        bindings.define("min", QueryType::Scalar(ScalarType::Int));

        let (file, _) = crate::parse_query("let min = 5; users[age > min]");
        let (typed, diags) = typecheck_with_bindings(file.unwrap(), &test_schema(), &bindings);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
        let typed = typed.unwrap();
        let output = typed.result.as_ref().unwrap();
        let TypedExprKind::Method { args, .. } = &output.kind else {
            panic!("expected filter method");
        };
        let TypedExprKind::Binary { rhs, .. } = &args[0].expr.kind else {
            panic!("expected binary argument");
        };
        let TypedExprKind::Ident {
            name,
            binding,
            depth,
        } = &rhs.kind
        else {
            panic!("expected ident");
        };
        assert_eq!(name.value, "min");
        assert_eq!(*binding, IdentBinding::Let);
        assert_eq!(*depth, 1);
    }

    #[test]
    fn root_unknown_identifier() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("posts");
        let result = infer(file.unwrap().output().unwrap(), &mut env);
        assert!(result.is_err());
    }
}
