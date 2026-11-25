use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use grove_query::{TypedMutationStmt, TypedQueryFile, TypedStatement};
use grove_schema::validated::{Field, FieldId, StructId, ValidatedSchema};
use grove_types::NodeId;

use crate::storage::FetchConstraints;

mod builder;
mod chain;
mod translate;

use builder::PlanBuilder;
use chain::Need;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeRewrite {
    /// Operation already folded into fetch constraints
    Skip,
    /// nth/find already folded; evaluate as `first`
    First,
    /// Reverse the fetched window (at final list-producing operation)
    ReverseOutput,
    /// any() folded to filtered LIMIT 1 fetch
    NonEmpty,
    /// all() folded to NOT-filtered LIMIT 1 fetch
    Empty,
}

/// Runtime rewrite annotation, with the fetch trigger which applied it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RewritePlan {
    pub rewrite: RuntimeRewrite,
    pub trigger: NodeId,
}

/// Annotation map for pushdown
#[derive(Debug, Clone, Default)]
pub struct PushdownPlan {
    fetches: HashMap<NodeId, FetchConstraints>,
    fields: HashMap<StructId, Vec<FieldId>>,
    rewrites: HashMap<NodeId, RewritePlan>,
    prefetch_skip: HashSet<FieldId>,
}

impl PushdownPlan {
    /// Analyze a typed file and build the pushdown plan
    pub fn analyze(
        file: &TypedQueryFile,
        schema: &ValidatedSchema,
        now: DateTime<Utc>,
        materialize_lets: bool,
    ) -> PushdownPlan {
        let mut builder = PlanBuilder {
            schema,
            now,
            chains: HashMap::new(),
            ref_sites: HashMap::new(),
            usage: HashMap::new(),
            rewrites: HashMap::new(),
        };
        for statement in &file.statements {
            match statement {
                TypedStatement::Let(let_stmt) => {
                    let need = if materialize_lets {
                        Need::Whole
                    } else {
                        Need::Narrow
                    };
                    builder.visit(&let_stmt.value, need);
                }
                TypedStatement::Expr(_) => {}
                TypedStatement::Mutation(mutation) => match mutation {
                    TypedMutationStmt::Insert { base, arg, .. }
                    | TypedMutationStmt::Update { base, arg, .. } => {
                        builder.visit(base, Need::Narrow);
                        builder.visit(arg, Need::Narrow);
                    }
                    TypedMutationStmt::Delete { base, .. } => {
                        builder.visit(base, Need::Narrow);
                    }
                },
            }
        }
        if let Some(result) = &file.result {
            builder.visit(result, Need::Whole);
        }
        builder.finish()
    }

    pub fn disabled(schema: &ValidatedSchema) -> PushdownPlan {
        PushdownPlan {
            fetches: HashMap::new(),
            fields: all_struct_fields(schema),
            rewrites: HashMap::new(),
            prefetch_skip: HashSet::new(),
        }
    }

    pub fn constraints_for(&self, node: NodeId) -> Option<&FetchConstraints> {
        self.fetches.get(&node)
    }

    pub fn fields_for(&self, struct_id: StructId) -> &[FieldId] {
        self.fields
            .get(&struct_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    #[cfg(test)]
    fn runtime_rewrite(&self, node: NodeId) -> Option<RuntimeRewrite> {
        self.rewrites.get(&node).map(|n| n.rewrite)
    }

    pub fn rewrite_plan(&self, node: NodeId) -> Option<RewritePlan> {
        self.rewrites.get(&node).copied()
    }

    pub fn prefetch_skip(&self) -> &HashSet<FieldId> {
        &self.prefetch_skip
    }
}

/// Non-Ref fields of every struct in declaration order
fn all_struct_fields(schema: &ValidatedSchema) -> HashMap<StructId, Vec<FieldId>> {
    schema
        .structs
        .iter()
        .enumerate()
        .map(|(index, struct_def)| {
            let struct_id = StructId::new(index);
            let fields = struct_def
                .fields
                .iter()
                .enumerate()
                .filter(|(_, field)| !matches!(field, Field::Ref { .. }))
                .map(|(local, _)| FieldId::new(struct_id, local))
                .collect();
            (struct_id, fields)
        })
        .collect()
}

#[cfg(test)]
mod tests;
