use grove_query::ast::{BinaryOp, SortDir, UnaryOp};
use grove_query::methods::MethodId;
use grove_query::{IdentBinding, QueryType, TypedExpr, TypedExprKind, TypedMethodArg};
use grove_schema::validated::{Field, FieldId, ScalarType, StructId, ValueType};

use super::builder::PlanBuilder;
use crate::storage::{CmpOp, PushExpr, SortKey, StringOp};
use crate::value::literal::{decode_literal, eval_type_constant};
use crate::value::{Val, enum_variant_scalar};

impl<'a> PlanBuilder<'a> {
    pub(super) fn translate_sort(
        &self,
        args: &[TypedMethodArg],
        source: StructId,
        default_desc: bool,
    ) -> Option<Vec<SortKey>> {
        if args.is_empty() {
            return None;
        }
        args.iter()
            .map(|arg| {
                let TypedExprKind::Ident {
                    binding: IdentBinding::Field(field),
                    depth: 0,
                    ..
                } = &arg.expr.kind
                else {
                    return None;
                };
                self.scalar_field(*field, source)?;
                let desc = arg
                    .direction
                    .as_ref()
                    .map_or(default_desc, |direction| direction.value == SortDir::Desc);
                Some(SortKey {
                    field: *field,
                    desc,
                })
            })
            .collect()
    }

    pub(super) fn scalar_field(&self, field: FieldId, source: StructId) -> Option<ScalarType> {
        if field.struct_id() != source {
            return None;
        }
        match self.schema.field(field)? {
            Field::Value {
                ty: ValueType::Scalar(scalar),
                columns,
                ..
            } if columns.len() == 1 => Some(*scalar),
            Field::Value {
                ty: ValueType::Enum(id),
                columns,
                ..
            } if columns.len() == 1 => self
                .schema
                .enums
                .get(id.index())
                .map(|enum_def| enum_def.underlying.scalar_type()),
            _ => None,
        }
    }

    pub(super) fn push_scalar_type(&self, ty: &QueryType) -> Option<ScalarType> {
        match ty {
            QueryType::Scalar(scalar) => Some(*scalar),
            QueryType::Enum(id) => self
                .schema
                .enums
                .get(id.index())
                .map(|enum_def| enum_def.underlying.scalar_type()),
            _ => None,
        }
    }

    /// Attempt to translate an expression into backend-agnostic simple PushExpr
    pub(super) fn translate_expr(&self, expr: &TypedExpr, source: StructId) -> Option<PushExpr> {
        match &expr.kind {
            TypedExprKind::Literal(literal) => match decode_literal(literal, self.now) {
                Val::Scalar(scalar) => Some(PushExpr::Literal(scalar)),
                _ => None,
            },
            TypedExprKind::TypeConstant { ty, name } => {
                match eval_type_constant(ty.value, name.value) {
                    Val::Scalar(scalar) => Some(PushExpr::Literal(scalar)),
                    _ => None,
                }
            }
            TypedExprKind::EnumVariant { enum_id, variant } => {
                let enum_def = self.schema.enums.get(enum_id.index())?;
                enum_variant_scalar(enum_def, *variant).map(PushExpr::Literal)
            }
            TypedExprKind::Ident {
                binding: IdentBinding::Field(field),
                depth: 0,
                ..
            } => {
                self.scalar_field(*field, source)?;
                Some(PushExpr::Field(*field))
            }
            TypedExprKind::Binary { op, lhs, rhs } => match op.value {
                BinaryOp::Eq
                | BinaryOp::Ne
                | BinaryOp::Lt
                | BinaryOp::Le
                | BinaryOp::Gt
                | BinaryOp::Ge => {
                    let op = match op.value {
                        BinaryOp::Eq => CmpOp::Eq,
                        BinaryOp::Ne => CmpOp::Ne,
                        BinaryOp::Lt => CmpOp::Lt,
                        BinaryOp::Le => CmpOp::Le,
                        BinaryOp::Gt => CmpOp::Gt,
                        BinaryOp::Ge => CmpOp::Ge,
                        _ => unreachable!(),
                    };
                    Some(PushExpr::Cmp {
                        op,
                        lhs: Box::new(self.translate_expr(lhs, source)?),
                        rhs: Box::new(self.translate_expr(rhs, source)?),
                    })
                }
                BinaryOp::And => Some(PushExpr::And(
                    Box::new(self.translate_expr(lhs, source)?),
                    Box::new(self.translate_expr(rhs, source)?),
                )),
                BinaryOp::Or => Some(PushExpr::Or(
                    Box::new(self.translate_expr(lhs, source)?),
                    Box::new(self.translate_expr(rhs, source)?),
                )),
                BinaryOp::In => {
                    let translated_lhs = self.translate_expr(lhs, source)?;
                    let scalar = self.push_scalar_type(&lhs.ty)?;
                    let TypedExprKind::Tuple { elements } = &rhs.kind else {
                        return None;
                    };
                    if elements.is_empty() {
                        return None;
                    }
                    let mut items = Vec::with_capacity(elements.len());
                    for element in elements {
                        if self.push_scalar_type(&element.ty)? != scalar {
                            return None;
                        }
                        let PushExpr::Literal(value) = self.translate_expr(element, source)? else {
                            return None;
                        };
                        items.push(PushExpr::Literal(value));
                    }
                    Some(PushExpr::In {
                        lhs: Box::new(translated_lhs),
                        items,
                    })
                }
                _ => None,
            },
            TypedExprKind::Unary { op, expr: inner } if op.value == UnaryOp::Not => {
                Some(PushExpr::Not(Box::new(self.translate_expr(inner, source)?)))
            }
            TypedExprKind::Method {
                method,
                base,
                args,
                opt_depth: 0,
                ..
            } => {
                let QueryType::Scalar(ScalarType::String) = &base.ty else {
                    return None;
                };
                match method {
                    MethodId::StringContains
                    | MethodId::StringStartsWith
                    | MethodId::StringEndsWith
                        if args.len() == 1 =>
                    {
                        let op = match method {
                            MethodId::StringContains => StringOp::Contains,
                            MethodId::StringStartsWith => StringOp::StartsWith,
                            MethodId::StringEndsWith => StringOp::EndsWith,
                            _ => unreachable!(),
                        };
                        Some(PushExpr::StringOp {
                            op,
                            base: Box::new(self.translate_expr(base, source)?),
                            needle: Box::new(self.translate_expr(&args[0].expr, source)?),
                        })
                    }
                    MethodId::StringLen if args.is_empty() => Some(PushExpr::StringLen(Box::new(
                        self.translate_expr(base, source)?,
                    ))),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}
