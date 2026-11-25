use super::harness::*;
use super::{Env, EvalCtx, ExecOptions, ExecSession, eval};
use crate::error::ExecError;
use crate::pushdown::PushdownPlan;
use crate::testdata;
use crate::value::Key;
use crate::value::{CollectionOrigin, RowOrigin, Val};
use grove_query::TypeBindings;
use grove_query::{
    IdentBinding, QueryType, RecordSource, TargetProvenance, TypedExpr, TypedExprKind,
    TypedQueryFile,
};
use grove_schema::validated::{ScalarType, ValidatedSchema};
use grove_types::{NodeId, Span, Spanned};
use rust_decimal::dec;
use std::rc::Rc;

fn typed_session(src: &str, schema: &ValidatedSchema, types: &TypeBindings) -> TypedQueryFile {
    let (file, parse_diags) = grove_query::parse_query(src);
    assert!(
        parse_diags
            .iter()
            .all(|d| d.severity == grove_types::Severity::Warning),
        "{parse_diags:?}"
    );
    let (typed, diags) =
        grove_query::typecheck::typecheck_with_bindings(file.unwrap(), schema, types);
    assert!(diags.is_empty(), "{diags:?}");
    typed.unwrap()
}

fn record_field<'a>(value: &'a Val, name: &str) -> &'a Val {
    let Val::Record(fields) = value else {
        panic!("expected record, got {value:?}");
    };
    fields
        .iter()
        .find(|(field_name, _)| field_name.as_ref() == name)
        .map(|(_, value)| value)
        .expect("expected record field")
}

#[test]
fn eval_let_binding() {
    assert_eq!(run_src("let x = 1; x"), Ok(int(1)));
    assert_eq!(run_src("let x = 1; let x = 2; x"), Ok(int(2)));
    assert_eq!(
        run_src("let x = true; let x = \"hello\"; x"),
        Ok(str("hello"))
    );
    assert_eq!(run_src("let a = 1; let b = a; b"), Ok(int(1)));
}

#[test]
fn eval_tuple_access() {
    assert_eq!(run_src("(\"a\", 1).0"), Ok(str("a")));
    assert_eq!(run_src("(\"a\", 1).1"), Ok(int(1)));
    assert_eq!(run_src("((1, 2), 3).0.1"), Ok(int(2)));
    assert_eq!(
        run_src("(10, 5, 7, 15, 8, 13, 3, 2, 14, 4, 16, 1, 9, 6, 11, (1,2,3,4)).15.3"),
        Ok(int(4))
    );
    assert_eq!(run_src("(1.5, -2.5).0"), Ok(dec(dec!(1.5))),);
    assert_eq!(run_src("[(\"a\", 1)].first()?.0"), Ok(some(str("a"))));
    assert_eq!(run_src("[(\"a\", 1)].skip(5).first()?.1"), Ok(Val::None));
    match run_seeded("users { x = pos.0 }") {
        Ok(Val::List(rows)) => {
            let firsts: Vec<Val> = rows
                .iter()
                .map(|row| match row {
                    Val::Record(fields) => {
                        match fields.iter().find(|(name, _)| name.as_ref() == "x") {
                            Some((_, value)) => value.clone(),
                            got => panic!("expected x, got {got:?}"),
                        }
                    }
                    other => panic!("expected record, got {other}"),
                })
                .collect();
            assert_eq!(
                firsts,
                [float(1.5), float(0.0), float(-1.25), float(2.0), float(9.5)]
            );
        }
        got => panic!("expected projected users, got {got:?}"),
    }
    match run_seeded("users { x = home?.1 }") {
        Ok(Val::List(rows)) => {
            let seconds: Vec<Val> = rows
                .iter()
                .map(|row| match row {
                    Val::Record(fields) => {
                        match fields.iter().find(|(name, _)| name.as_ref() == "x") {
                            Some((_, value)) => value.clone(),
                            got => panic!("expected x, got {got:?}"),
                        }
                    }
                    other => panic!("expected record, got {other}"),
                })
                .collect();
            assert_eq!(
                seconds,
                [
                    some(float(4.0)),
                    Val::None,
                    Val::None,
                    some(float(1.0)),
                    Val::None
                ]
            );
        }
        got => panic!("expected projected users, got {got:?}"),
    }
    assert_eq!(
        run_seeded("users.first().unwrap().extra.1.len()"),
        Ok(int(2))
    );
    match run_seeded("users { e = extra.1.len() }") {
        Ok(Val::List(rows)) => {
            let counts: Vec<Val> = rows
                .iter()
                .map(|row| match row {
                    Val::Record(fields) => {
                        match fields.iter().find(|(name, _)| name.as_ref() == "e") {
                            Some((_, value)) => value.clone(),
                            got => panic!("expected e, got {got:?}"),
                        }
                    }
                    other => panic!("expected record, got {other}"),
                })
                .collect();
            assert_eq!(counts, [int(2), int(0), int(0), int(1), int(0)]);
        }
        got => panic!("expected projected users, got {got:?}"),
    }
    match run_seeded("users { f = scores.first()?.0 }") {
        Ok(Val::List(rows)) => {
            let firsts: Vec<Val> = rows
                .iter()
                .map(|row| match row {
                    Val::Record(fields) => {
                        match fields.iter().find(|(name, _)| name.as_ref() == "f") {
                            Some((_, value)) => value.clone(),
                            got => panic!("expected f, got {got:?}"),
                        }
                    }
                    other => panic!("expected record, got {other}"),
                })
                .collect();
            assert_eq!(
                firsts,
                [some(int(1)), Val::None, Val::None, some(int(3)), Val::None]
            );
        }
        got => panic!("expected projected users, got {got:?}"),
    }
}

#[test]
fn eval_deep_optional_tuple_access() {
    assert_eq!(run_src("some(some((1, 2)))??.1"), Ok(some(some(int(2)))));
    assert_eq!(
        run_src("some(if true {none} else {some((1, 2))})??.0"),
        Ok(some(Val::None))
    );
}

#[test]
fn deep_optional_chain_pushdown_parity() {
    assert_eq!(
        run_seeded("users.first()?.nickname??.to_upper()"),
        Ok(some(some(str("A"))))
    );
    assert_eq!(
        run_seeded("users.first()?.manager??.name"),
        Ok(some(some(str("Bob"))))
    );
}

#[test]
fn eval_deep_optional_methods() {
    assert_eq!(
        run_src("some(some([1, 2]))??.map($ + 1)"),
        Ok(some(some(Val::list(vec![int(2), int(3)]))))
    );
    assert_eq!(
        run_src("some(some([1, 2]))??.filter($ > 1)"),
        Ok(some(some(Val::list(vec![int(2)]))))
    );
    assert_eq!(
        run_src("some(some([3, 1]))??.sort()"),
        Ok(some(some(Val::list(vec![int(1), int(3)]))))
    );
    assert_eq!(
        run_src("some(some(\"hi\"))??.len()"),
        Ok(some(some(int(2))))
    );
    assert_eq!(
        run_src("some(some([1, 2]))??.nth(0)"),
        Ok(some(some(some(int(1)))))
    );
    assert_eq!(
        run_src("some(some(some(1)))??.unwrap()"),
        Ok(some(some(int(1))))
    );
    assert_eq!(
        run_src("some(if true {none} else {some([1])})??.len()"),
        Ok(some(Val::None))
    );
}

#[test]
fn eval_if_scalar() {
    assert_eq!(run_src("if true { 1 } else { 2 }"), Ok(int(1)));
    assert_eq!(run_src("if false { 1 } else { 2 }"), Ok(int(2)));
    assert_eq!(
        run_src("if false { 1 } else if false { 2 } else { 3 }"),
        Ok(int(3))
    );
}

#[test]
fn eval_env_depth_collections() {
    let mut env = Env::new();
    env.define(Rc::from("xs"), Val::list(vec![int(1), Val::None]));
    let schema = ValidatedSchema::default();
    let plan = PushdownPlan::disabled(&schema);
    let mut ctx = EvalCtx::new(&mut env, FIXED_NOW, &NoStorage, &schema, &plan);
    let span = Span::new(0, 0);
    let expr = TypedExpr {
        kind: TypedExprKind::Ident {
            name: Spanned::new(span, "xs".to_string()),
            binding: IdentBinding::Let,
            depth: 0,
        },
        ty: QueryType::List(Box::new(QueryType::Optional(Box::new(QueryType::Scalar(
            ScalarType::Int,
        ))))),
        provenance: TargetProvenance::NONE,
        span,
        id: grove_types::NodeId(0),
    };
    assert_eq!(
        eval(&expr, &mut ctx),
        Ok(Val::list(vec![int(1), Val::None]))
    );
}

#[test]
fn enum_construction_and_casts() {
    let schema = testdata::schema();
    let priority = testdata::enum_id(&schema, "Priority");

    assert_eq!(
        run_with("Priority::High", schema.clone()),
        Ok(Val::Enum(priority, 2))
    );
    assert_eq!(
        run_with("Priority::High as String", schema.clone()),
        Ok(str("high"))
    );
    assert_eq!(run_with("Level::High as Int", schema), Ok(int(2)));
}

#[test]
fn enum_fields_decode_cast_and_filter() {
    let schema = testdata::schema();
    let priority = testdata::enum_id(&schema, "Priority");
    let level = testdata::enum_id(&schema, "Level");

    assert_eq!(
        run_seeded("settings.first().unwrap().priority as String"),
        Ok(str("high"))
    );
    assert_eq!(
        run_seeded("settings.first().unwrap().level as Int"),
        Ok(int(2))
    );
    assert_eq!(
        run_seeded("settings.first().unwrap().fallback"),
        Ok(some(Val::Enum(priority, 0)))
    );
    assert_eq!(
        run_seeded("settings.nth(1).unwrap().fallback"),
        Ok(Val::None)
    );
    assert_eq!(
        run_seeded("settings.first().unwrap().history"),
        Ok(Val::list(vec![
            Val::Enum(priority, 0),
            Val::Enum(priority, 2)
        ]))
    );
    assert_eq!(
        run_seeded("settings.first().unwrap().pair"),
        Ok(Val::Tuple(vec![
            Val::Enum(priority, 2),
            Val::Enum(level, 2)
        ]))
    );
    assert_eq!(
        run_seeded("settings[priority == Priority::High] { name }"),
        Ok(Val::list(vec![rec(vec![("name", str("alpha"))])]))
    );
    assert_eq!(
        run_seeded("settings[level in (Level::High, Level::Low)] { name }"),
        Ok(Val::list(vec![
            rec(vec![("name", str("alpha"))]),
            rec(vec![("name", str("beta"))]),
        ]))
    );
}

#[test]
fn exec_file_discarded_exprs() {
    assert_eq!(run_seeded("let x = 40 + 2; 1 / 0; x"), Ok(int(42)));
    assert_eq!(run_seeded("let a = 1; let b = a + 2; b"), Ok(int(3)));
    assert_eq!(
        run_seeded("users { name, x = 1 / 0 }; roles { name }"),
        Ok(Val::list(vec![
            rec(vec![("name", str("admin"))]),
            rec(vec![("name", str("editor"))]),
        ]))
    );
}

#[test]
fn exec_statement_only_files() {
    let schema = ValidatedSchema::default();
    assert_eq!(
        run_optional_with_storage("let x = 1;", &schema, &NoStorage),
        Ok(None)
    );
    assert_eq!(run_optional_with_storage("", &schema, &NoStorage), Ok(None));
    assert_eq!(
        run_optional_with_storage("let x = 1; x", &schema, &NoStorage),
        Ok(Some(int(1)))
    );
    assert_eq!(run_src("42;"), Ok(int(42)));
}

#[test]
fn session_bindings_persistence_alignment() {
    let schema = ValidatedSchema::default();
    let mut session = ExecSession::new(&NoStorage, &schema);
    let mut types = TypeBindings::new();

    let first = typed_session("let x = 41;", &schema, &types);
    assert_eq!(
        session.execute(&first, FIXED_NOW, ExecOptions::default()),
        Ok(None)
    );
    for (name, ty) in first.let_bindings() {
        types.define(name, ty.clone());
    }
    assert_eq!(session.binding("x"), Some(&int(41)));

    let second = typed_session("x + 1", &schema, &types);
    assert_eq!(
        session.execute(&second, FIXED_NOW, ExecOptions::default()),
        Ok(Some(int(42)))
    );
    assert_eq!(session.bindings().count(), 1);
}

#[test]
fn session_rollback_failed_turn() {
    let schema = ValidatedSchema::default();
    let mut session = ExecSession::new(&NoStorage, &schema);
    let mut types = TypeBindings::new();

    let first = typed_session("let x = 1;", &schema, &types);
    assert_eq!(
        session.execute(&first, FIXED_NOW, ExecOptions::default()),
        Ok(None)
    );
    for (name, ty) in first.let_bindings() {
        types.define(name, ty.clone());
    }

    let failed = typed_session("let y = 2; 1 / 0", &schema, &types);
    let err = session
        .execute(&failed, FIXED_NOW, ExecOptions::default())
        .unwrap_err();
    assert!(matches!(err, ExecError::Abort { .. }));
    assert!(session.binding("y").is_none());
    assert_eq!(session.binding("x"), Some(&int(1)));

    let third = typed_session("x", &schema, &types);
    assert_eq!(
        session.execute(&third, FIXED_NOW, ExecOptions::default()),
        Ok(Some(int(1)))
    );
}

#[test]
fn session_binding_depth_matches_scoped_methods() {
    let schema = ValidatedSchema::default();
    let mut session = ExecSession::new(&NoStorage, &schema);
    let mut types = TypeBindings::new();

    let first = typed_session("let min = 1;", &schema, &types);
    session
        .execute(&first, FIXED_NOW, ExecOptions::default())
        .unwrap();
    for (name, ty) in first.let_bindings() {
        types.define(name, ty.clone());
    }

    let second = typed_session("[1, 2, 3].filter($ > min)", &schema, &types);
    assert_eq!(
        session.execute(&second, FIXED_NOW, ExecOptions::default()),
        Ok(Some(Val::list(vec![int(2), int(3)])))
    );
}

#[test]
fn mutations_writable_storage_required() {
    let schema = testdata::schema();
    let mut session = ExecSession::new(&NoStorage, &schema);
    let typed = typed_session("users.delete();", &schema, &TypeBindings::new());

    let err = session
        .execute(&typed, FIXED_NOW, ExecOptions::default())
        .unwrap_err();
    assert!(matches!(err, ExecError::Unsupported { .. }), "{err:?}");
    assert_eq!(session.bindings().count(), 0);
}

#[test]
fn let_owning_refs_resolved() {
    let schema = testdata::schema();
    let mut conn = rusqlite::Connection::open_in_memory().expect("in-memory database");
    testdata::seed(&mut conn).expect("testdata initialization");
    let backend = crate::sqlite::SqliteBackend::new(conn, &schema).expect("backend");
    let mut session = ExecSession::new(&backend, &schema);

    let typed = typed_session(
        "let u = users.first().unwrap();",
        &schema,
        &TypeBindings::new(),
    );
    assert_eq!(
        session.execute(&typed, FIXED_NOW, ExecOptions::default()),
        Ok(None)
    );
    assert!(matches!(
        record_field(session.binding("u").expect("u binding"), "profile"),
        Val::Some(inner) if matches!(inner.as_ref(), Val::Record(_))
    ));
}

#[test]
fn session_remove_and_clear() {
    let schema = ValidatedSchema::default();
    let mut session = ExecSession::new(&NoStorage, &schema);
    let mut types = TypeBindings::new();

    for (src, name) in [("let x = 1;", "x"), ("let y = 2;", "y")] {
        let typed = typed_session(src, &schema, &types);
        session
            .execute(&typed, FIXED_NOW, ExecOptions::default())
            .unwrap();
        for (name, ty) in typed.let_bindings() {
            types.define(name, ty.clone());
        }
        assert!(session.binding(name).is_some());
    }

    assert_eq!(session.remove("x"), Some(int(1)));
    assert!(session.binding("x").is_none());
    assert!(session.binding("y").is_some());

    session.clear();
    assert_eq!(session.bindings().count(), 0);

    let typed = typed_session("1", &schema, &TypeBindings::new());
    assert_eq!(
        session.execute(&typed, FIXED_NOW, ExecOptions::default()),
        Ok(Some(int(1)))
    );
}

fn eval_row_ref_value(
    value: Val,
    input_ty: QueryType,
    output_ty: QueryType,
) -> Result<Val, ExecError> {
    let schema = testdata::schema();
    let plan = PushdownPlan::disabled(&schema);
    let mut env = Env::new();
    env.define("source", value);
    let mut ctx = EvalCtx::new(&mut env, FIXED_NOW, &NoStorage, &schema, &plan);
    let span = Span::new(0, 6);
    let source = TypedExpr {
        kind: TypedExprKind::Ident {
            name: Spanned::new(span, "source".to_string()),
            binding: IdentBinding::Let,
            depth: 0,
        },
        ty: input_ty,
        provenance: TargetProvenance::NONE,
        span,
        id: NodeId(0),
    };
    let expr = TypedExpr {
        kind: TypedExprKind::RowRef {
            expr: Box::new(source),
        },
        ty: output_ty,
        provenance: TargetProvenance::NONE,
        span,
        id: NodeId(1),
    };
    eval(&expr, &mut ctx)
}

#[test]
fn eval_row_ref_shapes() {
    let user = testdata::struct_id(&testdata::schema(), "User");
    let first = RowOrigin {
        struct_id: user,
        key: Key(10),
    };
    let second = RowOrigin {
        struct_id: user,
        key: Key(20),
    };
    let record_ty = QueryType::Record(RecordSource::Schema(user));
    let row_ref_ty = QueryType::RowRef(user);

    assert_eq!(
        eval_row_ref_value(
            Val::record_at(Vec::new(), first),
            record_ty.clone(),
            row_ref_ty.clone(),
        ),
        Ok(Val::RowRef(first))
    );

    let list = Val::list_at(
        vec![
            Val::record_at(Vec::new(), first),
            Val::record_at(Vec::new(), second),
        ],
        CollectionOrigin::Root {
            struct_id: user,
            root_idx: 0,
        },
    );
    let list_ty = QueryType::List(Box::new(record_ty.clone()));
    let output_list_ty = QueryType::List(Box::new(row_ref_ty.clone()));
    let result = eval_row_ref_value(list, list_ty, output_list_ty).unwrap();
    assert_eq!(
        result,
        Val::list(vec![Val::RowRef(first), Val::RowRef(second)])
    );
    assert_eq!(result.collection_origin(), None);

    let optional_ty = QueryType::Optional(Box::new(record_ty));
    let output_optional_ty = QueryType::Optional(Box::new(row_ref_ty));
    assert_eq!(
        eval_row_ref_value(
            Val::Some(Box::new(Val::record_at(Vec::new(), first))),
            optional_ty.clone(),
            output_optional_ty.clone(),
        ),
        Ok(Val::Some(Box::new(Val::RowRef(first))))
    );
    assert_eq!(
        eval_row_ref_value(Val::None, optional_ty, output_optional_ty),
        Ok(Val::None)
    );
}

#[test]
fn eval_row_ref_untracked_record_rejected() {
    let user = testdata::struct_id(&testdata::schema(), "User");
    let result = eval_row_ref_value(
        Val::record(Vec::new()),
        QueryType::Record(RecordSource::Schema(user)),
        QueryType::RowRef(user),
    );
    assert!(matches!(result, Err(ExecError::Internal { .. })));
}
