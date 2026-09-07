use chunk_contract::Deployment;
use chunk_js::{Key, Read, ReadHost};
use chunk_store::{DocumentKey, KeyRange, Snapshot};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub(crate) enum Dependency {
    Point(DocumentKey),
    Range(KeyRange),
}

impl Dependency {
    pub(crate) fn unchanged(&self, before: &Snapshot, after: &Snapshot) -> bool {
        match self {
            Self::Point(key) => before.get(key) == after.get(key),
            Self::Range(range) => match (before.scan(range), after.scan(range)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            },
        }
    }
}

pub(crate) type Trace = Arc<Mutex<Vec<Dependency>>>;

pub(crate) struct Host {
    pub snapshot: Arc<Snapshot>,
    pub deployment: Arc<Deployment>,
    pub trace: Trace,
}

impl ReadHost for Host {
    fn read(&mut self, request: Read, overlay: &BTreeMap<Key, Option<Value>>) -> Result<Value, String> {
        let table = match &request {
            Read::Get { table, .. } | Read::Scan { table, .. } => table,
        };
        if !self.deployment.tables.contains_key(table) {
            return Err("undeclared table".into());
        }
        let (dependency, value) = match request {
            Read::Get { table, id } => {
                let key = DocumentKey::new(&table, &id).map_err(|e| e.to_string())?;
                let value = overlay
                    .get(&Key { table, id })
                    .cloned()
                    .unwrap_or_else(|| self.snapshot.get(&key).map(|doc| doc.value.clone()));
                (Dependency::Point(key), value.unwrap_or(Value::Null))
            }
            Read::Scan { table, start, end } => {
                let range = KeyRange {
                    table: table.clone(),
                    start,
                    end,
                };
                let mut rows: BTreeMap<String, Value> = self
                    .snapshot
                    .scan(&range)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|(id, doc)| (id.to_owned(), doc.value.clone()))
                    .collect();
                for (key, value) in overlay {
                    if key.table == table
                        && range.start.as_ref().is_none_or(|start| &key.id >= start)
                        && range.end.as_ref().is_none_or(|end| &key.id < end)
                    {
                        if let Some(value) = value {
                            rows.insert(key.id.clone(), value.clone());
                        } else {
                            rows.remove(&key.id);
                        }
                    }
                }
                let value = Value::Array(
                    rows.into_iter()
                        .map(|(id, value)| json!({"id": id, "value": value}))
                        .collect(),
                );
                (Dependency::Range(range), value)
            }
        };
        self.trace.lock().map_err(|_| "trace poisoned")?.push(dependency);
        Ok(value)
    }
}
