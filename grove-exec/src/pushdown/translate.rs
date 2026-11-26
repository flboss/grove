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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pushdown::tests::*;
    use crate::value::Scalar;

    #[test]
    fn matcher_bytes_predicates() {
        let (file, plan, schema) = typed_and_plan(r#"blobs[data == hex"0001"]"#);
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(PushExpr::Field(field(&schema, "Blob", "data"))),
                rhs: Box::new(PushExpr::Literal(Scalar::Bytes(vec![0x00, 0x01]))),
            }]
        );

        let (file, plan, schema) = typed_and_plan(r#"blobs[data > hex"00"]"#);
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::Cmp {
                op: CmpOp::Gt,
                lhs: Box::new(PushExpr::Field(field(&schema, "Blob", "data"))),
                rhs: Box::new(PushExpr::Literal(Scalar::Bytes(vec![0x00]))),
            }]
        );

        let (file, plan, schema) = typed_and_plan(r#"blobs[data in (hex"0001", hex"00ff")]"#);
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::In {
                lhs: Box::new(PushExpr::Field(field(&schema, "Blob", "data"))),
                items: vec![
                    PushExpr::Literal(Scalar::Bytes(vec![0x00, 0x01])),
                    PushExpr::Literal(Scalar::Bytes(vec![0x00, 0xff])),
                ],
            }]
        );
    }

    #[test]
    fn bytes_sort_and_unpushed() {
        let (file, plan, schema) = typed_and_plan("blobs.sort_over(data)");
        assert_eq!(
            root_constraints(&plan, &file).sort,
            vec![SortKey {
                field: field(&schema, "Blob", "data"),
                desc: false,
            }]
        );

        for source in [
            r#"blobs[data.len() > 1]"#,
            r#"blobs[("x" as Bytes) == data]"#,
        ] {
            let (file, plan, _schema) = typed_and_plan(source);
            let constraints = root_constraints(&plan, &file);
            assert!(
                constraints.filters.is_empty(),
                "{source} should not push: {constraints:?}"
            );
        }
    }

    #[test]
    fn matcher_predicate_translation() {
        let (file, plan, schema) = typed_and_plan("users[active]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::Field(field(&schema, "User", "active"))]
        );

        let (file, plan, schema) = typed_and_plan("users[id in (1, 2, 3)]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::In {
                lhs: Box::new(PushExpr::Field(field(&schema, "User", "id"))),
                items: vec![
                    PushExpr::Literal(Scalar::Int(1)),
                    PushExpr::Literal(Scalar::Int(2)),
                    PushExpr::Literal(Scalar::Int(3)),
                ],
            }]
        );

        let (file, plan, schema) = typed_and_plan("users[created > @now]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::Cmp {
                op: CmpOp::Gt,
                lhs: Box::new(PushExpr::Field(field(&schema, "User", "created"))),
                rhs: Box::new(PushExpr::Literal(Scalar::Instant(FIXED_NOW))),
            }]
        );

        let (file, plan, schema) = typed_and_plan("users[name.contains(\"A\")]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::StringOp {
                op: StringOp::Contains,
                base: Box::new(PushExpr::Field(field(&schema, "User", "name"))),
                needle: Box::new(PushExpr::Literal(Scalar::String("A".to_string()))),
            }]
        );
    }

    #[test]
    fn matcher_enum_predicates() {
        let (file, plan, schema) = typed_and_plan("settings[priority == Priority::High]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::Cmp {
                op: CmpOp::Eq,
                lhs: Box::new(PushExpr::Field(field(&schema, "Setting", "priority"))),
                rhs: Box::new(PushExpr::Literal(Scalar::String("high".to_string()))),
            }]
        );

        let (file, plan, schema) = typed_and_plan("settings[level in (Level::High, Level::Low)]");
        assert_eq!(
            root_constraints(&plan, &file).filters,
            vec![PushExpr::In {
                lhs: Box::new(PushExpr::Field(field(&schema, "Setting", "level"))),
                items: vec![
                    PushExpr::Literal(Scalar::Int(2)),
                    PushExpr::Literal(Scalar::Int(-1)),
                ],
            }]
        );

        let (file, plan, schema) = typed_and_plan("settings[priority == Priority::High] { name }");
        assert_eq!(root_constraints(&plan, &file).filters.len(), 1);
        assert_fields(&plan, &schema, "Setting", &["name", "priority"]);
    }

    #[test]
    fn matcher_unsupported_predicates() {
        let sources = [
            "users[nickname == some(\"A\")]",
            "users[profile?.bio?.len() == some(6)]",
            "users[pos.0 > 1.5f]",
            "users[orders.len() > 1]",
            "users[id + 1 > 2]",
            "users[name.to_upper() == \"ADA\"]",
        ];
        for source in sources {
            let (file, plan, _schema) = typed_and_plan(source);
            let constraints = root_constraints(&plan, &file);
            assert!(
                constraints.filters.is_empty(),
                "{source} should not push: {constraints:?}"
            );
        }
    }

    #[test]
    fn sort_keys() {
        let (file, plan, schema) = typed_and_plan("users.sort_over(name, desc id)");
        assert_eq!(
            root_constraints(&plan, &file).sort,
            vec![
                SortKey {
                    field: field(&schema, "User", "name"),
                    desc: false,
                },
                SortKey {
                    field: field(&schema, "User", "id"),
                    desc: true,
                },
            ]
        );

        let (file, plan, schema) = typed_and_plan("users.sort_desc_over(id)");
        assert_eq!(
            root_constraints(&plan, &file).sort,
            vec![SortKey {
                field: field(&schema, "User", "id"),
                desc: true,
            }]
        );
    }
}
