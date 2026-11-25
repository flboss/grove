use super::*;
use crate::ast::{
    BinaryOp, ConstantName, Expr, Literal, MutationKind, SortDir, Statement, TypeExpr, TypeName,
    UnaryOp,
};
use crate::parse_query;
use grove_types::Diagnostic;
use rust_decimal::dec;

fn codes(diags: &[Diagnostic]) -> Vec<&str> {
    diags.iter().map(|d| d.code.as_ref()).collect()
}

fn parse_ok(src: &str) -> Expr {
    let (file, diags) = crate::parse_query(src);
    assert!(diags.is_empty(), "unexpected diags for `{src}`: {diags:?}");
    file.expect("file should parse")
        .into_statements_and_output()
        .1
        .expect("file should have output")
}

#[test]
fn empty_is_valid_without_output() {
    let (file, diags) = parse_query("");
    let file = file.expect("empty file should parse");
    assert!(file.statements.is_empty());
    assert!(file.output().is_none());
    assert!(diags.is_empty());
}

#[test]
fn comment_is_valid_without_output() {
    let (file, diags) = parse_query("// nothing here");
    let file = file.expect("comment-only file should parse");
    assert!(file.statements.is_empty());
    assert!(file.output().is_none());
    assert!(diags.is_empty());
}

#[test]
fn repl_parse_complete() {
    for src in ["let x = 1;", "users", "users;", "let x = 1; x"] {
        assert!(
            matches!(
                crate::parse_query_repl(src),
                crate::ParseOutcome::Complete { .. }
            ),
            "expected complete parse for `{src}`"
        );
    }
}

#[test]
fn repl_parse_incomplete() {
    for src in [
        "users[",
        "1 +",
        "let x = 1",
        "if true {",
        "\"unclosed",
        "/* unclosed",
    ] {
        assert!(
            matches!(
                crate::parse_query_repl(src),
                crate::ParseOutcome::Incomplete { .. }
            ),
            "expected incomplete parse for `{src}`"
        );
    }
}

#[test]
fn repl_parse_invalid() {
    for src in [";", "let x 1;", "42 \"leftover\""] {
        assert!(
            matches!(
                crate::parse_query_repl(src),
                crate::ParseOutcome::Invalid { .. }
            ),
            "expected invalid parse for `{src}`"
        );
    }
}

#[test]
fn trailing_input() {
    let (file, diags) = parse_query("42 \"leftover\"");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0002"]);
}

#[test]
fn decimal_literal_expr() {
    match parse_ok("1e10") {
        Expr::Literal(lit) => {
            let Literal::Dec(d) = lit.value else {
                panic!("expected Dec literal");
            };
            assert_eq!(d, dec!(1e10));
        }
        other => panic!("expected literal, got {other:?}"),
    }
}

#[test]
fn instant_literal_expr() {
    match parse_ok("@now") {
        Expr::Literal(lit) => assert_eq!(lit.value, Literal::Now),
        other => panic!("expected literal, got {other:?}"),
    }

    match parse_ok("@2024-01-15") {
        Expr::Literal(lit) => {
            let Literal::Instant(_) = lit.value else {
                panic!("expected Instant literal");
            };
        }
        other => panic!("expected literal, got {other:?}"),
    }
}

#[test]
fn bytes_literal_expr() {
    let src = r#"hex"DEAD_be ef""#;
    match parse_ok(src) {
        Expr::Literal(lit) => {
            assert_eq!(lit.value, Literal::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
            assert_eq!(lit.span.start, 0);
            assert_eq!(lit.span.end, src.len());
        }
        other => panic!("expected literal, got {other:?}"),
    }

    match parse_ok(r#"b64"Zm9v""#) {
        Expr::Literal(lit) => assert_eq!(lit.value, Literal::Bytes(b"foo".to_vec())),
        other => panic!("expected literal, got {other:?}"),
    }
}

#[test]
fn ident_expr() {
    match parse_ok("users") {
        Expr::Ident(name) => assert_eq!(name.value, "users"),
        other => panic!("expected ident, got {other:?}"),
    }
    match parse_ok("prev") {
        Expr::Ident(name) => assert_eq!(name.value, "prev"),
        other => panic!("expected ident, got {other:?}"),
    }
}

#[test]
fn grouping_parens_dropped() {
    assert!(matches!(parse_ok("(42)"),
        Expr::Literal(lit) if lit.value== Literal::Int(42),
    ));
    assert!(matches!(parse_ok("((true))"),
        Expr::Literal(lit) if lit.value== Literal::Bool(true),
    ));
}

#[test]
fn tuple_expr() {
    match parse_ok("(1, 2)") {
        Expr::Tuple { elements, .. } => {
            assert_eq!(elements.len(), 2);
            let Expr::Literal(lit) = &elements[0] else {
                panic!("expected literal");
            };
            assert_eq!(lit.value, Literal::Int(1));
        }
        other => panic!("expected tuple, got {other:?}"),
    }
    match parse_ok("(1, 2,)") {
        Expr::Tuple { elements, .. } => assert_eq!(elements.len(), 2),
        other => panic!("expected tuple, got {other:?}"),
    }
}

#[test]
fn single_element_tuple() {
    match parse_ok("(1,)") {
        Expr::Tuple { elements, .. } => {
            assert_eq!(elements.len(), 1);
            let Expr::Literal(lit) = &elements[0] else {
                panic!("expected literal");
            };
            assert_eq!(lit.value, Literal::Int(1));
        }
        other => panic!("expected tuple, got {other:?}"),
    }
}

#[test]
fn unclosed_tuple() {
    let (file, diags) = parse_query("(1, 2");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0003"]);
}

#[test]
fn array_expr() {
    match parse_ok("[]") {
        Expr::Array { elements, .. } => assert!(elements.is_empty()),
        other => panic!("expected array, got {other:?}"),
    }
    match parse_ok("[{ name = \"a\" }]") {
        Expr::Array { elements, .. } => {
            assert_eq!(elements.len(), 1);
            let Expr::Struct { fields, .. } = &elements[0] else {
                panic!("expected struct literal");
            };
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].0.value, "name");
        }
        other => panic!("expected array, got {other:?}"),
    }
}

#[test]
fn unclosed_array() {
    let (file, diags) = parse_query("[1, 2");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0005"]);
}

#[test]
fn some_expr() {
    match parse_ok("some(42)") {
        Expr::Some { value, .. } => {
            let Expr::Literal(lit) = &*value else {
                panic!("expected literal");
            };
            assert_eq!(lit.value, Literal::Int(42));
        }
        other => panic!("expected some, got {other:?}"),
    }
}

#[test]
fn malformed_some_expr() {
    let (file, diags) = parse_query("some 42");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0006"]);

    let (file, diags) = parse_query("some(42");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0007"]);
}

#[test]
fn type_constants() {
    match parse_ok("Int::MIN") {
        Expr::TypeConstant { ty, name } => {
            assert_eq!(ty.value, TypeName::Int);
            assert_eq!(name.value, ConstantName::Min);
        }
        other => panic!("expected type constant, got {other:?}"),
    }
    match parse_ok("Float::MAX") {
        Expr::TypeConstant { ty, name } => {
            assert_eq!(ty.value, TypeName::Float);
            assert_eq!(name.value, ConstantName::Max);
        }
        other => panic!("expected type constant, got {other:?}"),
    }
}

#[test]
fn malformed_type_constants() {
    let (file, diags) = parse_query("Int FOO");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0008"]);

    let (file, diags) = parse_query("Int::FOO");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0009"]);
}

#[test]
fn if_else_expr() {
    let src = "if active { 1 } else if retired { 2 } else { 3 }";
    match parse_ok(src) {
        Expr::If {
            arms,
            default,
            span,
        } => {
            assert_eq!(arms.len(), 2);
            let Expr::Literal(lit) = &*default else {
                panic!("expected literal in else");
            };
            assert_eq!(lit.value, Literal::Int(3));
            assert_eq!(span.start, 0);
            assert_eq!(span.end, src.len());
        }
        other => panic!("expected if, got {other:?}"),
    }
}

#[test]
fn missing_else() {
    let src = "if active { 1 }";
    let (file, diags) = parse_query(src);
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0012"]);
    let diag = &diags[0];
    let label = &diag.labels[0];
    assert_eq!(label.span.start, 0);
    assert_eq!(label.span.end, src.len());
}

#[test]
fn malformed_if_blocks() {
    let (file, diags) = parse_query("if active 1 else { 2 }");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0010"]);

    let (file, diags) = parse_query("if active { 1");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0011"]);

    let (file, diags) = parse_query("if active { 1 } else 2");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0013"]);
}

#[test]
fn parenthesized_if_condition() {
    match parse_ok("if (true) { 1 } else { 2 }") {
        Expr::If { arms, .. } => {
            let Expr::Literal(lit) = &arms[0].0 else {
                panic!("expected literal condition");
            };
            assert_eq!(lit.value, Literal::Bool(true));
        }
        other => panic!("expected if, got {other:?}"),
    }
    parse_ok("if ((true)) { 1 } else { 2 }");
}

#[test]
fn unclosed_explicit_if_condition_parens() {
    let (file, diags) = parse_query("if (true { 1 } else { 2 }");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0022", "QP0003"]);
}

#[test]
fn struct_literal_expr() {
    match parse_ok("{ name = 1 }") {
        Expr::Struct { fields, span } => {
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].0.value, "name");
            assert_eq!(span.start, 0);
            assert_eq!(span.end, 12);
        }
        other => panic!("expected struct literal, got {other:?}"),
    }

    match parse_ok("if { name = 1 } { 2 } else { 3 }") {
        Expr::If { arms, .. } => {
            assert!(matches!(&arms[0].0, Expr::Struct { .. }));
        }
        other => panic!("expected if, got {other:?}"),
    }
}

#[test]
fn malformed_struct_literal() {
    let (file, diags) = parse_query("{ }");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0014"]);

    let (file, diags) = parse_query("{ name }");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0015"]);

    let (file, diags) = parse_query("{ name = }");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0001"]);
}

#[test]
fn method_call_no_args() {
    let expr = parse_ok("users.len()");
    assert!(matches!(expr, Expr::Method {
            name: Spanned { value: ref n, .. },
            args,
            ..
        } if n == "len" && args.is_empty()));
}

#[test]
fn tuple_access_single() {
    let expr = parse_ok("pos.1");
    assert!(matches!(
        expr,
        Expr::TupleAccess {
            index: Spanned { value: 1, .. },
            opt_depth: 0,
            ..
        }
    ));
}

#[test]
fn tuple_access_chained() {
    let expr = parse_ok("trio.2.1");
    assert!(matches!(expr, Expr::TupleAccess {
        base,
        index: Spanned { value: 1, .. },
        ..
    } if matches!(base.as_ref(), Expr::TupleAccess {
        index: Spanned { value: 2, .. },
        ..
    })));
}

#[test]
fn tuple_access_optional() {
    let expr = parse_ok("t?.2");
    assert!(matches!(
        expr,
        Expr::TupleAccess {
            index: Spanned { value: 2, .. },
            opt_depth: 1,
            ..
        }
    ));
}

#[test]
fn optional_access_depths() {
    assert!(matches!(
        parse_ok("foo??.bar"),
        Expr::Field { opt_depth: 2, .. }
    ));
    assert!(matches!(
        parse_ok("foo???.0"),
        Expr::TupleAccess { opt_depth: 3, .. }
    ));
    assert!(matches!(
        parse_ok("foo????.map($ + 1)"),
        Expr::Method { opt_depth: 4, .. }
    ));

    let Expr::Projection { items, .. } = parse_ok("users { manager??.name }") else {
        panic!("expected projection");
    };
    assert!(matches!(&items[0].value, Expr::Field { opt_depth: 2, .. }));
}

#[test]
fn dollar_as_ident() {
    let expr = parse_ok("[1, 2].filter($ > 0)");
    let Expr::Method { args, .. } = expr else {
        panic!("expected method");
    };
    assert_eq!(args.len(), 1);
    assert!(matches!(&args[0].expr, Expr::Binary { lhs, .. }
            if matches!(lhs.as_ref(), Expr::Ident(name) if name.value == "$")));
}

#[test]
fn method_call_with_args() {
    let expr = parse_ok("user.sort_asc(name)");
    assert!(matches!(expr, Expr::Method {
            name: Spanned { value: ref n, .. },
            args,
            ..
        } if n == "sort_asc" && args.len() == 1));
}

#[test]
fn method_call_sort_directions() {
    let expr = parse_ok("user.sort(asc name, desc age)");
    let Expr::Method { args, .. } = expr else {
        panic!()
    };
    assert_eq!(args.len(), 2);
    assert!(args[0].direction.is_some());
    assert!(args[1].direction.is_some());
}

#[test]
fn bracket_filter_method_desugar() {
    let expr = parse_ok("users[active]");
    assert!(matches!(expr, Expr::Method {
            name: Spanned { value: ref n, .. },
            args,
            ..
        } if n == "filter" && args.len() == 1));
}

#[test]
fn unclosed_filter() {
    let (file, diags) = parse_query("users[active");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0019"]);
}

#[test]
fn as_cast() {
    let expr = parse_ok("1 as Int");
    assert!(matches!(
        expr,
        Expr::Cast {
            ty: Spanned {
                value: TypeExpr::Scalar(TypeName::Int),
                ..
            },
            ..
        }
    ));
}

#[test]
fn as_cast_bytes() {
    let expr = parse_ok(r#""abc" as Bytes"#);
    assert!(matches!(
        expr,
        Expr::Cast {
            ty: Spanned {
                value: TypeExpr::Scalar(TypeName::Bytes),
                ..
            },
            ..
        }
    ));
}

#[test]
fn as_fallible_cast() {
    let expr = parse_ok("1 as? Int");
    assert!(matches!(
        expr,
        Expr::Cast {
            fallible: true,
            ty: Spanned {
                value: TypeExpr::Scalar(TypeName::Int),
                ..
            },
            ..
        }
    ));
}

#[test]
fn as_fallible_cast_greedy_question() {
    let expr = parse_ok("1 as??Int");
    assert!(matches!(
        expr,
        Expr::Cast {
            fallible: true,
            ty: Spanned {
                value: TypeExpr::Optional { .. },
                ..
            },
            ..
        }
    ));
    let expr = parse_ok("1 as ?Int");
    assert!(matches!(
        expr,
        Expr::Cast {
            fallible: false,
            ty: Spanned {
                value: TypeExpr::Optional { .. },
                ..
            },
            ..
        }
    ));
}

#[test]
fn enum_variant_expr() {
    let expr = parse_ok("Priority::High");
    match expr {
        Expr::EnumVariant {
            enum_name,
            variant,
            span,
        } => {
            assert_eq!(enum_name.value, "Priority");
            assert_eq!(variant.value, "High");
            assert_eq!(span.start, 0);
            assert_eq!(span.end, "Priority::High".len());
        }
        other => panic!("expected enum variant, got {other:?}"),
    }
}

#[test]
fn bare_ident_not_enum_variant() {
    let expr = parse_ok("Priority");
    assert!(matches!(expr, Expr::Ident(name) if name.value == "Priority"));
}

#[test]
fn enum_variant_missing_name() {
    let (file, diags) = parse_query("Priority::");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0030"]);
}

#[test]
fn enum_variant_cast_to_string() {
    let expr = parse_ok("Priority::High as String");
    assert!(matches!(
        expr,
        Expr::Cast {
            expr,
            ty: Spanned {
                value: TypeExpr::Scalar(TypeName::String),
                ..
            },
            ..
        } if matches!(expr.as_ref(), Expr::EnumVariant { .. })
    ));
}

fn cast_type(src: &str) -> TypeExpr {
    match parse_ok(src) {
        Expr::Cast { ty, .. } => ty.value,
        other => panic!("expected cast, got {other:?}"),
    }
}

#[test]
fn as_cast_named_type() {
    let expr = parse_ok("1 as Something");
    assert!(matches!(
        expr,
        Expr::Cast {
            ty: Spanned {
                value: TypeExpr::Named(name),
                ..
            },
            ..
        } if name == "Something"
    ));
}

#[test]
fn as_cast_scalars_and_named() {
    for (source, name) in [
        ("1 as Int", TypeName::Int),
        ("1 as Float", TypeName::Float),
        ("1 as Dec", TypeName::Dec),
        ("1 as String", TypeName::String),
        ("1 as Bytes", TypeName::Bytes),
        ("1 as Bool", TypeName::Bool),
        ("1 as Instant", TypeName::Instant),
        ("1 as Duration", TypeName::Duration),
    ] {
        assert_eq!(
            cast_type(source),
            TypeExpr::Scalar(name),
            "source: {source}"
        );
    }
    assert_eq!(cast_type("1 as User"), TypeExpr::Named("User".to_string()));
}

#[test]
fn type_annotation_optional_list_tuple_record() {
    assert!(matches!(
        cast_type("none as ?String"),
        TypeExpr::Optional { inner, .. }
            if inner.value == TypeExpr::Scalar(TypeName::String)
    ));
    assert!(matches!(
        cast_type("some(none) as ??String"),
        TypeExpr::Optional { inner, .. }
            if matches!(
                &inner.value,
                TypeExpr::Optional { inner, .. }
                    if inner.value == TypeExpr::Scalar(TypeName::String)
            )
    ));
    assert!(matches!(
        cast_type("[] as List<Int>"),
        TypeExpr::List { element, .. }
            if element.value == TypeExpr::Scalar(TypeName::Int)
    ));
    assert!(matches!(
        cast_type("(1,) as (Int)"),
        TypeExpr::Tuple { ref elements, .. } if elements.len() == 1
    ));
    assert!(matches!(
        cast_type("(1, \"\") as (Int, String)"),
        TypeExpr::Tuple { ref elements, .. } if elements.len() == 2
    ));

    let record = cast_type("1 as {a: Int, b: List<String>,}");
    let TypeExpr::Record { fields, .. } = record else {
        panic!("expected record type");
    };
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].0.value, "a");
    assert_eq!(fields[0].1.value, TypeExpr::Scalar(TypeName::Int));
    assert_eq!(fields[1].0.value, "b");
    assert!(matches!(
        fields[1].1.value,
        TypeExpr::List { ref element, .. }
            if element.value == TypeExpr::Scalar(TypeName::String)
    ));
}

#[test]
fn type_annotation_complex() {
    let ty = cast_type("[] as List<(?Int, List<Duration>, ??String, {a: Instant, d: Dec})>");
    let TypeExpr::List { element, .. } = ty else {
        panic!("expected list type");
    };
    let TypeExpr::Tuple { elements, .. } = element.value else {
        panic!("expected tuple type");
    };
    assert_eq!(elements.len(), 4);
    assert!(matches!(
        elements[0].value,
        TypeExpr::Optional { ref inner, .. }
            if inner.value == TypeExpr::Scalar(TypeName::Int)
    ));
    assert!(matches!(
        elements[1].value,
        TypeExpr::List { ref element, .. }
            if element.value == TypeExpr::Scalar(TypeName::Duration)
    ));
    assert!(matches!(
        elements[2].value,
        TypeExpr::Optional { ref inner, .. }
            if matches!(&inner.value, TypeExpr::Optional { inner, .. }
                if inner.value == TypeExpr::Scalar(TypeName::String))
    ));
    assert!(matches!(
        elements[3].value,
        TypeExpr::Record { ref fields, .. } if fields.len() == 2
    ));
}

#[test]
fn type_annotation_errors() {
    for (source, code) in [
        ("1 as", "QP0021"),
        ("1 as ()", "QP0031"),
        ("1 as {}", "QP0032"),
        ("1 as {,}", "QP0033"),
        ("1 as {a Int}", "QP0034"),
        ("1 as {a: Int b: String}", "QP0035"),
        ("1 as List Int", "QP0036"),
        ("1 as List<Int", "QP0037"),
        ("1 as {a: Int, a: String}", "QP0038"),
        ("1 as (Int String)", "QP0003"),
    ] {
        let (file, diags) = parse_query(source);
        assert!(file.is_none(), "source: {source}");
        assert_eq!(codes(&diags), vec![code], "source: {source}");
    }
}

#[test]
fn empty_projection() {
    let expr = parse_ok("users { }");
    assert!(matches!(expr, Expr::Projection { items, .. } if items.is_empty()));
}

#[test]
fn projection_plain_fields() {
    let expr = parse_ok("users { name, email }");
    assert!(matches!(expr, Expr::Projection { items, .. }
        if items.len() == 2 && items.iter().all(|i| i.alias.is_none())
    ));
}

#[test]
fn projection_aliased_computed() {
    let expr = parse_ok("users { total = some(1) }");
    let Expr::Projection { items, .. } = expr else {
        panic!()
    };
    assert_eq!(items.len(), 1);
    assert!(items[0].alias.is_some());
}

#[test]
fn projection_nested_aliased() {
    let expr = parse_ok("users { orders = orders { total } }");
    let Expr::Projection { items, .. } = expr else {
        panic!()
    };
    assert_eq!(items.len(), 1);
    assert!(items[0].alias.is_some());
}

#[test]
fn projection_spread_only() {
    let expr = parse_ok("users { ... }");
    assert!(matches!(
        expr,
        Expr::Projection {
            items,
            spread: Some(_),
            ..
        } if items.is_empty()
    ));
}

#[test]
fn projection_spread_last() {
    let expr = parse_ok("users { name, ..., }");
    let Expr::Projection {
        items,
        spread: Some(_),
        ..
    } = expr
    else {
        panic!("expected projection with spread");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].alias, None);
}

#[test]
fn projection_spread_not_last() {
    for source in ["users { ..., name }", "users { name, ..., age, }"] {
        let (file, diags) = parse_query(source);
        assert!(file.is_none(), "source: {source}");
        assert_eq!(codes(&diags), vec!["QP0040"], "source: {source}");
    }
}

#[test]
fn projection_nested_missing_alias() {
    let (file, diags) = parse_query("users { orders { total } }");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0022", "QP0002"]);
}

#[test]
fn unclosed_projection() {
    let (file, diags) = parse_query("users { name");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0020"]);
}

#[test]
fn missing_ident_after_dot() {
    let (_, diags) = parse_query("user.");
    assert_eq!(codes(&diags), vec!["QP0017"]);
}

#[test]
fn postfixes() {
    let expr = parse_ok("users[x].sort(asc name).take(5) { total }");
    let Expr::Projection { base, items, .. } = expr else {
        panic!("expected projection");
    };
    assert!(items.len() == 1);
    let Expr::Method { base, name, .. } = *base else {
        panic!("expected method call");
    };
    assert_eq!(name.value, "take");
    let Expr::Method {
        base, name, args, ..
    } = *base
    else {
        panic!("expected method call");
    };
    assert_eq!(name.value, "sort");
    assert_eq!(
        **args.first().unwrap().direction.as_ref().unwrap(),
        SortDir::Asc
    );
    let Expr::Method {
        base, name, args, ..
    } = *base
    else {
        panic!("expected filter");
    };
    assert_eq!(name.value, "filter");
    assert!(matches!(args.first().unwrap().expr, Expr::Ident(_)));
    assert!(matches!(*base, Expr::Ident(_)));
}

#[test]
fn unary_neg() {
    match parse_ok("-1") {
        Expr::Unary { op, expr, .. } => {
            assert_eq!(op.value, UnaryOp::Neg);
            assert!(matches!(*expr, Expr::Literal(lit)
                if lit.value == Literal::Int(1)
            ));
        }
        other => panic!("expected unary, got {other:?}"),
    }
}

#[test]
fn unary_not() {
    match parse_ok("!true") {
        Expr::Unary { op, expr, .. } => {
            assert_eq!(op.value, UnaryOp::Not);
            assert!(matches!(*expr, Expr::Literal(lit)
                if lit.value == Literal::Bool(true)
            ));
        }
        other => panic!("expected unary, got {other:?}"),
    }
}

#[test]
fn double_unary_neg() {
    match parse_ok("--1") {
        Expr::Unary { op, expr, .. } => {
            assert_eq!(op.value, UnaryOp::Neg);
            match *expr {
                Expr::Unary { op, expr, .. } => {
                    assert_eq!(op.value, UnaryOp::Neg);
                    assert!(matches!(*expr, Expr::Literal(lit)
                        if lit.value == Literal::Int(1)
                    ));
                }
                other => panic!("expected inner unary, got {other:?}"),
            }
        }
        other => panic!("expected unary, got {other:?}"),
    }
}

#[test]
fn row_ref() {
    match parse_ok("&users") {
        Expr::RowRef { expr, .. } => {
            assert!(matches!(*expr, Expr::Ident(name) if name.value == "users"));
        }
        other => panic!("expected row ref, got {other:?}"),
    }
}

#[test]
fn row_ref_precedence_over_postfix_chain() {
    match parse_ok("&users.first()") {
        Expr::RowRef { expr, .. } => match *expr {
            Expr::Method { base, name, .. } => {
                assert_eq!(name.value, "first");
                assert!(matches!(*base, Expr::Ident(name) if name.value == "users"));
            }
            other => panic!("expected method inside row ref, got {other:?}"),
        },
        other => panic!("expected row ref, got {other:?}"),
    }
}

#[test]
fn row_ref_logical_and() {
    match parse_ok("&a&&&b") {
        Expr::Binary { op, lhs, rhs, .. } => {
            assert_eq!(op.value, BinaryOp::And);
            assert!(matches!(*lhs, Expr::RowRef { .. }));
            assert!(matches!(*rhs, Expr::RowRef { .. }));
        }
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_add() {
    match parse_ok("1 + 2") {
        Expr::Binary { op, lhs, rhs, .. } => {
            assert_eq!(op.value, BinaryOp::Add);
            assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(1)));
            assert!(matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(2)));
        }
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_sub() {
    match parse_ok("1 - 2") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Sub),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_mul() {
    match parse_ok("1 * 2") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Mul),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_div() {
    match parse_ok("1 / 2") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Div),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_rem() {
    match parse_ok("1 % 2") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Rem),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_mod() {
    match parse_ok("1 mod 2") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Mod),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_eq() {
    match parse_ok("a == b") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Eq),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_ne() {
    match parse_ok("a != b") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Ne),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_lt() {
    match parse_ok("a < b") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Lt),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_gt() {
    match parse_ok("a > b") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Gt),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_le() {
    match parse_ok("a <= b") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Le),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_ge() {
    match parse_ok("a >= b") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Ge),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_and() {
    match parse_ok("a && b") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::And),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_or() {
    match parse_ok("a || b") {
        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Or),
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn binary_in() {
    match parse_ok("a in (1, 2)") {
        Expr::Binary { op, lhs, rhs, .. } => {
            assert_eq!(op.value, BinaryOp::In);
            assert!(matches!(*lhs, Expr::Ident(_)));
            assert!(matches!(*rhs, Expr::Tuple { .. }));
        }
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn mul_add_precedence() {
    match parse_ok("1 + 2 * 3") {
        Expr::Binary { op, lhs, rhs, .. } => {
            assert_eq!(op.value, BinaryOp::Add);
            assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(1)));
            match *rhs {
                Expr::Binary { op, lhs, rhs, .. } => {
                    assert_eq!(op.value, BinaryOp::Mul);
                    assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(2)));
                    assert!(matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(3)));
                }
                other => panic!("expected mul, got {other:?}"),
            }
        }
        other => panic!("expected add, got {other:?}"),
    }
}

#[test]
fn left_associativity() {
    match parse_ok("1 - 2 + 3") {
        Expr::Binary { op, lhs, .. } => {
            assert_eq!(op.value, BinaryOp::Add);
            match *lhs {
                Expr::Binary { op, lhs, rhs, .. } => {
                    assert_eq!(op.value, BinaryOp::Sub);
                    assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(1)));
                    assert!(matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(2)));
                }
                other => panic!("expected sub, got {other:?}"),
            }
        }
        other => panic!("expected add, got {other:?}"),
    }
}

#[test]
fn parens_override_precedence() {
    match parse_ok("(1 + 2) * 3") {
        Expr::Binary { op, lhs, rhs, .. } => {
            assert_eq!(op.value, BinaryOp::Mul);
            match *lhs {
                Expr::Binary { op, .. } => {
                    assert_eq!(op.value, BinaryOp::Add);
                }
                other => panic!("expected add, got {other:?}"),
            }
            assert!(matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(3)));
        }
        other => panic!("expected mul, got {other:?}"),
    }
}

#[test]
fn cast_binary_precedence() {
    match parse_ok("1 + 2 as Int") {
        Expr::Binary { op, lhs, rhs, .. } => {
            assert_eq!(op.value, BinaryOp::Add);
            assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(1)));
            assert!(matches!(*rhs, Expr::Cast { .. }));
        }
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn unary_in_binary_rhs() {
    match parse_ok("1 + -2") {
        Expr::Binary { op, rhs, .. } => {
            assert_eq!(op.value, BinaryOp::Add);
            assert!(matches!(*rhs, Expr::Unary { op, .. } if op.value == UnaryOp::Neg));
        }
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn mixed_precedence() {
    match parse_ok("1 + 2 * 3 <= 4.0 as Int - 5 / 6 % 7") {
        Expr::Binary { op, lhs, rhs, .. } => {
            assert_eq!(op.value, BinaryOp::Le);
            match *lhs {
                Expr::Binary { op, lhs, rhs, .. } => {
                    assert_eq!(op.value, BinaryOp::Add);
                    assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(1)));
                    match *rhs {
                        Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Mul),
                        other => panic!("expected mul, got {other:?}"),
                    }
                }
                other => panic!("expected add, got {other:?}"),
            }
            match *rhs {
                Expr::Binary { op, lhs, rhs, .. } => {
                    assert_eq!(op.value, BinaryOp::Sub);
                    match *lhs {
                        Expr::Cast { ty, .. } => {
                            assert_eq!(ty.value, TypeExpr::Scalar(TypeName::Int))
                        }
                        other => panic!("expected cast, got {other:?}"),
                    }
                    match *rhs {
                        Expr::Binary { op, lhs, rhs, .. } => {
                            assert_eq!(op.value, BinaryOp::Rem);
                            match *lhs {
                                Expr::Binary { op, lhs, rhs, .. } => {
                                    assert_eq!(op.value, BinaryOp::Div);
                                    assert!(
                                        matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(5))
                                    );
                                    assert!(
                                        matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(6))
                                    );
                                }
                                other => panic!("expected cast, got {other:?}"),
                            }
                            assert!(
                                matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(7))
                            );
                        }
                        other => panic!("expected mul, got {other:?}"),
                    }
                }
                other => panic!("expected sub, got {other:?}"),
            }
        }
        other => panic!("expected le, got {other:?}"),
    }
}

#[test]
fn binary_missing_rhs() {
    let (file, diags) = parse_query("1 +");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0001"]);
}

#[test]
fn binary_missing_lhs() {
    let (file, diags) = parse_query("+ 1");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0001"]);
}

#[test]
fn unary_neg_binary_sub() {
    match parse_ok("a - -b") {
        Expr::Binary { op, rhs, .. } => {
            assert_eq!(op.value, BinaryOp::Sub);
            assert!(matches!(*rhs, Expr::Unary { op, .. } if op.value == UnaryOp::Neg));
        }
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn comparison_precedence() {
    let expr = parse_ok("a < b && c >= d");
    assert!(matches!(expr, Expr::Binary { op, .. } if op.value == BinaryOp::And));
    match parse_ok("a < b && c >= d") {
        Expr::Binary { op, lhs, rhs, .. } => {
            assert_eq!(op.value, BinaryOp::And);
            assert!(matches!(*lhs, Expr::Binary { op, .. } if op.value == BinaryOp::Lt));
            assert!(matches!(*rhs, Expr::Binary { op, .. } if op.value == BinaryOp::Ge));
        }
        other => panic!("expected binary, got {other:?}"),
    }
}

#[test]
fn if_condition_expr() {
    match parse_ok("if x == 0 { 1 } else { 2 }") {
        Expr::If { arms, .. } => {
            assert!(matches!(&arms[0].0, Expr::Binary { op, .. } if op.value == BinaryOp::Eq));
        }
        other => panic!("expected if, got {other:?}"),
    }
}

#[test]
fn mutation_insert_statement() {
    let src = "users.insert({name = \"Alice\"}); 0";
    let (file, diags) = parse_query(src);
    let file = file.expect("should parse");
    assert_eq!(file.statements.len(), 2);
    assert!(
        matches!(&file.statements[0], Statement::Mutation(m) if m.kind.value == MutationKind::Insert)
    );
    assert!(matches!(file.output(), Some(Expr::Literal(lit)) if lit.value == Literal::Int(0)));
    assert!(
        diags
            .iter()
            .all(|d| d.severity != grove_types::Severity::Warning)
    );
}

#[test]
fn mutation_all_args_preserved_opt_depth() {
    let (file, diags) = parse_query("users?.insert({name = \"A\"}, {name = \"B\"});");
    let file = file.expect("should parse");
    let Statement::Mutation(mutation) = &file.statements[0] else {
        panic!("expected mutation");
    };
    assert_eq!(mutation.opt_depth, 1);
    assert_eq!(mutation.args.len(), 2);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn mutation_delete_statement() {
    let src = "users[!active].delete();";
    let file = parse_query(src).0.expect("should parse");
    assert_eq!(file.statements.len(), 1);
    assert!(
        matches!(&file.statements[0], Statement::Mutation(m) if m.kind.value == MutationKind::Delete)
    );
    assert!(file.output().is_none());
}

#[test]
fn mutation_update_statement() {
    let src = "users.update({name = \"Bob\"});";
    let file = parse_query(src).0.expect("should parse");
    assert_eq!(file.statements.len(), 1);
    assert!(
        matches!(&file.statements[0], Statement::Mutation(m) if m.kind.value == MutationKind::Update)
    );
    assert!(file.output().is_none());
}

#[test]
fn discarded_query_warning() {
    let src = "users { name }; 0";
    let (file, diags) = parse_query(src);
    let file = file.expect("should parse");
    assert_eq!(file.statements.len(), 2);
    assert!(matches!(&file.statements[0], Statement::Expr(_)));
    assert!(matches!(file.output(), Some(Expr::Literal(lit)) if lit.value == Literal::Int(0)));
    let warnings: Vec<_> = diags
        .iter()
        .filter(|d| d.severity == grove_types::Severity::Warning)
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].code.as_ref(), "QP0024");
}

#[test]
fn empty_statement_error() {
    let (file, diags) = parse_query(";");
    assert!(file.is_none());
    assert!(diags.iter().any(|d| d.code.as_ref() == "QP0023"));
}

#[test]
fn trailing_semicolon_expression_is_output() {
    let (file, diags) = parse_query("users { name };");
    let file = file.expect("should parse");
    assert!(file.output().is_some());
    assert!(diags.is_empty());
}

#[test]
fn let_statement() {
    let (file, diags) = parse_query("let x = 1; x");
    let file = file.expect("should parse");
    assert_eq!(file.statements.len(), 2);
    assert!(matches!(&file.statements[0], Statement::Let(let_stmt)
                if let_stmt.name.value == "x"
                    && matches!(&let_stmt.value, Expr::Literal(lit) if lit.value == Literal::Int(1))));
    assert!(matches!(file.output(), Some(Expr::Ident(name)) if name.value == "x"));
    assert!(
        diags
            .iter()
            .all(|d| d.severity != grove_types::Severity::Warning)
    );
}

#[test]
fn let_chain() {
    let file = parse_query("let a = 1; let b = a; b")
        .0
        .expect("should parse");
    assert_eq!(file.statements.len(), 3);
    assert!(matches!(&file.statements[0], Statement::Let(_)));
    assert!(matches!(&file.statements[1], Statement::Let(_)));
    assert!(matches!(file.output(), Some(Expr::Ident(name)) if name.value == "b"));
}

#[test]
fn let_backtick_name() {
    let file = parse_query("let `let` = 1; `let`").0.expect("should parse");
    assert!(
        matches!(&file.statements[0], Statement::Let(let_stmt) if let_stmt.name.value == "let")
    );
}

#[test]
fn let_missing_equals() {
    let (file, diags) = parse_query("let x 1;");
    assert!(file.is_none());
    assert!(diags.iter().any(|d| d.code.as_ref() == "QP0026"));
}

#[test]
fn let_missing_name() {
    let (file, diags) = parse_query("let = 1;");
    assert!(file.is_none());
    assert!(diags.iter().any(|d| d.code.as_ref() == "QP0029"));
}

#[test]
fn let_missing_semicolon() {
    let (file, diags) = parse_query("let x = 1");
    assert!(file.is_none());
    assert!(diags.iter().any(|d| d.code.as_ref() == "QP0039"));
}

#[test]
fn let_mutation_value() {
    let (file, diags) = parse_query("let x = users.insert({name = \"Alice\"});");
    assert!(file.is_none());
    assert!(diags.iter().any(|d| d.code.as_ref() == "QP0028"));
}

#[test]
fn let_without_output_is_valid() {
    let (file, diags) = parse_query("let x = 1;");
    let file = file.expect("statement-only file should parse");
    assert_eq!(file.statements.len(), 1);
    assert!(matches!(&file.statements[0], Statement::Let(_)));
    assert!(file.output().is_none());
    assert!(diags.is_empty());
}

#[test]
fn let_trailing_input() {
    let (file, diags) = parse_query("let x = 1 foo;");
    assert!(file.is_none());
    assert!(diags.iter().any(|d| d.code.as_ref() == "QP0002"));
}

#[test]
fn multiple_statements() {
    let src = "users.insert({name = \"Alice\"}); users.insert({name = \"Bob\"}); 42";
    let file = parse_query(src).0.expect("should parse");
    assert_eq!(file.statements.len(), 3);
    assert!(
        file.statements[..2]
            .iter()
            .all(|s| matches!(s, Statement::Mutation(_)))
    );
    assert!(matches!(file.output(), Some(Expr::Literal(lit)) if lit.value == Literal::Int(42)));
}

#[test]
fn statement_and_discarded() {
    let src = "users.insert({name = \"Alice\"}); users { name }; 42";
    let file = parse_query(src).0.expect("should parse");
    assert_eq!(file.statements.len(), 3);
    assert!(matches!(&file.statements[0], Statement::Mutation(_)));
    assert!(matches!(&file.statements[1], Statement::Expr(_)));
    assert!(matches!(file.output(), Some(Expr::Literal(lit)) if lit.value == Literal::Int(42)));
}

#[test]
fn mutation_without_output_is_valid() {
    let (file, diags) = parse_query("users.insert({name = \"Alice\"});");
    let file = file.expect("mutation-only file should parse");
    assert_eq!(file.statements.len(), 1);
    assert!(matches!(&file.statements[0], Statement::Mutation(_)));
    assert!(file.output().is_none());
    assert!(diags.is_empty());
}

#[test]
fn mutation_missing_semicolon() {
    let (file, diags) = parse_query("users.insert({name = \"Alice\"})");
    assert!(file.is_none());
    assert!(diags.iter().any(|d| d.code.as_ref() == "QP0039"));
}

#[test]
fn multi_error_projection_items() {
    let (file, diags) = parse_query("users { orders { total }, 42 }");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0022", "QP0002"]);
}

#[test]
fn multi_error_method_args() {
    let (_, diags) = parse_query("users.sort(asc , asc )");
    assert_eq!(codes(&diags), vec!["QP0001", "QP0001"]);
}

#[test]
fn multi_error_array_elements() {
    let (file, diags) = parse_query("[,]");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0001"]);
}

#[test]
fn recovery_skips_to_comma_in_args() {
    let (_, diags) = parse_query("users.sort(name, , age)");
    assert_eq!(codes(&diags), vec!["QP0001"]);
}

#[test]
fn recovery_skips_to_comma_in_projection() {
    let (file, diags) = parse_query("users { , name }");
    assert!(file.is_none());
    assert_eq!(codes(&diags), vec!["QP0022"]);
}

#[test]
fn sync_to_stops_at_semicolon() {
    let src = "users.sort(bad; 42";
    let (file, diags) = parse_query(src);
    assert!(file.is_none());
    let errors: Vec<_> = diags
        .iter()
        .filter(|d| d.severity == grove_types::Severity::Error)
        .collect();
    assert!(!errors.is_empty());
}
