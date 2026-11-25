mod fetch;
mod field;
mod owning;
mod prefetch;
mod projection;
mod refs;

pub(super) use fetch::exec_root;
pub(super) use field::{exec_field, resolve_base_link};
pub(super) use owning::{resolve_ident_value, resolve_owning_values};
pub(super) use prefetch::{collect_owning_refs_in, owning_refs_in, prefetch_owning};
pub(super) use projection::resolve_projection_refs;

#[cfg(test)]
pub(super) use fetch::wrap_fetched;

#[cfg(test)]
mod tests;
