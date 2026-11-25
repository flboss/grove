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
            let (name, ty) = match field {
                Field::Value { name, ty, .. } => (name.clone(), ty.into()),
                Field::Array { name, element, .. } => {
                    (name.clone(), QueryType::List(Box::new(element.into())))
                }
                Field::Ref {
                    name,
                    target,
                    optional,
                    is_list,
                    ..
                } => {
                    let inner = QueryType::Record(RecordSource::Schema(*target));
                    let ty = if *is_list {
                        QueryType::List(Box::new(inner))
                    } else if *optional {
                        inner.wrap_optional()
                    } else {
                        inner
                    };
                    (name.clone(), ty)
                }
            };
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
