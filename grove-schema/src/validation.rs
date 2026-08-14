use std::collections::{HashMap, HashSet};

use crate::ast::{
    BuiltinType, ColumnMapping, Field as FieldDecl, FkMapping, ListStorage, RelationArrow, Schema,
    TypeExpr,
};
use crate::validated::{
    Column, ColumnId, Field, FieldId, Relation, Root, ScalarType, StorageTable, Struct, StructId,
    Table, TableId, ValidatedSchema, ValueType,
};
use crate::validation_error::SchemaValidationError;
use grove_types::{Diagnostic, Span};

pub struct Builder<'s> {
    schema: &'s Schema,
    declared_structs: HashSet<&'s str>,
    forward_ref_relations: HashMap<(&'s str, &'s str), usize>,
    tables: Vec<Table>,
    table_index: HashMap<&'s str, TableId>,
    column_index: HashMap<(TableId, &'s str), ColumnId>,
    struct_table: Vec<TableId>,
    struct_index: HashMap<&'s str, StructId>,
    roots: Vec<Root>,
    structs: Vec<Struct>,
    field_index: HashMap<(&'s str, &'s str), FieldId>,
    relations: Vec<Relation>,
    diags: Vec<Diagnostic>,
}

pub fn validate(schema: Schema) -> (Option<ValidatedSchema>, Vec<Diagnostic>) {
    let mut builder = Builder::new(&schema);
    builder.stage_identity();
    builder.stage_physical();
    builder.stage_structs();
    builder.stage_relations();

    // TODO: `ValidatedSchema` assembly
    (None, builder.diags)
}

impl<'s> Builder<'s> {
    fn new(schema: &'s Schema) -> Builder<'s> {
        Builder {
            schema,
            declared_structs: HashSet::new(),
            forward_ref_relations: HashMap::new(),
            tables: Vec::new(),
            table_index: HashMap::new(),
            column_index: HashMap::new(),
            struct_table: Vec::new(),
            struct_index: HashMap::new(),
            roots: Vec::new(),
            structs: Vec::new(),
            field_index: HashMap::new(),
            relations: Vec::new(),
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
                RelationArrow::OneToMany | RelationArrow::ManyToMany => {}
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
        for rel in &self.schema.relations {
            let is_one_to_many = matches!(rel.arrow.value, RelationArrow::OneToMany);
            if rel.child.struct_name.value == struct_name {
                match &rel.fk {
                    FkMapping::Direct { child, parent } => {
                        let table = if is_one_to_many {
                            &parent.table
                        } else {
                            &child.table
                        };
                        candidates.insert(table);
                    }
                    FkMapping::Indirect { a, .. } => {
                        candidates.insert(&a.target.table);
                    }
                }
            }
            if rel.parent.struct_name.value == struct_name {
                match &rel.fk {
                    FkMapping::Direct { child, parent } => {
                        let table = if is_one_to_many {
                            &child.table
                        } else {
                            &parent.table
                        };
                        candidates.insert(table);
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
        for rel in &self.schema.relations {
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
                let (fk_tid, pk_tid) = if matches!(rel.arrow.value, RelationArrow::OneToMany) {
                    (parent_tid, child_tid)
                } else {
                    (child_tid, parent_tid)
                };
                self.claim_reference(fk_tid, &child.col, child.col.span);
                self.claim_reference(pk_tid, &parent.col, parent.col.span);
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

    fn stage_structs(&mut self) {
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

    fn stage_relations(&mut self) {
        for rel in &self.schema.relations {
            let Some(&child) = self.struct_index.get(rel.child.struct_name.as_str()) else {
                continue;
            };
            let Some(&parent) = self.struct_index.get(rel.parent.struct_name.as_str()) else {
                continue;
            };
            let is_self = child == parent;

            let (has_forward, forward_fid) = self.resolve_forward(rel, child, is_self);
            let needs_forward = matches!(
                rel.arrow.value,
                RelationArrow::OneToOne | RelationArrow::ManyToOne
            );
            if needs_forward && !has_forward {
                continue;
            }

            let backrefs = self.backref_plan(rel, child, parent, is_self);
            if !self.check_backref_collisions(&backrefs) {
                continue;
            }

            let columns = match self.resolve_relation_columns(rel, child, parent) {
                Ok(columns) => columns,
                Err(note) => {
                    self.emit_error(SchemaValidationError::ColumnMismatch {
                        span: rel.arrow.span,
                        note,
                    });
                    continue;
                }
            };

            let fids: Vec<FieldId> = backrefs
                .iter()
                .map(|b| {
                    let fid = FieldId::new(b.sid, self.structs[b.sid.index()].fields.len());
                    self.structs[b.sid.index()].fields.push(Field::BackRef {
                        name: b.name.to_string(),
                        target: if b.sid == child { parent } else { child },
                        is_list: b.is_list,
                        optional: b.optional,
                    });
                    fid
                })
                .collect();

            let relation = match rel.arrow.value {
                RelationArrow::OneToOne | RelationArrow::ManyToOne => {
                    let forward_fid =
                        forward_fid.expect("owning relations resolve a forward endpoint");
                    let (child_ref, parent_ref) = if is_self {
                        (forward_fid, fids[0])
                    } else {
                        (fids[0], forward_fid)
                    };
                    let RelationColumns::Direct { fk, pk } = columns else {
                        unreachable!("owning relations use direct FK mappings")
                    };
                    if matches!(rel.arrow.value, RelationArrow::ManyToOne) {
                        Relation::ManyToOne {
                            child_ref,
                            parent_ref,
                            fk,
                            pk,
                        }
                    } else {
                        Relation::OneToOne {
                            child_ref,
                            parent_ref,
                            fk,
                            pk,
                        }
                    }
                }
                RelationArrow::OneToMany => {
                    let RelationColumns::Direct { fk, pk } = columns else {
                        unreachable!("OneToMany uses a direct FK mapping")
                    };
                    Relation::OneToMany {
                        a_ref: fids[0],
                        b_ref: fids[1],
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
                        a_ref: fids[0],
                        b_ref: fids[1],
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

    fn resolve_forward(
        &mut self,
        rel: &'s crate::ast::Relation,
        child: StructId,
        is_self: bool,
    ) -> (bool, Option<FieldId>) {
        if matches!(
            rel.arrow.value,
            RelationArrow::OneToMany | RelationArrow::ManyToMany
        ) {
            return (true, None);
        }
        let (owner, field, span) = if is_self {
            (
                rel.child.struct_name.as_str(),
                rel.child.field_name.as_str(),
                rel.child.field_name.span,
            )
        } else {
            (
                rel.parent.struct_name.as_str(),
                rel.parent.field_name.as_str(),
                rel.parent.field_name.span,
            )
        };

        let Some(&fid) = self.field_index.get(&(owner, field)) else {
            self.emit_error(SchemaValidationError::ForwardRefMissing {
                span,
                struct_name: owner.to_string(),
                field: field.to_string(),
            });
            return (false, None);
        };

        let model = self.structs[fid.struct_id().index()].fields[fid.local()].clone();
        match model {
            Field::Ref {
                target, is_list, ..
            } => {
                let wants_list = matches!(rel.arrow.value, RelationArrow::ManyToOne) && !is_self;
                if is_list != wants_list {
                    self.emit_error(SchemaValidationError::ForwardRefTypeMismatch {
                        span,
                        struct_name: owner.to_string(),
                        field: field.to_string(),
                    });
                }
                if target != child {
                    let actual = self.structs[target.index()].name.clone();
                    let expected = self.structs[child.index()].name.clone();
                    self.emit_error(SchemaValidationError::ForwardRefTargetMismatch {
                        span,
                        struct_name: owner.to_string(),
                        field: field.to_string(),
                        actual,
                        expected,
                    });
                }
                (true, Some(fid))
            }
            _ => {
                self.emit_error(SchemaValidationError::ForwardRefTypeMismatch {
                    span,
                    struct_name: owner.to_string(),
                    field: field.to_string(),
                });
                (true, Some(fid))
            }
        }
    }

    fn backref_plan(
        &self,
        rel: &'s crate::ast::Relation,
        child: StructId,
        parent: StructId,
        is_self: bool,
    ) -> Vec<BackrefPlan<'s>> {
        match rel.arrow.value {
            RelationArrow::OneToOne | RelationArrow::ManyToOne => {
                let (sid, struct_name, name, span) = if is_self {
                    (
                        parent,
                        rel.parent.struct_name.as_str(),
                        rel.parent.field_name.as_str(),
                        rel.parent.field_name.span,
                    )
                } else {
                    (
                        child,
                        rel.child.struct_name.as_str(),
                        rel.child.field_name.as_str(),
                        rel.child.field_name.span,
                    )
                };
                let is_list = matches!(rel.arrow.value, RelationArrow::ManyToOne) && is_self;
                vec![BackrefPlan {
                    sid,
                    struct_name,
                    name,
                    span,
                    is_list,
                    optional: false,
                }]
            }
            RelationArrow::OneToMany => vec![
                BackrefPlan {
                    sid: child,
                    struct_name: rel.child.struct_name.as_str(),
                    name: rel.child.field_name.as_str(),
                    span: rel.child.field_name.span,
                    is_list: true,
                    optional: false,
                },
                BackrefPlan {
                    sid: parent,
                    struct_name: rel.parent.struct_name.as_str(),
                    name: rel.parent.field_name.as_str(),
                    span: rel.parent.field_name.span,
                    is_list: false,
                    optional: true,
                },
            ],
            RelationArrow::ManyToMany => vec![
                BackrefPlan {
                    sid: child,
                    struct_name: rel.child.struct_name.as_str(),
                    name: rel.child.field_name.as_str(),
                    span: rel.child.field_name.span,
                    is_list: true,
                    optional: false,
                },
                BackrefPlan {
                    sid: parent,
                    struct_name: rel.parent.struct_name.as_str(),
                    name: rel.parent.field_name.as_str(),
                    span: rel.parent.field_name.span,
                    is_list: true,
                    optional: false,
                },
            ],
        }
    }

    fn check_backref_collisions(&mut self, backrefs: &[BackrefPlan<'s>]) -> bool {
        let mut conflict = false;
        for b in backrefs {
            if self.field_index.contains_key(&(b.struct_name, b.name)) {
                self.emit_error(SchemaValidationError::BackRefInStruct {
                    span: b.span,
                    struct_name: b.struct_name.to_string(),
                    field: b.name.to_string(),
                });
                conflict = true;
            }
            let duplicate = self.structs[b.sid.index()]
                .fields
                .iter()
                .any(|f| matches!(f, Field::BackRef { name, .. } if name == b.name));
            if duplicate {
                self.emit_error(SchemaValidationError::DuplicateBackRefName {
                    span: b.span,
                    struct_name: b.struct_name.to_string(),
                    field: b.name.to_string(),
                });
                conflict = true;
            }
        }
        !conflict
    }

    fn resolve_relation_columns(
        &self,
        rel: &'s crate::ast::Relation,
        child: StructId,
        parent: StructId,
    ) -> Result<RelationColumns, String> {
        let child_tid = self.struct_table[child.index()];
        let parent_tid = self.struct_table[parent.index()];
        let is_one_to_many = matches!(rel.arrow.value, RelationArrow::OneToMany);
        let is_many_to_many = matches!(rel.arrow.value, RelationArrow::ManyToMany);
        match &rel.fk {
            FkMapping::Direct { child, parent } => {
                if is_many_to_many {
                    return Err(
                        "an N:M relation (`<<->>`) requires a `via` join table mapping, not a direct FK"
                            .into(),
                    );
                }
                let (fk_tid, pk_tid) = if is_one_to_many {
                    (parent_tid, child_tid)
                } else {
                    (child_tid, parent_tid)
                };
                let fk = self
                    .column_index
                    .get(&(fk_tid, child.col.as_str()))
                    .copied();
                let pk = self
                    .column_index
                    .get(&(pk_tid, parent.col.as_str()))
                    .copied();
                match (fk, pk) {
                    (Some(fk), Some(pk)) => Ok(RelationColumns::Direct { fk, pk }),
                    _ => Err(format!(
                        "could not resolve FK/PK columns `{}.{}` and `{}.{}`",
                        child.table.value, child.col.value, parent.table.value, parent.col.value
                    )),
                }
            }
            FkMapping::Indirect {
                join_table, a, b, ..
            } => {
                if !is_many_to_many {
                    return Err(
                        "an owning or OneToMany relation requires a direct `(fk -> pk)` mapping, not `via`"
                            .into(),
                    );
                }
                let j_tid = self.table_index[join_table.as_str()];
                let a_col = self
                    .column_index
                    .get(&(j_tid, a.join_col.as_str()))
                    .copied();
                let a_pk = self
                    .column_index
                    .get(&(child_tid, a.target.col.as_str()))
                    .copied();
                let b_col = self
                    .column_index
                    .get(&(j_tid, b.join_col.as_str()))
                    .copied();
                let b_pk = self
                    .column_index
                    .get(&(parent_tid, b.target.col.as_str()))
                    .copied();
                match (a_col, a_pk, b_col, b_pk) {
                    (Some(a_col), Some(a_pk), Some(b_col), Some(b_pk)) => {
                        Ok(RelationColumns::Indirect {
                            join_table: j_tid,
                            a_col,
                            a_pk,
                            b_col,
                            b_pk,
                        })
                    }
                    _ => Err(format!(
                        "could not resolve join-table columns for `{}`",
                        join_table.value
                    )),
                }
            }
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

        match field_kind(&field.exposed_type) {
            FieldKind::Value => {
                let declared = value_column_refs(field).len();
                let expected = value_scalar_leaves(&field.exposed_type).len();
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
                let columns = value_column_refs(field)
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
                let expected = storage_value_types(element).len();
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
                let (link_name, _) = array_link_column(field);
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
                    .forward_ref_relations
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
                })
            }
        }
    }

    fn value_type(&self, ty: &TypeExpr) -> ValueType {
        match ty {
            TypeExpr::Primitive(spanned) => ValueType::Scalar(builtin_scalar(spanned)),
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

struct BackrefPlan<'s> {
    sid: StructId,
    struct_name: &'s str,
    name: &'s str,
    span: Span,
    is_list: bool,
    optional: bool,
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
    fn one_to_many_claims_no_forward() {
        stage_identity(
            "struct A {} struct B {} rel A.x <->> B.y (a.a -> b.b);",
            |b| {
                assert!(b.diags.is_empty());
                assert!(!b.forward_ref_relations.contains_key(&("A", "x")));
                assert!(!b.forward_ref_relations.contains_key(&("B", "y")));
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

    #[test]
    fn one_to_many_swap_table_roles() {
        stage_physical(
            "struct Image {} struct Message {} \
             rel Image.messages <->> Message.image (messages.image_id -> images.id);",
            |b| {
                assert!(b.diags.is_empty());
                let image = *b.struct_index.get("Image").unwrap();
                let message = *b.struct_index.get("Message").unwrap();
                assert_eq!(b.tables[image.index()].name, "images");
                assert_eq!(b.tables[message.index()].name, "messages");
                assert_eq!(
                    b.tables[image.index()].columns,
                    vec![Column {
                        name: "id".into(),
                        ty: None,
                    }]
                );
                assert_eq!(
                    b.tables[message.index()].columns,
                    vec![Column {
                        name: "image_id".into(),
                        ty: None,
                    }]
                );
            },
        );
    }

    #[test]
    fn one_to_many_pk_reuses_declared_column() {
        stage_physical(
            "root images: Image; struct Image { id: Int } struct Message {} \
             rel Image.messages <->> Message.image (messages.image_id -> images.id);",
            |b| {
                assert!(b.diags.is_empty());
                let image = *b.struct_index.get("Image").unwrap();
                assert_eq!(
                    b.tables[image.index()].columns,
                    vec![Column {
                        name: "id".into(),
                        ty: Some(ScalarType::Int),
                    }]
                );
                let id = b.column_index[&(b.struct_table[image.index()], "id")];
                assert_eq!(id.local(), 0);
                let message = *b.struct_index.get("Message").unwrap();
                assert_eq!(
                    b.tables[message.index()].columns,
                    vec![Column {
                        name: "image_id".into(),
                        ty: None,
                    }]
                );
            },
        );
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
             struct User { name: String, manager: ?User, orders: List<Order>, \
             tags: List<String>@id via user_tags[id, tag_value] } \
             struct Order { total: Dec@order_total } \
             rel User.manager <<-> User.subordinates (users.manager_id -> users.id); \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert!(b.diags.is_empty());
                assert_eq!(b.structs.len(), 2);
                let user = &b.structs[0];
                assert_eq!(user.name, "User");
                assert_eq!(user.table, TableId::new(0));
                assert_eq!(user.fields.len(), 4);

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
                        ..
                    } if target == StructId::new(0)
                ));

                let orders = b.field_index[&("User", "orders")];
                assert_eq!(orders.local(), 2);
                assert!(matches!(
                    user.fields[orders.local()],
                    Field::Ref {
                        target,
                        is_list: true,
                        ..
                    } if target == StructId::new(1)
                ));

                let tags = b.field_index[&("User", "tags")];
                assert_eq!(tags.local(), 3);
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
    fn one_to_many_endpoint_field_unmatched() {
        stage_structs(
            "root images: Image; root messages: Message; \
             struct Image {} struct Message { image: ?Image } \
             rel Image.messages <->> Message.image (messages.image_id -> images.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0012"]);
                assert!(b.structs[0].fields.is_empty());
                assert!(b.structs[1].fields.is_empty());
                assert!(!b.field_index.contains_key(&("Message", "image")));
            },
        );
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
             struct Profile { bio: String } struct User { profile: Profile } \
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
                    Field::BackRef {
                        name: ref n,
                        target,
                        is_list: false,
                        optional: false,
                    } if n == "user" && target == user
                ));
            },
        );
    }

    #[test]
    fn relations_many_to_one() {
        stage_relations(
            "root users: User; struct User { orders: List<Order> } \
             struct Order { total: Dec } \
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
            "root users: User; struct User { manager: ?User } \
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
                    Field::BackRef {
                        name: ref n,
                        target,
                        is_list: true,
                        optional: false,
                    } if n == "subordinates" && target == user
                ));
            },
        );
    }

    #[test]
    fn relations_one_to_many() {
        stage_relations(
            "root images: Image; root messages: Message; \
             struct Image { uploaded: Instant } struct Message { text: String } \
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
                    Field::BackRef {
                        name: ref n,
                        target,
                        is_list: true,
                        optional: false,
                    } if n == "messages" && target == message
                ));
                assert!(matches!(
                    b.structs[message.index()].fields[1],
                    Field::BackRef {
                        name: ref n,
                        target,
                        is_list: false,
                        optional: true,
                    } if n == "image" && target == image
                ));
            },
        );
    }

    #[test]
    fn relations_many_to_many() {
        stage_relations(
            "root roles: Role; root users: User; struct Role { name: String } \
             struct User { name: String } \
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
                    Field::BackRef {
                        name: ref n,
                        is_list: true,
                        ..
                    } if n == "users"
                ));
                assert!(matches!(
                    b.structs[user.index()].fields[1],
                    Field::BackRef {
                        name: ref n,
                        is_list: true,
                        ..
                    } if n == "roles"
                ));
            },
        );
    }

    #[test]
    fn forward_ref_missing() {
        stage_relations(
            "root users: User; struct User {} struct Order {} \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0013"]);
                assert!(b.relations.is_empty());
                let order = *b.struct_index.get("Order").unwrap();
                assert!(b.structs[order.index()].fields.is_empty());
            },
        );
    }

    #[test]
    fn forward_ref_type_mismatch() {
        stage_relations(
            "root users: User; struct User { orders: Order } struct Order {} \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0014"]);
                assert_eq!(b.relations.len(), 1);
            },
        );
    }

    #[test]
    fn forward_ref_target_mismatch() {
        stage_relations(
            "root users: User; root roles: Role; struct User { orders: List<Role> } \
             struct Order {} struct Role {} \
             rel Order.user <<-> User.orders (orders.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0016"]);
                assert_eq!(b.relations.len(), 1);
            },
        );
    }

    #[test]
    fn backref_in_struct() {
        stage_relations(
            "root profiles: Profile; root users: User; \
             struct Profile { user: String, bio: String } struct User { profile: Profile } \
             rel Profile.user <-> User.profile (profiles.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0015"]);
                assert!(b.relations.is_empty());
                let profile = *b.struct_index.get("Profile").unwrap();
                assert_eq!(b.structs[profile.index()].fields.len(), 2);
            },
        );
    }

    #[test]
    fn duplicate_backref_name() {
        stage_relations(
            "root users: User; root orders: Order; \
             struct User { a: List<Order>, b: List<Order> } struct Order {} \
             rel Order.user <<-> User.a (orders.user_id -> users.id); \
             rel Order.user <<-> User.b (orders.user_id2 -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0018"]);
                assert_eq!(b.relations.len(), 1);
                let order = *b.struct_index.get("Order").unwrap();
                assert_eq!(b.structs[order.index()].fields.len(), 1);
            },
        );
    }

    #[test]
    fn column_mismatch_via_on_owning() {
        stage_relations(
            "root users: User; struct User { orders: List<Order> } struct Order {} \
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
            "root users: User; root roles: Role; struct User {} struct Role {} \
             rel Role.users <<->> User.roles (roles.user_id -> users.id);",
            |b| {
                assert_eq!(codes(&b.diags), vec!["SV0017"]);
                assert!(b.relations.is_empty());
            },
        );
    }
}
