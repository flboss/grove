use super::*;
use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use rust_decimal::dec;

macro_rules! assert_token {
    ($lexer:expr, $kind:pat) => {
        let tok = $lexer.next_token();
        assert!(
            matches!(tok.value, $kind),
            "expected token kind {}, got {:?}",
            stringify!($kind),
            tok.value,
        );
    };
}

macro_rules! assert_ident {
    ($lexer:expr, $name:expr) => {
        let tok = $lexer.next_token();
        match tok.value {
            TokenKind::Ident(s) => assert_eq!(s, $name),
            ref other => panic!("expected Ident({}), got {:?}", $name, other),
        }
    };
}

macro_rules! assert_eof {
    ($lexer:expr) => {
        assert_token!($lexer, TokenKind::Eof);
    };
}

#[test]
fn empty_input() {
    let mut lex = Lexer::new("");
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn whitespace_is_skipped() {
    let mut lex = Lexer::new("  \n\t root  \r\n");
    assert_ident!(lex, "root");
    assert_eof!(lex);
}

#[test]
fn line_comments_are_skipped() {
    let mut lex = Lexer::new("// a comment\nusers");
    assert_ident!(lex, "users");
    assert_eof!(lex);
}

#[test]
fn line_comment_at_eof() {
    let mut lex = Lexer::new("// just a comment");
    assert_eof!(lex);
}

#[test]
fn line_comment_between_tokens() {
    let mut lex = Lexer::new("a // note\n b");
    assert_ident!(lex, "a");
    assert_ident!(lex, "b");
    assert_eof!(lex);
}

#[test]
fn block_comment_is_skipped() {
    let mut lex = Lexer::new("/* hi */ users");
    assert_ident!(lex, "users");
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn block_comment_spans_lines() {
    let mut lex = Lexer::new("/* line 1\n line 2 */ true");
    assert_token!(lex, TokenKind::True);
    assert_eof!(lex);
}

#[test]
fn nested_block_comments() {
    let mut lex = Lexer::new("/* a /* b */ c */ users");
    assert_ident!(lex, "users");
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn block_comment_inside_line_comment() {
    let mut lex = Lexer::new("// /* not a comment\nusers");
    assert_ident!(lex, "users");
    assert_eof!(lex);
}

#[test]
fn unclosed_block_comment() {
    let mut lex = Lexer::new("/* never closed");
    assert_eof!(lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn slash_as_division() {
    let mut lex = Lexer::new("a / b");
    assert_ident!(lex, "a");
    assert_token!(lex, TokenKind::Slash);
    assert_ident!(lex, "b");
    assert_eof!(lex);
}

#[test]
fn keywords() {
    let mut lex = Lexer::new("none some true false prev in as if else mod asc desc let");
    assert_token!(lex, TokenKind::None);
    assert_token!(lex, TokenKind::Some);
    assert_token!(lex, TokenKind::True);
    assert_token!(lex, TokenKind::False);
    assert_token!(lex, TokenKind::Prev);
    assert_token!(lex, TokenKind::In);
    assert_token!(lex, TokenKind::As);
    assert_token!(lex, TokenKind::If);
    assert_token!(lex, TokenKind::Else);
    assert_token!(lex, TokenKind::Mod);
    assert_token!(lex, TokenKind::Asc);
    assert_token!(lex, TokenKind::Desc);
    assert_token!(lex, TokenKind::Let);
    assert_eof!(lex);
}

#[test]
fn type_keywords() {
    let mut lex = Lexer::new("Int Float Dec String Bytes List Bool Instant Duration");
    assert_token!(lex, TokenKind::Int);
    assert_token!(lex, TokenKind::Float);
    assert_token!(lex, TokenKind::Dec);
    assert_token!(lex, TokenKind::String);
    assert_token!(lex, TokenKind::Bytes);
    assert_token!(lex, TokenKind::List);
    assert_token!(lex, TokenKind::Bool);
    assert_token!(lex, TokenKind::Instant);
    assert_token!(lex, TokenKind::Duration);
    assert_eof!(lex);
}

#[test]
fn idents() {
    let mut lex = Lexer::new("foo _bar baz123");
    assert_ident!(lex, "foo");
    assert_ident!(lex, "_bar");
    assert_ident!(lex, "baz123");
    assert_eof!(lex);
}

#[test]
fn ident_does_not_match_keyword() {
    let mut lex = Lexer::new("incoming truex Intx outlet Listx");
    assert_ident!(lex, "incoming");
    assert_ident!(lex, "truex");
    assert_ident!(lex, "Intx");
    assert_ident!(lex, "outlet");
    assert_ident!(lex, "Listx");
    assert_eof!(lex);
}

#[test]
fn backtick_ident_escapes_keywords() {
    let mut lex = Lexer::new("`prev` `true` `foo` `let`");
    assert_ident!(lex, "prev");
    assert_ident!(lex, "true");
    assert_ident!(lex, "foo");
    assert_ident!(lex, "let");
    assert_eof!(lex);
}

#[test]
fn unclosed_backtick() {
    let mut lex = Lexer::new("`prev");
    assert_ident!(lex, "prev");
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn string_literal() {
    let mut lex = Lexer::new(r#""hello world""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("hello world".into()));
    assert_eof!(lex);
}

#[test]
fn string_escapes() {
    let mut lex = Lexer::new(r#""a\n\t\r\\\"\0""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("a\n\t\r\\\"\0".into()));
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn string_hex_and_unicode_escapes() {
    let mut lex = Lexer::new(r#""\x41\u{1F600}""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("A\u{1F600}".into()));
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn unclosed_string() {
    let mut lex = Lexer::new(r#""hello"#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("hello".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn string_cannot_span_lines() {
    let mut lex = Lexer::new("\"abc\ndef\"");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("abc".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn non_ascii_hex_escape_is_error() {
    let mut lex = Lexer::new(r#""\x80""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn invalid_hex_escape_is_error() {
    let mut lex = Lexer::new(r#""\xG1""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("G1".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn unknown_escape_is_error() {
    let mut lex = Lexer::new(r#""\z""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn unicode_escape_empty() {
    let mut lex = Lexer::new(r#""\u{}""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn unicode_escape_missing_lbrace() {
    let mut lex = Lexer::new(r#""\u41""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("41".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn unicode_escape_invalid_digit() {
    let mut lex = Lexer::new(r#""\u{41x}""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn unicode_escape_too_many_digits() {
    let mut lex = Lexer::new(r#""\u{1234567}""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn unicode_escape_out_of_range() {
    let mut lex = Lexer::new(r#""\u{110000}""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn unicode_escape_surrogate() {
    let mut lex = Lexer::new(r#""\u{D800}""#);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::StringLit("".into()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn hex_bytes_literals() {
    for (src, expected) in [
        (
            r#"hex"0E2E 4A68 795B 3D1F""#,
            vec![0x0e, 0x2e, 0x4a, 0x68, 0x79, 0x5b, 0x3d, 0x1f],
        ),
        (r#"hex"DE AD_BE EF""#, vec![0xde, 0xad, 0xbe, 0xef]),
        (r#"hex"""#, Vec::new()),
    ] {
        let mut lex = Lexer::new(src);
        assert_eq!(
            lex.next_token().value,
            TokenKind::BytesLit(expected),
            "for `{src}`"
        );
        assert!(lex.take_diagnostics().is_empty(), "for `{src}`");
    }
}

#[test]
fn base64_bytes_literals() {
    for (src, expected) in [
        (r#"b64"Zg==""#, b"f".to_vec()),
        (r#"b64"Zm8=""#, b"fo".to_vec()),
        (r#"b64"Zm9v""#, b"foo".to_vec()),
        (r#"b64"3q2+_7w==""#, vec![0xde, 0xad, 0xbe, 0xef]),
        (r#"b64" Z m 8 = ""#, b"fo".to_vec()),
        (r#"b64"IGhlbGxvLA==""#, b" hello,".to_vec()),
        (r#"b64"""#, Vec::new()),
    ] {
        let mut lex = Lexer::new(src);
        assert_eq!(
            lex.next_token().value,
            TokenKind::BytesLit(expected),
            "for `{src}`"
        );
        assert!(lex.take_diagnostics().is_empty(), "for `{src}`");
    }
}

#[test]
fn bytes_prefix_immediate() {
    let mut lex = Lexer::new(r#"hex "41" b64 "Zg==" hexdata b64x"#);
    assert_ident!(lex, "hex");
    assert_eq!(lex.next_token().value, TokenKind::StringLit("41".into()));
    assert_ident!(lex, "b64");
    assert_eq!(lex.next_token().value, TokenKind::StringLit("Zg==".into()));
    assert_ident!(lex, "hexdata");
    assert_ident!(lex, "b64x");
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn bytes_literal_errors() {
    for (src, expected) in [
        (r#"hex"0g""#, "QL0041"),
        (r#"hex"abc""#, "QL0042"),
        (r#"hex"de"#, "QL0040"),
        (r#"b64"Z-""#, "QL0040"),
        (r#"b64"Zg""#, "QL0044"),
        (r#"b64"Zg===""#, "QL0044"),
        (r#"b64"Z=g=""#, "QL0044"),
        (r#"b64"Zh==""#, "QL0045"),
        (r#"b64"Zm9=""#, "QL0045"),
        (r#"b64"Zg=="#, "QL0040"),
    ] {
        let mut lex = Lexer::new(src);
        assert_eq!(lex.next_token().value, TokenKind::BytesLit(Vec::new()));
        let diags = lex.take_diagnostics();
        let codes: Vec<&str> = diags.iter().map(|d| d.code.as_ref()).collect();
        assert_eq!(codes, vec![expected], "for `{src}`");
    }
}

#[test]
fn bytes_literal_single_line() {
    let mut lex = Lexer::new("hex\"41\n\"");
    assert_eq!(lex.next_token().value, TokenKind::BytesLit(Vec::new()));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn punctuation() {
    let mut lex = Lexer::new("{}[](),;.+*%-");
    assert_token!(lex, TokenKind::LBrace);
    assert_token!(lex, TokenKind::RBrace);
    assert_token!(lex, TokenKind::LBracket);
    assert_token!(lex, TokenKind::RBracket);
    assert_token!(lex, TokenKind::LParen);
    assert_token!(lex, TokenKind::RParen);
    assert_token!(lex, TokenKind::Comma);
    assert_token!(lex, TokenKind::Semicolon);
    assert_token!(lex, TokenKind::Dot);
    assert_token!(lex, TokenKind::Plus);
    assert_token!(lex, TokenKind::Star);
    assert_token!(lex, TokenKind::Percent);
    assert_token!(lex, TokenKind::Minus);
    assert_eof!(lex);
}

#[test]
fn triple_dot() {
    let mut lex = Lexer::new(".. ... .0...");
    assert_token!(lex, TokenKind::Dot);
    assert_token!(lex, TokenKind::Dot);
    assert_token!(lex, TokenKind::TripleDot);
    assert_token!(lex, TokenKind::Dot);
    assert_eq!(lex.next_token().value, TokenKind::IntLit(0));
    assert_token!(lex, TokenKind::TripleDot);
    assert_eof!(lex);
}

#[test]
fn as_question() {
    let mut lex = Lexer::new("as? as ?Int");
    assert_token!(lex, TokenKind::AsQuestion);
    assert_token!(lex, TokenKind::As);
    assert_token!(lex, TokenKind::Question(1));
    assert_token!(lex, TokenKind::Int);
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn two_char_operators() {
    let mut lex = Lexer::new("== != <= >= && || :: ?.");
    assert_token!(lex, TokenKind::EqualsEquals);
    assert_token!(lex, TokenKind::BangEquals);
    assert_token!(lex, TokenKind::LessEquals);
    assert_token!(lex, TokenKind::GreaterEquals);
    assert_token!(lex, TokenKind::AmpAmp);
    assert_token!(lex, TokenKind::PipePipe);
    assert_token!(lex, TokenKind::DoubleColon);
    assert_token!(lex, TokenKind::QuestionDot(1));
    assert_eof!(lex);
}

#[test]
fn question_dot_depth() {
    let mut lex = Lexer::new("?. ??. ????. foo??.1");
    assert_token!(lex, TokenKind::QuestionDot(1));
    assert_token!(lex, TokenKind::QuestionDot(2));
    assert_token!(lex, TokenKind::QuestionDot(4));
    assert_ident!(lex, "foo");
    assert_token!(lex, TokenKind::QuestionDot(2));
    assert_eq!(lex.next_token().value, TokenKind::IntLit(1));
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn question_depth() {
    for (source, depth) in [("?", 1), ("??", 2), ("????", 4)] {
        let mut lex = Lexer::new(source);
        assert_eq!(lex.next_token().value, TokenKind::Question(depth));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty(), "{source}");
    }
    let mut lex = Lexer::new("??x");
    assert_token!(lex, TokenKind::Question(2));
    assert_ident!(lex, "x");
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn single_char_operator_forms() {
    let mut lex = Lexer::new("= ! < >");
    assert_token!(lex, TokenKind::Equals);
    assert_token!(lex, TokenKind::Bang);
    assert_token!(lex, TokenKind::Less);
    assert_token!(lex, TokenKind::Greater);
    assert_eof!(lex);
}

#[test]
fn ampersand_ref_logical_and() {
    let mut lex = Lexer::new("& && &&&");
    assert_token!(lex, TokenKind::Ampersand);
    assert_token!(lex, TokenKind::AmpAmp);
    assert_token!(lex, TokenKind::AmpAmp);
    assert_token!(lex, TokenKind::Ampersand);
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn lone_pipe_is_error() {
    let mut lex = Lexer::new("a | b");
    assert_ident!(lex, "a");
    assert_ident!(lex, "b");
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn lone_colon() {
    let mut lex = Lexer::new("a : b");
    assert_ident!(lex, "a");
    assert_token!(lex, TokenKind::Colon);
    assert_ident!(lex, "b");
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn invalid_char_is_error() {
    let mut lex = Lexer::new("a \0 b");
    assert_ident!(lex, "a");
    assert_ident!(lex, "b");
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn postdot_digits_lex_as_ints() {
    let mut lex = Lexer::new("trio.2.1");
    assert_ident!(lex, "trio");
    assert_token!(lex, TokenKind::Dot);
    assert_token!(lex, TokenKind::IntLit(2));
    assert_token!(lex, TokenKind::Dot);
    assert_token!(lex, TokenKind::IntLit(1));
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn postdot_float_shape_stays_split() {
    let mut lex = Lexer::new("a.1.5");
    assert_ident!(lex, "a");
    assert_token!(lex, TokenKind::Dot);
    assert_token!(lex, TokenKind::IntLit(1));
    assert_token!(lex, TokenKind::Dot);
    assert_token!(lex, TokenKind::IntLit(5));
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn dollar_is_own_token() {
    let mut lex = Lexer::new("[$ > 0]");
    assert_token!(lex, TokenKind::LBracket);
    assert_token!(lex, TokenKind::Dollar(1));
    assert_token!(lex, TokenKind::Greater);
    assert_token!(lex, TokenKind::IntLit(0));
    assert_token!(lex, TokenKind::RBracket);
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn dollar_token_count() {
    let mut lex = Lexer::new("$$ $$$ $");
    assert_token!(lex, TokenKind::Dollar(2));
    assert_token!(lex, TokenKind::Dollar(3));
    assert_token!(lex, TokenKind::Dollar(1));
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn peek_peek2_token_lookahead() {
    let mut lex = Lexer::new("users true false");
    assert_eq!(lex.peek().value, TokenKind::Ident("users".into()));
    assert_eq!(lex.peek2().value, TokenKind::True);
    assert_eq!(lex.peek().value, TokenKind::Ident("users".into()));
    assert_eq!(lex.next_token().value, TokenKind::Ident("users".into()));
    assert_eq!(lex.peek().value, TokenKind::True);
    assert_eq!(lex.peek2().value, TokenKind::False);
    assert_eq!(lex.next_token().value, TokenKind::True);
    assert_eq!(lex.peek2().value, TokenKind::Eof);
}

#[test]
fn span_offsets() {
    let mut lex = Lexer::new("  foo  ");
    let tok = lex.next_token();
    assert_eq!(tok.span.start, 2);
    assert_eq!(tok.span.end, 5);
    assert!(matches!(tok.value, TokenKind::Ident(_)));
}

#[test]
fn span_of_keyword() {
    let mut lex = Lexer::new("in");
    let tok = lex.next_token();
    assert_eq!(tok.span.start, 0);
    assert_eq!(tok.span.end, 2);
    assert_eq!(tok.value, TokenKind::In);
}

#[test]
fn int_literals() {
    let mut lex = Lexer::new("42 0 007");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::IntLit(42));
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::IntLit(0));
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::IntLit(7));
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn int_overflow_is_error() {
    let mut lex = Lexer::new("9223372036854775808");
    assert_eof!(lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn decimal_literals() {
    let mut lex = Lexer::new("3.14 42.0");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DecLit(dec!(3.14)));
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DecLit(dec!(42.0)));
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn decimal_exponents() {
    let mut lex = Lexer::new("1e10 1e-3 123.456e2 123.456e-2 1E5");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DecLit(dec!(10000000000)));
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DecLit(dec!(0.001)));
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DecLit(dec!(12345.6)));
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DecLit(dec!(1.23456)));
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DecLit(dec!(100000)));
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn float_literals() {
    let mut lex = Lexer::new("4.14f 42f 1e10f 0.5f");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::FloatLit(4.14));
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::FloatLit(42.0));
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::FloatLit(1e10));
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::FloatLit(0.5));
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn float_infinity() {
    let mut lex = Lexer::new("1e400f");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::FloatLit(f64::INFINITY));
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn incomplete_exponent_is_error() {
    let mut lex = Lexer::new("1e");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::IntLit(1));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn incomplete_exponent_with_sign_is_error() {
    let mut lex = Lexer::new("1.5e+");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DecLit(dec!(1.5)));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn double_dot_in_number_is_error() {
    let mut lex = Lexer::new("1.2.3");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DecLit(dec!(1.2)));
    assert_token!(lex, TokenKind::Dot);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::IntLit(3));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn number_stops_at_method_dot() {
    let mut lex = Lexer::new("1.to_string() 3.14.to_string()");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::IntLit(1));
    assert_token!(lex, TokenKind::Dot);
    assert_ident!(lex, "to_string");
    assert_token!(lex, TokenKind::LParen);
    assert_token!(lex, TokenKind::RParen);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DecLit(dec!(3.14)));
    assert_token!(lex, TokenKind::Dot);
    assert_ident!(lex, "to_string");
    assert_token!(lex, TokenKind::LParen);
    assert_token!(lex, TokenKind::RParen);
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn decimal_bankers_rounding_round_down() {
    let mut lex = Lexer::new("1.00000000000000000000000000005");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::DecLit(dec!(1.0000000000000000000000000000))
    );
    let diags = lex.take_diagnostics();
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].severity, grove_types::Severity::Warning);
}

#[test]
fn decimal_bankers_rounding_round_up() {
    let mut lex = Lexer::new("1.00000000000000000000000000015");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::DecLit(dec!(1.0000000000000000000000000002))
    );
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn decimal_rounding_carry() {
    let mut lex = Lexer::new("9.99999999999999999999999999995");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::DecLit(dec!(10.000000000000000000000000000))
    );
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn decimal_underflow() {
    let mut lex = Lexer::new("1e-29");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::DecLit(dec!(0.0000000000000000000000000000))
    );
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn decimal_overflow_exponent_is_error() {
    let mut lex = Lexer::new("1e29");
    assert_eof!(lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn decimal_overflow_is_error() {
    let mut lex = Lexer::new("99999999999999999999999999999");
    assert_eof!(lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn decimal_max() {
    let mut lex = Lexer::new("79228162514264337593543950335.0");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::DecLit(dec!(79228162514264337593543950335))
    );
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn decimal_over_max_is_error() {
    let mut lex = Lexer::new("79228162514264337593543950336e0");
    assert_eof!(lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn decimal_scale_28() {
    let mut lex = Lexer::new("0.0000000000000000000000000001");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::DecLit(dec!(0.0000000000000000000000000001))
    );
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn decimal_normalizes_trailing_zeros() {
    let mut lex = Lexer::new("42.0 1.2300 0.0000000000000000000000000001");
    let TokenKind::DecLit(d) = lex.next_token().value else {
        panic!("expected DecLit");
    };
    assert_eq!(d, "42".parse().unwrap());
    assert_eq!(d.scale(), 0);
    let TokenKind::DecLit(d) = lex.next_token().value else {
        panic!("expected DecLit");
    };
    assert_eq!(d, "1.23".parse().unwrap());
    assert_eq!(d.scale(), 2);
    let TokenKind::DecLit(d) = lex.next_token().value else {
        panic!("expected DecLit");
    };
    assert_eq!(d, "0.0000000000000000000000000001".parse().unwrap());
    assert_eq!(d.scale(), 28);
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn decimal_rounds_whole_fraction_away() {
    let mut lex = Lexer::new("70000000000000000000000000000.5000000000000000000000000000");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::DecLit(dec!(70000000000000000000000000000))
    );
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn decimal_overflow_integer_part_too_large() {
    let mut lex = Lexer::new("99999999999999999999999999999.5");
    assert_eof!(lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32, ns: u32) -> DateTime<Utc> {
    let date = NaiveDate::from_ymd_opt(y, mo, d).unwrap();
    let time = NaiveTime::from_hms_nano_opt(h, mi, s, ns).unwrap();
    DateTime::<Utc>::from_naive_utc_and_offset(date.and_time(time), Utc)
}

fn assert_instant(src: &str, expected: DateTime<Utc>) {
    let mut lex = Lexer::new(src);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::InstantLit(expected), "for `{src}`");
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty(), "for `{src}`");
}

fn drain(lex: &mut Lexer) {
    while !matches!(lex.next_token().value, TokenKind::Eof) {}
}

#[test]
fn instant_date_forms() {
    assert_instant("@2026", utc(2026, 1, 1, 0, 0, 0, 0));
    assert_instant("@2026-02", utc(2026, 2, 1, 0, 0, 0, 0));
    assert_instant("@2026-02-16", utc(2026, 2, 16, 0, 0, 0, 0));
    assert_instant("@2026-02-16_14:30", utc(2026, 2, 16, 14, 30, 0, 0));
    assert_instant("@2026-02-16_14:30:00", utc(2026, 2, 16, 14, 30, 0, 0));
    assert_instant(
        "@2026-02-16_14:30:00.123456789",
        utc(2026, 2, 16, 14, 30, 0, 123456789),
    );
    assert_instant(
        "@2026-02-16_14:30:00.5",
        utc(2026, 2, 16, 14, 30, 0, 500000000),
    );
}

#[test]
fn instant_leap_year() {
    assert_instant("@2024-02-29", utc(2024, 2, 29, 0, 0, 0, 0));
}

#[test]
fn instant_year_time() {
    assert_instant("@2024_14:30", utc(2024, 1, 1, 14, 30, 0, 0));
}

#[test]
fn instant_negative_and_long_years() {
    assert_instant("@-0044", utc(-44, 1, 1, 0, 0, 0, 0));
    assert_instant("@-10000", utc(-10000, 1, 1, 0, 0, 0, 0));
    assert_instant("@20240", utc(20240, 1, 1, 0, 0, 0, 0));
    assert_instant("@-2024-01-15", utc(-2024, 1, 15, 0, 0, 0, 0));
}

#[test]
fn instant_named_forms() {
    let mut lex = Lexer::new("@now @today");
    assert_token!(lex, TokenKind::Now);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::Today(None));
    assert_eof!(lex);
}

#[test]
fn instant_today_with_time() {
    let mut lex = Lexer::new("@today_08:52:00.000");
    let tok = lex.next_token();
    let expect = NaiveTime::from_hms_opt(8, 52, 0).unwrap();
    assert_eq!(tok.value, TokenKind::Today(Some(expect)));
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn instant_unix_epoch() {
    assert_instant(
        "@unix_1767225600",
        DateTime::<Utc>::from_timestamp(1767225600, 0).unwrap(),
    );
}

#[test]
fn instant_unix_fraction() {
    assert_instant(
        "@unix_1767225600.123",
        DateTime::<Utc>::from_timestamp(1767225600, 123000000).unwrap(),
    );
}

#[test]
fn instant_unix_signed() {
    assert_instant(
        "@unix_-123.456",
        DateTime::<Utc>::from_timestamp(-123, 456000000).unwrap(),
    );
    assert_instant(
        "@unix_+123",
        DateTime::<Utc>::from_timestamp(123, 0).unwrap(),
    );
}

#[test]
fn instant_stops_at_method_dot() {
    let mut lex = Lexer::new("@now.year() @2024-01-15.year()");
    assert_token!(lex, TokenKind::Now);
    assert_token!(lex, TokenKind::Dot);
    assert_ident!(lex, "year");
    assert_token!(lex, TokenKind::LParen);
    assert_token!(lex, TokenKind::RParen);
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::InstantLit(utc(2024, 1, 15, 0, 0, 0, 0))
    );
    assert_token!(lex, TokenKind::Dot);
    assert_ident!(lex, "year");
    assert_token!(lex, TokenKind::LParen);
    assert_token!(lex, TokenKind::RParen);
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty());
}

#[test]
fn instant_invalid_month() {
    let mut lex = Lexer::new("@2026-13");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_invalid_day() {
    let mut lex = Lexer::new("@2024-02-30");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

fn assert_instant_error(src: &str, code: &str, start: usize, end: usize) {
    let mut lex = Lexer::new(src);
    drain(&mut lex);
    let diags = lex.take_diagnostics();
    assert_eq!(diags.len(), 1, "for `{src}`");
    assert_eq!(&diags[0].code[..], code, "for `{src}`");
    assert_eq!(
        diags[0].labels[0].span,
        Span::new(start, end),
        "for `{src}`"
    );
}

#[test]
fn instant_year_out_of_range() {
    assert_instant_error("@300000-01-01", "QL0024", 1, 7);
    assert_instant_error("@262143-06-15", "QL0024", 1, 7);
    assert_instant_error("@-300000-06-15", "QL0024", 2, 8);
    assert_instant_error("@-262144-01-01", "QL0024", 2, 8);
}

#[test]
fn instant_year_range_boundaries() {
    use chrono::Datelike;
    let (lo, hi) = (NaiveDate::MIN.year(), NaiveDate::MAX.year());
    assert_instant(&format!("@{hi}-12-31"), utc(hi, 12, 31, 0, 0, 0, 0));
    assert_instant(&format!("@{lo}-01-01"), utc(lo, 1, 1, 0, 0, 0, 0));
    let over = format!("@{}-01-01", hi + 1);
    assert_instant_error(&over, "QL0024", 1, 1 + (hi + 1).to_string().len());
    let under = format!("@{}-01-01", lo - 1);
    let digits = (lo - 1).unsigned_abs().to_string().len();
    assert_instant_error(&under, "QL0024", 2, 2 + digits);
}

#[test]
fn instant_bad_day() {
    assert_instant_error("@2024-02-30", "QL0026", 9, 11);
    assert_instant_error("@2023-02-29", "QL0026", 9, 11);
    assert_instant_error("@2024-01-00", "QL0026", 9, 11);
}

#[test]
fn instant_invalid_hour() {
    let mut lex = Lexer::new("@2026-10-20_25:00");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_invalid_minute() {
    let mut lex = Lexer::new("@2026-10-20_12:60");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_invalid_second() {
    let mut lex = Lexer::new("@2026-10-20_14:30:60");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_missing_time_colon() {
    let mut lex = Lexer::new("@2026-10-20_1430");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_invalid_name() {
    let mut lex = Lexer::new("@foo");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_name_with_suffix() {
    let mut lex = Lexer::new("@todayx");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_unix_missing_seconds() {
    let mut lex = Lexer::new("@unix_");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_unix_overflow() {
    let mut lex = Lexer::new("@unix_99999999999999999999");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_stray_suffix() {
    let mut lex = Lexer::new("@2024x");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_short_year() {
    let mut lex = Lexer::new("@202 @-1");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 2);
}

#[test]
fn instant_fraction_rounds_with_carry() {
    let mut lex = Lexer::new("@2026-10-16_14:30:00.9999999995");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::InstantLit(utc(2026, 10, 16, 14, 30, 1, 0))
    );
    let diags = lex.take_diagnostics();
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].severity, grove_types::Severity::Warning);
}

#[test]
fn instant_fraction_rounding_day_rollover() {
    let mut lex = Lexer::new("@2026-10-16_23:59:59.9999999995");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::InstantLit(utc(2026, 10, 17, 0, 0, 0, 0))
    );
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_fraction_bankers_half_even() {
    let mut lex = Lexer::new("@2026-10-16_14:30:00.1234567895");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::InstantLit(utc(2026, 10, 16, 14, 30, 0, 123456790))
    );
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn instant_unix_fraction_rounding_carry() {
    let mut lex = Lexer::new("@unix_1.9999999995");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::InstantLit(DateTime::<Utc>::from_timestamp(2, 0).unwrap())
    );
    assert_eq!(lex.take_diagnostics().len(), 1);
}

fn assert_duration(src: &str, expected: TimeDelta) {
    let mut lex = Lexer::new(src);
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DurationLit(expected), "for `{src}`");
    assert_eof!(lex);
    assert!(lex.take_diagnostics().is_empty(), "for `{src}`");
}

#[test]
fn duration_single_units() {
    assert_duration("#30d", TimeDelta::days(30));
    assert_duration("#5m", TimeDelta::minutes(5));
    assert_duration("#1h", TimeDelta::hours(1));
    assert_duration("#0.5s", TimeDelta::milliseconds(500));
    assert_duration("#1y", TimeDelta::days(365));
    assert_duration("#1w", TimeDelta::weeks(1));
}

#[test]
fn duration_compound() {
    assert_duration(
        "#1y2w3d4h5m6.789s",
        TimeDelta::days(365)
            + TimeDelta::weeks(2)
            + TimeDelta::days(3)
            + TimeDelta::hours(4)
            + TimeDelta::minutes(5)
            + TimeDelta::seconds(6)
            + TimeDelta::nanoseconds(789_000_000),
    );
}

#[test]
fn duration_any_order() {
    assert_duration("#5s2m", TimeDelta::minutes(2) + TimeDelta::seconds(5));
}

#[test]
fn duration_fractional_seconds_precision() {
    assert_duration(
        "#1.234567891s",
        TimeDelta::seconds(1) + TimeDelta::nanoseconds(234_567_891),
    );
}

#[test]
fn duration_fraction_rounds_with_warning() {
    let mut lex = Lexer::new("#0.1234567895s");
    let tok = lex.next_token();
    assert_eq!(
        tok.value,
        TokenKind::DurationLit(TimeDelta::nanoseconds(123_456_790))
    );
    let diags = lex.take_diagnostics();
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].severity, grove_types::Severity::Warning);
}

#[test]
fn duration_fraction_carry_to_seconds() {
    let mut lex = Lexer::new("#1.9999999995s");
    let tok = lex.next_token();
    assert_eq!(tok.value, TokenKind::DurationLit(TimeDelta::seconds(2)));
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn duration_duplicate_unit() {
    let mut lex = Lexer::new("#1h1h");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn duration_fraction_on_non_second() {
    let mut lex = Lexer::new("#1.5m");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}

#[test]
fn duration_missing_unit() {
    let mut lex = Lexer::new("#30 #30x");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 2);
}

#[test]
fn duration_expected() {
    let mut lex = Lexer::new("# #x");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 2);
}

#[test]
fn duration_overflow() {
    let mut lex = Lexer::new("#99999999999999999999y");
    drain(&mut lex);
    assert_eq!(lex.take_diagnostics().len(), 1);
}
