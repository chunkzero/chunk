use chunk_contract::Deployment;
use serde::{Deserialize, Serialize};

use crate::IndexDefinition;

/// What an installed deployment may wait on before it is ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Work {
    /// Builds the physical index of a definition.
    Index(IndexDefinition),
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
        }
    }
}

/// Durable work and its progress, in units its kind defines. An index build is one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWork {
    pub id: u64,
    pub work: Work,
    pub done: u64,
    pub total: u64,
}
