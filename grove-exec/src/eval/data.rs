mod fetch;
mod field;
mod owning;
mod prefetch;
mod projection;
mod refs;

pub(super) use fetch::exec_root;
pub(super) use field::{exec_field, resolve_base_link};
pub(super) use owning::{resolve_ident_value, resolve_owning_values};
pub(super) use prefetch::{collect_owning_refs_in, owning_refs_in, prefetch_owning};
pub(super) use projection::resolve_projection_refs;

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use grove_schema::validated::{FieldId, RelationId, StructId, ValidatedSchema};

    use crate::error::ExecError;
    use crate::eval::{ExecOptions, harness::*};
    use crate::sqlite::SqliteBackend;
    use crate::storage::{FetchConstraints, Row, Storage};
    use crate::testdata;
    use crate::value::{CollectionOrigin, Key, RowOrigin, Val};

    fn row_origins(value: &Val) -> Vec<RowOrigin> {
        let Val::List(items) = value else {
            panic!("expected list, got {value}");
        };
        items
            .iter()
            .map(|item| *item.row_origin().expect("row origin"))
            .collect()
    }

    #[test]
    fn root_operations_pushdown_parity() {
        assert_eq!(
            run_seeded("users.sort_desc_over(name).take(2).name"),
            Ok(Val::list(vec![str("Eli"), str("Dan")]))
        );
        assert_eq!(
            run_seeded("users.skip(1).take(2).name"),
            Ok(Val::list(vec![str("Bob"), str("Cyd")]))
        );
        assert_eq!(
            run_seeded("users.take(2).sort_desc_over(name).name"),
            Ok(Val::list(vec![str("Bob"), str("Ada")]))
        );
        assert_eq!(
            run_seeded("users.sort_over(name).take(10).skip(2).take(2).name"),
            Ok(Val::list(vec![str("Cyd"), str("Dan")]))
        );
        assert_eq!(
            run_seeded("users[id > 2][orders.len() > 1].map($.name)"),
            Ok(Val::list(Vec::new()))
        );
        assert_eq!(run_seeded("users.first()?.name"), Ok(some(str("Ada"))));
        assert_eq!(run_seeded("users.nth(-1)?.name"), Ok(some(str("Eli"))));
        assert_eq!(
            run_seeded("users.take(10).nth(3)?.name"),
            Ok(some(str("Dan")))
        );
        assert_eq!(
            run_seeded("users.skip(1).nth(2)?.name"),
            Ok(some(str("Dan")))
        );
    }

    #[test]
    fn bytes_root_pushdown_parity() {
        assert_eq!(
            run_seeded(r#"blobs[data == hex"0001"].id"#),
            Ok(Val::list(vec![int(1)]))
        );
        assert_eq!(
            run_seeded(r#"blobs[data in (hex"0001", hex"00ff")].id"#),
            Ok(Val::list(vec![int(1), int(3)]))
        );
        assert_eq!(
            run_seeded("blobs.sort_over(data).id"),
            Ok(Val::list(vec![int(2), int(1), int(3)]))
        );
        assert_eq!(
            run_seeded(r#"blobs.first().unwrap().maybe"#),
            Ok(some(bytes(&[0xff])))
        );
        assert_eq!(
            run_seeded("blobs.first().unwrap().chunks"),
            Ok(Val::list(vec![bytes(&[0x01]), bytes(&[0x01, 0x00])]))
        );
        assert_eq!(
            run_seeded("blobs[data.len() > 1].id"),
            Ok(Val::list(vec![int(1), int(3)]))
        );
        assert_eq!(
            run_seeded(r#"blobs[("x" as Bytes) == data].id"#),
            Ok(Val::list(Vec::new()))
        );
    }

    #[test]
    fn root_collection_and_row_origins() {
        let user = testdata::struct_id(&testdata::schema(), "User");
        let Ok(Val::List(users)) = run_seeded("users") else {
            panic!("expected users list");
        };
        assert_eq!(
            users.origin,
            Some(CollectionOrigin::Root {
                struct_id: user,
                root_idx: 0,
            })
        );
        assert_eq!(users.len(), 5);
        for user_row in users.iter() {
            let origin = user_row.row_origin().expect("root row origin");
            assert_eq!(origin.struct_id, user);
            assert!(origin.key.0 > 0);
        }
    }

    #[test]
    fn related_collection_and_row_origins() {
        let schema = testdata::schema();
        let user = testdata::struct_id(&schema, "User");
        let order = testdata::struct_id(&schema, "Order");
        let orders_field = testdata::field_id(&schema, user, "orders");

        let Ok(user_value) = run_seeded("users.first().unwrap()") else {
            panic!("expected first user");
        };
        let user_origin = *user_value.row_origin().expect("user row origin");

        let Ok(Val::List(orders)) = run_seeded("users.first().unwrap().orders") else {
            panic!("expected orders list");
        };
        assert_eq!(
            orders.origin,
            Some(CollectionOrigin::Ref {
                field: orders_field,
                parent: user_origin,
            })
        );
        assert_eq!(orders.len(), 2);
        for order_row in orders.iter() {
            let origin = order_row.row_origin().expect("order row origin");
            assert_eq!(origin.struct_id, order);
            assert!(origin.key.0 > 0);
        }
    }

    #[test]
    fn list_methods_origins_preserved() {
        let user = testdata::struct_id(&testdata::schema(), "User");
        let root_origin = CollectionOrigin::Root {
            struct_id: user,
            root_idx: 0,
        };

        for src in [
            "users.filter(active).sort_over(name).take(2).reverse()",
            "users.unique_over(id)",
            "users.manager.compact()",
        ] {
            // run_seeded() asserts equal origin annotations regardless of pushdown
            let value = run_seeded(src).expect("query");
            assert_eq!(value.collection_origin(), Some(&root_origin), "{src}");
            for origin in row_origins(&value) {
                assert_eq!(origin.struct_id, user, "{src}");
            }
        }
    }

    #[test]
    fn merged_and_constructed_values_origin_empty() {
        let schema = testdata::schema();
        let user = testdata::struct_id(&schema, "User");
        let order = testdata::struct_id(&schema, "Order");

        for src in ["users.map($)", "users.extend(users.take(0))"] {
            let value = run_seeded(src).expect("query");
            assert_eq!(value.collection_origin(), None, "{src}");
            for origin in row_origins(&value) {
                assert_eq!(origin.struct_id, user, "{src}");
            }
        }

        let value = run_seeded("users.orders.flatten()").expect("query");
        assert_eq!(value.collection_origin(), None);
        for origin in row_origins(&value) {
            assert_eq!(origin.struct_id, order);
        }

        let value = run_seeded("users { name }").expect("query");
        assert_eq!(value.collection_origin(), None);
        let Val::List(rows) = value else {
            panic!("expected projection list");
        };
        assert!(rows.iter().all(|row| row.row_origin().is_none()));

        let value = run_seeded("[{ a = 1 }]").expect("query");
        assert_eq!(value.collection_origin(), None);
        let Val::List(rows) = value else {
            panic!("expected array list");
        };
        assert!(rows.iter().all(|row| row.row_origin().is_none()));
    }

    struct RecordingBackend<'a, 's> {
        inner: &'a SqliteBackend<'s>,
        recording: Recording,
    }

    #[derive(Default)]
    pub(super) struct Recording {
        pub(super) fetches: RefCell<Vec<(StructId, Vec<FieldId>)>>,
        pub(super) constraints: RefCell<Vec<FetchConstraints>>,
        pub(super) related_fetches: RefCell<Vec<(RelationId, bool, Vec<FieldId>)>>,
        pub(super) related_constraints: RefCell<Vec<FetchConstraints>>,
        pub(super) related_parent_counts: RefCell<Vec<usize>>,
    }

    impl<'a, 's> RecordingBackend<'a, 's> {
        pub fn new(backend: &'a SqliteBackend<'s>) -> RecordingBackend<'a, 's> {
            RecordingBackend {
                inner: backend,
                recording: Default::default(),
            }
        }
    }

    impl Storage for RecordingBackend<'_, '_> {
        fn fetch(
            &self,
            struct_id: StructId,
            fields: &[FieldId],
            constraints: &FetchConstraints,
        ) -> Result<Vec<Row>, ExecError> {
            self.recording
                .fetches
                .borrow_mut()
                .push((struct_id, fields.to_vec()));
            self.recording
                .constraints
                .borrow_mut()
                .push(constraints.clone());
            self.inner.fetch(struct_id, fields, constraints)
        }

        fn fetch_related(
            &self,
            rel: RelationId,
            fetch_from_first: bool,
            parents: &[Key],
            fields: &[FieldId],
            constraints: &FetchConstraints,
        ) -> Result<Vec<Vec<Row>>, ExecError> {
            self.recording.related_fetches.borrow_mut().push((
                rel,
                fetch_from_first,
                fields.to_vec(),
            ));
            self.recording
                .related_constraints
                .borrow_mut()
                .push(constraints.clone());
            self.recording
                .related_parent_counts
                .borrow_mut()
                .push(parents.len());
            self.inner
                .fetch_related(rel, fetch_from_first, parents, fields, constraints)
        }
    }

    pub(super) fn run_seeded_recording(src: &str) -> Result<(Val, Recording), ExecError> {
        let schema = testdata::schema();
        let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory database");
        testdata::seed(&mut conn).expect("testdata initialization");
        let backend = SqliteBackend::new(conn, &schema).expect("backend");
        let recording = RecordingBackend::new(&backend);
        let result = run_with_storage_opts(src, &schema, &recording, ExecOptions::default())?;
        // run without recording for differential tests
        run_with_storage(src, &schema, &backend)?;
        Ok((result, recording.recording))
    }

    pub(super) fn record_field_value<'a>(value: &'a Val, name: &str) -> &'a Val {
        let Val::Record(fields) = value else {
            panic!("expected record, got {value:?}");
        };
        fields
            .iter()
            .find(|(field_name, _)| field_name.as_ref() == name)
            .map(|(_, value)| value)
            .expect("expected record field")
    }

    pub(super) fn related_fields_for<'a>(
        schema: &ValidatedSchema,
        related: &'a [(RelationId, bool, Vec<FieldId>)],
        field: FieldId,
    ) -> Option<&'a [FieldId]> {
        let relation = schema.relation_of_field(field).expect("relation for field");
        related
            .iter()
            .find(|(rel, _, _)| *rel == relation)
            .map(|(_, _, fields)| fields.as_slice())
    }
}
