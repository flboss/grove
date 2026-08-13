use std::collections::{HashMap, HashSet};

use crate::ast::{
    BuiltinType, ColumnMapping, Field as FieldDecl, FkMapping, ListStorage, RelationArrow, Schema,
    TypeExpr,
};
use crate::validated::{
    Column, ColumnId, Root, ScalarType, StructId, Table, TableId, ValidatedSchema,
};
use crate::validation_error::SchemaValidationError;
use grove_types::{Diagnostic, Span};

pub struct Builder<'s> {
    schema: &'s Schema,
    declared_structs: HashSet<&'s str>,
    forward_ref_relations: HashMap<(&'s str, &'s str), usize>,
    excluded_relations: HashSet<usize>,
    tables: Vec<Table>,
    table_index: HashMap<&'s str, TableId>,
    column_index: HashMap<(TableId, &'s str), ColumnId>,
    struct_table: Vec<TableId>,
    struct_index: HashMap<&'s str, StructId>,
    roots: Vec<Root>,
    diags: Vec<Diagnostic>,
}

pub fn validate(schema: Schema) -> (Option<ValidatedSchema>, Vec<Diagnostic>) {
    let mut builder = Builder::new(&schema);
    builder.stage_identity();
    builder.stage_physical();

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
            tables: Vec::new(),
            table_index: HashMap::new(),
            column_index: HashMap::new(),
            struct_table: Vec::new(),
            struct_index: HashMap::new(),
            roots: Vec::new(),
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
                    let child = relation.child.struct_name.as_str();
                    let parent = relation.parent.struct_name.as_str();
                    if self.declared_structs.contains(child)
                        && self.declared_structs.contains(parent)
                    {
                        let (owner, field) = if child == parent {
                            (
                                relation.child.struct_name.as_str(),
                                relation.child.field_name.as_str(),
                            )
                        } else {
                            (
                                relation.parent.struct_name.as_str(),
                                relation.parent.field_name.as_str(),
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

        for (name, mut spans) in unknown_refs {
            spans.sort_unstable();
            self.emit_error(SchemaValidationError::UnknownStructRef {
                name: name.to_string(),
                occurrences: spans,
            });
        }
    }

    fn stage_physical(&mut self) {
        let mut first_table_claims: HashMap<&'s str, Span> = HashMap::new();
        let mut declared_columns: HashMap<(TableId, &'s str), Span> = HashMap::new();

        self.match_structs_tables(&mut first_table_claims);
        self.collect_tables_and_columns(&mut first_table_claims, &mut declared_columns);
        self.resolve_roots();
    }

    fn match_structs_tables(&mut self, first_claims: &mut HashMap<&'s str, Span>) {
        for def in &self.schema.structs {
            let candidates = self.struct_table_candidates(&def.name);
            match candidates.len() {
                0 => {
                    self.emit_error(SchemaValidationError::StructNoTable {
                        span: def.name.span,
                        name: def.name.value.clone(),
                    });
                }
                1 => {
                    let table_name = candidates.into_iter().next().expect("one candidate");
                    if self.table_index.contains_key(&table_name) {
                        let previous = *first_claims
                            .get(table_name)
                            .expect("a conflicting table was claimed first");
                        self.emit_error(SchemaValidationError::DuplicateTable {
                            span: def.name.span,
                            name: table_name.to_string(),
                            previous,
                        });
                        continue;
                    }
                    let tid = self.register_table(table_name, def.name.span, first_claims);
                    let sid = StructId::new(self.struct_table.len());
                    self.struct_table.push(tid);
                    self.struct_index.insert(&def.name, sid);
                }
                _ => {
                    let mut tables: Vec<String> =
                        candidates.into_iter().map(String::from).collect();
                    tables.sort();
                    self.emit_error(SchemaValidationError::TableMismatch {
                        span: def.name.span,
                        name: def.name.value.clone(),
                        tables,
                    });
                }
            }
        }
    }

    fn struct_table_candidates(&self, struct_name: &str) -> HashSet<&'s str> {
        let mut candidates: HashSet<&str> = HashSet::new();
        for root in &self.schema.roots {
            if root.struct_name.value == struct_name {
                candidates.insert(root.table.as_ref().unwrap_or(&root.name));
            }
        }
        for (index, rel) in self.schema.relations.iter().enumerate() {
            if self.excluded_relations.contains(&index) {
                continue;
            }
            if rel.child.struct_name.value == struct_name {
                match &rel.fk {
                    FkMapping::Direct { child, .. } => {
                        candidates.insert(&child.table);
                    }
                    FkMapping::Indirect { a, .. } => {
                        candidates.insert(&a.target.table);
                    }
                }
            }
            if rel.parent.struct_name.value == struct_name {
                match &rel.fk {
                    FkMapping::Direct { parent, .. } => {
                        candidates.insert(&parent.table);
                    }
                    FkMapping::Indirect { b, .. } => {
                        candidates.insert(&b.target.table);
                    }
                }
            }
        }
        candidates
    }

    fn register_table(
        &mut self,
        name: &'s str,
        span: Span,
        first_claims: &mut HashMap<&'s str, Span>,
    ) -> TableId {
        if let Some(&tid) = self.table_index.get(name) {
            if let Some(&previous) = first_claims.get(name) {
                self.emit_error(SchemaValidationError::DuplicateTable {
                    span,
                    name: name.to_string(),
                    previous,
                });
            }
            tid
        } else {
            let tid = TableId::new(self.tables.len());
            self.tables.push(Table {
                name: name.to_string(),
                columns: Vec::new(),
            });
            self.table_index.insert(name, tid);
            first_claims.insert(name, span);
            tid
        }
    }

    fn collect_tables_and_columns(
        &mut self,
        first_claims: &mut HashMap<&'s str, Span>,
        declared_columns: &mut HashMap<(TableId, &'s str), Span>,
    ) {
        for def in &self.schema.structs {
            let Some(&sid) = self.struct_index.get(def.name.as_str()) else {
                continue;
            };
            let tid = self.struct_table[sid.index()];
            for field in &def.fields {
                self.collect_field_columns(tid, field, first_claims, declared_columns);
            }
        }
        for (index, rel) in self.schema.relations.iter().enumerate() {
            if self.excluded_relations.contains(&index) {
                continue;
            }
            let (Some(&child), Some(&parent)) = (
                self.struct_index.get(rel.child.struct_name.as_str()),
                self.struct_index.get(rel.parent.struct_name.as_str()),
            ) else {
                continue;
            };
            self.collect_relation_columns(child, parent, rel, first_claims, declared_columns);
        }
    }

    fn collect_field_columns(
        &mut self,
        tid: TableId,
        field: &'s FieldDecl,
        first_claims: &mut HashMap<&'s str, Span>,
        declared_columns: &mut HashMap<(TableId, &'s str), Span>,
    ) {
        match field_kind(&field.exposed_type) {
            FieldKind::Value | FieldKind::ListNoVia => {
                let scalars = value_scalar_leaves(&field.exposed_type);
                for (i, (name, span)) in value_column_refs(field).into_iter().enumerate() {
                    self.claim_declared(tid, name, span, scalars.get(i).copied(), declared_columns);
                }
            }
            FieldKind::Array { element, via } => {
                let (link, span) = array_link_column(field);
                self.claim_reference(tid, link, span);
                self.collect_via_storage(via, element, first_claims, declared_columns);
                self.collect_storage_tables(element, first_claims, declared_columns);
            }
            FieldKind::Ref => {}
        }
    }

    fn collect_via_storage(
        &mut self,
        via: &'s ListStorage,
        element: &'s TypeExpr,
        first_claims: &mut HashMap<&'s str, Span>,
        declared_columns: &mut HashMap<(TableId, &'s str), Span>,
    ) {
        let tid = self.register_table(&via.table, via.table.span, first_claims);
        self.claim_declared(tid, &via.key_col, via.key_col.span, None, declared_columns);
        let value_types = storage_value_types(element);
        match &via.value {
            ColumnMapping::Single(col) => {
                self.claim_declared(
                    tid,
                    col,
                    col.span,
                    value_types.first().copied().flatten(),
                    declared_columns,
                );
            }
            ColumnMapping::Multi(cols) => {
                for (i, col) in cols.iter().enumerate() {
                    self.claim_declared(
                        tid,
                        col,
                        col.span,
                        value_types.get(i).copied().flatten(),
                        declared_columns,
                    );
                }
            }
        }
    }

    fn collect_storage_tables(
        &mut self,
        ty: &'s TypeExpr,
        first_claims: &mut HashMap<&'s str, Span>,
        declared_columns: &mut HashMap<(TableId, &'s str), Span>,
    ) {
        match ty {
            TypeExpr::List {
                element,
                via: Some(via),
                ..
            } => {
                self.collect_via_storage(via, element, first_claims, declared_columns);
                self.collect_storage_tables(element, first_claims, declared_columns);
            }
            TypeExpr::Tuple { elements, .. } => {
                for element in elements {
                    self.collect_storage_tables(element, first_claims, declared_columns);
                }
            }
            TypeExpr::Optional { inner, .. } => {
                self.collect_storage_tables(inner, first_claims, declared_columns);
            }
            _ => {}
        }
    }

    fn collect_relation_columns(
        &mut self,
        child: StructId,
        parent: StructId,
        rel: &'s crate::ast::Relation,
        first_claims: &mut HashMap<&'s str, Span>,
        declared_columns: &mut HashMap<(TableId, &'s str), Span>,
    ) {
        let child_tid = self.struct_table[child.index()];
        let parent_tid = self.struct_table[parent.index()];
        match &rel.fk {
            FkMapping::Direct { child, parent } => {
                self.claim_reference(child_tid, &child.col, child.col.span);
                self.claim_reference(parent_tid, &parent.col, parent.col.span);
            }
            FkMapping::Indirect {
                join_table, a, b, ..
            } => {
                let j_tid = self.register_table(join_table, join_table.span, first_claims);
                self.claim_declared(j_tid, &a.join_col, a.join_col.span, None, declared_columns);
                self.claim_declared(j_tid, &b.join_col, b.join_col.span, None, declared_columns);
                self.claim_reference(child_tid, &a.target.col, a.target.col.span);
                self.claim_reference(parent_tid, &b.target.col, b.target.col.span);
            }
        }
    }

    fn claim_declared(
        &mut self,
        tid: TableId,
        name: &'s str,
        span: Span,
        ty: Option<ScalarType>,
        declared_columns: &mut HashMap<(TableId, &'s str), Span>,
    ) -> ColumnId {
        let cid = self.claim_column(tid, name, ty);
        let key = (tid, name);
        if let Some(previous) = declared_columns.insert(key, span) {
            self.emit_error(SchemaValidationError::DuplicateColumn {
                table: self.tables[tid.index()].name.clone(),
                name: name.to_string(),
                span,
                previous,
            });
        }
        cid
    }

    fn claim_reference(&mut self, tid: TableId, name: &'s str, _span: Span) -> ColumnId {
        self.claim_column(tid, name, None)
    }

    fn claim_column(&mut self, tid: TableId, name: &'s str, ty: Option<ScalarType>) -> ColumnId {
        let key = (tid, name);
        if let Some(&cid) = self.column_index.get(&key) {
            if let Some(upgrade) = ty {
                let col = &mut self.tables[tid.index()].columns[cid.local()];
                if col.ty.is_none() {
                    col.ty = Some(upgrade);
                }
            }
            cid
        } else {
            let cid = ColumnId::new(tid, self.tables[tid.index()].columns.len());
            self.tables[tid.index()].columns.push(Column {
                name: name.to_string(),
                ty,
            });
            self.column_index.insert(key, cid);
            cid
        }
    }

    fn resolve_roots(&mut self) {
        for root in &self.schema.roots {
            if let Some(&sid) = self.struct_index.get(root.struct_name.as_str()) {
                self.roots.push(Root {
                    name: root.name.value.clone(),
                    struct_id: sid,
                });
            }
        }
    }
}

fn field_kind(ty: &TypeExpr) -> FieldKind<'_> {
    match ty {
        TypeExpr::Primitive(_) | TypeExpr::Tuple { .. } => FieldKind::Value,
        TypeExpr::Struct(_) => FieldKind::Ref,
        TypeExpr::Optional { inner, .. } if matches!(inner.as_ref(), TypeExpr::Struct(_)) => {
            FieldKind::Ref
        }
        TypeExpr::Optional { .. } => FieldKind::Value,
        TypeExpr::List { element, .. } if matches!(element.as_ref(), TypeExpr::Struct(_)) => {
            FieldKind::Ref
        }
        TypeExpr::List {
            element,
            via: Some(via),
            ..
        } => FieldKind::Array { element, via },
        TypeExpr::List { .. } => FieldKind::ListNoVia,
    }
}

enum FieldKind<'a> {
    Value,
    Array {
        element: &'a TypeExpr,
        via: &'a ListStorage,
    },
    ListNoVia,
    Ref,
}

fn value_column_refs(field: &FieldDecl) -> Vec<(&str, Span)> {
    match &field.column {
        None => vec![(field.name.as_str(), field.name.span)],
        Some(ColumnMapping::Single(col)) => vec![(col.as_str(), col.span)],
        Some(ColumnMapping::Multi(cols)) => cols.iter().map(|c| (c.as_str(), c.span)).collect(),
    }
}

fn array_link_column(field: &FieldDecl) -> (&str, Span) {
    match &field.column {
        None => (field.name.as_str(), field.name.span),
        Some(ColumnMapping::Single(col)) => (col.as_str(), col.span),
        Some(ColumnMapping::Multi(cols)) => (cols[0].as_str(), cols[0].span),
    }
}

fn value_scalar_leaves(ty: &TypeExpr) -> Vec<ScalarType> {
    match ty {
        TypeExpr::Primitive(spanned) => vec![builtin_scalar(spanned)],
        TypeExpr::Optional { inner, .. } => value_scalar_leaves(inner),
        TypeExpr::Tuple { elements, .. } => elements.iter().flat_map(value_scalar_leaves).collect(),
        TypeExpr::List { element, .. } => value_scalar_leaves(element),
        TypeExpr::Struct(_) => Vec::new(),
    }
}

fn storage_value_types(ty: &TypeExpr) -> Vec<Option<ScalarType>> {
    match ty {
        TypeExpr::Primitive(spanned) => vec![Some(builtin_scalar(spanned))],
        TypeExpr::Optional { inner, .. } => storage_value_types(inner),
        TypeExpr::Tuple { elements, .. } => elements.iter().flat_map(storage_value_types).collect(),
        TypeExpr::List { .. } => vec![None],
        TypeExpr::Struct(_) => Vec::new(),
    }
}

fn builtin_scalar(builtin: &BuiltinType) -> ScalarType {
    match builtin {
        BuiltinType::Int => ScalarType::Int,
        BuiltinType::Float => ScalarType::Float,
        BuiltinType::Dec => ScalarType::Dec,
        BuiltinType::String => ScalarType::String,
        BuiltinType::Bool => ScalarType::Bool,
        BuiltinType::Instant => ScalarType::Instant,
        BuiltinType::Duration => ScalarType::Duration,
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

    fn stage_physical(source: &str, f: impl FnOnce(&Builder<'_>)) {
        let (schema, parse_diags) = parse_schema(source);
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let schema = schema.expect("source should parse");
        let mut builder = Builder::new(&schema);
        builder.stage_identity();
        builder.stage_physical();
        f(&builder);
    }

    #[test]
    fn root_table_defaults_to_root_name() {
        stage_physical("root users: User; struct User { name: String }", |b| {
            assert!(b.diags.is_empty());
            assert_eq!(b.tables.len(), 1);
            assert_eq!(b.tables[0].name, "users");
            assert_eq!(
                b.tables[0].columns,
                vec![Column {
                    name: "name".into(),
                    ty: Some(ScalarType::String),
                }]
            );
            assert_eq!(b.struct_index.get("User"), Some(&StructId::new(0)));
            assert_eq!(b.struct_table, vec![TableId::new(0)]);
            assert_eq!(
                b.roots,
                vec![Root {
                    name: "users".into(),
                    struct_id: StructId::new(0),
                }]
            );
        });
    }

    #[test]
    fn root_with_table_rename() {
        stage_physical(
            "root users: User@people; struct User { name: String }",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.tables[0].name, "people");
                assert_eq!(
                    b.roots,
                    vec![Root {
                        name: "users".into(),
                        struct_id: StructId::new(0),
                    }]
                );
            },
        );
    }

    #[test]
    fn struct_resolved_solely_via_relation() {
        stage_physical(
            "struct User { orders: List<Order> } struct Order {} \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.struct_index.len(), 2);
                let user = *b.struct_index.get("User").unwrap();
                let order = *b.struct_index.get("Order").unwrap();
                assert_eq!(b.tables[user.index()].name, "users");
                assert_eq!(b.tables[order.index()].name, "orders");
                assert_eq!(
                    b.tables[order.index()].columns,
                    vec![Column {
                        name: "user_id".into(),
                        ty: None,
                    }]
                );
                assert_eq!(
                    b.tables[user.index()].columns,
                    vec![Column {
                        name: "id".into(),
                        ty: None,
                    }]
                );
            },
        );
    }

    #[test]
    fn relation_pk_reuses_declared_column() {
        stage_physical(
            "root users: User; struct User { id: Int } struct Order {} \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert!(b.diags.is_empty());
                let user = *b.struct_index.get("User").unwrap();
                assert_eq!(
                    b.tables[user.index()].columns,
                    vec![Column {
                        name: "id".into(),
                        ty: Some(ScalarType::Int),
                    }]
                );
                let user_id = b.column_index[&(b.struct_table[user.index()], "id")];
                assert_eq!(user_id.local(), 0);
            },
        );
    }

    #[test]
    fn struct_no_table() {
        stage_physical("struct A {}", |b| {
            assert_eq!(codes(&b.diags), vec!["SV0004"]);
            assert!(b.struct_index.is_empty());
            assert!(b.tables.is_empty());
            assert!(b.struct_table.is_empty());
        });
    }

    #[test]
    fn struct_table_mismatch() {
        stage_physical(
            "root a: A; struct A {} struct B {} rel A.x <-> B.y (bx.fk -> bx.pk);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0003"]);
                assert_eq!(b.tables.len(), 1);
                assert_eq!(b.tables[0].name, "bx");
                assert!(b.struct_index.contains_key("B"));
                assert!(!b.struct_index.contains_key("A"));
            },
        );
    }

    #[test]
    fn duplicate_table_name_second_struct_excluded() {
        stage_physical(
            "struct C {} struct D {} struct E {} struct F {} \
             rel C.p <-> E.e (t.fk -> e.pk); rel D.q <-> F.f (t.fk2 -> f.pk);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0005"]);
                assert_eq!(b.diags[0].labels.len(), 2);
                assert!(b.struct_index.contains_key("C"));
                assert!(b.struct_index.contains_key("E"));
                assert!(b.struct_index.contains_key("F"));
                assert!(!b.struct_index.contains_key("D"));
            },
        );
    }

    #[test]
    fn duplicate_column_two_declared_sources() {
        stage_physical(
            "root users: User; struct User { a: Int@x, b: String@x }",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0006"]);
                assert_eq!(b.diags[0].labels.len(), 2);
                assert_eq!(b.tables[0].columns.len(), 1);
            },
        );
    }

    #[test]
    fn array_field_inventories_link_and_storage_tables() {
        stage_physical(
            "root users: User; \
             struct User { tags: List<String>@id via user_tags[user_key, tag_value] }",
            |b| {
                assert!(b.diags.is_empty());
                let names: Vec<&str> = b.tables.iter().map(|t| t.name.as_str()).collect();
                assert_eq!(names, vec!["users", "user_tags"]);
                assert_eq!(
                    b.tables[0].columns,
                    vec![Column {
                        name: "id".into(),
                        ty: None,
                    }]
                );
                let stacked: Vec<(&str, Option<ScalarType>)> = b.tables[1]
                    .columns
                    .iter()
                    .map(|c| (c.name.as_str(), c.ty))
                    .collect();
                assert_eq!(
                    stacked,
                    vec![("user_key", None), ("tag_value", Some(ScalarType::String)),]
                );
            },
        );
    }

    #[test]
    fn nested_and_tuple_storage_tables() {
        stage_physical(
            "root users: User; struct User { \
             matrix: List<List<Int> via inner_table[ref, data]>@outer_ref via outer_table[ref, inner_ref], \
             data: List<(Int, String)>@id via tuple_storage[id, (t_int, t_str)] }",
            |b| {
                assert!(b.diags.is_empty());
                let names: Vec<&str> = b.tables.iter().map(|t| t.name.as_str()).collect();
                assert_eq!(
                    names,
                    vec!["users", "outer_table", "inner_table", "tuple_storage"]
                );
                assert_eq!(b.tables[0].columns.len(), 2);
                assert!(b.tables[0].columns.iter().any(|c| c.name == "outer_ref"));
                assert!(b.tables[0].columns.iter().any(|c| c.name == "id"));
                let stacked = |t: &crate::validated::Table| -> Vec<(String, Option<ScalarType>)> {
                    t.columns.iter().map(|c| (c.name.clone(), c.ty)).collect()
                };
                assert_eq!(
                    stacked(&b.tables[1]),
                    vec![("ref".into(), None), ("inner_ref".into(), None)]
                );
                assert_eq!(
                    stacked(&b.tables[2]),
                    vec![("ref".into(), None), ("data".into(), Some(ScalarType::Int))]
                );
                assert_eq!(
                    stacked(&b.tables[3]),
                    vec![
                        ("id".into(), None),
                        ("t_int".into(), Some(ScalarType::Int)),
                        ("t_str".into(), Some(ScalarType::String)),
                    ]
                );
            },
        );
    }

    #[test]
    fn many_to_many_registers_join_table() {
        stage_physical(
            "root users: User; root roles: Role; struct User {} struct Role {} \
             rel Role.users <<->> User.roles via user_roles[role_id -> roles.id, user_id -> users.id];",
            |b| {
                assert!(b.diags.is_empty());
                let names: Vec<&str> = b.tables.iter().map(|t| t.name.as_str()).collect();
                assert_eq!(names, vec!["users", "roles", "user_roles"]);
                let cols: Vec<&str> = b.tables[2]
                    .columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect();
                assert_eq!(cols, vec!["role_id", "user_id"]);
                assert!(b.roots.iter().any(|r| r.name == "users"));
                assert!(b.roots.iter().any(|r| r.name == "roles"));
            },
        );
    }

    #[test]
    fn self_ref_single_table_candidate() {
        stage_physical(
            "root users: User; struct User { manager: ?User } \
             rel User.manager <<-> User.subordinates (users.manager_id -> users.id);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.tables.len(), 1);
                assert_eq!(b.tables[0].name, "users");
                assert_eq!(
                    b.tables[0].columns,
                    vec![
                        Column {
                            name: "manager_id".into(),
                            ty: None,
                        },
                        Column {
                            name: "id".into(),
                            ty: None,
                        },
                    ]
                );
            },
        );
    }
}
