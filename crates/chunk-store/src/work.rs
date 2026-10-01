use chunk_contract::{Deployment, Field};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::IndexDefinition;

/// What an installed deployment may wait on before it is ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Work {
    /// Builds the physical index of a definition.
    Index(IndexDefinition),
    /// Applies an expand migration's `to` to every row of one of its tables, in batches ordered by ID.
    Backfill { migration: String, table: String },
    /// Drops the old shape of a finished expand migration once no resident deployment declares it.
    Drop { migration: String },
}

impl Work {
    /// Whether `deployment` waits on this work.
    #[must_use]
    pub fn blocks(&self, deployment: &Deployment) -> bool {
        match self {
            Self::Index(index) => {
                deployment.tables.get(&index.table).and_then(|table| table.indexes.get(&index.name))
                    == Some(&index.fields)
            }
            Self::Backfill { migration, .. } => {
                deployment.contracts.migrations.iter().any(|applied| &applied.id == migration)
            }
            Self::Drop { .. } => false,
        }
    }
}

/// Durable work and its progress, in units its kind defines: an index build or a drop is one unit, and a backfill
/// counts rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWork {
    pub id: u64,
    pub work: Work,
    pub done: u64,
    pub total: u64,
}

/// Maps one row, with its `_id` and the fields of the migration's previous snapshot, through migration
/// `migration`'s `to` for `table`, returning an object of the added fields. A row's result depends on nothing else.
pub type Transform<'a> = dyn FnMut(&str, &str, Value) -> Result<Value, String> + 'a;

/// Whether a stored field serves a deployment that declares it as `declared`. A field an active expand migration
/// adds or removes (`migrating`) is stored as optional.
#[must_use]
pub fn compatible_field(stored: &Field, declared: &Field, migrating: bool) -> bool {
    stored == declared || (migrating && stored.optional && stored.schema == declared.schema)
}
