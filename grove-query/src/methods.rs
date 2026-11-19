use crate::typecheck::types::QueryType;
use grove_schema::validated::ScalarType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MethodId {
    StringContains,
    StringStartsWith,
    StringEndsWith,
    StringToUpper,
    StringToLower,
    StringConcat,
    StringSplit,
    StringTrim,
    StringTrimLeft,
    StringTrimRight,
    StringLen,
    BytesLen,
    InstantYear,
    InstantMonth,
    InstantDay,
    InstantHour,
    InstantMinute,
    InstantWeekday,
    InstantSecond,
    InstantEpoch,
    InstantToRfc3339,
    DurationAsSeconds,
    DurationAsMinutes,
    DurationAsHours,
    DurationAsDays,
    DecRoundDp,
    IntAbs,
    FloatAbs,
    DecAbs,
    DurationAbs,
    FloatIsNan,
    FloatIsFinite,
    FloatRound,
    FloatFloor,
    FloatCeil,
    OptionalIsSome,
    OptionalIsNone,
    OptionalUnwrap,
    OptionalUnwrapOr,
    ListLen,
    ListFirst,
    ListNth,
    ListFlatten,
    ListExtend,
    ListContains,
    ListJoin,
    ListUnique,
    ListUniqueOver,
    ListIsUnique,
    ListFilter,
    ListMap,
    ListSum,
    ListAvg,
    ListMax,
    ListMin,
    ListSortAsc,
    ListSortDesc,
    ListSortAscOver,
    ListSortDescOver,
    ListTake,
    ListSkip,
    ListSumOver,
    ListAvgOver,
    ListMaxBy,
    ListMinBy,
}

#[derive(Debug, Clone, Copy)]
pub struct MethodDef {
    pub id: MethodId,
    pub names: &'static [&'static str],
    pub receiver: ReceiverSpec,
    pub args: ArgSpec,
    pub scope: ScopeSpec,
    pub returns: ReturnSpec,
}

#[derive(Debug, Clone, Copy)]
pub enum ReceiverSpec {
    Scalar(ScalarType),
    Optional,
    List,
    ListOfScalar(ScalarType),
    ListOfList,
    ListOfSummable,
    ListOfAverageable,
    ListOfOrdered,
}

#[derive(Debug, Clone, Copy)]
pub enum ArgSpec {
    Fixed(&'static [ArgType]),
    Scoped {
        count: ArgCount,
        constraint: fn(&QueryType) -> bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum ArgType {
    Scalar(ScalarType),
    Inner,
    ListOfInner,
}

#[derive(Debug, Clone, Copy)]
pub enum ArgCount {
    Exact(usize),
    AtLeast(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeSpec {
    None,
    ElementOfReceiver,
    FieldsOrElement,
}

#[derive(Debug, Clone, Copy)]
pub enum ReturnSpec {
    Scalar(ScalarType),
    ListOfScalar(ScalarType),
    Inner,
    OptionalInner,
    ListOfInner,
    OptionalArgument,
    ListOfArgument,
}

impl ReceiverSpec {
    fn accepts(self, ty: &QueryType) -> bool {
        match (self, ty) {
            (ReceiverSpec::Scalar(expected), QueryType::Scalar(actual)) => expected == *actual,
            (ReceiverSpec::Optional, QueryType::Optional(_)) => true,
            (ReceiverSpec::List, QueryType::List(_)) => true,
            (ReceiverSpec::ListOfScalar(expected), QueryType::List(inner)) => {
                matches!(inner.as_ref(), QueryType::Scalar(actual) if expected == *actual)
            }
            (ReceiverSpec::ListOfList, QueryType::List(inner)) => {
                matches!(inner.as_ref(), QueryType::List(_))
            }
            (ReceiverSpec::ListOfSummable, QueryType::List(inner)) => inner.is_summable(),
            (ReceiverSpec::ListOfOrdered, QueryType::List(inner)) => inner.has_defined_order(),
            (ReceiverSpec::ListOfAverageable, QueryType::List(inner)) => {
                inner.is_summable()
                    || matches!(inner.as_ref(), QueryType::Scalar(ScalarType::Instant))
            }
            _ => false,
        }
    }

    #[cfg(test)]
    fn sample(self) -> QueryType {
        use QueryType::*;
        use ScalarType::*;
        match self {
            ReceiverSpec::Scalar(scalar) => Scalar(scalar),
            ReceiverSpec::Optional => Optional(Box::new(Scalar(Int))),
            ReceiverSpec::List => List(Box::new(Scalar(Int))),
            ReceiverSpec::ListOfScalar(scalar) => List(Box::new(Scalar(scalar))),
            ReceiverSpec::ListOfList => List(Box::new(List(Box::new(Scalar(Int))))),
            ReceiverSpec::ListOfSummable
            | ReceiverSpec::ListOfOrdered
            | ReceiverSpec::ListOfAverageable => List(Box::new(Scalar(Int))),
        }
    }
}

impl MethodDef {
    pub fn fixed_arg_types(&self, receiver: &QueryType) -> Vec<QueryType> {
        match &self.args {
            ArgSpec::Fixed(args) => args
                .iter()
                .map(|arg| arg.ty(receiver))
                .collect::<Option<Vec<_>>>()
                .unwrap_or_default(),
            ArgSpec::Scoped { .. } => Vec::new(),
        }
    }

    pub fn return_type(&self, receiver: &QueryType, first_arg: Option<&QueryType>) -> QueryType {
        match self.returns {
            ReturnSpec::Scalar(scalar) => QueryType::Scalar(scalar),
            ReturnSpec::ListOfScalar(scalar) => {
                QueryType::List(Box::new(QueryType::Scalar(scalar)))
            }
            ReturnSpec::Inner => receiver_inner(receiver).clone(),
            ReturnSpec::OptionalInner => receiver_inner(receiver).clone().wrap_optional(),
            ReturnSpec::ListOfInner => QueryType::List(Box::new(receiver_inner(receiver).clone())),
            ReturnSpec::ListOfArgument => {
                QueryType::List(Box::new(first_arg.cloned().unwrap_or(QueryType::Unknown)))
            }
            ReturnSpec::OptionalArgument => first_arg
                .cloned()
                .unwrap_or(QueryType::Unknown)
                .wrap_optional(),
        }
    }

    pub fn is_scalar_method(&self) -> bool {
        matches!(self.receiver, ReceiverSpec::Scalar(_))
    }

    pub fn is_scoped(&self) -> bool {
        !matches!(self.scope, ScopeSpec::None)
    }
}

impl ArgType {
    fn ty(self, receiver: &QueryType) -> Option<QueryType> {
        match self {
            ArgType::Scalar(scalar) => Some(QueryType::Scalar(scalar)),
            ArgType::Inner => receiver_inner_opt(receiver).cloned(),
            ArgType::ListOfInner => {
                receiver_inner_opt(receiver).map(|inner| QueryType::List(Box::new(inner.clone())))
            }
        }
    }
}

fn receiver_inner(ty: &QueryType) -> &QueryType {
    receiver_inner_opt(ty).unwrap_or(ty)
}

fn receiver_inner_opt(ty: &QueryType) -> Option<&QueryType> {
    match ty {
        QueryType::Optional(inner) | QueryType::List(inner) => Some(inner),
        _ => None,
    }
}

fn is_bool(ty: &QueryType) -> bool {
    ty.is_bool()
}

fn is_summable(ty: &QueryType) -> bool {
    ty.is_summable()
}

fn is_ordered(ty: &QueryType) -> bool {
    ty.has_defined_order()
}

fn is_averageable(ty: &QueryType) -> bool {
    ty.is_summable() || matches!(ty, QueryType::Scalar(ScalarType::Instant))
}

fn any_type(_: &QueryType) -> bool {
    true
}

use ArgCount::{AtLeast, Exact};
use ArgSpec::{Fixed, Scoped};
use ArgType::{Inner, ListOfInner, Scalar as ScalarArg};
use MethodId::*;
use ReceiverSpec::{
    List, ListOfAverageable, ListOfList, ListOfOrdered, ListOfScalar, ListOfSummable, Optional,
    Scalar,
};
use ReturnSpec::{
    Inner as InnerReturn, ListOfArgument, ListOfInner as ListOfInnerReturn,
    ListOfScalar as ListOfScalarReturn, OptionalArgument, OptionalInner, Scalar as ScalarReturn,
};
use ScalarType::{Bool, Bytes, Dec, Duration, Float, Instant, Int, String};
use ScopeSpec::{ElementOfReceiver, FieldsOrElement, None as NoScope};

static METHODS: &[MethodDef] = &[
    MethodDef {
        id: StringContains,
        names: &["contains"],
        receiver: Scalar(String),
        args: Fixed(&[ScalarArg(String)]),
        scope: NoScope,
        returns: ScalarReturn(Bool),
    },
    MethodDef {
        id: StringStartsWith,
        names: &["starts_with"],
        receiver: Scalar(String),
        args: Fixed(&[ScalarArg(String)]),
        scope: NoScope,
        returns: ScalarReturn(Bool),
    },
    MethodDef {
        id: StringEndsWith,
        names: &["ends_with"],
        receiver: Scalar(String),
        args: Fixed(&[ScalarArg(String)]),
        scope: NoScope,
        returns: ScalarReturn(Bool),
    },
    MethodDef {
        id: StringToUpper,
        names: &["to_upper"],
        receiver: Scalar(String),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(String),
    },
    MethodDef {
        id: StringToLower,
        names: &["to_lower"],
        receiver: Scalar(String),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(String),
    },
    MethodDef {
        id: StringConcat,
        names: &["concat"],
        receiver: Scalar(String),
        args: Fixed(&[ScalarArg(String)]),
        scope: NoScope,
        returns: ScalarReturn(String),
    },
    MethodDef {
        id: StringSplit,
        names: &["split"],
        receiver: Scalar(String),
        args: Fixed(&[ScalarArg(String)]),
        scope: NoScope,
        returns: ListOfScalarReturn(String),
    },
    MethodDef {
        id: StringTrim,
        names: &["trim"],
        receiver: Scalar(String),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(String),
    },
    MethodDef {
        id: StringTrimLeft,
        names: &["trim_left"],
        receiver: Scalar(String),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(String),
    },
    MethodDef {
        id: StringTrimRight,
        names: &["trim_right"],
        receiver: Scalar(String),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(String),
    },
    MethodDef {
        id: StringLen,
        names: &["len"],
        receiver: Scalar(String),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Int),
    },
    MethodDef {
        id: BytesLen,
        names: &["len"],
        receiver: Scalar(Bytes),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Int),
    },
    MethodDef {
        id: InstantYear,
        names: &["year"],
        receiver: Scalar(Instant),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Int),
    },
    MethodDef {
        id: InstantMonth,
        names: &["month"],
        receiver: Scalar(Instant),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Int),
    },
    MethodDef {
        id: InstantDay,
        names: &["day"],
        receiver: Scalar(Instant),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Int),
    },
    MethodDef {
        id: InstantHour,
        names: &["hour"],
        receiver: Scalar(Instant),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Int),
    },
    MethodDef {
        id: InstantMinute,
        names: &["minute"],
        receiver: Scalar(Instant),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Int),
    },
    MethodDef {
        id: InstantWeekday,
        names: &["weekday"],
        receiver: Scalar(Instant),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Int),
    },
    MethodDef {
        id: InstantSecond,
        names: &["second"],
        receiver: Scalar(Instant),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Dec),
    },
    MethodDef {
        id: InstantEpoch,
        names: &["epoch"],
        receiver: Scalar(Instant),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Dec),
    },
    MethodDef {
        id: InstantToRfc3339,
        names: &["to_rfc3339"],
        receiver: Scalar(Instant),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(String),
    },
    MethodDef {
        id: DurationAsSeconds,
        names: &["as_seconds"],
        receiver: Scalar(Duration),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Dec),
    },
    MethodDef {
        id: DurationAsMinutes,
        names: &["as_minutes"],
        receiver: Scalar(Duration),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Dec),
    },
    MethodDef {
        id: DurationAsHours,
        names: &["as_hours"],
        receiver: Scalar(Duration),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Dec),
    },
    MethodDef {
        id: DurationAsDays,
        names: &["as_days"],
        receiver: Scalar(Duration),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Dec),
    },
    MethodDef {
        id: DecRoundDp,
        names: &["round_dp"],
        receiver: Scalar(Dec),
        args: Fixed(&[ScalarArg(Int)]),
        scope: NoScope,
        returns: ScalarReturn(Dec),
    },
    MethodDef {
        id: IntAbs,
        names: &["abs"],
        receiver: Scalar(Int),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Int),
    },
    MethodDef {
        id: FloatAbs,
        names: &["abs"],
        receiver: Scalar(Float),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Float),
    },
    MethodDef {
        id: DecAbs,
        names: &["abs"],
        receiver: Scalar(Dec),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Dec),
    },
    MethodDef {
        id: DurationAbs,
        names: &["abs"],
        receiver: Scalar(Duration),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Duration),
    },
    MethodDef {
        id: FloatIsNan,
        names: &["is_nan"],
        receiver: Scalar(Float),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Bool),
    },
    MethodDef {
        id: FloatIsFinite,
        names: &["is_finite"],
        receiver: Scalar(Float),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Bool),
    },
    MethodDef {
        id: FloatRound,
        names: &["round"],
        receiver: Scalar(Float),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Float),
    },
    MethodDef {
        id: FloatFloor,
        names: &["floor"],
        receiver: Scalar(Float),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Float),
    },
    MethodDef {
        id: FloatCeil,
        names: &["ceil"],
        receiver: Scalar(Float),
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Float),
    },
    MethodDef {
        id: OptionalIsSome,
        names: &["is_some"],
        receiver: Optional,
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Bool),
    },
    MethodDef {
        id: OptionalIsNone,
        names: &["is_none"],
        receiver: Optional,
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Bool),
    },
    MethodDef {
        id: OptionalUnwrap,
        names: &["unwrap"],
        receiver: Optional,
        args: Fixed(&[]),
        scope: NoScope,
        returns: InnerReturn,
    },
    MethodDef {
        id: OptionalUnwrapOr,
        names: &["unwrap_or"],
        receiver: Optional,
        args: Fixed(&[Inner]),
        scope: NoScope,
        returns: InnerReturn,
    },
    MethodDef {
        id: ListLen,
        names: &["len"],
        receiver: List,
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Int),
    },
    MethodDef {
        id: ListFirst,
        names: &["first"],
        receiver: List,
        args: Fixed(&[]),
        scope: NoScope,
        returns: OptionalInner,
    },
    MethodDef {
        id: ListNth,
        names: &["nth"],
        receiver: List,
        args: Fixed(&[ScalarArg(Int)]),
        scope: NoScope,
        returns: OptionalInner,
    },
    MethodDef {
        id: ListFlatten,
        names: &["flatten"],
        receiver: ListOfList,
        args: Fixed(&[]),
        scope: NoScope,
        returns: InnerReturn,
    },
    MethodDef {
        id: ListExtend,
        names: &["extend"],
        receiver: List,
        args: Fixed(&[ListOfInner]),
        scope: NoScope,
        returns: ListOfInnerReturn,
    },
    MethodDef {
        id: ListContains,
        names: &["contains"],
        receiver: List,
        args: Fixed(&[Inner]),
        scope: NoScope,
        returns: ScalarReturn(Bool),
    },
    MethodDef {
        id: ListJoin,
        names: &["join"],
        receiver: ListOfScalar(String),
        args: Fixed(&[ScalarArg(String)]),
        scope: NoScope,
        returns: ScalarReturn(String),
    },
    MethodDef {
        id: ListUnique,
        names: &["unique"],
        receiver: List,
        args: Fixed(&[]),
        scope: NoScope,
        returns: ListOfInnerReturn,
    },
    MethodDef {
        id: ListUniqueOver,
        names: &["unique_over"],
        receiver: List,
        args: Scoped {
            count: Exact(1),
            constraint: any_type,
        },
        scope: FieldsOrElement,
        returns: ListOfInnerReturn,
    },
    MethodDef {
        id: ListIsUnique,
        names: &["is_unique"],
        receiver: List,
        args: Fixed(&[]),
        scope: NoScope,
        returns: ScalarReturn(Bool),
    },
    MethodDef {
        id: ListFilter,
        names: &["filter"],
        receiver: List,
        args: Scoped {
            count: Exact(1),
            constraint: is_bool,
        },
        scope: FieldsOrElement,
        returns: ListOfInnerReturn,
    },
    MethodDef {
        id: ListMap,
        names: &["map"],
        receiver: List,
        args: Scoped {
            count: Exact(1),
            constraint: any_type,
        },
        scope: ElementOfReceiver,
        returns: ListOfArgument,
    },
    MethodDef {
        id: ListSum,
        names: &["sum"],
        receiver: ListOfSummable,
        args: Fixed(&[]),
        scope: NoScope,
        returns: OptionalInner,
    },
    MethodDef {
        id: ListAvg,
        names: &["avg"],
        receiver: ListOfAverageable,
        args: Fixed(&[]),
        scope: NoScope,
        returns: OptionalInner,
    },
    MethodDef {
        id: ListMax,
        names: &["max"],
        receiver: ListOfOrdered,
        args: Fixed(&[]),
        scope: NoScope,
        returns: OptionalInner,
    },
    MethodDef {
        id: ListMin,
        names: &["min"],
        receiver: ListOfOrdered,
        args: Fixed(&[]),
        scope: NoScope,
        returns: OptionalInner,
    },
    MethodDef {
        id: ListSortAsc,
        names: &["sort", "sort_asc"],
        receiver: ListOfOrdered,
        args: Fixed(&[]),
        scope: NoScope,
        returns: ListOfInnerReturn,
    },
    MethodDef {
        id: ListSortDesc,
        names: &["sort_desc"],
        receiver: ListOfOrdered,
        args: Fixed(&[]),
        scope: NoScope,
        returns: ListOfInnerReturn,
    },
    MethodDef {
        id: ListSortAscOver,
        names: &["sort_over", "sort_asc_over"],
        receiver: List,
        args: Scoped {
            count: AtLeast(1),
            constraint: is_ordered,
        },
        scope: FieldsOrElement,
        returns: ListOfInnerReturn,
    },
    MethodDef {
        id: ListSortDescOver,
        names: &["sort_desc_over"],
        receiver: List,
        args: Scoped {
            count: AtLeast(1),
            constraint: is_ordered,
        },
        scope: FieldsOrElement,
        returns: ListOfInnerReturn,
    },
    MethodDef {
        id: ListTake,
        names: &["take"],
        receiver: List,
        args: Fixed(&[ScalarArg(Int)]),
        scope: NoScope,
        returns: ListOfInnerReturn,
    },
    MethodDef {
        id: ListSkip,
        names: &["skip"],
        receiver: List,
        args: Fixed(&[ScalarArg(Int)]),
        scope: NoScope,
        returns: ListOfInnerReturn,
    },
    MethodDef {
        id: ListSumOver,
        names: &["sum_over"],
        receiver: List,
        args: Scoped {
            count: Exact(1),
            constraint: is_summable,
        },
        scope: FieldsOrElement,
        returns: OptionalArgument,
    },
    MethodDef {
        id: ListAvgOver,
        names: &["avg_over"],
        receiver: List,
        args: Scoped {
            count: Exact(1),
            constraint: is_averageable,
        },
        scope: FieldsOrElement,
        returns: OptionalArgument,
    },
    MethodDef {
        id: ListMaxBy,
        names: &["max_by"],
        receiver: List,
        args: Scoped {
            count: Exact(1),
            constraint: is_ordered,
        },
        scope: FieldsOrElement,
        returns: OptionalInner,
    },
    MethodDef {
        id: ListMinBy,
        names: &["min_by"],
        receiver: List,
        args: Scoped {
            count: Exact(1),
            constraint: is_ordered,
        },
        scope: FieldsOrElement,
        returns: OptionalInner,
    },
];

pub fn resolve(base: &QueryType, name: &str) -> Option<&'static MethodDef> {
    METHODS
        .iter()
        .find(|def| def.names.contains(&name) && def.receiver.accepts(base))
}

pub fn method(id: MethodId) -> &'static MethodDef {
    METHODS
        .iter()
        .find(|def| def.id == id)
        .expect("every MethodId has exactly one definition")
}

pub fn methods() -> &'static [MethodDef] {
    METHODS
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn method_ids_and_names_are_unique() {
        let mut ids = HashSet::new();
        for def in METHODS {
            assert!(ids.insert(def.id), "duplicate method id {:?}", def.id);
            assert!(!def.names.is_empty(), "method {:?} has no name", def.id);
            let mut names = HashSet::new();
            for name in def.names {
                assert!(
                    names.insert(*name),
                    "duplicate alias `{name}` for method {:?}",
                    def.id
                );
            }
        }
    }

    #[test]
    fn method_name_resolution() {
        for def in METHODS {
            for name in def.names {
                let resolved = resolve(&def.receiver.sample(), name)
                    .unwrap_or_else(|| panic!("`{name}` did not resolve"));
                assert_eq!(resolved.id, def.id, "`{name}` resolved incorrectly");
            }
        }
    }

    #[test]
    fn method_id_resolution() {
        for def in METHODS {
            assert_eq!(method(def.id).id, def.id);
        }
    }
}
