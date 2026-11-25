use crate::codec;
use crate::storage::{CmpOp, FetchConstraints, PushExpr, Row, SortKey, Storage, StringOp};
use crate::testdata;
use crate::value::{Key, Scalar, Val};
use grove_schema::validated::{FieldId, RelationId, ScalarType, StructId, ValidatedSchema};
use rusqlite::types::{Value, ValueRef};

use super::*;
use chrono::{DateTime, TimeDelta};
use rust_decimal::dec;

#[test]
fn open_in_memory_constructor() {
    assert!(SqliteBackend::open_in_memory(&ValidatedSchema::default()).is_ok());
}

#[test]
fn decode_int_float_string() {
    assert_eq!(
        decode_scalar(ScalarType::Int, ValueRef::Integer(-7), false, true),
        Ok(Val::Scalar(Scalar::Int(-7)))
    );
    assert_eq!(
        decode_scalar(ScalarType::Float, ValueRef::Real(1.5), false, true),
        Ok(Val::Scalar(Scalar::Float(1.5)))
    );
    assert_eq!(
        decode_scalar(ScalarType::String, ValueRef::Text(b"hi"), false, true),
        Ok(Val::Scalar(Scalar::String("hi".to_string())))
    );
    assert!(decode_scalar(ScalarType::String, ValueRef::Text(b"\xFF"), false, true).is_err());
}

#[test]
fn decode_bool_strict() {
    assert_eq!(
        decode_scalar(ScalarType::Bool, ValueRef::Integer(0), false, true),
        Ok(Val::Scalar(Scalar::Bool(false)))
    );
    assert_eq!(
        decode_scalar(ScalarType::Bool, ValueRef::Integer(1), false, true),
        Ok(Val::Scalar(Scalar::Bool(true)))
    );
    assert!(decode_scalar(ScalarType::Bool, ValueRef::Integer(2), false, true).is_err());
    assert!(decode_scalar(ScalarType::Bool, ValueRef::Real(1.0), false, true).is_err());
}

#[test]
fn decode_null() {
    assert_eq!(
        decode_scalar(ScalarType::Int, ValueRef::Null, true, true),
        Ok(Val::None)
    );
    assert!(decode_scalar(ScalarType::Int, ValueRef::Null, false, true).is_err());
}

#[test]
fn decode_null_float_nan() {
    assert_eq!(
        decode_scalar(ScalarType::Float, ValueRef::Null, true, true),
        Ok(Val::None)
    );
    assert_eq!(
        decode_scalar(ScalarType::Float, ValueRef::Null, true, false),
        Ok(Val::None)
    );
    match decode_scalar(ScalarType::Float, ValueRef::Null, false, false) {
        Ok(Val::Scalar(Scalar::Float(x))) => assert!(x.is_nan()),
        other => panic!("expected NaN, got {other:?}"),
    }
    assert!(decode_scalar(ScalarType::Float, ValueRef::Null, false, true).is_err());
}

#[test]
fn fetch_null_float_checks() {
    use grove_schema::validated::{
        Column, ColumnId, Config, Field, FieldId, Struct, StructId, Table, TableId, ValueType,
    };
    fn schema_with_checks(float_checks: bool) -> ValidatedSchema {
        ValidatedSchema {
            config: Config {
                float_checks,
                ..Default::default()
            },
            tables: vec![Table {
                name: "t".to_string(),
                columns: vec![Column {
                    name: "x".to_string(),
                    ty: Some(ScalarType::Float),
                }],
            }],
            structs: vec![Struct {
                name: "S".to_string(),
                table: TableId::new(0),
                fields: vec![Field::Value {
                    name: "x".to_string(),
                    ty: ValueType::Scalar(ScalarType::Float),
                    columns: vec![ColumnId::new(TableId::new(0), 0)],
                }],
            }],
            ..Default::default()
        }
    }
    fn fetch_null_float(float_checks: bool) -> Result<Vec<Row>, ExecError> {
        let schema = schema_with_checks(float_checks);
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(x REAL); INSERT INTO t(x) VALUES (NULL);")
            .unwrap();
        let backend = SqliteBackend::new(conn, &schema).unwrap();
        let sid = StructId::new(0);
        backend.fetch(sid, &[FieldId::new(sid, 0)], &FetchConstraints::EMPTY)
    }
    match fetch_null_float(false).unwrap()[0].values[0] {
        Val::Scalar(Scalar::Float(x)) => assert!(x.is_nan()),
        ref other => panic!("expected NaN, got {other:?}"),
    }
    let err = fetch_null_float(true).unwrap_err();
    assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
}

#[test]
fn decode_bytes_blob() {
    let raw = [0x00, 0x01, 0xfe, 0xff];
    assert_eq!(
        decode_scalar(ScalarType::Bytes, ValueRef::Blob(&raw), false, true),
        Ok(Val::Scalar(Scalar::Bytes(raw.to_vec())))
    );
    assert_eq!(
        decode_scalar(ScalarType::Bytes, ValueRef::Blob(&[]), false, true),
        Ok(Val::Scalar(Scalar::Bytes(Vec::new())))
    );
    assert_eq!(
        scalar_value(&Scalar::Bytes(raw.to_vec())),
        Value::Blob(raw.to_vec())
    );
    assert!(decode_scalar(ScalarType::Bytes, ValueRef::Text(b"x"), false, true).is_err());
    assert_eq!(
        decode_scalar(ScalarType::Bytes, ValueRef::Null, true, true),
        Ok(Val::None)
    );
    assert!(decode_scalar(ScalarType::Bytes, ValueRef::Null, false, true).is_err());
}

#[test]
fn fetch_bytes_blob() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let blobs = testdata::struct_id(&schema, "Blob");
    let data_fid = schema.struct_field(blobs, "data").unwrap().0;
    let maybe_fid = schema.struct_field(blobs, "maybe").unwrap().0;
    let rows = backend
        .fetch(blobs, &[data_fid, maybe_fid], &FetchConstraints::EMPTY)
        .unwrap();

    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].values,
        vec![
            Val::Scalar(Scalar::Bytes(vec![0, 1])),
            Val::Some(Box::new(Val::Scalar(Scalar::Bytes(vec![255])))),
        ]
    );
    assert_eq!(
        rows[1].values,
        vec![Val::Scalar(Scalar::Bytes(Vec::new())), Val::None]
    );
}

#[test]
fn decode_storage_class_mismatch() {
    assert!(decode_scalar(ScalarType::String, ValueRef::Integer(1), false, true).is_err());
    assert!(decode_scalar(ScalarType::Int, ValueRef::Real(1.0), false, true).is_err());
    assert!(decode_scalar(ScalarType::Float, ValueRef::Integer(1), false, true).is_err());
    assert!(decode_scalar(ScalarType::Dec, ValueRef::Text(b"1.5"), false, true).is_err());
    assert!(decode_scalar(ScalarType::Instant, ValueRef::Integer(0), false, true).is_err());
}

#[test]
fn decode_blobs_round_trip() {
    let d = dec!(30.00);
    assert_eq!(
        decode_scalar(
            ScalarType::Dec,
            ValueRef::Blob(&codec::encode_dec(d)),
            false,
            true
        ),
        Ok(Val::Scalar(Scalar::Dec(d.normalize())))
    );
    let t = DateTime::from_timestamp(1_700_000_000, 500).unwrap();
    assert_eq!(
        decode_scalar(
            ScalarType::Instant,
            ValueRef::Blob(&codec::encode_instant(t)),
            false,
            true
        ),
        Ok(Val::Scalar(Scalar::Instant(t)))
    );
    let span = TimeDelta::milliseconds(-1500);
    match decode_scalar(
        ScalarType::Duration,
        ValueRef::Blob(&codec::encode_duration(span)),
        false,
        true,
    ) {
        Ok(Val::Scalar(Scalar::Duration(back))) => {
            assert_eq!(back.num_seconds(), span.num_seconds());
            assert_eq!(back.subsec_nanos(), span.subsec_nanos());
        }
        other => panic!("unexpected decode result: {other:?}"),
    }
}

fn test_backend(schema: &ValidatedSchema) -> SqliteBackend<'_> {
    let mut conn = Connection::open_in_memory().unwrap();
    testdata::seed(&mut conn).unwrap();
    SqliteBackend::new(conn, schema).unwrap()
}

fn user_struct(schema: &ValidatedSchema) -> StructId {
    testdata::struct_id(schema, "User")
}

fn user_field(schema: &ValidatedSchema, sid: StructId, name: &str) -> FieldId {
    testdata::field_id(schema, sid, name)
}

fn some(v: Val) -> Val {
    Val::Some(Box::new(v))
}

fn int(n: i64) -> Val {
    Val::Scalar(Scalar::Int(n))
}

#[test]
fn fetch_scalars_positional() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    let rows = backend
        .fetch(
            sid,
            &[
                user_field(&schema, sid, "name"),
                user_field(&schema, sid, "id"),
                user_field(&schema, sid, "balance"),
                user_field(&schema, sid, "created"),
                user_field(&schema, sid, "elapsed"),
                user_field(&schema, sid, "active"),
            ],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0].rowid, Key(1));
    assert_eq!(
        rows[0].values,
        vec![
            Val::Scalar(Scalar::String("Ada".to_string())),
            int(1),
            Val::Scalar(Scalar::Dec(dec!(30))),
            Val::Scalar(Scalar::Instant(
                DateTime::from_timestamp(1_700_000_000, 0).unwrap()
            )),
            Val::Scalar(Scalar::Duration(TimeDelta::seconds(90))),
            Val::Scalar(Scalar::Bool(true)),
        ]
    );
    assert_eq!(
        rows[2].values,
        vec![
            Val::Scalar(Scalar::String("Cyd".to_string())),
            int(3),
            Val::Scalar(Scalar::Dec(dec!(-5.25))),
            Val::Scalar(Scalar::Instant(
                DateTime::from_timestamp(-1, 500_000_000).unwrap()
            )),
            Val::Scalar(Scalar::Duration(TimeDelta::milliseconds(-1500))),
            Val::Scalar(Scalar::Bool(false)),
        ]
    );
}

#[test]
fn fetch_optional_and_tuples() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    let rows = backend
        .fetch(
            sid,
            &[
                user_field(&schema, sid, "id"),
                user_field(&schema, sid, "nickname"),
                user_field(&schema, sid, "pos"),
                user_field(&schema, sid, "home"),
            ],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows[0].values,
        vec![
            int(1),
            some(Val::Scalar(Scalar::String("A".to_string()))),
            Val::Tuple(vec![
                Val::Scalar(Scalar::Float(1.5)),
                Val::Scalar(Scalar::Float(-2.5)),
            ]),
            some(Val::Tuple(vec![
                Val::Scalar(Scalar::Float(3.0)),
                Val::Scalar(Scalar::Float(4.0)),
            ])),
        ]
    );
    assert_eq!(
        rows[1].values,
        vec![
            int(2),
            Val::None,
            Val::Tuple(vec![
                Val::Scalar(Scalar::Float(0.0)),
                Val::Scalar(Scalar::Float(0.0)),
            ]),
            Val::None,
        ]
    );
}

#[test]
fn fetch_partial_null_tuple_error() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    backend
        .conn
        .execute("UPDATE users SET hx = 3.0 WHERE id = 3", [])
        .unwrap();
    let sid = user_struct(&schema);
    let err = backend
        .fetch(
            sid,
            &[user_field(&schema, sid, "home")],
            &FetchConstraints::EMPTY,
        )
        .unwrap_err();
    assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
    assert!(err.to_string().contains("User.home"), "{err}");
}

#[test]
fn fetch_empty_table_empty_fields() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    let rows = backend.fetch(sid, &[], &FetchConstraints::EMPTY).unwrap();
    assert_eq!(rows.len(), 5);
    assert!(rows[0].values.is_empty());
    assert_eq!(rows[4].rowid, Key(5));
    backend.conn.execute("DELETE FROM users", []).unwrap();
    let rows = backend
        .fetch(
            sid,
            &[user_field(&schema, sid, "id")],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn fetch_reads_rowid_order() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    backend
        .conn
        .execute("DELETE FROM users WHERE rowid = 2", [])
        .unwrap();
    backend
        .conn
        .execute("INSERT INTO users(id) VALUES (9)", [])
        .unwrap();
    let sid = user_struct(&schema);
    let rows = backend
        .fetch(
            sid,
            &[user_field(&schema, sid, "id")],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(
        rows.iter().map(|r| r.rowid).collect::<Vec<_>>(),
        vec![Key(1), Key(3), Key(4), Key(5), Key(6)]
    );
    assert_eq!(
        rows.iter().map(|r| r.values.clone()).collect::<Vec<_>>(),
        vec![
            vec![int(1)],
            vec![int(3)],
            vec![int(4)],
            vec![int(5)],
            vec![int(9)],
        ]
    );
}

#[test]
fn fetch_arrays_grouped_by_link_in_side_rowid_order() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    let rows = backend
        .fetch(
            sid,
            &[
                user_field(&schema, sid, "id"),
                user_field(&schema, sid, "tags"),
                user_field(&schema, sid, "scores"),
                user_field(&schema, sid, "lucky"),
            ],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(rows.len(), 5);
    let str_list = |ss: &[&str]| {
        Val::list(
            ss.iter()
                .map(|s| Val::Scalar(Scalar::String(s.to_string())))
                .collect(),
        )
    };
    assert_eq!(
        rows[0].values,
        vec![
            int(1),
            str_list(&["a1", "a2"]),
            Val::list(vec![
                Val::Tuple(vec![int(1), Val::Scalar(Scalar::String("one".to_string()))]),
                Val::Tuple(vec![int(2), Val::Scalar(Scalar::String("two".to_string()))]),
            ]),
            Val::list(vec![some(int(5)), Val::None]),
        ]
    );
    assert_eq!(
        rows[1].values,
        vec![
            int(2),
            str_list(&["b2", "b1"]),
            Val::list(vec![]),
            Val::list(vec![]),
        ]
    );
    assert_eq!(
        rows[2].values,
        vec![
            int(3),
            Val::list(vec![]),
            Val::list(vec![]),
            Val::list(vec![])
        ]
    );
    assert_eq!(
        rows[3].values,
        vec![
            int(4),
            Val::list(vec![]),
            Val::list(vec![Val::Tuple(vec![
                int(3),
                Val::Scalar(Scalar::String("three".to_string()))
            ])]),
            Val::list(vec![]),
        ]
    );
    assert_eq!(
        rows[4].values,
        vec![
            int(5),
            str_list(&["a1", "a2"]),
            Val::list(vec![]),
            Val::list(vec![]),
        ]
    );
}

#[test]
fn fetch_ref_field_error() {
    use grove_schema::validated::{Field, Struct, Table, TableId};
    let schema = ValidatedSchema {
        tables: vec![Table {
            name: "t".to_string(),
            columns: vec![],
        }],
        structs: vec![Struct {
            name: "S".to_string(),
            table: TableId::new(0),
            fields: vec![Field::Ref {
                name: "r".to_string(),
                target: StructId::new(0),
                optional: false,
                is_list: false,
                owning: false,
            }],
        }],
        ..Default::default()
    };
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t(x INTEGER)").unwrap();
    let backend = SqliteBackend::new(conn, &schema).unwrap();
    let err = backend
        .fetch(
            StructId::new(0),
            &[FieldId::new(StructId::new(0), 0)],
            &FetchConstraints::EMPTY,
        )
        .unwrap_err();
    assert!(matches!(err, ExecError::Internal { .. }), "{err:?}");
    assert!(err.to_string().contains("Ref field `S.r`"), "{err}");
}

#[test]
fn fetch_mismatched_types() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    let id = user_field(&schema, sid, "id");
    let name = user_field(&schema, sid, "name");
    let active = user_field(&schema, sid, "active");
    let balance = user_field(&schema, sid, "balance");
    for (setup, field, label) in [
        (
            "INSERT INTO users(id, name) VALUES (1, x'313233')",
            name,
            "User.name",
        ),
        (
            "INSERT INTO users(id, name) VALUES (1.5, 'x')",
            id,
            "User.id",
        ),
        (
            "INSERT INTO users(id, name, active) VALUES (1, 'x', 2)",
            active,
            "User.active",
        ),
        (
            "INSERT INTO users(id, name, balance) VALUES (1, 'x', x'6a756e6b')",
            balance,
            "User.balance",
        ),
    ] {
        backend.conn.execute_batch("DELETE FROM users").unwrap();
        backend.conn.execute_batch(setup).unwrap();
        let err = backend
            .fetch(sid, &[field], &FetchConstraints::EMPTY)
            .unwrap_err();
        assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
        assert!(err.to_string().contains(label), "{err}");
    }
}

#[test]
fn fetch_bad_ids_error() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    assert!(matches!(
        backend
            .fetch(StructId::new(99), &[], &FetchConstraints::EMPTY)
            .unwrap_err(),
        ExecError::Internal { .. }
    ));
    assert!(matches!(
        backend
            .fetch(
                sid,
                &[FieldId::new(StructId::new(7), 0)],
                &FetchConstraints::EMPTY
            )
            .unwrap_err(),
        ExecError::Internal { .. }
    ));
}

#[test]
fn quoted_identifiers_dont_degrade() {
    let schema = ValidatedSchema::default();
    let backend = SqliteBackend::open_in_memory(&schema).unwrap();
    let err = backend
        .conn
        .prepare("SELECT \"no_such_column_xyz\"")
        .unwrap_err();
    assert!(err.to_string().contains("no such column"), "{err}");
}

fn verify_backend(tables: &[(&str, &[&str])], ddl: &str) -> Result<(), ExecError> {
    use grove_schema::validated::{Column, Table};
    let schema = ValidatedSchema {
        tables: tables
            .iter()
            .map(|(name, cols)| Table {
                name: name.to_string(),
                columns: cols
                    .iter()
                    .map(|c| Column {
                        name: c.to_string(),
                        ty: None,
                    })
                    .collect(),
            })
            .collect(),
        ..Default::default()
    };
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(ddl).unwrap();
    SqliteBackend::new(conn, &schema).map(|_| ())
}

#[test]
fn verify_missing_table() {
    let err = verify_backend(&[("t", &["a"])], "CREATE TABLE other(x);").unwrap_err();
    assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
    assert!(err.to_string().contains("unknown table `t`"), "{err}");
}

#[test]
fn verify_view_instead_of_table() {
    let err = verify_backend(
        &[("t", &["a"])],
        "CREATE TABLE base(x); CREATE VIEW t AS SELECT x AS a FROM base;",
    )
    .unwrap_err();
    assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
    assert!(err.to_string().contains("is a view"), "{err}");
}

#[test]
fn verify_without_rowid_table() {
    let err = verify_backend(
        &[("t", &["a"])],
        "CREATE TABLE t(a INTEGER PRIMARY KEY) WITHOUT ROWID;",
    )
    .unwrap_err();
    assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
    assert!(err.to_string().contains("WITHOUT ROWID"), "{err}");
}

#[test]
fn verify_missing_column() {
    let err = verify_backend(&[("t", &["a", "b"])], "CREATE TABLE t(a INTEGER);").unwrap_err();
    assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
    assert!(err.to_string().contains("missing column `b`"), "{err}");
}

#[test]
fn verify_ignores_unknown_and_affinity() {
    assert!(
        verify_backend(
            &[("t", &["a"])],
            "CREATE TABLE t(a TEXT, extra BLOB); CREATE TABLE other(x);"
        )
        .is_ok()
    );
}

#[test]
fn verify_case_insensitive_names() {
    assert!(verify_backend(&[("Users", &["A"])], "CREATE TABLE users(a INTEGER);").is_ok());
}

#[test]
fn verify_rowid_shadow_column() {
    let err =
        verify_backend(&[("t", &["a"])], "CREATE TABLE t(a INTEGER, _rowid_ TEXT);").unwrap_err();
    assert!(matches!(err, ExecError::Storage { .. }), "{err:?}");
    assert!(
        err.to_string().contains("shadowing the rowid alias"),
        "{err}"
    );
}

#[test]
fn decode_node_nested_tuples() {
    use grove_schema::validated::ValueType;
    let ty = ValueType::Tuple(vec![
        ValueType::Scalar(ScalarType::Int),
        ValueType::Tuple(vec![
            ValueType::Scalar(ScalarType::String),
            ValueType::Scalar(ScalarType::Bool),
        ]),
    ]);
    let vals = [
        ValueRef::Integer(1),
        ValueRef::Text(b"a"),
        ValueRef::Integer(1),
    ];
    let (v, used) = decode_node(&ty, &vals, true, &ValidatedSchema::default()).unwrap();
    assert_eq!(used, 3);
    assert_eq!(
        v,
        Val::Tuple(vec![
            int(1),
            Val::Tuple(vec![
                Val::Scalar(Scalar::String("a".to_string())),
                Val::Scalar(Scalar::Bool(true)),
            ]),
        ])
    );
}

#[test]
fn fetch_nested_via_array() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    let rows = backend
        .fetch(
            sid,
            &[user_field(&schema, sid, "matrix")],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows[0].values,
        vec![Val::list(vec![
            Val::list(vec![int(1), int(2)]),
            Val::list(vec![]),
        ])]
    );
    for row in &rows[1..] {
        assert_eq!(row.values, vec![Val::list(vec![])]);
    }
}

fn float(n: f64) -> Val {
    Val::Scalar(Scalar::Float(n))
}

fn bool(n: bool) -> Val {
    Val::Scalar(Scalar::Bool(n))
}

#[test]
fn fetch_deep_branched_via_array() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    let rows = backend
        .fetch(
            sid,
            &[
                user_field(&schema, sid, "id"),
                user_field(&schema, sid, "deep_branches"),
            ],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows[0].values,
        vec![
            int(1),
            Val::list(vec![Val::Tuple(vec![
                Val::list(vec![Val::list(vec![int(1), int(2)]), Val::list(vec![])]),
                Val::list(vec![float(2.5)]),
                bool(true),
            ])]),
        ]
    );
    for row in &rows[1..] {
        assert_eq!(row.values[1], Val::list(vec![]));
    }
}

fn str(s: &str) -> Val {
    Val::Scalar(Scalar::String(s.to_string()))
}

#[test]
fn fetch_tuple_embedded_array() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    let rows = backend
        .fetch(
            sid,
            &[
                user_field(&schema, sid, "id"),
                user_field(&schema, sid, "extra"),
            ],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows[0].values,
        vec![
            int(1),
            Val::Tuple(vec![int(1), Val::list(vec![str("x"), str("y")])]),
        ]
    );
    assert_eq!(
        rows[1].values,
        vec![int(2), Val::Tuple(vec![int(3), Val::list(vec![])])]
    );
    assert_eq!(
        rows[2].values,
        vec![int(3), Val::Tuple(vec![int(4), Val::list(vec![])])]
    );
    assert_eq!(
        rows[3].values,
        vec![int(4), Val::Tuple(vec![int(2), Val::list(vec![str("z")])])]
    );
    assert_eq!(
        rows[4].values,
        vec![int(5), Val::Tuple(vec![int(5), Val::list(vec![])])]
    );
}

#[test]
fn fetch_optional_tuple_embedded_array() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    let rows = backend
        .fetch(
            sid,
            &[
                user_field(&schema, sid, "id"),
                user_field(&schema, sid, "opt_extra"),
            ],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows[0].values,
        vec![
            int(1),
            some(Val::Tuple(vec![int(5), Val::list(vec![str("p")])])),
        ]
    );
    assert_eq!(rows[1].values, vec![int(2), Val::None]);
    assert_eq!(rows[2].values, vec![int(3), Val::None]);
    assert_eq!(
        rows[3].values,
        vec![int(4), some(Val::Tuple(vec![int(7), Val::list(vec![])]))]
    );
    assert_eq!(rows[4].values, vec![int(5), Val::None]);
}

fn rel_named(
    schema: &ValidatedSchema,
    a_struct: &str,
    a_field: &str,
    b_struct: &str,
    b_field: &str,
) -> RelationId {
    let asid = testdata::struct_id(schema, a_struct);
    let bsid = testdata::struct_id(schema, b_struct);
    testdata::relation_id(
        schema,
        testdata::field_id(schema, asid, a_field),
        testdata::field_id(schema, bsid, b_field),
    )
}

fn firsts(groups: &[Vec<Row>]) -> Vec<Vec<Val>> {
    groups
        .iter()
        .map(|g| g.iter().map(|row| row.values[0].clone()).collect())
        .collect()
}

#[test]
fn fetch_related_one_to_one() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let users = testdata::struct_id(&schema, "User");
    let profiles = testdata::struct_id(&schema, "Profile");
    let rel = rel_named(&schema, "Profile", "user", "User", "profile");
    let bio = testdata::field_id(&schema, profiles, "bio");
    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(1), Key(2), Key(3)],
            &[bio],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![str("AdaBio")], vec![], vec![str("CydBio")]]
    );
    let name = testdata::field_id(&schema, users, "name");
    let groups = backend
        .fetch_related(
            rel,
            true,
            &[Key(1), Key(2)],
            &[name],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(firsts(&groups), vec![vec![str("Ada")], vec![str("Cyd")]]);
}

#[test]
fn fetch_related_many_to_one() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let users = testdata::struct_id(&schema, "User");
    let orders = testdata::struct_id(&schema, "Order");
    let rel = rel_named(&schema, "Order", "user", "User", "orders");
    let name = testdata::field_id(&schema, users, "name");
    let groups = backend
        .fetch_related(
            rel,
            true,
            &[Key(1), Key(2), Key(3), Key(4)],
            &[name],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![str("Ada")], vec![str("Ada")], vec![], vec![str("Bob")]]
    );
    let amount = testdata::field_id(&schema, orders, "amount");
    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(1), Key(2), Key(3)],
            &[amount],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![int(100), int(200)], vec![int(400)], vec![]]
    );
}

#[test]
fn fetch_related_constraints_filter_sort() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
    let rel = rel_named(&schema, "Order", "user", "User", "orders");
    let constraints = FetchConstraints {
        filters: vec![push_cmp(
            CmpOp::Gt,
            target_field(&schema, "Order", "amount"),
            PushExpr::Literal(Scalar::Int(150)),
        )],
        sort: vec![SortKey {
            field: amount,
            desc: true,
        }],
        tie_desc: false,
        offset: 0,
        limit: None,
    };
    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(1), Key(2), Key(3)],
            &[amount],
            &constraints,
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![int(200)], vec![int(400)], vec![]]
    );
}

#[test]
fn fetch_related_tie_desc() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
    let rel = rel_named(&schema, "Order", "user", "User", "orders");
    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(1), Key(2)],
            &[amount],
            &FetchConstraints {
                filters: vec![],
                sort: vec![],
                tie_desc: true,
                offset: 0,
                limit: None,
            },
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![int(200), int(100)], vec![int(400)]]
    );
}

#[test]
fn fetch_related_caps_per_parent() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
    let rel = rel_named(&schema, "Order", "user", "User", "orders");

    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(1), Key(2), Key(3)],
            &[amount],
            &FetchConstraints {
                filters: vec![],
                sort: vec![],
                tie_desc: false,
                offset: 0,
                limit: Some(1),
            },
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![int(100)], vec![int(400)], vec![]]
    );

    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(1), Key(2), Key(3)],
            &[amount],
            &FetchConstraints {
                filters: vec![],
                sort: vec![],
                tie_desc: false,
                offset: 1,
                limit: Some(1),
            },
        )
        .unwrap();
    assert_eq!(firsts(&groups), vec![vec![int(200)], vec![], vec![]]);
}

#[test]
fn fetch_related_non_owning_constraints() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let name = testdata::field_id(&schema, testdata::struct_id(&schema, "Role"), "name");
    let rel = rel_named(&schema, "Role", "users", "User", "roles");
    let constraints = FetchConstraints {
        filters: vec![push_cmp(
            CmpOp::Eq,
            target_field(&schema, "Role", "name"),
            PushExpr::Literal(Scalar::String("editor".to_string())),
        )],
        sort: vec![],
        tie_desc: false,
        offset: 0,
        limit: Some(1),
    };
    let groups = backend
        .fetch_related(rel, false, &[Key(1), Key(3), Key(2)], &[name], &constraints)
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![str("editor")], vec![str("editor")], vec![]]
    );
}

#[test]
fn fetch_related_offset_only() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
    let rel = rel_named(&schema, "Order", "user", "User", "orders");
    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(1), Key(2)],
            &[amount],
            &FetchConstraints {
                filters: vec![],
                sort: vec![SortKey {
                    field: amount,
                    desc: false,
                }],
                tie_desc: false,
                offset: 1,
                limit: None,
            },
        )
        .unwrap();
    assert_eq!(firsts(&groups), vec![vec![int(200)], vec![]]);
}

#[test]
fn sql_cache_fetch_statements() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);
    let fields = vec![user_field(&schema, sid, "name")];
    let constraints = FetchConstraints {
        filters: vec![push_cmp(CmpOp::Gt, push_field(&schema, "id"), push_int(2))],
        sort: vec![],
        tie_desc: false,
        offset: 0,
        limit: None,
    };

    backend.fetch(sid, &fields, &constraints).unwrap();
    assert_eq!(backend.sql_cache.borrow().len(), 1);
    backend.fetch(sid, &fields, &constraints).unwrap();
    assert_eq!(backend.sql_cache.borrow().len(), 1);

    let capped = FetchConstraints {
        offset: 1,
        ..constraints.clone()
    };
    backend.fetch(sid, &fields, &capped).unwrap();
    assert_eq!(backend.sql_cache.borrow().len(), 2);

    backend
        .fetch(sid, &[user_field(&schema, sid, "id")], &constraints)
        .unwrap();
    assert_eq!(backend.sql_cache.borrow().len(), 3);
}

#[test]
fn fetch_enum_filter_and_cache() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = testdata::struct_id(&schema, "Setting");
    let name = testdata::field_id(&schema, sid, "name");
    let priority = FetchConstraints {
        filters: vec![push_cmp(
            CmpOp::Eq,
            target_field(&schema, "Setting", "priority"),
            push_str("high"),
        )],
        sort: vec![],
        tie_desc: false,
        offset: 0,
        limit: None,
    };
    let rows = backend.fetch(sid, &[name], &priority).unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.values[0].clone())
            .collect::<Vec<_>>(),
        vec![str("alpha")]
    );
    assert_eq!(backend.sql_cache.borrow().len(), 1);
    backend.fetch(sid, &[name], &priority).unwrap();
    assert_eq!(backend.sql_cache.borrow().len(), 1);

    let level = FetchConstraints {
        filters: vec![PushExpr::In {
            lhs: Box::new(target_field(&schema, "Setting", "level")),
            items: vec![push_int(2), push_int(-1)],
        }],
        sort: vec![],
        tie_desc: false,
        offset: 0,
        limit: None,
    };
    let rows = backend.fetch(sid, &[name], &level).unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.values[0].clone())
            .collect::<Vec<_>>(),
        vec![str("alpha"), str("beta")]
    );
    assert_eq!(backend.sql_cache.borrow().len(), 2);
}

#[test]
fn sql_cache_fetch_related_statements() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let amount = testdata::field_id(&schema, testdata::struct_id(&schema, "Order"), "amount");
    let rel = rel_named(&schema, "Order", "user", "User", "orders");
    let constraints = FetchConstraints {
        filters: vec![push_cmp(
            CmpOp::Gt,
            target_field(&schema, "Order", "amount"),
            PushExpr::Literal(Scalar::Int(150)),
        )],
        sort: vec![SortKey {
            field: amount,
            desc: true,
        }],
        tie_desc: false,
        offset: 0,
        limit: Some(1),
    };

    backend
        .fetch_related(rel, false, &[Key(1), Key(2)], &[amount], &constraints)
        .unwrap();
    assert_eq!(backend.sql_cache.borrow().len(), 1);
    backend
        .fetch_related(rel, false, &[Key(1), Key(2)], &[amount], &constraints)
        .unwrap();
    assert_eq!(backend.sql_cache.borrow().len(), 1);

    let uncapped = FetchConstraints {
        limit: None,
        ..constraints.clone()
    };
    backend
        .fetch_related(rel, false, &[Key(1), Key(2)], &[amount], &uncapped)
        .unwrap();
    assert_eq!(backend.sql_cache.borrow().len(), 2);

    backend
        .fetch_related(rel, false, &[Key(1)], &[amount], &constraints)
        .unwrap();
    assert_eq!(backend.sql_cache.borrow().len(), 3);
}

#[test]
fn fetch_related_one_to_many() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let images = testdata::struct_id(&schema, "Image");
    let messages = testdata::struct_id(&schema, "Message");
    let rel = rel_named(&schema, "Image", "messages", "Message", "image");
    let body = testdata::field_id(&schema, messages, "body");
    let groups = backend
        .fetch_related(
            rel,
            true,
            &[Key(1), Key(2)],
            &[body],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![str("nice"), str("wow")], vec![str("meh")]]
    );
    let title = testdata::field_id(&schema, images, "title");
    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(1), Key(4)],
            &[title],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(firsts(&groups), vec![vec![str("Sunset")], vec![]]);
}

#[test]
fn fetch_related_many_to_many() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let users = testdata::struct_id(&schema, "User");
    let roles = testdata::struct_id(&schema, "Role");
    let rel = rel_named(&schema, "Role", "users", "User", "roles");
    let rname = testdata::field_id(&schema, roles, "name");
    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(1), Key(3), Key(2)],
            &[rname],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![
            vec![str("admin"), str("editor"), str("editor")],
            vec![str("editor")],
            vec![],
        ]
    );
    assert_eq!(
        groups[0].iter().map(|row| row.rowid).collect::<Vec<_>>(),
        vec![Key(1), Key(2), Key(2)]
    );
    let uname = testdata::field_id(&schema, users, "name");
    let groups = backend
        .fetch_related(
            rel,
            true,
            &[Key(2), Key(1)],
            &[uname],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![str("Ada"), str("Ada"), str("Cyd")], vec![str("Ada")]]
    );
    assert_eq!(
        groups[0].iter().map(|row| row.rowid).collect::<Vec<_>>(),
        vec![Key(1), Key(1), Key(3)]
    );
}

#[test]
fn fetch_related_self_reference() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let users = testdata::struct_id(&schema, "User");
    let rel = rel_named(&schema, "User", "manager", "User", "subordinates");
    let name = testdata::field_id(&schema, users, "name");
    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(2), Key(1)],
            &[name],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![str("Ada"), str("Cyd")], vec![str("Dan")]]
    );
    let groups = backend
        .fetch_related(
            rel,
            true,
            &[Key(1), Key(4), Key(2)],
            &[name],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(
        firsts(&groups),
        vec![vec![str("Bob")], vec![str("Ada")], vec![]]
    );
}

#[test]
fn fetch_related_with_array_fields() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let profiles = testdata::struct_id(&schema, "Profile");
    let rel = rel_named(&schema, "Profile", "user", "User", "profile");
    let groups = backend
        .fetch_related(
            rel,
            false,
            &[Key(1), Key(3)],
            &[
                testdata::field_id(&schema, profiles, "bio"),
                testdata::field_id(&schema, profiles, "notes"),
            ],
            &FetchConstraints::EMPTY,
        )
        .unwrap();
    assert_eq!(groups.len(), 2);
    assert_eq!(
        groups[0][0].values,
        vec![str("AdaBio"), Val::list(vec![str("a-note")])]
    );
    assert_eq!(groups[1][0].values, vec![str("CydBio"), Val::list(vec![])]);
}

#[test]
fn fetch_related_edge_cases() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let rel = rel_named(&schema, "Profile", "user", "User", "profile");
    // empty parents: no rows, fields arg irrelevant (never resolved)
    let groups = backend
        .fetch_related(rel, true, &[], &[], &FetchConstraints::EMPTY)
        .unwrap();
    assert!(groups.is_empty());
    let err = backend
        .fetch_related(
            RelationId::new(99),
            true,
            &[Key(1)],
            &[],
            &FetchConstraints::EMPTY,
        )
        .unwrap_err();
    assert!(matches!(err, ExecError::Internal { .. }), "{err:?}");
}

fn push_field(schema: &ValidatedSchema, name: &str) -> PushExpr {
    PushExpr::Field(user_field(schema, user_struct(schema), name))
}

fn target_field(schema: &ValidatedSchema, struct_name: &str, name: &str) -> PushExpr {
    PushExpr::Field(testdata::field_id(
        schema,
        testdata::struct_id(schema, struct_name),
        name,
    ))
}

fn push_int(value: i64) -> PushExpr {
    PushExpr::Literal(Scalar::Int(value))
}

fn push_str(value: &str) -> PushExpr {
    PushExpr::Literal(Scalar::String(value.to_string()))
}

fn push_cmp(op: CmpOp, lhs: PushExpr, rhs: PushExpr) -> PushExpr {
    PushExpr::Cmp {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn user_names(
    backend: &SqliteBackend<'_>,
    schema: &ValidatedSchema,
    filters: Vec<PushExpr>,
    sort: Vec<SortKey>,
    offset: u64,
    limit: Option<u64>,
) -> Vec<String> {
    user_names_with_tie(backend, schema, filters, sort, false, offset, limit)
}

fn user_names_with_tie(
    backend: &SqliteBackend<'_>,
    schema: &ValidatedSchema,
    filters: Vec<PushExpr>,
    sort: Vec<SortKey>,
    tie_desc: bool,
    offset: u64,
    limit: Option<u64>,
) -> Vec<String> {
    let sid = user_struct(schema);
    let rows = backend
        .fetch(
            sid,
            &[user_field(schema, sid, "name")],
            &FetchConstraints {
                filters,
                sort,
                tie_desc,
                offset,
                limit,
            },
        )
        .unwrap();
    rows.into_iter()
        .map(|row| match row.values.as_slice() {
            [Val::Scalar(Scalar::String(name))] => name.clone(),
            other => panic!("expected one name value, got {other:?}"),
        })
        .collect()
}

fn names(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[test]
fn fetch_filter_comparisons() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);

    let filters = vec![push_cmp(CmpOp::Gt, push_field(&schema, "id"), push_int(2))];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Cyd", "Dan", "Eli"])
    );

    let filters = vec![push_cmp(
        CmpOp::Eq,
        push_field(&schema, "name"),
        push_str("Bob"),
    )];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Bob"])
    );

    let filters = vec![push_cmp(CmpOp::Ne, push_field(&schema, "id"), push_int(1))];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Bob", "Cyd", "Dan", "Eli"])
    );

    let filters = vec![push_cmp(
        CmpOp::Eq,
        push_field(&schema, "active"),
        PushExpr::Literal(Scalar::Bool(false)),
    )];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Cyd", "Eli"])
    );

    let filters = vec![PushExpr::In {
        lhs: Box::new(push_field(&schema, "id")),
        items: vec![push_int(1), push_int(3), push_int(5)],
    }];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Ada", "Cyd", "Eli"])
    );

    let filters = vec![PushExpr::And(
        Box::new(push_field(&schema, "active")),
        Box::new(push_cmp(CmpOp::Gt, push_field(&schema, "id"), push_int(1))),
    )];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Bob", "Dan"])
    );

    let filters = vec![PushExpr::Or(
        Box::new(push_cmp(CmpOp::Eq, push_field(&schema, "id"), push_int(1))),
        Box::new(push_cmp(CmpOp::Eq, push_field(&schema, "id"), push_int(5))),
    )];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Ada", "Eli"])
    );

    let filters = vec![PushExpr::Not(Box::new(push_field(&schema, "active")))];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Cyd", "Eli"])
    );
}

#[test]
fn fetch_filter_string_operations() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);

    let string_op = |op: StringOp, needle: &str| {
        vec![PushExpr::StringOp {
            op,
            base: Box::new(push_field(&schema, "name")),
            needle: Box::new(push_str(needle)),
        }]
    };
    assert_eq!(
        user_names(
            &backend,
            &schema,
            string_op(StringOp::Contains, "a"),
            vec![],
            0,
            None
        ),
        names(&["Ada", "Dan"])
    );
    assert_eq!(
        user_names(
            &backend,
            &schema,
            string_op(StringOp::Contains, ""),
            vec![],
            0,
            None
        ),
        names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
    );

    assert_eq!(
        user_names(
            &backend,
            &schema,
            string_op(StringOp::StartsWith, "A"),
            vec![],
            0,
            None
        ),
        names(&["Ada"])
    );
    assert_eq!(
        user_names(
            &backend,
            &schema,
            string_op(StringOp::StartsWith, ""),
            vec![],
            0,
            None
        ),
        names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
    );

    assert_eq!(
        user_names(
            &backend,
            &schema,
            string_op(StringOp::EndsWith, "d"),
            vec![],
            0,
            None
        ),
        names(&["Cyd"])
    );
    assert_eq!(
        user_names(
            &backend,
            &schema,
            string_op(StringOp::EndsWith, ""),
            vec![],
            0,
            None
        ),
        names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
    );

    let filters = vec![push_cmp(
        CmpOp::Ge,
        PushExpr::StringLen(Box::new(push_field(&schema, "name"))),
        push_int(3),
    )];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Ada", "Bob", "Cyd", "Dan", "Eli"])
    );
}

#[test]
fn fetch_filter_temporal_and_decimal() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);

    let filters = vec![push_cmp(
        CmpOp::Gt,
        push_field(&schema, "balance"),
        PushExpr::Literal(Scalar::Dec(dec!(30.00))),
    )];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Dan"])
    );

    let filters = vec![push_cmp(
        CmpOp::Ge,
        push_field(&schema, "balance"),
        PushExpr::Literal(Scalar::Dec(dec!(30.00))),
    )];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Ada", "Dan"])
    );

    let filters = vec![push_cmp(
        CmpOp::Ge,
        push_field(&schema, "created"),
        PushExpr::Literal(Scalar::Instant(
            DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        )),
    )];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Ada", "Dan", "Eli"])
    );

    let filters = vec![push_cmp(
        CmpOp::Gt,
        push_field(&schema, "elapsed"),
        PushExpr::Literal(Scalar::Duration(TimeDelta::zero())),
    )];
    assert_eq!(
        user_names(&backend, &schema, filters, vec![], 0, None),
        names(&["Ada", "Dan", "Eli"])
    );
}

#[test]
fn fetch_bytes_filter_sort_and_cache() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = testdata::struct_id(&schema, "Blob");
    let id = testdata::field_id(&schema, sid, "id");
    let data = testdata::field_id(&schema, sid, "data");
    let maybe = testdata::field_id(&schema, sid, "maybe");
    let chunks = testdata::field_id(&schema, sid, "chunks");

    let constraints = FetchConstraints {
        filters: vec![push_cmp(
            CmpOp::Gt,
            target_field(&schema, "Blob", "data"),
            PushExpr::Literal(Scalar::Bytes(vec![0x00, 0x00])),
        )],
        sort: vec![SortKey {
            field: data,
            desc: false,
        }],
        tie_desc: false,
        offset: 0,
        limit: None,
    };
    let rows = backend.fetch(sid, &[id], &constraints).unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.values[0].clone())
            .collect::<Vec<_>>(),
        vec![int(1), int(3)]
    );
    assert_eq!(backend.sql_cache.borrow().len(), 1);
    backend.fetch(sid, &[id], &constraints).unwrap();
    assert_eq!(backend.sql_cache.borrow().len(), 1);

    let rows = backend
        .fetch(sid, &[maybe, chunks], &FetchConstraints::EMPTY)
        .unwrap();
    assert_eq!(
        rows[0].values,
        vec![
            some(Val::Scalar(Scalar::Bytes(vec![0xff]))),
            Val::list(vec![
                Val::Scalar(Scalar::Bytes(vec![0x01])),
                Val::Scalar(Scalar::Bytes(vec![0x01, 0x00])),
            ]),
        ]
    );
    assert_eq!(rows[1].values, vec![Val::None, Val::list(vec![])]);
    assert_eq!(
        rows[2].values,
        vec![
            some(Val::Scalar(Scalar::Bytes(vec![0x00]))),
            Val::list(vec![]),
        ]
    );
}

#[test]
fn fetch_sort_limit_offset() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);

    let sort = vec![SortKey {
        field: user_field(&schema, sid, "id"),
        desc: true,
    }];
    assert_eq!(
        user_names(&backend, &schema, vec![], sort, 0, None),
        names(&["Eli", "Dan", "Cyd", "Bob", "Ada"])
    );

    let sort = vec![
        SortKey {
            field: user_field(&schema, sid, "active"),
            desc: true,
        },
        SortKey {
            field: user_field(&schema, sid, "id"),
            desc: false,
        },
    ];
    assert_eq!(
        user_names(&backend, &schema, vec![], sort, 0, None),
        names(&["Ada", "Bob", "Dan", "Cyd", "Eli"])
    );

    let sort = vec![SortKey {
        field: user_field(&schema, sid, "active"),
        desc: false,
    }];
    assert_eq!(
        user_names(&backend, &schema, vec![], sort, 0, None),
        names(&["Cyd", "Eli", "Ada", "Bob", "Dan"])
    );

    assert_eq!(
        user_names(&backend, &schema, vec![], vec![], 0, Some(2)),
        names(&["Ada", "Bob"])
    );
    assert_eq!(
        user_names(&backend, &schema, vec![], vec![], 2, None),
        names(&["Cyd", "Dan", "Eli"])
    );
    assert_eq!(
        user_names(&backend, &schema, vec![], vec![], 3, Some(1)),
        names(&["Dan"])
    );
    assert_eq!(
        user_names(&backend, &schema, vec![], vec![], 0, Some(0)),
        Vec::<String>::new()
    );
    assert_eq!(
        user_names(&backend, &schema, vec![], vec![], 10, None),
        Vec::<String>::new()
    );

    let sort = vec![SortKey {
        field: user_field(&schema, sid, "name"),
        desc: true,
    }];
    assert_eq!(
        user_names(&backend, &schema, vec![], sort, 0, Some(2)),
        names(&["Eli", "Dan"])
    );

    assert_eq!(
        user_names_with_tie(&backend, &schema, vec![], vec![], true, 0, None),
        names(&["Eli", "Dan", "Cyd", "Bob", "Ada"])
    );

    let sort = vec![SortKey {
        field: user_field(&schema, sid, "active"),
        desc: true,
    }];
    assert_eq!(
        user_names_with_tie(&backend, &schema, vec![], sort, true, 0, None),
        names(&["Dan", "Bob", "Ada", "Eli", "Cyd"])
    );
}

#[test]
fn fetch_enums_optional_array_tuple() {
    let schema = testdata::schema();
    let sid = testdata::struct_id(&schema, "Setting");
    let priority = testdata::enum_id(&schema, "Priority");
    let level = testdata::enum_id(&schema, "Level");
    let fields = [
        testdata::field_id(&schema, sid, "name"),
        testdata::field_id(&schema, sid, "priority"),
        testdata::field_id(&schema, sid, "level"),
        testdata::field_id(&schema, sid, "fallback"),
        testdata::field_id(&schema, sid, "history"),
        testdata::field_id(&schema, sid, "pair"),
    ];
    let rows = test_backend(&schema)
        .fetch(sid, &fields, &FetchConstraints::EMPTY)
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].values,
        vec![
            str("alpha"),
            Val::Enum(priority, 2),
            Val::Enum(level, 2),
            some(Val::Enum(priority, 0)),
            Val::list(vec![Val::Enum(priority, 0), Val::Enum(priority, 2)]),
            Val::Tuple(vec![Val::Enum(priority, 2), Val::Enum(level, 2)]),
        ]
    );
    assert_eq!(
        rows[1].values,
        vec![
            str("beta"),
            Val::Enum(priority, 0),
            Val::Enum(level, 0),
            Val::None,
            Val::list(vec![Val::Enum(priority, 2)]),
            Val::Tuple(vec![Val::Enum(priority, 0), Val::Enum(level, 0)]),
        ]
    );
    assert_eq!(
        rows[2].values,
        vec![
            str("gamma"),
            Val::Enum(priority, 1),
            Val::Enum(level, 1),
            some(Val::Enum(priority, 1)),
            Val::list(vec![]),
            Val::Tuple(vec![Val::Enum(priority, 1), Val::Enum(level, 1)]),
        ]
    );
}

#[test]
fn fetch_invalid_enum_value() {
    let schema = testdata::schema();
    let sid = testdata::struct_id(&schema, "Setting");
    let priority = testdata::field_id(&schema, sid, "priority");

    let mut conn = Connection::open_in_memory().unwrap();
    testdata::seed(&mut conn).unwrap();
    conn.execute(
        "UPDATE settings SET priority = 'urgent' WHERE name = 'alpha'",
        [],
    )
    .unwrap();
    let backend = SqliteBackend::new(conn, &schema).unwrap();
    let err = backend
        .fetch(sid, &[priority], &FetchConstraints::EMPTY)
        .unwrap_err();
    match err {
        ExecError::Integrity { message } => {
            assert!(message.contains("Priority"), "{message}");
            assert!(message.contains("urgent"), "{message}");
        }
        other => panic!("expected integrity error, got {other:?}"),
    }
}

#[test]
fn fetch_enum_mismatched_storage_class() {
    let schema = testdata::schema();
    let sid = testdata::struct_id(&schema, "Setting");
    let level = testdata::field_id(&schema, sid, "level");

    let mut conn = Connection::open_in_memory().unwrap();
    testdata::seed(&mut conn).unwrap();
    conn.execute(
        "UPDATE settings SET level = 'high' WHERE name = 'alpha'",
        [],
    )
    .unwrap();
    let backend = SqliteBackend::new(conn, &schema).unwrap();
    let err = backend
        .fetch(sid, &[level], &FetchConstraints::EMPTY)
        .unwrap_err();
    assert!(matches!(err, ExecError::Integrity { .. }), "{err:?}");
    assert!(err.to_string().contains("expected Int"), "{err}");
}

#[test]
fn fetch_invalid_enum_array_value() {
    let schema = testdata::schema();
    let sid = testdata::struct_id(&schema, "Setting");
    let history = testdata::field_id(&schema, sid, "history");

    let mut conn = Connection::open_in_memory().unwrap();
    testdata::seed(&mut conn).unwrap();
    conn.execute(
        "UPDATE setting_history SET priority = 'urgent' WHERE setting_key = 700",
        [],
    )
    .unwrap();
    let backend = SqliteBackend::new(conn, &schema).unwrap();
    let err = backend
        .fetch(sid, &[history], &FetchConstraints::EMPTY)
        .unwrap_err();
    match err {
        ExecError::Integrity { message } => {
            assert!(message.contains("Priority"), "{message}");
            assert!(message.contains("urgent"), "{message}");
        }
        other => panic!("expected integrity error, got {other:?}"),
    }
}

#[test]
fn fetch_constraints_rejected_fields() {
    let schema = testdata::schema();
    let backend = test_backend(&schema);
    let sid = user_struct(&schema);

    let order = testdata::struct_id(&schema, "Order");
    let amount = testdata::field_id(&schema, order, "amount");
    let err = backend
        .fetch(
            sid,
            &[user_field(&schema, sid, "name")],
            &FetchConstraints {
                filters: vec![PushExpr::Field(amount)],
                sort: vec![],
                tie_desc: false,
                offset: 0,
                limit: None,
            },
        )
        .unwrap_err();
    assert!(matches!(err, ExecError::Internal { .. }), "{err:?}");

    let pos = user_field(&schema, sid, "pos");
    let err = backend
        .fetch(
            sid,
            &[user_field(&schema, sid, "name")],
            &FetchConstraints {
                filters: vec![PushExpr::Field(pos)],
                sort: vec![],
                tie_desc: false,
                offset: 0,
                limit: None,
            },
        )
        .unwrap_err();
    assert!(matches!(err, ExecError::Internal { .. }), "{err:?}");

    let err = backend
        .fetch(
            sid,
            &[user_field(&schema, sid, "name")],
            &FetchConstraints {
                filters: vec![],
                sort: vec![SortKey {
                    field: pos,
                    desc: false,
                }],
                tie_desc: false,
                offset: 0,
                limit: None,
            },
        )
        .unwrap_err();
    assert!(matches!(err, ExecError::Internal { .. }), "{err:?}");
}

fn write_schema() -> ValidatedSchema {
    grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root items: Item;
                struct Item {
                    id: Int,
                    name: String,
                    maybe: ?Int,
                }
                "#,
        )
        .0
        .expect("invalid write schema"),
    )
    .0
    .expect("invalid write schema")
}

fn write_item_fields(schema: &ValidatedSchema) -> (StructId, FieldId, FieldId, FieldId) {
    let item = schema.roots[0].struct_id;
    (
        item,
        schema.struct_field(item, "id").unwrap().0,
        schema.struct_field(item, "name").unwrap().0,
        schema.struct_field(item, "maybe").unwrap().0,
    )
}

fn write_test_backend(schema: &ValidatedSchema) -> Result<SqliteBackend<'_>, rusqlite::Error> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch("CREATE TABLE items(id INTEGER, name TEXT, maybe INTEGER)")?;
    Ok(SqliteBackend::new(conn, schema).expect("write test backend should pass verification"))
}

#[test]
fn write_txn_insert_commit_read() {
    let schema = write_schema();
    let backend = write_test_backend(&schema).unwrap();
    let (item, id, name, maybe) = write_item_fields(&schema);

    let mut txn = backend.begin_write().unwrap();
    let key = txn
        .insert_row(
            item,
            &[(id, int(1)), (name, str("alpha")), (maybe, Val::None)],
        )
        .unwrap();
    txn.commit().unwrap();

    let rows = backend
        .fetch(item, &[id, name, maybe], &FetchConstraints::EMPTY)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].rowid, key);
    assert_eq!(rows[0].values, vec![int(1), str("alpha"), Val::None]);
}

#[test]
fn write_txn_drop_rollback() {
    let schema = write_schema();
    let backend = write_test_backend(&schema).unwrap();
    let (item, id, name, maybe) = write_item_fields(&schema);

    let mut txn = backend.begin_write().unwrap();
    txn.insert_row(
        item,
        &[(id, int(1)), (name, str("alpha")), (maybe, Val::None)],
    )
    .unwrap();
    drop(txn);

    let rows = backend
        .fetch(item, &[id, name, maybe], &FetchConstraints::EMPTY)
        .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn write_txn_update_delete_batch_insert() {
    let schema = write_schema();
    let backend = write_test_backend(&schema).unwrap();
    let (item, id, name, maybe) = write_item_fields(&schema);

    let mut txn = backend.begin_write().unwrap();
    let keys = txn
        .insert_rows(
            item,
            &[
                vec![(id, int(1)), (name, str("alpha")), (maybe, Val::None)],
                vec![(id, int(2)), (name, str("beta")), (maybe, Val::None)],
            ],
        )
        .unwrap();
    assert_eq!(keys.len(), 2);

    txn.update_row(item, keys[0], &[(name, str("gamma"))])
        .unwrap();
    txn.commit().unwrap();

    let rows = backend
        .fetch(item, &[id, name, maybe], &FetchConstraints::EMPTY)
        .unwrap();
    assert_eq!(rows[0].values, vec![int(1), str("gamma"), Val::None]);
    assert_eq!(rows[1].values, vec![int(2), str("beta"), Val::None]);

    let mut txn = backend.begin_write().unwrap();
    txn.delete_rows(item, &keys).unwrap();
    txn.commit().unwrap();

    let rows = backend
        .fetch(item, &[id, name, maybe], &FetchConstraints::EMPTY)
        .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn write_txn_readonly_connection_rejected() {
    let schema = write_schema();
    let path = std::env::temp_dir().join(format!(
        "grove-write-txn-readonly-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE items(id INTEGER, name TEXT, maybe INTEGER)")
            .unwrap();
    }
    let conn =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let backend = SqliteBackend::new(conn, &schema).unwrap();
    assert!(matches!(
        backend.begin_write(),
        Err(ExecError::ReadOnly { .. })
    ));
    drop(backend);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_txn_delete_array_field_recursive() {
    let schema = grove_schema::validate(
        grove_schema::parse_schema(
            r#"
                root rows: Row;
                struct Row {
                    id: Int,
                    tags: List<String>@tags_ref via row_tags[row_key, tag],
                    nums: List<Int>@nums_ref via row_nums[row_key, num],
                    matrix: List<List<Int> via inner_nums[inner_key, val]>@matrix_ref
                        via outer_nums[outer_key, inner_key],
                    extra: (Int, List<String> via extra_tags[ekey, eval])@(eid, elink),
                }
                "#,
        )
        .0
        .unwrap(),
    )
    .0
    .unwrap();
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE rows(id INTEGER, tags_ref INTEGER, nums_ref INTEGER,
                 matrix_ref INTEGER, eid INTEGER, elink INTEGER);
             CREATE TABLE row_tags(row_key INTEGER, tag TEXT);
             CREATE TABLE row_nums(row_key INTEGER, num INTEGER);
             CREATE TABLE outer_nums(outer_key INTEGER, inner_key INTEGER);
             CREATE TABLE inner_nums(inner_key INTEGER, val INTEGER);
             CREATE TABLE extra_tags(ekey INTEGER, eval TEXT);
             INSERT INTO rows VALUES (1, 100, 110, 200, 7, 300);
             INSERT INTO row_tags VALUES (100, 'a'), (100, 'b');
             INSERT INTO row_nums VALUES (110, 1), (110, 2);
             INSERT INTO outer_nums VALUES (200, 20);
             INSERT INTO inner_nums VALUES (20, 1), (20, 2);
             INSERT INTO extra_tags VALUES (300, 'x');",
    )
    .unwrap();
    let backend = SqliteBackend::new(conn, &schema).unwrap();
    let row = schema.roots[0].struct_id;
    let tags = schema.struct_field(row, "tags").unwrap().0;
    let nums = schema.struct_field(row, "nums").unwrap().0;
    let matrix = schema.struct_field(row, "matrix").unwrap().0;
    let extra = schema.struct_field(row, "extra").unwrap().0;

    let mut txn = backend.begin_write().unwrap();
    for field in [tags, nums, matrix, extra] {
        txn.delete_array_field(field, &[Key(1)]).unwrap();
    }
    txn.commit().unwrap();

    for table in [
        "row_tags",
        "row_nums",
        "outer_nums",
        "inner_nums",
        "extra_tags",
    ] {
        let count: i64 = backend
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
}
