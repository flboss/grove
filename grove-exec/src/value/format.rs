use std::fmt;

use grove_schema::validated::{Field, ValidatedSchema};

use super::time::fmt_duration;
use crate::value::{Scalar, Val};

impl fmt::Display for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scalar::Int(n) => write!(f, "{n}"),
            Scalar::Float(x) => write!(f, "{x:?}f"),
            Scalar::Dec(d) => write!(f, "{d}{}", if d.scale() == 0 { ".0" } else { "" }),
            Scalar::String(s) => write!(f, "{s:?}"),
            Scalar::Bytes(bytes) => {
                write!(f, "hex\"")?;
                for byte in bytes {
                    write!(f, "{byte:02x}")?;
                }
                write!(f, "\"")
            }
            Scalar::Bool(b) => write!(f, "{b}"),
            Scalar::Instant(t) => write!(
                f,
                "{}",
                t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
            ),
            Scalar::Duration(d) => write!(f, "{}", fmt_duration(d)),
        }
    }
}

impl fmt::Display for Val {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Val::None => write!(f, "none"),
            Val::Some(inner) => write!(f, "some({inner})"),
            Val::Scalar(s) => write!(f, "{s}"),
            Val::Enum(id, variant) => write!(f, "enum({}:{})", id.index(), variant),
            Val::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Val::Tuple(items) => {
                write!(f, "(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, ")")
            }
            Val::Record(fields) => {
                write!(f, "{{")?;
                for (i, (name, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}: {value}")?;
                }
                write!(f, "}}")
            }
            Val::Ref { field, .. } => {
                write!(f, "ref({}:{})", field.struct_id().index(), field.local())
            }
            Val::RowRef(origin) => {
                write!(f, "&Struct#{}@{}", origin.struct_id.index(), origin.key.0)
            }
        }
    }
}

impl Val {
    pub fn formatter<'v, 's>(&'v self, schema: &'s ValidatedSchema) -> ValFormatter<'v, 's> {
        ValFormatter::new(self, schema)
    }

    fn fmt_with(&self, schema: &ValidatedSchema, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Val::None => write!(f, "none"),
            Val::Some(inner) => write!(f, "some({})", inner.formatter(schema)),
            Val::Scalar(s) => write!(f, "{s}"),
            Val::Enum(id, variant) => {
                if let Some(enum_def) = schema.enums.get(id.index())
                    && let Some(variant_def) = enum_def.variants.get(*variant)
                {
                    return write!(f, "{}::{}", enum_def.name, variant_def.name);
                }
                write!(f, "enum({}:{})", id.index(), variant)
            }
            Val::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item.formatter(schema))?;
                }
                write!(f, "]")
            }
            Val::Tuple(items) => {
                write!(f, "(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item.formatter(schema))?;
                }
                write!(f, ")")
            }
            Val::Record(fields) => {
                write!(f, "{{")?;
                for (i, (name, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{name}: {}", value.formatter(schema))?;
                }
                write!(f, "}}")
            }
            Val::Ref { field, parent } => {
                let Some(Field::Ref {
                    target,
                    optional,
                    is_list,
                    owning,
                    ..
                }) = schema.field(*field)
                else {
                    return write!(f, "ref({}:{})", field.struct_id().index(), field.local());
                };
                let Some(target) = schema.structs.get(target.index()) else {
                    return write!(f, "ref({}:{})", field.struct_id().index(), field.local());
                };
                if !owning {
                    write!(f, "&")?;
                }
                match (is_list, optional) {
                    (true, _) => write!(f, "List<{}>", target.name)?,
                    (false, true) => write!(f, "?{}", target.name)?,
                    (false, false) => write!(f, "{}", target.name)?,
                }
                write!(f, "#{}", parent.0)
            }
            Val::RowRef(origin) => {
                let Some(struct_) = schema.structs.get(origin.struct_id.index()) else {
                    return write!(f, "&Struct#{}@{}", origin.struct_id.index(), origin.key.0);
                };
                write!(f, "&{}#{}", struct_.name, origin.key.0)
            }
        }
    }
}

pub struct ValFormatter<'v, 's> {
    value: &'v Val,
    schema: &'s ValidatedSchema,
}

impl<'v, 's> ValFormatter<'v, 's> {
    pub(crate) fn new(value: &'v Val, schema: &'s ValidatedSchema) -> Self {
        ValFormatter { value, schema }
    }
}

impl<'v, 's> fmt::Display for ValFormatter<'v, 's> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt_with(self.schema, f)
    }
}
