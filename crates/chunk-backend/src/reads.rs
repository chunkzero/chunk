use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use chunk_js::{Key, Read, ReadHost};
use chunk_store::{Document, DocumentKey, KeyRange, Revision, Snapshot, Write};
use serde_json::Value;

use crate::{Error, Result};

#[derive(Clone)]
pub(crate) struct View {
    pub base: Snapshot,
    pub revision: Revision,
    pub overlay: BTreeMap<DocumentKey, Option<Document>>,
}

impl View {
    pub fn new(base: Snapshot) -> Self {
        Self {
            revision: base.revision,
            base,
            overlay: BTreeMap::new(),
        }
    }

    pub fn get(&self, key: &DocumentKey) -> Result<Option<Document>> {
        match self.overlay.get(key) {
            Some(value) => Ok(value.clone()),
            None => Ok(self.base.get(key)?),
        }
    }

    pub fn scan(&self, range: &KeyRange) -> Result<BTreeMap<String, Value>> {
        let mut rows: BTreeMap<_, _> = self
            .base
            .scan(range)?
            .into_iter()
            .map(|(id, doc)| (id, doc.value))
            .collect();
        for (key, value) in &self.overlay {
            if covers(range, key) {
                match value {
                    Some(doc) => {
                        rows.insert(key.id.clone(), doc.value.clone());
                    }
                    None => {
                        rows.remove(&key.id);
                    }
                }
            }
        }
        Ok(rows)
    }

    pub fn apply(&mut self, revision: Revision, writes: &[Write]) {
        for write in writes {
            self.overlay.insert(
                write.key.clone(),
                write.value.clone().map(|value| Document { revision, value }),
            );
        }
        self.revision = revision;
    }

    pub fn validate(&self, writes: &[Write]) -> Result<usize> {
        let mut bytes = 0;
        for write in writes {
            write.key.validate()?;
            let table = self
                .base
                .schema()
                .get(&write.key.table)
                .ok_or(Error::Invalid("undeclared table"))?;
            if let Some(value) = &write.value {
                if !table.accepts(value) {
                    return Err(Error::Invalid("document does not match schema"));
                }
                bytes += serde_json::to_vec(value)?.len();
            }
            bytes += write.key.table.len() + write.key.id.len();
        }
        Ok(bytes)
    }
}

#[derive(Default)]
pub(crate) struct Dependencies {
    points: BTreeSet<DocumentKey>,
    ranges: Vec<KeyRange>,
}

impl Dependencies {
    pub fn affected(&self, writes: &[Write]) -> bool {
        writes
            .iter()
            .any(|write| self.points.contains(&write.key) || self.ranges.iter().any(|range| covers(range, &write.key)))
    }
}

pub(crate) struct Host {
    pub view: Rc<View>,
    pub trace: Rc<RefCell<Dependencies>>,
}

impl Host {
    fn read_value(&self, request: Read, overlay: &BTreeMap<Key, Option<Value>>) -> Result<Value> {
        match request {
            Read::Get { table, id } => {
                let key = DocumentKey::new(&table, &id)?;
                if !self.view.base.schema().contains_key(&table) {
                    return Err(Error::Invalid("undeclared table"));
                }
                self.trace.borrow_mut().points.insert(key.clone());
                let value = match overlay.get(&Key { table, id }) {
                    Some(value) => value.clone(),
                    None => self.view.get(&key)?.map(|doc| doc.value),
                };
                Ok(value.unwrap_or(Value::Null))
            }
            Read::Scan { table, start, end } => {
                let range = KeyRange { table, start, end };
                range.validate()?;
                let mut rows = self.view.scan(&range)?;
                for (key, value) in overlay {
                    if key.table == range.table && in_range(&range, &key.id) {
                        match value {
                            Some(value) => {
                                rows.insert(key.id.clone(), value.clone());
                            }
                            None => {
                                rows.remove(&key.id);
                            }
                        }
                    }
                }
                self.trace.borrow_mut().ranges.push(range);
                Ok(serde_json::to_value(rows.into_iter().collect::<Vec<_>>())?)
            }
        }
    }
}

impl ReadHost for Host {
    fn read(&mut self, request: Read, overlay: &BTreeMap<Key, Option<Value>>) -> std::result::Result<Value, String> {
        self.read_value(request, overlay).map_err(|error| error.to_string())
    }
}

fn covers(range: &KeyRange, key: &DocumentKey) -> bool {
    range.table == key.table && in_range(range, &key.id)
}

fn in_range(range: &KeyRange, id: &str) -> bool {
    range.start.as_deref().is_none_or(|start| id >= start) && range.end.as_deref().is_none_or(|end| id < end)
}
