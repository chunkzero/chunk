use std::collections::BTreeMap;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde::Deserialize;
use serde_json::Value;

use crate::{Cancellation, Key, Mode, Read, ReadHost};

pub(crate) struct Capabilities {
    pub host: Box<dyn ReadHost>,
    pub mode: Mode,
    pub cancellation: Cancellation,
    pub writes: BTreeMap<Key, Option<Value>>,
    pub calls: usize,
    pub write_bytes: BTreeMap<Key, usize>,
}

impl Capabilities {
    fn charge(&mut self) -> Result<(), JsErrorBox> {
        self.calls += 1;
        if self.cancellation.is_cancelled() || self.calls > 4096 {
            return Err(JsErrorBox::generic("Capability budget exhausted"));
        }
        Ok(())
    }
}

#[op2]
#[string]
fn op_chunk_read(state: &mut OpState, #[string] request: &str) -> Result<String, JsErrorBox> {
    let capabilities = state.borrow_mut::<Capabilities>();
    capabilities.charge()?;
    if request.len() > 4096 {
        return Err(JsErrorBox::generic("Read request exceeds size limit"));
    }
    let request: Read = serde_json::from_str(request).map_err(JsErrorBox::from_err)?;
    let value = capabilities
        .host
        .read(request, &capabilities.writes)
        .map_err(JsErrorBox::generic)?;
    let encoded = serde_json::to_string(&value).map_err(JsErrorBox::from_err)?;
    if encoded.len() > 1024 * 1024 {
        return Err(JsErrorBox::generic("Read result exceeds size limit"));
    }
    Ok(encoded)
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WriteRequest {
    Put { key: Key, value: Value },
    Delete { key: Key },
}

#[op2(fast)]
fn op_chunk_write(state: &mut OpState, #[string] request: &str) -> Result<(), JsErrorBox> {
    let capabilities = state.borrow_mut::<Capabilities>();
    capabilities.charge()?;
    if capabilities.mode != Mode::Mutation {
        return Err(JsErrorBox::generic("Query cannot write"));
    }
    if request.len() > 1024 * 1024 {
        return Err(JsErrorBox::generic("Document size limit exceeded"));
    }
    let bytes = request.len();
    let request: WriteRequest = serde_json::from_str(request).map_err(JsErrorBox::from_err)?;
    let (key, value) = match request {
        WriteRequest::Put { key, value } => (key, Some(value)),
        WriteRequest::Delete { key } => (key, None),
    };
    if key.table.is_empty()
        || key.table.len() > 64
        || !key.table.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        || key.id.is_empty()
        || key.id.len() > 256
        || key.id.contains('\0')
    {
        return Err(JsErrorBox::generic("Invalid document key"));
    }
    if capabilities.writes.len() >= 256 && !capabilities.writes.contains_key(&key) {
        return Err(JsErrorBox::generic("Write count limit exceeded"));
    }
    let total: usize = capabilities.write_bytes.values().sum();
    let previous = capabilities.write_bytes.get(&key).copied().unwrap_or(0);
    if total - previous + bytes > 8 * 1024 * 1024 {
        return Err(JsErrorBox::generic("Write byte limit exceeded"));
    }
    capabilities.write_bytes.insert(key.clone(), bytes);
    capabilities.writes.insert(key, value);
    Ok(())
}

deno_core::extension!(chunk_capabilities, ops = [op_chunk_read, op_chunk_write]);
