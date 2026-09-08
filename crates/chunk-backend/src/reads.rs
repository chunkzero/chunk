use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::Arc,
};

use chunk_js::{Key, ReadHost};
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
    pub fn extend(&mut self, other: Self) {
        self.points.extend(other.points);
        self.ranges.extend(other.ranges);
    }
    pub fn affected(&self, writes: &[Write]) -> bool {
        writes
            .iter()
            .any(|write| self.points.contains(&write.key) || self.ranges.iter().any(|range| covers(range, &write.key)))
    }
}

pub(crate) struct Host {
    pub view: Rc<View>,
    pub trace: Rc<RefCell<Dependencies>>,
    pub contract: Option<Arc<chunk_contract::Deployment>>,
}

pub(crate) fn project(table: &chunk_contract::TableSchema, value: &Value) -> Result<Value> {
    let object = value.as_object().ok_or(Error::Contract)?;
    let projected = Value::Object(
        object
            .iter()
            .filter(|(key, _)| table.fields.contains_key(*key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    if !table.accepts(&projected) {
        return Err(Error::Contract);
    }
    chunk_contract::validate_wire_value(&projected).map_err(Error::Invalid)?;
    Ok(projected)
}

impl Host {
    fn value(&self, table: &str, value: Value) -> Result<Value> {
        if let Some(contract) = &self.contract {
            project(contract.tables.get(table).ok_or(Error::Contract)?, &value)
        } else {
            Ok(value)
        }
    }

    fn table(&self, table: &str) -> std::result::Result<(), String> {
        if self.contract.as_ref().is_some_and(|c| !c.tables.contains_key(table))
            || !self.view.base.schema().contains_key(table)
        {
            return Err("undeclared table".into());
        }
        Ok(())
    }
}

impl ReadHost for Host {
    fn get(&mut self, key: &Key) -> std::result::Result<Option<Value>, String> {
        let key = DocumentKey::new(&key.table, &key.id).map_err(|error| error.to_string())?;
        self.table(&key.table)?;
        self.trace.borrow_mut().points.insert(key.clone());
        self.view
            .get(&key)
            .and_then(|document| document.map(|doc| self.value(&key.table, doc.value)).transpose())
            .map_err(|error| error.to_string())
    }
    fn scan(
        &mut self,
        table: &str,
        start: Option<&str>,
        end: Option<&str>,
    ) -> std::result::Result<Vec<(String, Value)>, String> {
        self.table(table)?;
        let range = KeyRange {
            table: table.into(),
            start: start.map(str::to_owned),
            end: end.map(str::to_owned),
        };
        range.validate().map_err(|error| error.to_string())?;
        self.trace.borrow_mut().ranges.push(range.clone());
        self.view
            .scan(&range)
            .and_then(|rows| {
                rows.into_iter()
                    .map(|(id, value)| Ok((id, self.value(table, value)?)))
                    .collect()
            })
            .map_err(|error| error.to_string())
    }
}

fn covers(range: &KeyRange, key: &DocumentKey) -> bool {
    range.table == key.table && in_range(range, &key.id)
}

fn in_range(range: &KeyRange, id: &str) -> bool {
    range.start.as_deref().is_none_or(|start| id >= start) && range.end.as_deref().is_none_or(|end| id < end)
}
