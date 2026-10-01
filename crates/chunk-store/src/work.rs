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

/// One batch of a backfill: the rows after `cursor`, with the fields their migration's `to` reads, ready to be
/// transformed outside the store. `complete` means no row follows the batch.
#[derive(Debug, Clone)]
pub struct Backfill {
    pub migration: String,
    pub table: String,
    pub cursor: Option<String>,
    pub rows: Vec<BackfillRow>,
    pub complete: bool,
}

/// A row as a backfill read it. It is skipped when committed if its revision has moved since.
#[derive(Debug, Clone)]
pub struct BackfillRow {
    pub id: String,
    pub revision: u64,
    /// The transform's input: the row's `_id` and the fields `to` reads.
    pub input: Value,
}

impl Backfill {
    /// Keeps the first `rows` rows; the rest are read again by the next batch.
    pub fn shrink(&mut self, rows: usize) {
        if rows < self.rows.len() {
            self.rows.truncate(rows);
            self.complete = false;
        }
    }

    /// The error for a transform that fails on `row`.
    #[must_use]
    pub fn failure(&self, row: &str, reason: &str) -> crate::Error {
        crate::Error::Migration(format!(
            "migration {} failed to backfill {}: row {row}: {reason}",
            self.migration, self.table
        ))
    }
}

/// The first of `contracted`, the expands whose old shape the environment dropped, that changes a table in
/// `declared` and that `journal` neither contains nor replaces with a baseline. A deployment without it writes the
/// shape from before the contraction.
#[must_use]
pub fn rolled_back_past<'a>(
    contracted: impl IntoIterator<Item = &'a chunk_contract::Migration>,
    declared: &chunk_contract::DatabaseSchema,
    journal: &[chunk_contract::Migration],
) -> Option<&'a str> {
    let touched =
        contracted.into_iter().filter(|migration| migration.tables.keys().any(|table| declared.contains_key(table)));
    touched.map(|migration| migration.id.as_str()).find(|id| {
        let number = chunk_contract::migration_number(id);
        !journal.iter().any(|entry| {
            entry.id == *id
                || (entry.kind == chunk_contract::MigrationKind::Baseline
                    && chunk_contract::migration_number(&entry.id)
                        .zip(number)
                        .is_some_and(|((space, baseline), (other, n))| space == other && n <= baseline))
        })
    })
}

/// The error for a deployment that `rolled_back_past` rejects.
#[must_use]
pub fn rollback_error(migration: &str) -> crate::Error {
    crate::Error::Migration(format!(
        "rolling back past migration {migration} is no longer possible: the environment dropped its old shape"
    ))
}

/// Whether a stored field serves a deployment that declares it as `declared`. A field an active expand migration
/// adds or removes (`migrating`) is stored as optional.
#[must_use]
pub fn compatible_field(stored: &Field, declared: &Field, migrating: bool) -> bool {
    stored == declared || (migrating && stored.optional && stored.schema == declared.schema)
}
