use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::Arc,
};

use chunk_contract::IndexQuery;
use chunk_js::{IndexRows, Key, ReadHost};
use chunk_store::{Document, DocumentKey, IndexRange, KeyRange, ReadBudget, Revision, Snapshot, Write};
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
        Self { revision: base.revision, base, overlay: BTreeMap::new() }
    }

    pub fn get(&self, key: &DocumentKey, budget: &mut ReadBudget) -> Result<Option<Document>> {
        match self.overlay.get(key) {
            Some(value) => {
                if let Some(document) = value {
                    budget.charge(serde_json::to_vec(&document.value)?.len() + key.id.len())?;
                }
                Ok(value.clone())
            }
            None => Ok(self.base.get_bounded(key, budget)?),
        }
    }

    pub fn scan(&self, range: &KeyRange, budget: &mut ReadBudget) -> Result<BTreeMap<String, Value>> {
        let mut rows: BTreeMap<_, _> =
            self.base.scan_bounded(range, budget)?.into_iter().map(|(id, doc)| (id, doc.value)).collect();
        for (key, value) in &self.overlay {
            if covers(range, key) {
                match value {
                    Some(doc) => {
                        budget.charge(serde_json::to_vec(&doc.value)?.len() + key.id.len())?;
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

    pub fn index(&self, query: &IndexQuery, budget: &mut ReadBudget) -> Result<IndexRows> {
        let table = self.base.schema().get(&query.table).ok_or(Error::Contract)?;
        let fields = table.indexes.get(&query.index).ok_or(Error::Contract)?;
        let extra = self.overlay.keys().filter(|key| key.table == query.table).count();
        let range = IndexRange {
            table: query.table.clone(),
            index: query.index.clone(),
            prefix: query.prefix.clone(),
            start: query.start.clone(),
            end: query.end.clone(),
            limit: query.limit + extra,
        };
        let mut rows: BTreeMap<_, _> = self
            .base
            .scan_index_bounded(&range, budget)?
            .into_iter()
            .map(|(id, document)| (id, document.value))
            .collect();
        for (key, document) in &self.overlay {
            if key.table == query.table {
                rows.remove(&key.id);
                if let Some(document) = document
                    && query.matches(fields, &document.value)
                {
                    budget.charge(serde_json::to_vec(&document.value)?.len() + key.id.len())?;
                    rows.insert(key.id.clone(), document.value.clone());
                }
            }
        }
        let mut rows: Vec<_> = rows.into_iter().collect();
        rows.sort_by(|a, b| IndexQuery::compare(fields, a, b));
        rows.truncate(query.limit);
        Ok(IndexRows { fields: fields.clone(), rows })
    }

    pub fn changes(&self, writes: &[Write]) -> Result<Vec<Change>> {
        let mut budget = ReadBudget::default();
        writes
            .iter()
            .map(|write| {
                Ok(Change {
                    key: write.key.clone(),
                    before: self.get(&write.key, &mut budget)?.map(|d| d.value),
                    after: write.value.clone(),
                })
            })
            .collect()
    }

    pub fn apply(&mut self, revision: Revision, writes: &[Write]) {
        for write in writes {
            self.overlay.insert(write.key.clone(), write.value.clone().map(|value| Document { revision, value }));
        }
        self.revision = revision;
    }

    pub fn validate(&self, writes: &[Write]) -> Result<usize> {
        let mut bytes = 0;
        for write in writes {
            write.key.validate()?;
            let table = self.base.schema().get(&write.key.table).ok_or(Error::Invalid("undeclared table"))?;
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

#[derive(Clone)]
pub(crate) struct Change {
    pub key: DocumentKey,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

/// What one evaluation read. Reading the caller counts: the result may then differ per caller.
#[derive(Default)]
pub(crate) struct Dependencies {
    pub points: BTreeSet<DocumentKey>,
    pub ranges: Vec<KeyRange>,
    pub indexes: Vec<(IndexQuery, Vec<String>)>,
    pub caller: bool,
}

impl Dependencies {
    pub fn affected(&self, changes: &[Change]) -> bool {
        changes.iter().any(|change| {
            self.points.contains(&change.key)
                || self.ranges.iter().any(|range| covers(range, &change.key))
                || self.indexes.iter().any(|(query, fields)| change.matches(query, fields))
        })
    }
}

impl Change {
    pub fn matches(&self, query: &IndexQuery, fields: &[String]) -> bool {
        query.table == self.key.table
            && self.before.iter().chain(self.after.iter()).any(|value| query.matches(fields, value))
    }
}

pub(crate) struct Host {
    pub operation: Option<String>,
    pub view: Rc<View>,
    pub trace: Rc<RefCell<Dependencies>>,
    pub contract: Option<Arc<chunk_contract::Deployment>>,
    pub budget: ReadBudget,
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
    fn read_caller(&mut self) {
        self.trace.borrow_mut().caller = true;
    }

    fn schedule_id(&self, sequence: u32) -> std::result::Result<String, String> {
        use sha2::Digest;
        let operation = self.operation.as_ref().ok_or("Scheduled jobs require a mutation operation")?;
        Ok(format!("{:x}-{sequence}", sha2::Sha256::digest(operation.as_bytes())))
    }

    fn get(&mut self, key: &Key) -> std::result::Result<Option<Value>, String> {
        let key = DocumentKey::new(&key.table, &key.id).map_err(|error| error.to_string())?;
        self.table(&key.table)?;
        self.trace.borrow_mut().points.insert(key.clone());
        self.view
            .get(&key, &mut self.budget)
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
        let range = KeyRange { table: table.into(), start: start.map(str::to_owned), end: end.map(str::to_owned) };
        range.validate().map_err(|error| error.to_string())?;
        self.trace.borrow_mut().ranges.push(range.clone());
        self.view
            .scan(&range, &mut self.budget)
            .and_then(|rows| rows.into_iter().map(|(id, value)| Ok((id, self.value(table, value)?))).collect())
            .map_err(|error| error.to_string())
    }
    fn scan_index(&mut self, query: &IndexQuery) -> std::result::Result<IndexRows, String> {
        self.table(&query.table)?;
        if self.contract.as_ref().is_some_and(|c| !c.tables[&query.table].indexes.contains_key(&query.index)) {
            return Err("undeclared index".into());
        }
        let table = self.view.base.schema().get(&query.table).ok_or("undeclared table")?;
        let fields = table.indexes.get(&query.index).ok_or("undeclared index")?.clone();
        self.trace.borrow_mut().indexes.push((query.clone(), fields));
        let mut indexed = self.view.index(query, &mut self.budget).map_err(|e| e.to_string())?;
        indexed.rows = indexed
            .rows
            .into_iter()
            .map(|(id, value)| self.value(&query.table, value).map(|value| (id, value)))
            .collect::<Result<_>>()
            .map_err(|e| e.to_string())?;
        Ok(indexed)
    }
}

fn covers(range: &KeyRange, key: &DocumentKey) -> bool {
    range.table == key.table && in_range(range, &key.id)
}

fn in_range(range: &KeyRange, id: &str) -> bool {
    range.start.as_deref().is_none_or(|start| id >= start) && range.end.as_deref().is_none_or(|end| id < end)
}

pub(crate) fn read_budget() -> ReadBudget {
    ReadBudget::new(4096, 4 * 1024 * 1024)
}
