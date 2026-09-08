use std::collections::BTreeMap;

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde::Deserialize;
use serde_json::Value;

use crate::model::bounds;
use crate::{Cancellation, Key, Mode, Read, ReadHost};

pub(crate) struct Buffered {
    pub value: Option<Value>,
    bytes: usize,
}

pub(crate) struct Capabilities {
    pub generation: u32,
    pub host: Box<dyn ReadHost>,
    pub mode: Mode,
    pub cancellation: Cancellation,
    pub writes: BTreeMap<Key, Buffered>,
    pub calls: usize,
    pub write_bytes: usize,
}

impl Capabilities {
    fn charge(&mut self) -> Result<(), JsErrorBox> {
        self.calls += 1;
        if self.cancellation.is_cancelled() || self.calls > bounds::CAPABILITY_CALLS {
            return Err(JsErrorBox::generic("Capability budget exhausted"));
        }
        Ok(())
    }
}

#[op2]
#[string]
fn op_chunk_read(state: &mut OpState, generation: u32, #[string] request: &str) -> Result<String, JsErrorBox> {
    let capabilities = state
        .borrow_mut::<Option<Capabilities>>()
        .as_mut()
        .filter(|capabilities| capabilities.generation == generation)
        .ok_or_else(|| JsErrorBox::generic("Invocation capability expired"))?;
    capabilities.charge()?;
    if request.len() > bounds::READ_REQUEST_BYTES {
        return Err(JsErrorBox::generic("Read request exceeds size limit"));
    }
    let request: Read = serde_json::from_str(request).map_err(JsErrorBox::from_err)?;
    let value = match request {
        Read::Get { table, id } => {
            let key = Key { table, id };
            let base = capabilities.host.get(&key).map_err(JsErrorBox::generic)?;
            capabilities
                .writes
                .get(&key)
                .map_or(base, |write| write.value.clone())
                .unwrap_or(Value::Null)
        }
        Read::Scan { table, start, end } => {
            let mut rows: BTreeMap<_, _> = capabilities
                .host
                .scan(&table, start.as_deref(), end.as_deref())
                .map_err(JsErrorBox::generic)?
                .into_iter()
                .collect();
            for (key, write) in &capabilities.writes {
                if key.table == table
                    && start.as_deref().is_none_or(|start| key.id.as_str() >= start)
                    && end.as_deref().is_none_or(|end| key.id.as_str() < end)
                {
                    if let Some(value) = &write.value {
                        rows.insert(key.id.clone(), value.clone());
                    } else {
                        rows.remove(&key.id);
                    }
                }
            }
            serde_json::to_value(rows.into_iter().collect::<Vec<_>>()).map_err(JsErrorBox::from_err)?
        }
        Read::Index { query } => {
            if query.limit == 0 || query.limit > 1024 {
                return Err(JsErrorBox::generic("Index result limit must be 1..1024"));
            }
            let extra = capabilities
                .writes
                .keys()
                .filter(|key| key.table == query.table)
                .count();
            let candidates = chunk_contract::IndexQuery {
                limit: query.limit + extra,
                ..query.clone()
            };
            let indexed = capabilities.host.scan_index(&candidates).map_err(JsErrorBox::generic)?;
            let mut rows: BTreeMap<_, _> = indexed.rows.into_iter().collect();
            for (key, write) in &capabilities.writes {
                if key.table == query.table {
                    rows.remove(&key.id);
                    if let Some(value) = &write.value
                        && query.matches(&indexed.fields, value)
                    {
                        rows.insert(key.id.clone(), value.clone());
                    }
                }
            }
            let mut rows: Vec<_> = rows.into_iter().collect();
            rows.sort_by(|a, b| chunk_contract::IndexQuery::compare(&indexed.fields, a, b));
            rows.truncate(query.limit);
            serde_json::to_value(rows).map_err(JsErrorBox::from_err)?
        }
    };
    let encoded = serde_json::to_string(&value).map_err(JsErrorBox::from_err)?;
    if encoded.len() > bounds::JSON_BYTES {
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
fn op_chunk_write(state: &mut OpState, generation: u32, #[string] request: &str) -> Result<(), JsErrorBox> {
    let capabilities = state
        .borrow_mut::<Option<Capabilities>>()
        .as_mut()
        .filter(|capabilities| capabilities.generation == generation)
        .ok_or_else(|| JsErrorBox::generic("Invocation capability expired"))?;
    capabilities.charge()?;
    if capabilities.mode != Mode::Mutation {
        return Err(JsErrorBox::generic("Query cannot write"));
    }
    if request.len() > bounds::JSON_BYTES {
        return Err(JsErrorBox::generic("Document size limit exceeded"));
    }
    let bytes = request.len();
    let request: WriteRequest = serde_json::from_str(request).map_err(JsErrorBox::from_err)?;
    let (key, value) = match request {
        WriteRequest::Put { key, value } => (key, Some(value)),
        WriteRequest::Delete { key } => (key, None),
    };
    if key.table.is_empty()
        || key.table.len() > bounds::TABLE_BYTES
        || key.id.is_empty()
        || key.id.len() > bounds::DOCUMENT_ID_BYTES
    {
        return Err(JsErrorBox::generic("Invalid document key"));
    }
    if capabilities.writes.len() >= bounds::WRITES && !capabilities.writes.contains_key(&key) {
        return Err(JsErrorBox::generic("Write count limit exceeded"));
    }
    let total = capabilities.write_bytes;
    let previous = capabilities.writes.get(&key).map_or(0, |write| write.bytes);
    if total - previous + bytes > bounds::WRITE_BYTES {
        return Err(JsErrorBox::generic("Write byte limit exceeded"));
    }
    capabilities.write_bytes = total - previous + bytes;
    capabilities.writes.insert(key, Buffered { value, bytes });
    Ok(())
}

deno_core::extension!(chunk_capabilities, ops = [op_chunk_read, op_chunk_write]);
