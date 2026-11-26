use grove_query::{IdentBinding, TypedExpr, TypedExprKind, TypedLetStmt};

use super::composite::{eval_array, eval_projection, eval_struct, eval_tuple};
use super::context::EvalCtx;
use super::data::{
    exec_field, exec_root, resolve_base_link, resolve_ident_value, resolve_owning_values,
};
use super::methods::eval_method;
use super::operators::{eval_binary, eval_cast, eval_unary};
use crate::error::ExecError;
use crate::value::literal::{decode_literal, eval_type_constant};
use crate::value::{Scalar, Val, rewrap_optional, unwrap_optional_depth};

pub(crate) fn eval(expr: &TypedExpr, ctx: &mut EvalCtx<'_>) -> Result<Val, ExecError> {
    match &expr.kind {
        TypedExprKind::Literal(lit) => Ok(decode_literal(lit, ctx.now)),
        TypedExprKind::Ident {
            name,
            binding,
            depth,
        } => {
            match binding {
                IdentBinding::Let => {
                    ctx.env
                        .lookup(name, *depth)
                        .cloned()
                        .ok_or_else(|| ExecError::Internal {
                            message: format!("unbound variable `{}`", name.value),
                        })
                }
                IdentBinding::Root { root_idx } => exec_root(ctx, *root_idx, expr.id),
                IdentBinding::Field(field) => {
                    let value = ctx.env.lookup(name, *depth).cloned().ok_or_else(|| {
                        ExecError::Internal {
                            message: format!("unbound field `{}`", name.value),
                        }
                    })?;
                    resolve_ident_value(ctx, expr.id, *field, value)
                }
                IdentBinding::ProjectedField { .. } => ctx
                    .env
                    .lookup(name, *depth)
                    .cloned()
                    .ok_or_else(|| ExecError::Internal {
                        message: format!("unbound field `{}`", name.value),
                    }),
                IdentBinding::ListElement => {
                    ctx.env
                        .lookup(name, *depth)
                        .cloned()
                        .ok_or_else(|| ExecError::Internal {
                            message: format!("unbound element `{}`", name.value),
                        })
                }
                IdentBinding::Prev { .. } => {
                    ctx.env
                        .lookup(name, *depth)
                        .cloned()
                        .ok_or_else(|| ExecError::Internal {
                            message: format!("unbound `prev` for `{}`", name.value),
                        })
                }
            }
        }
        TypedExprKind::EnumVariant { enum_id, variant } => Ok(Val::Enum(*enum_id, *variant)),
        TypedExprKind::Some { value } => eval(value, ctx).map(|v| Val::Some(Box::new(v))),
        TypedExprKind::TypeConstant { ty, name } => Ok(eval_type_constant(ty.value, name.value)),
        TypedExprKind::If { arms, default } => {
            for (cond, value) in arms {
                match eval(cond, ctx)? {
                    Val::Scalar(Scalar::Bool(true)) => return eval(value, ctx),
                    Val::Scalar(Scalar::Bool(false)) => {}
                    _ => {
                        return Err(ExecError::Internal {
                            message: "if condition must be a Bool".to_string(),
                        });
                    }
                }
            }
            eval(default, ctx)
        }
        TypedExprKind::Binary { op, lhs, rhs } => eval_binary(op, lhs, rhs, ctx, expr.span),
        TypedExprKind::Unary { op, expr: inner } => eval_unary(op, inner, ctx, expr.span),
        TypedExprKind::RowRef { expr: inner } => {
            let value = eval(inner, ctx)?;
            eval_row_ref(value)
        }
        TypedExprKind::Field {
            base,
            name,
            opt_depth,
        } => exec_field(base, name, *opt_depth, expr.id, ctx),
        TypedExprKind::TupleAccess {
            base,
            index,
            opt_depth,
        } => eval_tuple_access(base, *index, *opt_depth, ctx),
        TypedExprKind::Method { .. } => eval_method(expr, ctx),
        TypedExprKind::Cast { expr: inner, kind } => {
            let value = eval(inner, ctx)?;
            eval_cast(*kind, value, expr.span, ctx.schema)
        }
        TypedExprKind::Tuple { elements } => eval_tuple(elements, ctx),
        TypedExprKind::Array { elements } => eval_array(elements, ctx),
        TypedExprKind::Struct { fields } => eval_struct(fields, ctx),
        TypedExprKind::Projection { base, items } => eval_projection(base, items, ctx),
    }
}

/// Convert a traceable value into row references, preserving list/optional shape
fn eval_row_ref(value: Val) -> Result<Val, ExecError> {
    match value {
        Val::Record(record) => record
            .origin
            .map(Val::RowRef)
            .ok_or_else(|| ExecError::Internal {
                message: "row reference source record has no row origin".to_string(),
            }),
        Val::Some(inner) => eval_row_ref(*inner).map(|value| Val::Some(Box::new(value))),
        Val::None => Ok(Val::None),
        Val::List(list) => list
            .items
            .into_iter()
            .map(eval_row_ref)
            .collect::<Result<Vec<_>, _>>()
            .map(Val::list),
        other => Err(ExecError::Internal {
            message: format!("row reference source is not a record, list, or optional: {other}"),
        }),
    }
}

/// Evaluate 0-based tuple element access
fn eval_tuple_access(
    base: &TypedExpr,
    index: usize,
    opt_depth: usize,
    ctx: &mut EvalCtx<'_>,
) -> Result<Val, ExecError> {
    let base = eval(base, ctx)?;
    let base = resolve_base_link(ctx, base)?;
    let (base, peeled) = unwrap_optional_depth(base, opt_depth)?;
    if peeled < opt_depth {
        return Ok(rewrap_optional(peeled, Val::None));
    }
    let element = match base {
        Val::Tuple(elements) => {
            elements
                .into_iter()
                .nth(index)
                .ok_or_else(|| ExecError::Internal {
                    message: format!("tuple index {index} out of range"),
                })?
        }
        other => {
            return Err(ExecError::Internal {
                message: format!("tuple access on non-tuple value: {other}"),
            });
        }
    };
    Ok(rewrap_optional(opt_depth, element))
}

/// Evaluate a `let` and bind its value
pub(crate) fn eval_let(stmt: &TypedLetStmt, ctx: &mut EvalCtx<'_>) -> Result<(), ExecError> {
    let mut value = eval(&stmt.value, ctx)?;
    resolve_owning_values(ctx, std::slice::from_mut(&mut value), None)?;
    ctx.env.define(ctx.interner.intern(&stmt.name.value), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::Env;
    use crate::eval::harness::{
        FIXED_NOW, NoStorage, dec, float, int, rec, run_seeded, run_src, run_with, some, str,
    };
    use crate::pushdown::PushdownPlan;
    use crate::testdata;
    use crate::value::{CollectionOrigin, Key, RowOrigin};
    use grove_query::{QueryType, RecordSource, TargetProvenance};
    use grove_types::{NodeId, Span, Spanned};
    use rust_decimal::dec;

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
    fn eval_if_scalar() {
        assert_eq!(run_src("if true { 1 } else { 2 }"), Ok(int(1)));
        assert_eq!(run_src("if false { 1 } else { 2 }"), Ok(int(2)));
        assert_eq!(
            run_src("if false { 1 } else if false { 2 } else { 3 }"),
            Ok(int(3))
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
}
