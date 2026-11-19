use grove_query::methods::MethodId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaseNeed {
    Narrow,
    Whole,
    Output,
}

pub(crate) fn base_need(method: MethodId) -> BaseNeed {
    use MethodId::*;

    match method {
        ListMap | ListSumOver | ListAvgOver | ListMaxBy | ListMinBy | ListSum | ListAvg
        | ListMax | ListMin | ListLen | StringLen | BytesLen | OptionalIsSome | OptionalIsNone => {
            BaseNeed::Narrow
        }
        StringContains | ListContains | StringStartsWith | StringEndsWith | StringToUpper
        | StringToLower | StringConcat | StringSplit | StringTrim | StringTrimLeft
        | StringTrimRight | InstantYear | InstantMonth | InstantDay | InstantHour
        | InstantMinute | InstantWeekday | InstantSecond | InstantEpoch | InstantToRfc3339
        | DurationAsSeconds | DurationAsMinutes | DurationAsHours | DurationAsDays | DecRoundDp
        | IntAbs | FloatAbs | DecAbs | DurationAbs | FloatIsNan | FloatIsFinite | FloatRound
        | FloatFloor | FloatCeil | ListJoin | ListUnique | ListIsUnique => BaseNeed::Whole,
        ListFilter | ListSortAsc | ListSortDesc | ListSortAscOver | ListSortDescOver | ListTake
        | ListSkip | ListFirst | ListNth | ListFlatten | ListExtend | ListUniqueOver
        | OptionalUnwrap | OptionalUnwrapOr | OptionalMap | OptionalFlatten => BaseNeed::Output,
    }
}

pub(crate) fn is_element_preserving(method: MethodId) -> bool {
    use MethodId::*;

    matches!(
        method,
        ListFilter
            | ListSortAsc
            | ListSortDesc
            | ListSortAscOver
            | ListSortDescOver
            | ListTake
            | ListSkip
            | ListFlatten
            | ListFirst
            | ListNth
            | ListUnique
            | ListUniqueOver
            | OptionalUnwrap
            | OptionalFlatten
    )
}
