use crate::ast::EnumValue as AstEnumValue;
use crate::validated::{Enum, EnumId, EnumUnderlying, EnumValue, EnumVariant};

use super::Builder;

impl<'s> Builder<'s> {
    pub(super) fn stage_enums(&mut self) {
        for def in &self.schema.enums {
            let Some(first) = def.variants.first() else {
                continue;
            };

            let underlying = match first.value.value {
                AstEnumValue::Int(_) => EnumUnderlying::Int,
                AstEnumValue::String(_) => EnumUnderlying::String,
            };

            let variants = def
                .variants
                .iter()
                .map(|variant| EnumVariant {
                    name: variant.name.value.clone(),
                    value: match &variant.value.value {
                        AstEnumValue::Int(n) => EnumValue::Int(*n),
                        AstEnumValue::String(s) => EnumValue::String(s.clone()),
                    },
                })
                .collect();

            let id = EnumId::new(self.enums.len());
            self.enum_index.insert(def.name.as_str(), id);
            self.enums.push(Enum {
                name: def.name.value.clone(),
                underlying,
                variants,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_schema;

    fn stage_enums(source: &str, f: impl FnOnce(&Builder<'_>)) {
        let (schema, parse_diags) = parse_schema(source);
        assert!(
            parse_diags.is_empty(),
            "expected no parse errors: {parse_diags:?}"
        );
        let schema = schema.expect("source should parse");
        let mut builder = Builder::new(&schema);
        builder.stage_enums();
        f(&builder);
    }

    #[test]
    fn enums_indexed_in_declaration_order() {
        stage_enums(
            r#"enum Priority { Low => "low", High => "high" }
               enum Level { Low => 0, High => 2 }"#,
            |b| {
                assert_eq!(b.enums.len(), 2);
                assert_eq!(b.enums[0].name, "Priority");
                assert_eq!(b.enums[1].name, "Level");
                assert_eq!(b.enum_index.get("Priority"), Some(&EnumId::new(0)));
                assert_eq!(b.enum_index.get("Level"), Some(&EnumId::new(1)));
            },
        );
    }

    #[test]
    fn enum_variants_and_underlying_converted() {
        stage_enums(
            r#"enum Priority { Low => "low", High => "high" }
               enum Level { Low => -1, High => 2 }"#,
            |b| {
                assert_eq!(b.enums[0].underlying, EnumUnderlying::String);
                assert_eq!(
                    b.enums[0]
                        .variants
                        .iter()
                        .map(|v| (v.name.as_str(), v.value.clone()))
                        .collect::<Vec<_>>(),
                    vec![
                        ("Low", EnumValue::String("low".into())),
                        ("High", EnumValue::String("high".into())),
                    ]
                );
                assert_eq!(b.enums[1].underlying, EnumUnderlying::Int);
                assert_eq!(
                    b.enums[1]
                        .variants
                        .iter()
                        .map(|v| v.value.clone())
                        .collect::<Vec<_>>(),
                    vec![EnumValue::Int(-1), EnumValue::Int(2)]
                );
            },
        );
    }
}
