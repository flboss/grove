use grove_types::Span;

use super::Builder;
use crate::{
    ast::{ColumnMapping, Field as FieldDecl, ListStorage, TypeExpr},
    validated::{Field, FieldId, StorageTable, Struct, TableId, ValueType},
    validation::FieldKind,
    validation_error::SchemaValidationError,
};

impl<'s> Builder<'s> {
    pub(super) fn stage_structs(&mut self) {
        for def in &self.schema.structs {
            let Some(&sid) = self.struct_index.get(def.name.as_str()) else {
                continue;
            };
            let tid = self.struct_table[sid.index()];
            let mut fields: Vec<Field> = Vec::new();
            for field in &def.fields {
                if let Some(model) = self.classify_field(def, field, tid) {
                    self.field_index.insert(
                        (def.name.as_str(), field.name.as_str()),
                        FieldId::new(sid, fields.len()),
                    );
                    fields.push(model);
                }
            }
            self.structs.push(Struct {
                name: def.name.value.clone(),
                table: tid,
                fields,
            });
        }
    }

    fn classify_field(
        &mut self,
        def: &'s crate::ast::StructDef,
        field: &'s FieldDecl,
        tid: TableId,
    ) -> Option<Field> {
        let name = field.name.value.clone();
        let struct_name = def.name.value.clone();

        if let Some(nested) = nested_struct_ref(&field.exposed_type) {
            self.emit_error(SchemaValidationError::NestedStructRef {
                span: field.exposed_type.span(),
                struct_name,
                field: name,
                nested,
            });
            return None;
        }

        match super::field_kind(&field.exposed_type) {
            FieldKind::Value | FieldKind::Array { .. } | FieldKind::ListNoVia
                if field.non_owning =>
            {
                self.emit_error(SchemaValidationError::NonOwningNonRef {
                    span: field.exposed_type.span(),
                    struct_name,
                    field: name,
                });
                None
            }
            FieldKind::Value => {
                let declared = super::value_column_refs(field).len();
                let expected = super::value_scalar_leaves(&field.exposed_type).len();
                if declared != expected {
                    let columns = field
                        .column
                        .as_ref()
                        .map_or(field.name.span, ColumnMapping::span);
                    self.emit_error(SchemaValidationError::TupleArityMismatch {
                        span: field.exposed_type.span(),
                        columns,
                        struct_name,
                        field: name,
                        declared,
                        expected,
                    });
                    return None;
                }
                if let Some(span) = value_list_without_via(&field.exposed_type) {
                    self.emit_error(SchemaValidationError::NoViaScalarList {
                        span,
                        struct_name,
                        field: name,
                    });
                    return None;
                }
                let columns = super::value_column_refs(field)
                    .iter()
                    .map(|(col, _)| self.column_index[&(tid, *col)])
                    .collect();
                let ty = self.value_type(&field.exposed_type);
                Some(Field::Value { name, ty, columns })
            }
            FieldKind::Array { element, via } => {
                let declared = match &via.value {
                    ColumnMapping::Single(_) => 1,
                    ColumnMapping::Multi(cols) => cols.len(),
                };
                let expected = super::storage_value_types(element).len();
                if declared != expected {
                    self.emit_error(SchemaValidationError::ViaArityMismatch {
                        span: field.exposed_type.span(),
                        columns: via.value.span(),
                        struct_name,
                        field: name,
                        declared,
                        expected,
                    });
                    return None;
                }
                if let Some(span) = value_list_without_via(element) {
                    self.emit_error(SchemaValidationError::NoViaScalarList {
                        span,
                        struct_name,
                        field: name,
                    });
                    return None;
                }
                let (link_name, _) = super::array_link_column(field);
                let link = self.column_index[&(tid, link_name)];
                let storage = self.storage_table(via);
                let element = self.value_type(element);
                Some(Field::Array {
                    name,
                    element,
                    link,
                    storage,
                })
            }
            FieldKind::ListNoVia => {
                self.emit_error(SchemaValidationError::NoViaScalarList {
                    span: field.exposed_type.span(),
                    struct_name,
                    field: name,
                });
                None
            }
            FieldKind::Ref => {
                if matches!(field.exposed_type, TypeExpr::List { via: Some(_), .. }) {
                    self.emit_error(SchemaValidationError::ViaOnStructList {
                        span: field.exposed_type.span(),
                        struct_name,
                        field: name,
                    });
                    return None;
                }
                if !self
                    .relation_endpoint_fields
                    .contains_key(&(def.name.as_str(), field.name.as_str()))
                {
                    self.emit_error(SchemaValidationError::UnmatchedForwardRef {
                        span: field.exposed_type.span(),
                        struct_name,
                        field: name,
                    });
                    return None;
                }
                let target = self
                    .struct_index
                    .get(ref_struct_name(&field.exposed_type))
                    .copied();
                let Some(target) = target else {
                    self.emit_error(SchemaValidationError::UnmatchedForwardRef {
                        span: field.exposed_type.span(),
                        struct_name,
                        field: name,
                    });
                    return None;
                };
                let optional = matches!(field.exposed_type, TypeExpr::Optional { .. });
                let is_list = matches!(field.exposed_type, TypeExpr::List { .. });
                Some(Field::Ref {
                    name,
                    target,
                    optional,
                    is_list,
                    owning: !field.non_owning,
                })
            }
        }
    }

    fn value_type(&self, ty: &TypeExpr) -> ValueType {
        match ty {
            TypeExpr::Primitive(spanned) => ValueType::Scalar(super::builtin_scalar(spanned)),
            TypeExpr::Optional { inner, .. } => {
                ValueType::Optional(Box::new(self.value_type(inner)))
            }
            TypeExpr::Tuple { elements, .. } => {
                ValueType::Tuple(elements.iter().map(|e| self.value_type(e)).collect())
            }
            TypeExpr::List {
                element,
                via: Some(via),
                ..
            } => ValueType::Array {
                element: Box::new(self.value_type(element)),
                storage: self.storage_table(via),
            },
            TypeExpr::Struct(_) | TypeExpr::List { via: None, .. } => {
                unreachable!("struct or via-less list reached ValueType conversion")
            }
        }
    }

    fn storage_table(&self, via: &'s ListStorage) -> StorageTable {
        let table = self.table_index[via.table.as_str()];
        let key = self.column_index[&(table, via.key_col.as_str())];
        let value = match &via.value {
            ColumnMapping::Single(col) => vec![self.column_index[&(table, col.as_str())]],
            ColumnMapping::Multi(cols) => cols
                .iter()
                .map(|c| self.column_index[&(table, c.as_str())])
                .collect(),
        };
        StorageTable { table, key, value }
    }
}

fn nested_struct_ref(ty: &TypeExpr) -> Option<Span> {
    match ty {
        TypeExpr::Primitive(_) | TypeExpr::Struct(_) => None,
        TypeExpr::Optional { inner, .. } => match inner.as_ref() {
            TypeExpr::Struct(_) => None,
            other => struct_in_value_position(other),
        },
        TypeExpr::List { element, .. } => match element.as_ref() {
            TypeExpr::Struct(_) => None,
            other => struct_in_value_position(other),
        },
        TypeExpr::Tuple { elements, .. } => elements.iter().find_map(struct_in_value_position),
    }
}

fn struct_in_value_position(ty: &TypeExpr) -> Option<Span> {
    match ty {
        TypeExpr::Primitive(_) => None,
        TypeExpr::Struct(spanned) => Some(spanned.span),
        TypeExpr::Optional { inner, .. } => struct_in_value_position(inner),
        TypeExpr::List { element, .. } => struct_in_value_position(element),
        TypeExpr::Tuple { elements, .. } => elements.iter().find_map(struct_in_value_position),
    }
}

fn value_list_without_via(ty: &TypeExpr) -> Option<Span> {
    match ty {
        TypeExpr::Primitive(_) | TypeExpr::Struct(_) => None,
        TypeExpr::Optional { inner, .. } => value_list_without_via(inner),
        TypeExpr::Tuple { elements, .. } => elements.iter().find_map(value_list_without_via),
        TypeExpr::List {
            via: None, span, ..
        } => Some(*span),
        TypeExpr::List { element, .. } => value_list_without_via(element),
    }
}

fn ref_struct_name(ty: &TypeExpr) -> &str {
    match ty {
        TypeExpr::Struct(spanned) => spanned.as_str(),
        TypeExpr::Optional { inner, .. } => ref_struct_name(inner),
        TypeExpr::List { element, .. } => ref_struct_name(element),
        _ => unreachable!("only called on reference-shaped types"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        parse_schema,
        validated::{ColumnId, ScalarType, StorageTable, StructId, ValueType},
    };
    use grove_types::Diagnostic;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    fn stage_structs(source: &str, f: impl FnOnce(&Builder<'_>)) {
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
        f(&builder);
    }

    #[test]
    fn structs_with_declared_fields() {
        stage_structs(
            "root users: User; \
             struct User { name: String, manager: &?User, subordinates: &List<User>, \
             orders: List<Order>, tags: List<String>@id via user_tags[id, tag_value] } \
             struct Order { total: Dec@order_total, user: &User } \
             rel User.manager <<-> User.subordinates (users.manager_id -> users.id); \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.structs.len(), 2);
                let user = &b.structs[0];
                assert_eq!(user.name, "User");
                assert_eq!(user.table, TableId::new(0));
                assert_eq!(user.fields.len(), 5);

                let name = b.field_index[&("User", "name")];
                assert_eq!(name.struct_id(), StructId::new(0));
                assert_eq!(name.local(), 0);
                assert!(matches!(
                    user.fields[name.local()],
                    Field::Value { ref ty, .. }
                    if matches!(ty, ValueType::Scalar(ScalarType::String))
                ));

                let manager = b.field_index[&("User", "manager")];
                assert_eq!(manager.struct_id(), StructId::new(0));
                assert_eq!(manager.local(), 1);
                assert!(matches!(
                    user.fields[manager.local()],
                    Field::Ref {
                        target,
                        optional: true,
                        is_list: false,
                        owning: false,
                        ..
                    } if target == StructId::new(0)
                ));

                let subordinates = b.field_index[&("User", "subordinates")];
                assert_eq!(subordinates.local(), 2);
                assert!(matches!(
                    user.fields[subordinates.local()],
                    Field::Ref {
                        target,
                        is_list: true,
                        owning: false,
                        ..
                    } if target == StructId::new(0)
                ));

                let orders = b.field_index[&("User", "orders")];
                assert_eq!(orders.local(), 3);
                assert!(matches!(
                    user.fields[orders.local()],
                    Field::Ref {
                        target,
                        is_list: true,
                        owning: true,
                        ..
                    } if target == StructId::new(1)
                ));

                let tags = b.field_index[&("User", "tags")];
                assert_eq!(tags.local(), 4);
                let Field::Array {
                    element,
                    link,
                    storage,
                    ..
                } = &user.fields[tags.local()]
                else {
                    panic!("tags should be an Array field");
                };
                assert!(matches!(element, ValueType::Scalar(ScalarType::String)));
                assert_eq!(link.table_id(), TableId::new(0));
                assert!(matches!(
                    storage,
                    StorageTable {
                        table,
                        key,
                        value,
                    } if *table == TableId::new(2)
                        && key.local() == 0
                        && *value == vec![ColumnId::new(TableId::new(2), 1)]
                ));

                let total = b.field_index[&("Order", "total")];
                assert_eq!(total.struct_id(), StructId::new(1));
                let order = &b.structs[1];
                assert!(matches!(
                    order.fields[total.local()],
                    Field::Value {
                        ref columns,
                        ..
                    } if *columns == vec![ColumnId::new(TableId::new(1), 0)]
                ));
            },
        );
    }

    #[test]
    fn array_of_tuples_element_type() {
        stage_structs(
            "root users: User; \
             struct User { data: List<(Int, String)>@id via tuple_storage[id, (t_int, t_str)] }",
            |b| {
                assert!(b.diags.is_empty());
                let fid = b.field_index[&("User", "data")];
                let Field::Array { element, .. } = &b.structs[0].fields[fid.local()] else {
                    panic!("data should be an Array field");
                };
                assert!(matches!(
                    element,
                    ValueType::Tuple(ts)
                    if ts
                        == &vec![
                            ValueType::Scalar(ScalarType::Int),
                            ValueType::Scalar(ScalarType::String)
                        ]
                ));
            },
        );
    }

    #[test]
    fn nested_array_element_type() {
        stage_structs(
            "root users: User; struct User { \
             matrix: List<List<Int> via inner_table[ref, data]>@outer_ref via outer_table[ref, inner_ref] }",
            |b| {
                assert!(b.diags.is_empty());
                let fid = b.field_index[&("User", "matrix")];
                let Field::Array {
                    element,
                    link,
                    storage,
                    ..
                } = &b.structs[0].fields[fid.local()]
                else {
                    panic!("matrix should be an Array field");
                };
                assert_eq!(link.table_id(), TableId::new(0));
                assert_eq!(link.local(), 0);
                assert!(matches!(
                    storage,
                    StorageTable {
                        table,
                        key,
                        ..
                    } if *table == TableId::new(1) && key.local() == 0
                ));
                assert!(matches!(
                    element,
                    ValueType::Array {
                        element: inner,
                        storage: inner_storage,
                    }
                    if matches!(**inner, ValueType::Scalar(ScalarType::Int))
                        && inner_storage.table == TableId::new(2)
                        && inner_storage.key.local() == 0
                        && inner_storage.value
                            == vec![ColumnId::new(TableId::new(2), 1)]
                ));
            },
        );
    }

    #[test]
    fn value_optional_fields() {
        stage_structs(
            "root users: User; struct User { nickname: ?String, pos: ?Tuple<Float, Float>@(x, y) }",
            |b| {
                assert!(b.diags.is_empty());
                let nickname = b.field_index[&("User", "nickname")];
                let Field::Value { ty, .. } = &b.structs[0].fields[nickname.local()] else {
                    panic!("nickname should be a Value field");
                };
                assert!(matches!(
                    ty,
                    ValueType::Optional(inner)
                    if matches!(**inner, ValueType::Scalar(ScalarType::String))
                ));
                let pos = b.field_index[&("User", "pos")];
                let Field::Value { ty, columns, .. } = &b.structs[0].fields[pos.local()] else {
                    panic!("pos should be a Value field");
                };
                let ValueType::Optional(inner) = ty else {
                    panic!("pos should be Optional<Tuple<Float, Float>>");
                };
                let ValueType::Tuple(ts) = inner.as_ref() else {
                    panic!("pos should wrap a Tuple");
                };
                assert_eq!(ts.len(), 2);
                assert_eq!(columns.len(), 2);
            },
        );
    }

    #[test]
    fn list_scalar_without_via() {
        stage_structs(
            "root users: User; struct User { tags: List<String> }",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0011"]);
                assert!(!b.field_index.contains_key(&("User", "tags")));
                assert!(b.structs[0].fields.is_empty());
            },
        );
    }

    #[test]
    fn nested_value_list_without_via() {
        stage_structs(
            "root users: User; struct User { data: Tuple<List<Int>, String>@(a, b) }",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0011"]);
                assert!(b.structs[0].fields.is_empty());
            },
        );
    }

    #[test]
    fn via_on_struct_list() {
        stage_structs(
            "root users: User; root orders: Order; \
             struct User { items: List<Order>@i via user_orders[k, v] } struct Order {}",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0008"]);
                assert!(b.structs[0].fields.is_empty());
                assert!(!b.field_index.contains_key(&("User", "items")));
            },
        );
    }

    #[test]
    fn nested_struct_ref_excluded() {
        stage_structs(
            "root users: User; root orders: Order; \
             struct User { a: Tuple<Int, ?List<Order>>, b: List<List<Order>>, \
             c: List<Tuple<Order, Int>>@id via t[k, v] } struct Order { x: Int }",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0007", "SV0007", "SV0007"]);
                assert!(b.structs[0].fields.is_empty());
                assert_eq!(b.structs[1].fields.len(), 1);
            },
        );
    }

    #[test]
    fn value_arity_mismatch() {
        stage_structs(
            "root users: User; struct User { pos: Tuple<Float, Float>@(x) }",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0009"]);
                assert!(b.structs[0].fields.is_empty());
            },
        );
    }

    #[test]
    fn via_arity_mismatch() {
        stage_structs(
            "root users: User; struct User { data: List<Int>@id via ts[id, (a, b)] }",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0010"]);
                assert!(b.structs[0].fields.is_empty());
            },
        );
    }

    #[test]
    fn unmatched_forward_ref_excluded() {
        stage_structs(
            "root users: User; root orders: Order; \
             struct User { orders: List<Order> } struct Order { total: Dec }",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0012"]);
                assert!(b.structs[0].fields.is_empty());
                assert_eq!(b.structs[1].fields.len(), 1);
            },
        );
    }

    #[test]
    fn non_owning_on_value_shape_excluded() {
        stage_structs(
            "root users: User; struct User { \
             a: &Int, b: &List<Int>, c: &Tuple<Int, Int>, d: &List<String>@id via t[k, v] }",
            |b| {
                assert_eq!(
                    codes(&b.diags),
                    vec!["SV0021", "SV0021", "SV0021", "SV0021"]
                );
                assert!(b.structs[0].fields.is_empty());
                assert!(!b.field_index.contains_key(&("User", "a")));
                assert!(!b.field_index.contains_key(&("User", "d")));
            },
        );
    }
}
