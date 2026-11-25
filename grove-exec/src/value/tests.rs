use std::cmp::Ordering;
use std::hash::{DefaultHasher, Hash, Hasher};

use chrono::{DateTime, TimeDelta};

use crate::eval::harness::{int, rec};
use crate::testdata;

use super::*;
use grove_schema::validated::StructId;
use rust_decimal::dec;

fn some(v: Val) -> Val {
    Val::Some(Box::new(v))
}

#[test]
fn scalar_display() {
    assert_eq!(Scalar::Int(-7).to_string(), "-7");
    assert_eq!(Scalar::Float(1.0).to_string(), "1.0f");
    assert_eq!(Scalar::Float(1.5).to_string(), "1.5f");
    assert_eq!(Scalar::Bool(true).to_string(), "true");
    assert_eq!(Scalar::String("a\"b".to_string()).to_string(), "\"a\\\"b\"");
    assert_eq!(Scalar::Dec(dec!(30.00)).to_string(), "30.00");
    assert_eq!(Scalar::Bytes(Vec::new()).to_string(), "hex\"\"");
    assert_eq!(
        Scalar::Bytes(vec![0x00, 0x01, 0xde, 0xff]).to_string(),
        "hex\"0001deff\""
    );
}

#[test]
fn scalar_bytes_compare_hash() {
    let span = Span::new(0, 0);
    let a = Scalar::Bytes(vec![0x01]);
    let b = Scalar::Bytes(vec![0x02]);

    assert_eq!(a.cmp(&b, span), Ok(Ordering::Less));
    assert_eq!(a.eq(&a), Some(true));
    assert_eq!(a.eq(&b), Some(false));
    assert_eq!(a.eq(&Scalar::String("01".to_string())), None);

    let hash = |value: &Scalar| {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    };
    assert_eq!(hash(&a), hash(&a.clone()));
    assert_ne!(hash(&a), hash(&Scalar::String("01".to_string())));
}

#[test]
fn option_display() {
    assert_eq!(Val::None.to_string(), "none");
    assert_eq!(some(Val::Scalar(Scalar::Int(3))).to_string(), "some(3)");
    assert_eq!(
        some(some(Val::Scalar(Scalar::Int(3)))).to_string(),
        "some(some(3))"
    );
}

#[test]
fn duration_display() {
    let s = |secs: i64| Scalar::Duration(TimeDelta::seconds(secs)).to_string();
    let ms = |millis: i64| Scalar::Duration(TimeDelta::milliseconds(millis)).to_string();
    let ns = |nanos: i64| Scalar::Duration(TimeDelta::nanoseconds(nanos)).to_string();
    assert_eq!(s(0), "0s");
    assert_eq!(s(90), "1m30s");
    assert_eq!(s(60 * 60), "1h");
    assert_eq!(s(60 * 61 + 1), "1h1m1s");
    assert_eq!(s(24 * 60 * 60), "1d");
    assert_eq!(s(25 * 60 * 60), "1d1h");
    assert_eq!(s(-365 * 24 * 60 * 60 - 60 * 60 - 2), "-365d1h2s");
    assert_eq!(s(-90), "-1m30s");
    assert_eq!(ms(500), "0.5s");
    assert_eq!(ms(1500), "1.5s");
    assert_eq!(ms(61_500), "1m1.5s");
    assert_eq!(ms(-1500), "-1.5s");
    assert_eq!(ns(-1), "-0.000000001s");
    assert_eq!(ns(1_234_567_890), "1.23456789s");
}

#[test]
fn instant_display() {
    let at = |nanos: u32| {
        Scalar::Instant(DateTime::from_timestamp(1_700_000_000, nanos).unwrap()).to_string()
    };
    assert_eq!(at(0), "2023-11-14T22:13:20Z");
    assert_eq!(at(123_000_000), "2023-11-14T22:13:20.123Z");
    assert_eq!(at(123_456_000), "2023-11-14T22:13:20.123456Z");
    assert_eq!(at(123_456_789), "2023-11-14T22:13:20.123456789Z");
    assert_eq!(at(1), "2023-11-14T22:13:20.000000001Z");
}

#[test]
fn record_eq_order_independent() {
    assert_eq!(
        rec(vec![("b", int(2)), ("a", int(1))]).eq(&rec(vec![("a", int(1)), ("b", int(2))])),
        Some(true)
    );
    assert_eq!(
        rec(vec![("b", int(2)), ("a", int(1))]).eq(&rec(vec![("a", int(9)), ("b", int(2))])),
        Some(false)
    );
    assert_eq!(
        rec(vec![("b", int(2)), ("a", int(1))]).eq(&rec(vec![("a", int(1)), ("c", int(2))])),
        None
    );
    assert_eq!(
        rec(vec![("a", int(1))]).eq(&rec(vec![("a", int(1)), ("b", int(2))])),
        None
    );
    assert_eq!(
        rec(vec![
            ("outer", rec(vec![("y", int(2)), ("x", int(1))])),
            ("z", int(3))
        ])
        .eq(&rec(vec![
            ("z", int(3)),
            ("outer", rec(vec![("x", int(1)), ("y", int(2))]))
        ])),
        Some(true)
    );
}

#[test]
fn optional_depth_helpers() {
    let value = some(some(int(1)));
    assert_eq!(unwrap_optional_depth(value, 2).unwrap(), (int(1), 2));
    assert_eq!(
        unwrap_optional_depth(some(Val::None), 2).unwrap(),
        (Val::None, 1)
    );
    assert_eq!(rewrap_optional(2, int(1)), some(some(int(1))));
    assert_eq!(rewrap_optional(1, Val::None), some(Val::None));
}

#[test]
fn enum_display_and_equality() {
    let high = Val::Enum(EnumId::new(0), 1);
    assert_eq!(high.to_string(), "enum(0:1)");
    assert_eq!(high.eq(&Val::Enum(EnumId::new(0), 1)), Some(true));
    assert_eq!(high.eq(&Val::Enum(EnumId::new(0), 0)), Some(false));
    assert_eq!(high.eq(&Val::Enum(EnumId::new(1), 1)), None);
    assert!(!high.contains_ref());
}

#[test]
fn contextual_enum_display() {
    let schema = testdata::schema();
    let priority = testdata::enum_id(&schema, "Priority");
    let value = Val::Enum(priority, 1);
    assert_eq!(value.to_string(), "enum(0:1)");
    assert_eq!(value.formatter(&schema).to_string(), "Priority::Medium");

    assert_eq!(
        Val::Enum(priority, 99).formatter(&schema).to_string(),
        "enum(0:99)"
    );
    assert_eq!(
        Val::Enum(EnumId::new(99), 0).formatter(&schema).to_string(),
        "enum(99:0)"
    );
}

#[test]
fn contextual_ref_display() {
    let schema = testdata::schema();
    let user = testdata::struct_id(&schema, "User");
    let profile = testdata::struct_id(&schema, "Profile");
    let key = Key(123);
    let link = |struct_id: StructId, name: &str| Val::Ref {
        field: testdata::field_id(&schema, struct_id, name),
        parent: key,
    };

    assert_eq!(
        link(user, "orders").formatter(&schema).to_string(),
        "List<Order>#123"
    );
    assert_eq!(
        link(user, "profile").formatter(&schema).to_string(),
        "?Profile#123"
    );
    assert_eq!(
        link(user, "roles").formatter(&schema).to_string(),
        "&List<Role>#123"
    );
    assert_eq!(
        link(user, "manager").formatter(&schema).to_string(),
        "&?User#123"
    );
    assert_eq!(
        link(profile, "user").formatter(&schema).to_string(),
        "&User#123"
    );
}

#[test]
fn contextual_ref_display_raw_fallback() {
    let schema = testdata::schema();
    let user = testdata::struct_id(&schema, "User");
    let field = testdata::field_id(&schema, user, "id");
    let value = Val::Ref {
        field,
        parent: Key(1),
    };
    assert_eq!(
        value.formatter(&schema).to_string(),
        format!("ref({}:{})", field.struct_id().index(), field.local())
    );

    let value = Val::Ref {
        field: FieldId::new(StructId::new(99), 0),
        parent: Key(1),
    };
    assert_eq!(value.formatter(&schema).to_string(), "ref(99:0)");
}

#[test]
fn contextual_composite_display() {
    let schema = testdata::schema();
    let priority = testdata::enum_id(&schema, "Priority");
    let user = testdata::struct_id(&schema, "User");
    let value = Val::record(vec![
        ("p".into(), Val::Some(Box::new(Val::Enum(priority, 2)))),
        (
            "m".into(),
            Val::list(vec![Val::Ref {
                field: crate::testdata::field_id(&schema, user, "manager"),
                parent: Key(7),
            }]),
        ),
        ("t".into(), Val::Tuple(vec![Val::None, int(3)])),
    ]);
    assert_eq!(
        value.formatter(&schema).to_string(),
        "{p: some(Priority::High), m: [&?User#7], t: (none, 3)}"
    );
}

#[test]
fn composite_display() {
    let mut interner = Interner::default();
    let x = interner.intern("x");
    let y = interner.intern("y");
    assert!(Rc::ptr_eq(&x, &interner.intern("x")));
    assert!(!Rc::ptr_eq(&x, &y));
    assert_eq!(interner.names.len(), 2);
    let list = Val::list(vec![Val::Scalar(Scalar::Int(1)), Val::None]);
    assert_eq!(list.to_string(), "[1, none]");
    let tuple = Val::Tuple(vec![
        Val::Scalar(Scalar::String("a".to_string())),
        Val::Scalar(Scalar::Int(1)),
    ]);
    assert_eq!(tuple.to_string(), "(\"a\", 1)");
    let record = Val::record(vec![(x, Val::Scalar(Scalar::Int(1))), (y, list)]);
    assert_eq!(record.to_string(), "{x: 1, y: [1, none]}");
    assert_eq!(Val::list(vec![]).to_string(), "[]");
}

#[test]
fn value_equality_unaffected_by_origin() {
    let schema = testdata::schema();
    let user = testdata::struct_id(&schema, "User");
    let orders = testdata::field_id(&schema, user, "orders");
    let fields = vec![(Rc::from("x"), Val::Scalar(Scalar::Int(1)))];
    let origin = RowOrigin {
        struct_id: user,
        key: Key(7),
    };
    assert_eq!(Val::record_at(fields.clone(), origin), Val::record(fields));
    assert_eq!(
        Val::list_at(
            vec![int(1)],
            CollectionOrigin::Ref {
                field: orders,
                parent: origin,
            },
        ),
        Val::list(vec![int(1)])
    );
}

#[test]
fn row_ref_value() {
    let schema = testdata::schema();
    let user = testdata::struct_id(&schema, "User");
    let origin = RowOrigin {
        struct_id: user,
        key: Key(7),
    };
    let other = RowOrigin {
        struct_id: user,
        key: Key(8),
    };
    let value = Val::RowRef(origin);

    assert_eq!(value.to_string(), format!("&Struct#{}@7", user.index()));
    assert_eq!(value.formatter(&schema).to_string(), "&User#7");
    assert_eq!(value.row_ref_origin(), Some(&origin));
    assert_eq!(value.row_origin(), None);
    assert_eq!(value.eq(&Val::RowRef(origin)), Some(true));
    assert_eq!(value.eq(&Val::RowRef(other)), Some(false));
    assert!(value.contains_ref());
}
