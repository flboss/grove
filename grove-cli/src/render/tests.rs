use std::rc::Rc;

use grove_exec::value::{Key, Scalar, Val};
use grove_query::{QueryType, RecordSource};
use grove_schema::ValidatedSchema;
use grove_schema::validated::{FieldId, ScalarType, StructId};

use super::*;

fn schema() -> ValidatedSchema {
    let source = r#"
        root users: User;
        struct User {
            name: String,
            age: Int,
            active: Bool,
        }
        enum Role { Admin => "admin", User => "user" }
    "#;
    let (schema, diagnostics) = grove_schema::parse_schema(source);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let (schema, diagnostics) = grove_schema::validate(schema.unwrap());
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    schema.unwrap()
}

fn options(format: Format) -> RenderOptions {
    RenderOptions {
        format,
        color: false,
        width: 80,
        max_rows: 0,
        max_depth: 6,
        max_cell_width: 40,
        headers: true,
        hide_refs: false,
    }
}

fn int(value: i64) -> Val {
    Val::Scalar(Scalar::Int(value))
}

fn string(value: &str) -> Val {
    Val::Scalar(Scalar::String(value.to_string()))
}

fn user(name: &str, age: i64, active: bool) -> Val {
    Val::record(vec![
        (Rc::from("name"), string(name)),
        (Rc::from("age"), int(age)),
        (Rc::from("active"), Val::Scalar(Scalar::Bool(active))),
    ])
}

fn record_ty() -> QueryType {
    QueryType::Record(RecordSource::Schema(StructId::new(0)))
}

fn list_record_ty() -> QueryType {
    QueryType::List(Box::new(record_ty()))
}

fn render_value(mut value: Val, ty: &QueryType, options: &RenderOptions) -> String {
    render(&mut value, Some(ty), &schema(), options).expect("render value")
}

#[test]
fn scalar_and_optional_auto_inline() {
    let int_ty = QueryType::Scalar(ScalarType::Int);
    let optional_int = QueryType::Optional(Box::new(int_ty.clone()));

    assert_eq!(render_value(int(42), &int_ty, &options(Format::Auto)), "42");
    assert_eq!(
        render_value(Val::None, &optional_int, &options(Format::Auto)),
        "none"
    );
    assert_eq!(
        render_value(
            Val::Some(Box::new(int(3))),
            &optional_int,
            &options(Format::Auto)
        ),
        "some(3)"
    );
}

#[test]
fn scalar_list_auto_inline() {
    let value = Val::list(vec![int(1), int(2), int(3)]);
    let ty = QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)));
    assert_eq!(
        render_value(value, &ty, &options(Format::Auto)),
        "[1, 2, 3]"
    );
}

#[test]
fn tuple_auto_inline() {
    let value = Val::Tuple(vec![int(1), string("a")]);
    let ty = QueryType::Tuple(vec![
        QueryType::Scalar(ScalarType::Int),
        QueryType::Scalar(ScalarType::String),
    ]);
    assert_eq!(
        render_value(value, &ty, &options(Format::Auto)),
        "(1, \"a\")"
    );
}

#[test]
fn record_auto_vertical() {
    let value = user("Alice", 30, true);
    let output = render_value(value, &record_ty(), &options(Format::Auto));
    assert_eq!(output, "╭ name  : \"Alice\"\n┊ age   : 30\n╰ active: true");
}

#[test]
fn single_field_record_dot() {
    let value = Val::record(vec![(Rc::from("name"), string("Alice"))]);
    let output = render_value(value, &record_ty(), &options(Format::Vertical));
    assert_eq!(output, "· name: \"Alice\"");
}

#[test]
fn tuple_vertical_rails() {
    let value = Val::Tuple(vec![int(1), string("a"), Val::Scalar(Scalar::Bool(true))]);
    let ty = QueryType::Tuple(vec![
        QueryType::Scalar(ScalarType::Int),
        QueryType::Scalar(ScalarType::String),
        QueryType::Scalar(ScalarType::Bool),
    ]);
    let output = render_value(value, &ty, &options(Format::Vertical));
    assert_eq!(output, "╭ .0: 1\n┊ .1: \"a\"\n╰ .2: true");
}

#[test]
fn some_record_vertical() {
    let value = Val::Some(Box::new(user("Alice", 30, true)));
    let output = render_value(value, &record_ty(), &options(Format::Vertical));
    assert_eq!(
        output,
        "some\n  ╭ name  : \"Alice\"\n  ┊ age   : 30\n  ╰ active: true"
    );
}

#[test]
fn record_field_some_vertical() {
    let value = Val::record(vec![(
        Rc::from("profile"),
        Val::Some(Box::new(user("Alice", 30, true))),
    )]);
    let output = render_value(value, &record_ty(), &options(Format::Vertical));
    assert_eq!(
        output,
        "· profile: some\n    ╭ name  : \"Alice\"\n    ┊ age   : 30\n    ╰ active: true"
    );
}

#[test]
fn some_scalar_compact_vertical() {
    let value = Val::Some(Box::new(int(3)));
    let output = render_value(value, &record_ty(), &options(Format::Vertical));
    assert_eq!(output, "some(3)");
}

#[test]
fn record_nested_vertical_rails() {
    let value = Val::list(vec![user("Alice", 30, true), user("Bob", 9, false)]);
    let output = render_value(value, &list_record_ty(), &options(Format::Vertical));
    assert!(output.contains("├─ ╭ name"), "{output}");
    assert!(output.contains("│  ┊ age"), "{output}");
    assert!(output.contains("│  ╰ active"), "{output}");
    assert!(output.contains("└─ ╭ name"), "{output}");
    assert!(output.contains("   ┊ age"), "{output}");
    assert!(output.contains("   ╰ active"), "{output}");
}

#[test]
fn record_list_auto_table() {
    let value = Val::list(vec![user("Alice", 30, true), user("Bob", 9, false)]);
    let output = render_value(value, &list_record_ty(), &options(Format::Auto));
    assert!(output.contains("┌"), "{output}");
    assert!(output.contains("name"), "{output}");
    assert!(output.contains("Alice"), "{output}");
    assert!(output.contains("Bob"), "{output}");
    assert!(!output.contains(":"), "{output}");
}

#[test]
fn empty_record_list_type_headers() {
    let output = render_value(
        Val::list(Vec::new()),
        &list_record_ty(),
        &options(Format::Auto),
    );
    assert!(output.contains("┌"), "{output}");
    assert!(output.contains("name"), "{output}");
    assert!(output.contains("age"), "{output}");
}

#[test]
fn wide_table_auto_vertical_fallback() {
    let value = Val::list(vec![user("Alice", 30, true), user("Bob", 9, false)]);
    let mut options = options(Format::Auto);
    options.width = 20;
    let output = render_value(value, &list_record_ty(), &options);
    assert!(!output.contains("┌"), "{output}");
    assert!(output.contains("name  : \"Alice\""), "{output}");
}

#[test]
fn table_row_cap() {
    let value = Val::list(vec![
        user("Alice", 30, true),
        user("Bob", 9, false),
        user("Cara", 41, true),
    ]);
    let mut options = options(Format::Auto);
    options.max_rows = 2;
    let output = render_value(value, &list_record_ty(), &options);
    assert!(output.contains("1 more row"), "{output}");
    assert!(!output.contains("Cara"), "{output}");
    let note_line = output
        .lines()
        .find(|line| line.contains("1 more row"))
        .expect("note line");
    assert!(note_line.starts_with('│'), "{note_line}");
    assert!(note_line.ends_with('│'), "{note_line}");
}

#[test]
fn scalar_list_row_cap() {
    let value = Val::list(vec![int(1), int(2), int(3)]);
    let ty = QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)));
    let mut options = options(Format::Auto);
    options.max_rows = 2;
    let output = render_value(value, &ty, &options);
    assert!(output.contains("├─ 1"), "{output}");
    assert!(!output.contains("├─ 3"), "{output}");
    assert!(output.contains("└─ … 1 more item"), "{output}");
}

#[test]
fn table_format_non_tabular_rejected() {
    let int_ty = QueryType::Scalar(ScalarType::Int);
    assert!(
        render(
            &mut int(1),
            Some(&int_ty),
            &schema(),
            &options(Format::Table)
        )
        .is_err()
    );
}

#[test]
fn color_style_scalar() {
    let int_ty = QueryType::Scalar(ScalarType::Int);
    let mut options = options(Format::Auto);
    options.color = true;
    let output = render_value(int(7), &int_ty, &options);
    assert!(output.contains("\x1b[36m7\x1b[0m"), "{output}");
}

#[test]
fn table_cells_keep_part_styles() {
    let value = Val::list(vec![Val::Tuple(vec![Val::Some(Box::new(int(3)))])]);
    let ty = QueryType::List(Box::new(QueryType::Tuple(vec![QueryType::Optional(
        Box::new(QueryType::Scalar(ScalarType::Int)),
    )])));
    let mut options = options(Format::Table);
    options.color = true;
    let output = render_value(value, &ty, &options);
    assert!(output.contains("some(\x1b[36m3\x1b[0m)"), "{output}");
}

#[test]
fn hide_refs_omits_fields_and_list_items() {
    let schema = schema();
    let reference = Val::Ref {
        field: FieldId::new(StructId::new(0), 0),
        parent: Key(7),
    };
    let mut record = Val::record(vec![
        (Rc::from("name"), string("Alice")),
        (Rc::from("link"), reference.clone()),
    ]);
    let mut opts = options(Format::Vertical);
    opts.hide_refs = true;
    let output = render(&mut record, None, &schema, &opts).unwrap();
    assert!(output.contains("name"), "{output}");
    assert!(!output.contains("link"), "{output}");
    assert!(!output.contains("ref("), "{output}");

    let mut list = Val::list(vec![reference, int(1)]);
    let mut opts = options(Format::Auto);
    opts.hide_refs = true;
    let output = render(&mut list, None, &schema, &opts).unwrap();
    assert_eq!(output, "[1]");
}

#[test]
fn hide_refs_removes_table_columns() {
    let schema = schema();
    let reference = Val::Ref {
        field: FieldId::new(StructId::new(0), 0),
        parent: Key(7),
    };
    let mut value = Val::list(vec![Val::record(vec![
        (Rc::from("name"), string("Alice")),
        (Rc::from("link"), reference),
    ])]);
    let mut opts = options(Format::Table);
    opts.hide_refs = true;
    let output = render(&mut value, None, &schema, &opts).unwrap();
    assert!(output.contains("name"), "{output}");
    assert!(!output.contains("link"), "{output}");
}

#[test]
fn ref_fallback_without_schema_metadata() {
    let mut value = Val::Ref {
        field: FieldId::new(StructId::new(0), 0),
        parent: Key(7),
    };
    let output = render(&mut value, None, &schema(), &options(Format::Auto)).unwrap();
    assert!(output.contains("ref("), "{output}");
}

#[test]
fn nested_list_auto_vertical_indentation() {
    let value = Val::list(vec![
        Val::list(vec![int(1), int(2)]),
        Val::list(vec![int(3)]),
    ]);
    let ty = QueryType::List(Box::new(QueryType::List(Box::new(QueryType::Scalar(
        ScalarType::Int,
    )))));
    let output = render_value(value, &ty, &options(Format::Vertical));
    assert!(output.contains("├─┐"), "{output}");
    assert!(output.contains("│ ├─ 1"), "{output}");
    assert!(output.contains("  └─ 3"), "{output}");
}
