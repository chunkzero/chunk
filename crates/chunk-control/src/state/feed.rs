//! Recent claim and move changes, so a subscriber can resume from the last log position it saw.

use std::{collections::VecDeque, sync::Mutex};

use chunk_store::Write;
use tokio::sync::watch;

use super::{
    Generation,
    store::{CLAIMS, MOVES},
};

/// Changes retained for resuming subscribers; older positions must reload current state.
const RETAINED: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    Claims,
    Moves,
}

/// One claim or move row that a commit wrote. Read current state for its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub position: Generation,
    pub table: Table,
    /// The claim or move operation ID.
    pub id: String,
    pub removed: bool,
}

pub(super) type Row = (Table, String, bool);

/// The claim and move rows among `writes`, whose IDs start with `scope`.
pub(super) fn rows(writes: &[Write], scope: &str) -> Vec<Row> {
    writes
        .iter()
        .filter_map(|write| {
            let table = match write.key.table.as_str() {
                CLAIMS => Table::Claims,
                MOVES => Table::Moves,
                _ => return None,
            };
            let id = write.key.id.strip_prefix(scope).unwrap_or(&write.key.id);
            Some((table, id.to_owned(), write.value.is_none()))
        })
        .collect()
}

pub(crate) struct Feed {
    history: Mutex<History>,
    position: watch::Sender<Generation>,
}

struct History {
    changes: VecDeque<Change>,
    /// Every change up to and including this position has been evicted.
    floor: Generation,
}

impl Feed {
    pub(super) fn new(position: Generation) -> Self {
        let history = History { changes: VecDeque::new(), floor: position };
        Self { history: Mutex::new(history), position: watch::Sender::new(position) }
    }

    /// Records the claim and move rows a commit wrote, then announces its position.
    pub(super) fn record(&self, position: Generation, rows: Vec<Row>) {
        let changes = rows.into_iter().map(|(table, id, removed)| Change { position, table, id, removed });
        if let Ok(mut history) = self.history.lock() {
            history.changes.extend(changes);
            while history.changes.len() > RETAINED {
                if let Some(evicted) = history.changes.pop_front() {
                    history.floor = evicted.position;
                }
            }
        }
        self.position.send_replace(position);
    }

    /// Forgets history after state was reloaded, so earlier positions resynchronize.
    pub(super) fn reset(&self, position: Generation) {
        if let Ok(mut history) = self.history.lock() {
            *history = History { changes: VecDeque::new(), floor: position };
        }
        self.position.send_replace(position);
    }

    /// Changes after `position`, or `None` when that position is outside retained history: from another epoch, too
    /// old, or not yet committed.
    pub fn after(&self, position: Generation) -> Option<Vec<Change>> {
        let history = self.history.lock().ok()?;
        let current = *self.position.borrow();
        if position.epoch != current.epoch || position < history.floor || position > current {
            return None;
        }
        Some(history.changes.iter().filter(|change| change.position > position).cloned().collect())
    }

    pub fn subscribe(&self) -> watch::Receiver<Generation> {
        self.position.subscribe()
    }
}
