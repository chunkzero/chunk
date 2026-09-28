//! A stream's messages: changes split to fit, each charged its encoded bytes against the send budget.

use super::{
    super::{MESSAGE_BYTES, errors, transport::PREFIX_BYTES},
    Changes,
};
use chunk_backend::{SendBudget, SendCharge};
use chunk_proto::sync::v1::{Position, Update, entry::State};
use prost::Message;
use std::collections::VecDeque;
use tokio::time::Instant;

/// Room left in each message for its position, flags and stream ID.
const HEADER_BYTES: usize = 2048;

/// An update handed to the client, and the charge for its encoded bytes.
pub(super) struct Part {
    pub update: Update,
    pub charge: Option<SendCharge>,
}

/// Changes split into messages, each with its encoded size, and the charge covering them once the budget had room.
#[derive(Default)]
pub(super) struct Batch {
    parts: VecDeque<(Update, usize)>,
    /// The changes' own charge until the parts are charged, then theirs.
    charge: Option<SendCharge>,
    charged: bool,
    /// When the budget first had no room for the parts, until it had.
    refused: Option<Instant>,
}

impl Batch {
    pub fn new(mut changes: Changes) -> Self {
        let charge = changes.charge.take();
        let parts = split(changes.into_update()).into_iter().map(|part| {
            let bytes = part.encoded_len() + PREFIX_BYTES;
            (part, bytes)
        });
        Self { parts: parts.collect(), charge, charged: false, refused: None }
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// When the budget first had no room for the parts, while it still has none and so none was taken.
    pub fn refused(&self) -> Option<Instant> {
        self.refused
    }

    /// Merges `newer` changes into parts the budget has no room for, which keep when it first refused them.
    pub fn merge(&mut self, mut newer: Changes) {
        debug_assert!(self.refused.is_some(), "only refused parts take newer changes");
        let mut changes = Changes::default();
        for (part, _) in self.parts.drain(..) {
            changes.merge(part);
        }
        let charge = match (self.charge.take(), newer.charge.take()) {
            (Some(mut charge), Some(newer)) => {
                charge.merge(newer);
                Some(charge)
            }
            (charge, newer) => charge.or(newer),
        };
        changes.merge(newer.into_update());
        *self = Self { charge, refused: self.refused, ..Self::new(changes) };
    }

    /// The next part with its share of the charge, charging all the parts first, or none once they're all taken.
    pub fn next(&mut self, budget: &SendBudget) -> chunk_backend::Result<Option<Part>> {
        if self.parts.is_empty() {
            return Ok(None);
        }
        if !self.charged {
            let bytes = self.parts.iter().map(|(_, bytes)| bytes).sum();
            let charged = match &mut self.charge {
                Some(charge) => charge.resize(bytes),
                None => budget.charge(bytes).map(|charge| self.charge = Some(charge)),
            };
            if let Err(failure) = charged {
                self.refused.get_or_insert_with(Instant::now);
                return Err(failure);
            }
            self.charged = true;
            self.refused = None;
        }
        let (update, bytes) = self.parts.pop_front().expect("a part");
        let charge = if self.parts.is_empty() {
            self.charge.take()
        } else {
            self.charge.as_mut().and_then(|charge| charge.split(bytes))
        };
        Ok(Some(Part { update, charge }))
    }
}

/// Splits `update` so each part fits in a message. Every part but the last is `continued`, and only the first
/// carries the snapshot flag and stream ID.
fn split(mut update: Update) -> Vec<Update> {
    if update.encoded_len() <= MESSAGE_BYTES {
        return vec![update];
    }
    let upserts = std::mem::take(&mut update.upserts);
    let removed = std::mem::take(&mut update.removed);
    let position = update.position;
    let mut parts = vec![update];
    let mut size = 0;
    for mut entry in upserts {
        if entry.encoded_len() + 16 > MESSAGE_BYTES - HEADER_BYTES {
            let error = errors::invalid("the entry exceeds the 16 MiB message limit");
            entry.state = Some(State::Error(error));
        }
        let bytes = entry.encoded_len() + 16;
        room(&mut parts, &mut size, bytes, position).upserts.push(entry);
    }
    for key in removed {
        let bytes = key.len() + 16;
        room(&mut parts, &mut size, bytes, position).removed.push(key);
    }
    parts
}

/// The last part if it has room for `bytes` more, or else a new part continuing it.
fn room<'a>(parts: &'a mut Vec<Update>, size: &mut usize, bytes: usize, position: Option<Position>) -> &'a mut Update {
    let last = parts.last_mut().expect("a part");
    if *size + bytes > MESSAGE_BYTES - HEADER_BYTES && !(last.upserts.is_empty() && last.removed.is_empty()) {
        last.continued = true;
        parts.push(Update { position, ..Update::default() });
        *size = 0;
    }
    *size += bytes;
    parts.last_mut().expect("a part")
}
