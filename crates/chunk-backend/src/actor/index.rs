use std::collections::{BTreeMap, BTreeSet};

use chunk_contract::IndexQuery;

use crate::reads::{Change, Dependencies};

pub(super) type QueryId = u64;

/// Finds the subscribed queries a commit affects without checking each one.
/// Identical reads are stored once, so shared ranges cost one check per change.
#[derive(Default)]
pub(super) struct ReadIndex {
    tables: BTreeMap<String, Table>,
    /// Queries that read the snapshot time, which every commit changes.
    timed: BTreeSet<QueryId>,
}

#[derive(Default)]
struct Table {
    points: BTreeMap<String, BTreeSet<QueryId>>,
    ranges: BTreeMap<(Option<String>, Option<String>), BTreeSet<QueryId>>,
    indexes: BTreeMap<String, (IndexQuery, Vec<String>, BTreeSet<QueryId>)>,
}

impl Table {
    fn is_empty(&self) -> bool {
        self.points.is_empty() && self.ranges.is_empty() && self.indexes.is_empty()
    }
}

fn key(query: &IndexQuery) -> String {
    serde_json::to_string(query).expect("index queries serialize")
}

impl ReadIndex {
    pub fn insert(&mut self, id: QueryId, reads: &Dependencies) {
        if reads.time {
            self.timed.insert(id);
        }
        for point in &reads.points {
            self.table(&point.table).points.entry(point.id.clone()).or_default().insert(id);
        }
        for range in &reads.ranges {
            let ranges = &mut self.table(&range.table).ranges;
            ranges.entry((range.start.clone(), range.end.clone())).or_default().insert(id);
        }
        for (query, fields) in &reads.indexes {
            let indexes = &mut self.table(&query.table).indexes;
            indexes.entry(key(query)).or_insert_with(|| (query.clone(), fields.clone(), BTreeSet::new())).2.insert(id);
        }
    }

    pub fn remove(&mut self, id: QueryId, reads: &Dependencies) {
        self.timed.remove(&id);
        for point in &reads.points {
            if let Some(table) = self.tables.get_mut(&point.table)
                && let Some(ids) = table.points.get_mut(&point.id)
            {
                ids.remove(&id);
                if ids.is_empty() {
                    table.points.remove(&point.id);
                }
            }
        }
        for range in &reads.ranges {
            if let Some(table) = self.tables.get_mut(&range.table) {
                let key = (range.start.clone(), range.end.clone());
                if let Some(ids) = table.ranges.get_mut(&key) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        table.ranges.remove(&key);
                    }
                }
            }
        }
        for (query, _) in &reads.indexes {
            if let Some(table) = self.tables.get_mut(&query.table) {
                let key = key(query);
                if let Some((_, _, ids)) = table.indexes.get_mut(&key) {
                    ids.remove(&id);
                    if ids.is_empty() {
                        table.indexes.remove(&key);
                    }
                }
            }
        }
        let tables = reads
            .points
            .iter()
            .map(|point| &point.table)
            .chain(reads.ranges.iter().map(|range| &range.table))
            .chain(reads.indexes.iter().map(|(query, _)| &query.table));
        for table in tables {
            if self.tables.get(table).is_some_and(Table::is_empty) {
                self.tables.remove(table);
            }
        }
    }

    /// Adds every query a commit with these changes affects.
    pub fn affected(&self, changes: &[Change], found: &mut BTreeSet<QueryId>) {
        found.extend(&self.timed);
        for change in changes {
            let Some(table) = self.tables.get(&change.key.table) else {
                continue;
            };
            if let Some(ids) = table.points.get(&change.key.id) {
                found.extend(ids);
            }
            let id = change.key.id.as_str();
            for ((start, end), ids) in &table.ranges {
                if start.as_deref().is_none_or(|start| id >= start) && end.as_deref().is_none_or(|end| id < end) {
                    found.extend(ids);
                }
            }
            for (query, fields, ids) in table.indexes.values() {
                if change.matches(query, fields) {
                    found.extend(ids);
                }
            }
        }
    }

    fn table(&mut self, name: &str) -> &mut Table {
        self.tables.entry(name.to_owned()).or_default()
    }
}
