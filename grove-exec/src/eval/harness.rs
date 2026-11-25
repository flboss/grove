use std::rc::Rc;

use chrono::{DateTime, Utc};
use grove_query::TypedQueryFile;
use grove_schema::validated::ValidatedSchema;
use rusqlite::Connection;

use super::{ExecOptions, exec_file_with_options};
use crate::error::ExecError;
use crate::sqlite::SqliteBackend;
use crate::storage::Storage;
use crate::testdata;
use crate::value::{Scalar, Val};

pub(crate) fn run_src(src: &str) -> Result<Val, ExecError> {
    run_with(src, ValidatedSchema::default())
}

pub(crate) fn run_src_config(src: &str, config_block: &str) -> Result<Val, ExecError> {
    let schema_src = format!("{config_block}\nroot users: User; struct User {{ name: String }}");
    let (schema, schema_diags) = grove_schema::parse_schema(&schema_src);
    assert!(schema_diags.is_empty(), "{schema_diags:?}");
    let (schema, schema_diags) = grove_schema::validate(schema.unwrap());
    assert!(schema_diags.is_empty(), "{schema_diags:?}");
    run_with(src, schema.unwrap())
}

pub(crate) fn run_with(src: &str, schema: ValidatedSchema) -> Result<Val, ExecError> {
    let storage = NoStorage;
    run_with_storage(src, &schema, &storage)
}

pub(crate) fn run_with_storage<S: Storage>(
    src: &str,
    schema: &ValidatedSchema,
    storage: &S,
) -> Result<Val, ExecError> {
    let typed = typed_for(src, schema);
    let pushed = exec_file_with_options(&typed, storage, schema, FIXED_NOW, ExecOptions::default());
    let unpushed = exec_file_with_options(
        &typed,
        storage,
        schema,
        FIXED_NOW,
        ExecOptions {
            pushdown: false,
            materialize_lets: false,
            read_only: false,
            debug_rollback: false,
        },
    );
    assert_pushdown_parity(src, &pushed, &unpushed);
    pushed.map(|value| value.expect("query produced no output"))
}

pub fn assert_pushdown_parity(
    src: &str,
    pushed: &Result<Option<Val>, ExecError>,
    unpushed: &Result<Option<Val>, ExecError>,
) {
    match (pushed, unpushed) {
        (Ok(Some(left)), Ok(Some(right))) => assert!(
            same_value(left, right),
            "pushdown changed the result for `{src}`: {left:?} vs {right:?}"
        ),
        (Ok(None), Ok(None)) => {}
        (Err(left), Err(right)) => {
            assert_eq!(left, right, "pushdown changed the error for `{src}`")
        }
        _ => panic!("pushdown changed success/error for `{src}`: {pushed:?} vs {unpushed:?}"),
    }
}

fn same_value(left: &Val, right: &Val) -> bool {
    if left.row_origin() != right.row_origin()
        || left.collection_origin() != right.collection_origin()
    {
        return false;
    }
    match (left, right) {
        (Val::Scalar(Scalar::Float(a)), Val::Scalar(Scalar::Float(b))) => {
            (a.is_nan() && b.is_nan()) || a == b
        }
        (Val::Some(a), Val::Some(b)) => same_value(a, b),
        (Val::List(a), Val::List(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_value(a, b))
        }
        (Val::Tuple(a), Val::Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_value(a, b))
        }
        (Val::Record(a), Val::Record(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|((name_a, a), (name_b, b))| name_a == name_b && same_value(a, b))
        }
        _ => left == right,
    }
}

pub(crate) fn run_with_storage_opts<S: Storage>(
    src: &str,
    schema: &ValidatedSchema,
    storage: &S,
    options: ExecOptions,
) -> Result<Val, ExecError> {
    let typed = typed_for(src, schema);
    exec_file_with_options(&typed, storage, schema, FIXED_NOW, options)
        .map(|value| value.expect("query produced no output"))
}

pub(crate) fn run_optional_with_storage<S: Storage>(
    src: &str,
    schema: &ValidatedSchema,
    storage: &S,
) -> Result<Option<Val>, ExecError> {
    let typed = typed_for(src, schema);
    exec_file_with_options(&typed, storage, schema, FIXED_NOW, ExecOptions::default())
}

fn typed_for(src: &str, schema: &ValidatedSchema) -> TypedQueryFile {
    let (file, parse_diags) = grove_query::parse_query(src);
    assert!(
        parse_diags
            .iter()
            .all(|d| d.severity == grove_types::Severity::Warning),
        "{parse_diags:?}"
    );
    let (typed, diags) = grove_query::typecheck::typecheck(file.unwrap(), schema);
    assert!(diags.is_empty(), "{diags:?}");
    typed.unwrap()
}

pub(crate) fn run_seeded(src: &str) -> Result<Val, ExecError> {
    let schema = testdata::schema();
    let mut conn = Connection::open_in_memory().expect("in-memory database should open");
    testdata::seed(&mut conn).expect("testdata initialization should succeed");
    let backend = SqliteBackend::new(conn, &schema).expect("backend should verify");
    run_with_storage(src, &schema, &backend)
}

pub(crate) const FIXED_NOW: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();

pub(crate) struct NoStorage;

impl Storage for NoStorage {
    fn fetch(
        &self,
        _struct_id: grove_schema::validated::StructId,
        _fields: &[grove_schema::validated::FieldId],
        _constraints: &crate::storage::FetchConstraints,
    ) -> Result<Vec<crate::storage::Row>, ExecError> {
        Err(ExecError::Internal {
            message: "NoStorage backend cannot provide storage".to_string(),
        })
    }

    fn fetch_related(
        &self,
        _rel: grove_schema::validated::RelationId,
        _fetch_from_first: bool,
        _parents: &[crate::value::Key],
        _fields: &[grove_schema::validated::FieldId],
        _constraints: &crate::storage::FetchConstraints,
    ) -> Result<Vec<Vec<crate::storage::Row>>, ExecError> {
        Err(ExecError::Internal {
            message: "NoStorage backend cannot provide storage".to_string(),
        })
    }
}

pub(crate) fn int(n: i64) -> Val {
    Val::Scalar(crate::value::Scalar::Int(n))
}

pub(crate) fn float(x: f64) -> Val {
    Val::Scalar(crate::value::Scalar::Float(x))
}

pub(crate) fn dec(x: rust_decimal::Decimal) -> Val {
    Val::Scalar(crate::value::Scalar::Dec(x))
}

pub(crate) fn str(s: &str) -> Val {
    Val::Scalar(crate::value::Scalar::String(s.to_string()))
}

pub(crate) fn bytes(v: &[u8]) -> Val {
    Val::Scalar(crate::value::Scalar::Bytes(v.to_vec()))
}

pub(crate) fn bool(b: bool) -> Val {
    Val::Scalar(crate::value::Scalar::Bool(b))
}

pub(crate) fn rec(fields: Vec<(&str, Val)>) -> Val {
    Val::record(
        fields
            .into_iter()
            .map(|(name, value)| (Rc::from(name), value))
            .collect(),
    )
}

pub(crate) fn some(v: Val) -> Val {
    Val::Some(Box::new(v))
}
