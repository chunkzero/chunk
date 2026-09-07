use std::collections::BTreeSet;

use crate::{Commit, DocumentKey, Error, Result};

const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_WRITES: usize = 256;

pub(super) struct Prepared<'a> {
    pub writes: Vec<(&'a DocumentKey, Option<String>)>,
    pub result: String,
}

impl<'a> Prepared<'a> {
    pub fn new(commit: &'a Commit) -> Result<Self> {
        commit.operation.validate()?;
        if commit.writes.len() > MAX_WRITES {
            return Err(Error::Invalid("too many writes"));
        }
        let mut keys = BTreeSet::new();
        let mut writes = Vec::with_capacity(commit.writes.len());
        for write in &commit.writes {
            write.key.validate()?;
            if !keys.insert(&write.key) {
                return Err(Error::Invalid("duplicate document write"));
            }
            let json = write.value.as_ref().map(serde_json::to_string).transpose()?;
            if json.as_ref().is_some_and(|value| value.len() > MAX_DOCUMENT_BYTES) {
                return Err(Error::Capacity);
            }
            writes.push((&write.key, json));
        }
        let result = serde_json::to_string(&commit.result)?;
        if result.len() > MAX_DOCUMENT_BYTES {
            return Err(Error::Capacity);
        }
        Ok(Self { writes, result })
    }
}
