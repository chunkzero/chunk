//! Version 1 segment: `CHUNKLOG`, a little-endian `u32` version and `u64` epoch,
//! then one record per entry: `u32` length, `u32` CRC-32 of the payload, payload.
//! An entry payload is its `u64` sequence and revision, a `u32` statement count,
//! each statement as a `u32` length and UTF-8 text, then the changeset.

use crate::{Error, Result};

const MAGIC: &[u8; 8] = b"CHUNKLOG";
const VERSION: u32 = 1;

/// One replicated write transaction.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub sequence: u64,
    /// The environment revision after this transaction.
    pub revision: u64,
    /// Schema DDL, run before the changeset is applied.
    pub statements: Vec<String>,
    /// An SQLite session changeset of every row change.
    pub changeset: Vec<u8>,
}

impl Entry {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::with_capacity(20 + self.changeset.len());
        bytes.extend(self.sequence.to_le_bytes());
        bytes.extend(self.revision.to_le_bytes());
        bytes.extend(length(self.statements.len())?.to_le_bytes());
        for statement in &self.statements {
            bytes.extend(length(statement.len())?.to_le_bytes());
            bytes.extend(statement.as_bytes());
        }
        bytes.extend(&self.changeset);
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader(bytes);
        let sequence = reader.u64()?;
        let revision = reader.u64()?;
        let count = reader.u32()?;
        let statements = (0..count)
            .map(|_| {
                let length = reader.u32()?;
                String::from_utf8(reader.take(length)?.to_vec()).map_err(|_| Error::Corrupt("log statement"))
            })
            .collect::<Result<_>>()?;
        Ok(Self { sequence, revision, statements, changeset: reader.0.to_vec() })
    }
}

/// Frames encoded entries into a segment.
pub(crate) fn encode<'a>(epoch: u64, entries: impl IntoIterator<Item = &'a [u8]>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend(MAGIC);
    bytes.extend(VERSION.to_le_bytes());
    bytes.extend(epoch.to_le_bytes());
    for entry in entries {
        bytes.extend(length(entry.len())?.to_le_bytes());
        bytes.extend(crc32fast::hash(entry).to_le_bytes());
        bytes.extend(entry);
    }
    Ok(bytes)
}

pub(crate) fn decode(epoch: u64, bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut reader = Reader(bytes);
    if reader.take(8)? != MAGIC || reader.u32()? != VERSION || reader.u64()? != epoch {
        return Err(Error::Corrupt("unsupported log segment"));
    }
    let mut entries = Vec::new();
    while !reader.0.is_empty() {
        let length = reader.u32()?;
        let checksum = reader.u32()?;
        let payload = reader.take(length)?;
        if crc32fast::hash(payload) != checksum {
            return Err(Error::Corrupt("log segment checksum"));
        }
        entries.push(Entry::decode(payload)?);
    }
    Ok(entries)
}

fn length(length: usize) -> Result<u32> {
    u32::try_from(length).map_err(|_| Error::Capacity)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, length: impl TryInto<usize>) -> Result<&'a [u8]> {
        let length = length.try_into().map_err(|_| Error::Corrupt("truncated log"))?;
        if self.0.len() < length {
            return Err(Error::Corrupt("truncated log"));
        }
        let (head, tail) = self.0.split_at(length);
        self.0 = tail;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32> {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(self.take(4_usize)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64> {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(self.take(8_usize)?);
        Ok(u64::from_le_bytes(bytes))
    }
}

pub(crate) fn snapshot_key(epoch: u64, sequence: u64) -> String {
    format!("epochs/{epoch:020}/snapshots/{sequence:020}.db")
}

pub(crate) fn claim_key(epoch: u64) -> String {
    format!("{}/claim", epoch_key(epoch))
}

/// The directory holding every object of `epoch`.
pub(crate) fn epoch_key(epoch: u64) -> String {
    format!("epochs/{epoch:020}")
}

pub(crate) fn segment_key(epoch: u64, first: u64, last: u64) -> String {
    format!("epochs/{epoch:020}/segments/{first:020}-{last:020}.log")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Object {
    Claim { epoch: u64 },
    Snapshot { epoch: u64, sequence: u64 },
    Segment { epoch: u64, first: u64, last: u64, size: u64 },
}

impl Object {
    /// Ignores keys this format did not write.
    pub fn parse(key: &str, size: u64) -> Option<Self> {
        let (epoch, rest) = key.strip_prefix("epochs/")?.split_once('/')?;
        let epoch = epoch.parse().ok()?;
        if rest == "claim" {
            return Some(Self::Claim { epoch });
        }
        if let Some(name) = rest.strip_prefix("snapshots/") {
            return Some(Self::Snapshot { epoch, sequence: name.strip_suffix(".db")?.parse().ok()? });
        }
        let (first, last) = rest.strip_prefix("segments/")?.strip_suffix(".log")?.split_once('-')?;
        Some(Self::Segment { epoch, first: first.parse().ok()?, last: last.parse().ok()?, size })
    }

    pub fn epoch(&self) -> u64 {
        match self {
            Self::Claim { epoch } | Self::Snapshot { epoch, .. } | Self::Segment { epoch, .. } => *epoch,
        }
    }
}
