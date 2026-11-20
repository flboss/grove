use crate::typecheck::types::QueryType;
use grove_schema::validated::{EnumId, ScalarType, ValidatedSchema};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastMode {
    Infallible,
    Fallible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CastKind {
    IntToFloat,
    IntToDec,
    IntToString,
    FloatToInt,
    FloatToDec,
    FloatToString,
    DecToInt,
    DecToFloat,
    DecToString,
    BoolToInt,
    BoolToString,
    StringToBytes,
    EnumToInt(EnumId),
    EnumToString(EnumId),
    FloatToIntStrict,
    DecToIntStrict,
    FloatToDecStrict,
}

impl CastKind {
    pub fn target(self) -> QueryType {
        match self {
            CastKind::IntToFloat | CastKind::DecToFloat => QueryType::Scalar(ScalarType::Float),
            CastKind::IntToDec | CastKind::FloatToDec | CastKind::FloatToDecStrict => {
                QueryType::Scalar(ScalarType::Dec)
            }
            CastKind::IntToString
            | CastKind::FloatToString
            | CastKind::DecToString
            | CastKind::BoolToString => QueryType::Scalar(ScalarType::String),
            CastKind::FloatToInt
            | CastKind::DecToInt
            | CastKind::BoolToInt
            | CastKind::FloatToIntStrict
            | CastKind::DecToIntStrict => QueryType::Scalar(ScalarType::Int),
            CastKind::StringToBytes => QueryType::Scalar(ScalarType::Bytes),
            CastKind::EnumToInt(_) => QueryType::Scalar(ScalarType::Int),
            CastKind::EnumToString(_) => QueryType::Scalar(ScalarType::String),
        }
    }

    pub fn is_fallible(&self) -> bool {
        matches!(
            self,
            CastKind::FloatToIntStrict | CastKind::DecToIntStrict | CastKind::FloatToDecStrict
        )
    }
}

pub fn lookup(
    from: &QueryType,
    to: &QueryType,
    mode: CastMode,
    schema: &ValidatedSchema,
) -> Option<CastKind> {
    match mode {
        CastMode::Infallible => lookup_infallible(from, to, schema),
        CastMode::Fallible => lookup_fallible(from, to),
    }
}

fn lookup_infallible(
    from: &QueryType,
    to: &QueryType,
    schema: &ValidatedSchema,
) -> Option<CastKind> {
    match (from, to) {
        (QueryType::Scalar(ScalarType::Int), QueryType::Scalar(ScalarType::Float)) => {
            Some(CastKind::IntToFloat)
        }
        (QueryType::Scalar(ScalarType::Int), QueryType::Scalar(ScalarType::Dec)) => {
            Some(CastKind::IntToDec)
        }
        (QueryType::Scalar(ScalarType::Int), QueryType::Scalar(ScalarType::String)) => {
            Some(CastKind::IntToString)
        }
        (QueryType::Scalar(ScalarType::Float), QueryType::Scalar(ScalarType::Int)) => {
            Some(CastKind::FloatToInt)
        }
        (QueryType::Scalar(ScalarType::Float), QueryType::Scalar(ScalarType::Dec)) => {
            Some(CastKind::FloatToDec)
        }
        (QueryType::Scalar(ScalarType::Float), QueryType::Scalar(ScalarType::String)) => {
            Some(CastKind::FloatToString)
        }
        (QueryType::Scalar(ScalarType::Dec), QueryType::Scalar(ScalarType::Int)) => {
            Some(CastKind::DecToInt)
        }
        (QueryType::Scalar(ScalarType::Dec), QueryType::Scalar(ScalarType::Float)) => {
            Some(CastKind::DecToFloat)
        }
        (QueryType::Scalar(ScalarType::Dec), QueryType::Scalar(ScalarType::String)) => {
            Some(CastKind::DecToString)
        }
        (QueryType::Scalar(ScalarType::Bool), QueryType::Scalar(ScalarType::Int)) => {
            Some(CastKind::BoolToInt)
        }
        (QueryType::Scalar(ScalarType::Bool), QueryType::Scalar(ScalarType::String)) => {
            Some(CastKind::BoolToString)
        }
        (QueryType::Scalar(ScalarType::String), QueryType::Scalar(ScalarType::Bytes)) => {
            Some(CastKind::StringToBytes)
        }
        (QueryType::Enum(id), QueryType::Scalar(target)) => match schema
            .enums
            .get(id.index())
            .map(|def| def.underlying.scalar_type())
        {
            Some(ScalarType::Int) if *target == ScalarType::Int => Some(CastKind::EnumToInt(*id)),
            Some(ScalarType::String) if *target == ScalarType::String => {
                Some(CastKind::EnumToString(*id))
            }
            _ => None,
        },
        _ => None,
    }
}

fn lookup_fallible(from: &QueryType, to: &QueryType, schema: &ValidatedSchema) -> Option<CastKind> {
    match (from, to) {
        (QueryType::Scalar(ScalarType::Float), QueryType::Scalar(ScalarType::Int)) => {
            Some(CastKind::FloatToIntStrict)
        }
        (QueryType::Scalar(ScalarType::Dec), QueryType::Scalar(ScalarType::Int)) => {
            Some(CastKind::DecToIntStrict)
        }
        (QueryType::Scalar(ScalarType::Float), QueryType::Scalar(ScalarType::Dec)) => {
            Some(CastKind::FloatToDecStrict)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_schema::validated::EnumId;

    fn test_schema() -> ValidatedSchema {
        let (schema, diags) = grove_schema::parse_schema(
            r#"
            root tickets: Ticket;
            enum Priority { Low => "low", High => "high" }
            enum Level { Low => 0, High => 1 }
            struct Ticket { priority: Priority, level: Level }
            "#,
        );
        assert!(diags.is_empty(), "{diags:?}");
        grove_schema::validate(schema.unwrap()).0.unwrap()
    }

    #[test]
    fn infallible_scalar_casts_resolution() {
        use ScalarType::*;
        for (from, to, expected) in [
            (Int, Float, CastKind::IntToFloat),
            (Int, Dec, CastKind::IntToDec),
            (Int, String, CastKind::IntToString),
            (Float, Int, CastKind::FloatToInt),
            (Float, Dec, CastKind::FloatToDec),
            (Float, String, CastKind::FloatToString),
            (Dec, Int, CastKind::DecToInt),
            (Dec, Float, CastKind::DecToFloat),
            (Dec, String, CastKind::DecToString),
            (Bool, Int, CastKind::BoolToInt),
            (Bool, String, CastKind::BoolToString),
            (String, Bytes, CastKind::StringToBytes),
        ] {
            let kind = lookup(
                &QueryType::Scalar(from),
                &QueryType::Scalar(to),
                CastMode::Infallible,
                &test_schema(),
            );
            assert_eq!(kind, Some(expected), "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn enum_underlying_casts_resolution() {
        let schema = test_schema();
        let priority = schema.enum_by_name("Priority").unwrap().0;
        let level = schema.enum_by_name("Level").unwrap().0;
        assert_eq!(
            lookup(
                &QueryType::Enum(priority),
                &QueryType::Scalar(ScalarType::String),
                CastMode::Infallible,
                &schema,
            ),
            Some(CastKind::EnumToString(priority))
        );
        assert_eq!(
            lookup(
                &QueryType::Enum(level),
                &QueryType::Scalar(ScalarType::Int),
                CastMode::Infallible,
                &schema,
            ),
            Some(CastKind::EnumToInt(level))
        );
        assert_eq!(
            lookup(
                &QueryType::Enum(priority),
                &QueryType::Scalar(ScalarType::Int),
                CastMode::Infallible,
                &schema,
            ),
            None
        );
        assert_eq!(
            lookup(
                &QueryType::Enum(EnumId::new(99)),
                &QueryType::Scalar(ScalarType::String),
                CastMode::Infallible,
                &schema,
            ),
            None
        );
    }

    #[test]
    fn fallible_scalar_casts_resolution() {
        use ScalarType::*;
        for (from, to, expected) in [
            (Float, Int, CastKind::FloatToIntStrict),
            (Dec, Int, CastKind::DecToIntStrict),
            (Float, Dec, CastKind::FloatToDecStrict),
        ] {
            let kind = lookup(
                &QueryType::Scalar(from),
                &QueryType::Scalar(to),
                CastMode::Fallible,
                &test_schema(),
            );
            assert_eq!(kind, Some(expected), "{from:?} -> {to:?}");
        }
    }

    #[test]
    fn casts_mismatched_types_or_mode() {
        let schema = test_schema();
        assert_eq!(
            lookup(
                &QueryType::Scalar(ScalarType::String),
                &QueryType::Scalar(ScalarType::Int),
                CastMode::Infallible,
                &schema,
            ),
            None
        );
        assert_eq!(
            lookup(
                &QueryType::Scalar(ScalarType::String),
                &QueryType::Scalar(ScalarType::Int),
                CastMode::Fallible,
                &schema,
            ),
            None
        );
        assert_eq!(
            lookup(
                &QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::String))),
                &QueryType::Scalar(ScalarType::Int),
                CastMode::Fallible,
                &schema,
            ),
            None
        );
        assert_eq!(
            lookup(
                &QueryType::Scalar(ScalarType::String),
                &QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int))),
                CastMode::Infallible,
                &schema,
            ),
            None
        );
    }
}
