
use super::builder::{AliasGen, SelectBuilder, SelectItem, SqlBinOp, SqlExpr};
use super::model::ParamValue;
use crate::ast::{BinaryOp, ConstantName, Literal, TypeName, UnaryOp};
use crate::typecheck::types::{IdentBinding, QueryType, TypedExpr, TypedExprKind, TypedMethodArg};
use grove_schema::validated::{Field, ScalarType, ValidatedSchema};

pub struct Context<'s> {
    pub schema: &'s ValidatedSchema,
    pub aliases: AliasGen,
}

impl<'s> Context<'s> {
    pub fn new(schema: &'s ValidatedSchema) -> Self {
        Context {
            schema,
            aliases: AliasGen::default(),
        }
    }
}

#[allow(dead_code)]
pub enum ParentLink {
    Root { root_idx: usize },
}

#[allow(dead_code)]
pub struct Level {
    pub builder: SelectBuilder,
    pub element: QueryType,
    pub parent: ParentLink,
}

pub fn scalar(ctx: &mut Context, expr: &TypedExpr) -> SqlExpr {
    match &expr.kind {
        TypedExprKind::Literal(lit) => scalar_literal(&lit.value),
        TypedExprKind::TypeConstant { ty, name } => scalar_constant(ty.value, name.value),
        TypedExprKind::Ident { binding, depth, .. } => scalar_ident(ctx, binding, *depth),
        TypedExprKind::Field { .. } => todo!("M3 singular navigation / record access"),
        TypedExprKind::Method {
            base,
            name,
            args,
            optional,
        } => scalar_method(ctx, base, name, args, *optional),
        TypedExprKind::Binary { op, lhs, rhs } => scalar_binary(ctx, op, lhs, rhs),
        TypedExprKind::Unary { op, expr } => scalar_unary(op, scalar(ctx, expr), &expr.ty),
        TypedExprKind::Cast { expr, ty } => scalar_cast(scalar(ctx, expr), &expr.ty, ty.value),
        TypedExprKind::If { arms, default } => SqlExpr::Case {
            arms: arms
                .iter()
                .map(|(cond, then)| (scalar(ctx, cond), scalar(ctx, then)))
                .collect(),
            else_: Box::new(scalar(ctx, default)),
        },
        TypedExprKind::Some { value } => scalar(ctx, value),
        TypedExprKind::Tuple { .. }
        | TypedExprKind::Array { .. }
        | TypedExprKind::Struct { .. }
        | TypedExprKind::Projection { .. } => {
            unreachable!(
                "scalar() received {} {}: callers dispatch on node type, composite/row nodes never enter scalar demand",
                kind_name(expr),
                expr.ty
            )
        }
    }
}

fn kind_name(expr: &TypedExpr) -> &'static str {
    match &expr.kind {
        TypedExprKind::Literal(_) => "literal",
        TypedExprKind::Ident { .. } => "ident",
        TypedExprKind::Field { .. } => "field access",
        TypedExprKind::Method { .. } => "method call",
        TypedExprKind::Binary { .. } => "binary operator",
        TypedExprKind::Unary { .. } => "unary operator",
        TypedExprKind::Cast { .. } => "cast",
        TypedExprKind::If { .. } => "if",
        TypedExprKind::Some { .. } => "some()",
        TypedExprKind::Tuple { .. } => "tuple",
        TypedExprKind::Array { .. } => "array",
        TypedExprKind::Struct { .. } => "struct literal",
        TypedExprKind::Projection { .. } => "projection",
        TypedExprKind::TypeConstant { .. } => "type constant",
    }
}

pub fn rows(ctx: &mut Context, expr: &TypedExpr) -> Level {
    let _ = (ctx, expr);
    todo!("M2 flat pipelines: row demand");
}

pub fn lower_query(ctx: &mut Context, result: &TypedExpr) -> (SelectBuilder, Vec<String>) {
    if result.ty.is_scalar() {
        let mut builder = SelectBuilder::new();
        let output = ctx.aliases.generic();
        builder.select = vec![SelectItem::Expr {
            expr: scalar(ctx, result),
            output: output.clone(),
        }];
        return (builder, vec![output]);
    }
    let _ = rows(ctx, result);
    unreachable!("rows() is an M2 stub and diverges");
}

fn scalar_literal(lit: &Literal) -> SqlExpr {
    match lit {
        Literal::Int(n) => SqlExpr::Param(ParamValue::Int(*n)),
        Literal::Float(f) => SqlExpr::Param(ParamValue::Float(*f)),
        Literal::Dec(d) => SqlExpr::Param(ParamValue::Dec(*d)),
        Literal::String(s) => SqlExpr::Param(ParamValue::String(s.clone())),
        Literal::Bool(b) => SqlExpr::Param(ParamValue::Bool(*b)),
        Literal::Instant(t) => SqlExpr::Param(ParamValue::Instant(*t)),
        Literal::Duration(d) => SqlExpr::Param(ParamValue::Duration(*d)),
        Literal::Now => SqlExpr::Func {
            name: "$$now".to_string(),
            args: vec![],
        },
        Literal::Today(None) => SqlExpr::Func {
            name: "$$today".to_string(),
            args: vec![SqlExpr::Param(ParamValue::Null)],
        },
        Literal::Today(Some(time)) => {
            let midnight =
                chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("midnight is a valid time");
            SqlExpr::Func {
                name: "$$today".to_string(),
                args: vec![SqlExpr::Param(ParamValue::Duration(
                    time.signed_duration_since(midnight),
                ))],
            }
        }
        Literal::None => SqlExpr::Param(ParamValue::Null),
    }
}

fn scalar_constant(ty: TypeName, name: ConstantName) -> SqlExpr {
    let value = match (ty, name) {
        (TypeName::Int, ConstantName::Min) => ParamValue::Int(i64::MIN),
        (TypeName::Int, ConstantName::Max) => ParamValue::Int(i64::MAX),
        (TypeName::Float, ConstantName::Min) => ParamValue::Float(f64::MIN),
        (TypeName::Float, ConstantName::Max) => ParamValue::Float(f64::MAX),
        (TypeName::Dec, ConstantName::Min) => ParamValue::Dec(rust_decimal::Decimal::MIN),
        (TypeName::Dec, ConstantName::Max) => ParamValue::Dec(rust_decimal::Decimal::MAX),
    };
    SqlExpr::Param(value)
}

fn scalar_ident(ctx: &mut Context, binding: &IdentBinding, depth: usize) -> SqlExpr {
    match binding {
        IdentBinding::Root { .. } => {
            unreachable!("root idents are List-typed: scalar() only receives scalar inputs")
        }
        IdentBinding::Field(fid) => {
            let field = &ctx.schema.structs[fid.struct_id().index()].fields[fid.local()];
            match field {
                Field::Value { columns, .. } => {
                    let [column] = columns.as_slice() else {
                        unreachable!(
                            "tuple-valued field ident in scalar demand: callers dispatch tuple nodes by type"
                        )
                    };
                    SqlExpr::Column {
                        depth,
                        column: ctx.schema.column_name(*column).to_string(),
                    }
                }
                _ => {
                    unreachable!("reference/array field idents are never scalar-typed")
                }
            }
        }
        IdentBinding::ProjectedField { name } => SqlExpr::Column {
            depth,
            column: name.clone(),
        },
        IdentBinding::Prev { .. } => todo!("mutations era: prev"),
    }
}

fn scalar_method(
    ctx: &mut Context,
    base: &TypedExpr,
    name: &grove_types::Spanned<String>,
    args: &[TypedMethodArg],
    optional: bool,
) -> SqlExpr {
    let _ = optional;
    match &base.ty {
        QueryType::Scalar(s) => {
            let frag = scalar(ctx, base);
            apply_scalar_method(ctx, frag, *s, name, args)
        }
        QueryType::Optional(inner) => match inner.as_ref() {
            QueryType::Scalar(s) => {
                let frag = scalar(ctx, base);
                match name.value.as_str() {
                    "is_none" => SqlExpr::Binary {
                        op: SqlBinOp::Is,
                        lhs: Box::new(frag),
                        rhs: Box::new(SqlExpr::Param(ParamValue::Null)),
                    },
                    "is_some" => SqlExpr::Binary {
                        op: SqlBinOp::IsNot,
                        lhs: Box::new(frag),
                        rhs: Box::new(SqlExpr::Param(ParamValue::Null)),
                    },
                    "unwrap_or" => SqlExpr::Coalesce(vec![frag, scalar(ctx, one_arg(args, name))]),
                    "unwrap" => SqlExpr::Coalesce(vec![
                        frag,
                        SqlExpr::Func {
                            name: "$$panic".to_string(),
                            args: vec![SqlExpr::Param(ParamValue::String(
                                "unwrap failed: value was none".to_string(),
                            ))],
                        },
                    ]),
                    _ => apply_scalar_method(ctx, frag, *s, name, args),
                }
            }
            _ => todo!("M3 optional record/list/tuple mechanics"),
        },
        QueryType::List(_) => match name.value.as_str() {
            "len" | "sum" | "avg" | "max" | "min" | "sum_over" | "avg_over" | "max_by"
            | "min_by" => todo!("M5 aggregations: {}", name.value),
            "first" | "nth" => todo!("M3 single collapse: {}", name.value),
            "contains" => todo!("M3 flipped EXISTS over element rows"),
            other => {
                unreachable!(
                    "row-pipeline method {other} over List in scalar demand: caller dispatches by node type"
                )
            }
        },
        QueryType::Record(_) => {
            unreachable!("no methods take bare record bases: the checker rejects them")
        }
        QueryType::Tuple(_) => match name.value.as_str() {
            "contains" => {
                let against = scalar(ctx, one_arg(args, name));
                SqlExpr::In {
                    expr: Box::new(against),
                    items: tuple_elements(ctx, base),
                }
            }
            other => unreachable!("unknown tuple method {other}: the checker rejects it"),
        },
        QueryType::Void | QueryType::Unknown => {
            unreachable!("checker never emits Void/Unknown method bases")
        }
    }
}

fn one_arg<'a>(args: &'a [TypedMethodArg], name: &grove_types::Spanned<String>) -> &'a TypedExpr {
    match args {
        [arg] => &arg.expr,
        _ => unreachable!(
            "wrong arity for {}: the checker enforces Fixed arg counts",
            name.value
        ),
    }
}

fn tuple_elements(ctx: &mut Context, base: &TypedExpr) -> Vec<SqlExpr> {
    match &base.kind {
        TypedExprKind::Tuple { elements } => elements.iter().map(|e| scalar(ctx, e)).collect(),
        _ => unreachable!(
            "tuple method on non-tuple base: the checker resolves methods on inner types"
        ),
    }
}

fn apply_scalar_method(
    ctx: &mut Context,
    base: SqlExpr,
    scalar_ty: ScalarType,
    name: &grove_types::Spanned<String>,
    args: &[TypedMethodArg],
) -> SqlExpr {
    let method = name.value.as_str();
    match (scalar_ty, method) {
        (ScalarType::String, "contains") => SqlExpr::Binary {
            op: SqlBinOp::Gt,
            lhs: Box::new(SqlExpr::Func {
                name: "instr".to_string(),
                args: vec![base, scalar(ctx, one_arg(args, name))],
            }),
            rhs: Box::new(SqlExpr::Param(ParamValue::Int(0))),
        },
        (ScalarType::String, "starts_with") => SqlExpr::Binary {
            op: SqlBinOp::Eq,
            lhs: Box::new(SqlExpr::Func {
                name: "instr".to_string(),
                args: vec![base, scalar(ctx, one_arg(args, name))],
            }),
            rhs: Box::new(SqlExpr::Param(ParamValue::Int(1))),
        },
        (ScalarType::String, "ends_with") => {
            let sought = scalar(ctx, one_arg(args, name));
            SqlExpr::Binary {
                op: SqlBinOp::Eq,
                lhs: Box::new(SqlExpr::Func {
                    name: "substr".to_string(),
                    args: vec![
                        base.clone(),
                        SqlExpr::Func {
                            name: "$$add".to_string(),
                            args: vec![
                                SqlExpr::Func {
                                    name: "$$sub".to_string(),
                                    args: vec![
                                        SqlExpr::Func {
                                            name: "length".to_string(),
                                            args: vec![base],
                                        },
                                        SqlExpr::Func {
                                            name: "length".to_string(),
                                            args: vec![sought.clone()],
                                        },
                                    ],
                                },
                                SqlExpr::Param(ParamValue::Int(1)),
                            ],
                        },
                    ],
                }),
                rhs: Box::new(sought),
            }
        }
        (ScalarType::String, "to_upper") => SqlExpr::Func {
            name: "$$upper".to_string(),
            args: vec![base],
        },
        (ScalarType::String, "to_lower") => SqlExpr::Func {
            name: "$$lower".to_string(),
            args: vec![base],
        },
        (ScalarType::String, "len") => SqlExpr::Func {
            name: "length".to_string(),
            args: vec![base],
        },
        (ScalarType::Int | ScalarType::Float | ScalarType::Bool, "to_string") => SqlExpr::Cast {
            expr: Box::new(base),
            target: "TEXT",
        },
        (ScalarType::Dec, "to_string") => SqlExpr::Func {
            name: "$$dec_to_string".to_string(),
            args: vec![base],
        },
        (
            ScalarType::Instant,
            "year" | "month" | "day" | "hour" | "minute" | "second" | "weekday" | "epoch",
        ) => SqlExpr::Func {
            name: format!("$$instant_{method}"),
            args: vec![base],
        },
        (ScalarType::Instant, "to_rfc3339") => SqlExpr::Func {
            name: "$$instant_to_rfc3339".to_string(),
            args: vec![base],
        },
        (ScalarType::Duration, "as_seconds" | "as_minutes" | "as_hours" | "as_days") => {
            SqlExpr::Func {
                name: format!("$$duration_{method}"),
                args: vec![base],
            }
        }
        (ScalarType::Dec, "round_dp") => SqlExpr::Func {
            name: "$$dec_round_dp".to_string(),
            args: vec![base, scalar(ctx, one_arg(args, name))],
        },
        (ScalarType::Float, "round") => SqlExpr::Func {
            name: "$$float_round".to_string(),
            args: vec![base],
        },
        (ScalarType::Float, "floor") => SqlExpr::Func {
            name: "$$float_floor".to_string(),
            args: vec![base],
        },
        (ScalarType::Float, "ceil") => SqlExpr::Func {
            name: "$$float_ceil".to_string(),
            args: vec![base],
        },
        _ => {
            unreachable!("unknown scalar method {method} on {scalar_ty:?}: the checker rejects it")
        }
    }
}

fn scalar_binary(
    ctx: &mut Context,
    op: &grove_types::Spanned<BinaryOp>,
    lhs: &TypedExpr,
    rhs: &TypedExpr,
) -> SqlExpr {
    match op.value {
        BinaryOp::Add
        | BinaryOp::Sub
        | BinaryOp::Mul
        | BinaryOp::Div
        | BinaryOp::Rem
        | BinaryOp::Mod => {
            let name = match op.value {
                BinaryOp::Add => "$$add",
                BinaryOp::Sub => "$$sub",
                BinaryOp::Mul => "$$mul",
                BinaryOp::Div => "$$div",
                BinaryOp::Rem => "$$rem",
                _ => "$$mod",
            };
            let (left, right) = (scalar(ctx, lhs), scalar(ctx, rhs));
            match (operand_scalar(&lhs.ty), operand_scalar(&rhs.ty)) {
                (Some(l), Some(r)) if is_temporal(l) || is_temporal(r) => SqlExpr::Func {
                    name: name.to_string(),
                    args: vec![left, right, descriptor(l), descriptor(r)],
                },
                (Some(_), Some(_)) => SqlExpr::Func {
                    name: name.to_string(),
                    args: vec![left, right],
                },
                _ => unreachable!(
                    "non-scalar arithmetic operand: the checker only admits scalar pairs"
                ),
            }
        }
        BinaryOp::Eq | BinaryOp::Ne => {
            if !lhs.ty.is_scalar() || !rhs.ty.is_scalar() {
                todo!(
                    "comparison-overhaul: deep equality over {} vs {}",
                    lhs.ty,
                    rhs.ty
                );
            }
            let op = match op.value {
                BinaryOp::Eq if lhs.ty.is_optional() || rhs.ty.is_optional() => SqlBinOp::Is,
                BinaryOp::Ne if lhs.ty.is_optional() || rhs.ty.is_optional() => SqlBinOp::IsNot,
                BinaryOp::Eq => SqlBinOp::Eq,
                _ => SqlBinOp::Ne,
            };
            SqlExpr::Binary {
                op,
                lhs: Box::new(scalar(ctx, lhs)),
                rhs: Box::new(scalar(ctx, rhs)),
            }
        }
        BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => {
            if matches!(lhs.ty, QueryType::Tuple(_)) || matches!(rhs.ty, QueryType::Tuple(_)) {
                todo!(
                    "comparison-overhaul: tuple ordering over {} vs {}",
                    lhs.ty,
                    rhs.ty
                );
            }
            if !lhs.ty.is_scalar() || !rhs.ty.is_scalar() {
                unreachable!(
                    "non-scalar unordered comparison operand: the checker requires ordered operands"
                );
            }
            SqlExpr::Binary {
                op: match op.value {
                    BinaryOp::Lt => SqlBinOp::Lt,
                    BinaryOp::Gt => SqlBinOp::Gt,
                    BinaryOp::Le => SqlBinOp::Le,
                    _ => SqlBinOp::Ge,
                },
                lhs: Box::new(scalar(ctx, lhs)),
                rhs: Box::new(scalar(ctx, rhs)),
            }
        }
        BinaryOp::And | BinaryOp::Or => {
            if !lhs.ty.is_bool() || !rhs.ty.is_bool() {
                unreachable!("non-Bool logic operand: the checker requires Bool on both sides");
            }
            SqlExpr::Binary {
                op: match op.value {
                    BinaryOp::And => SqlBinOp::And,
                    _ => SqlBinOp::Or,
                },
                lhs: Box::new(scalar(ctx, lhs)),
                rhs: Box::new(scalar(ctx, rhs)),
            }
        }
        BinaryOp::In => match &rhs.kind {
            TypedExprKind::Tuple { elements } => {
                if !lhs.ty.is_scalar() || elements.iter().any(|e| !e.ty.is_scalar()) {
                    todo!("comparison-overhaul: deep `in` over {}", lhs.ty);
                }
                SqlExpr::In {
                    expr: Box::new(scalar(ctx, lhs)),
                    items: elements.iter().map(|e| scalar(ctx, e)).collect(),
                }
            }
            _ => unreachable!("non-tuple `in` RHS: the checker requires a tuple"),
        },
    }
}

fn operand_scalar(ty: &QueryType) -> Option<ScalarType> {
    match ty {
        QueryType::Scalar(s) => Some(*s),
        _ => None,
    }
}

fn is_temporal(s: ScalarType) -> bool {
    matches!(s, ScalarType::Instant | ScalarType::Duration)
}

fn descriptor(s: ScalarType) -> SqlExpr {
    match s {
        ScalarType::Instant => SqlExpr::Param(ParamValue::String("instant".to_string())),
        ScalarType::Duration => SqlExpr::Param(ParamValue::String("duration".to_string())),
        _ => SqlExpr::Param(ParamValue::Null),
    }
}

fn scalar_unary(op: &grove_types::Spanned<UnaryOp>, operand: SqlExpr, ty: &QueryType) -> SqlExpr {
    match op.value {
        UnaryOp::Not => {
            if !ty.is_bool() {
                unreachable!("non-Bool `!` operand: the checker requires Bool");
            }
            SqlExpr::Not(Box::new(operand))
        }
        UnaryOp::Neg => match operand_scalar(ty) {
            Some(ScalarType::Duration) => SqlExpr::Func {
                name: "$$neg".to_string(),
                args: vec![
                    operand,
                    SqlExpr::Param(ParamValue::String("duration".to_string())),
                ],
            },
            Some(_) => SqlExpr::Func {
                name: "$$neg".to_string(),
                args: vec![operand],
            },
            None => {
                unreachable!(
                    "non-scalar unary `-` operand: the checker admits numerics and Duration only"
                )
            }
        },
    }
}

fn scalar_cast(operand: SqlExpr, from: &QueryType, target: TypeName) -> SqlExpr {
    if matches!(from, QueryType::Scalar(ScalarType::Bool)) {
        return operand;
    }
    if !from.is_numeric() {
        unreachable!("non-numeric cast operand: the checker admits numerics and Bool→Int only");
    }
    SqlExpr::Func {
        name: "$$cast".to_string(),
        args: vec![
            operand,
            SqlExpr::Param(ParamValue::String(
                match target {
                    TypeName::Int => "int",
                    TypeName::Float => "float",
                    TypeName::Dec => "dec",
                }
                .to_string(),
            )),
        ],
    }
}
