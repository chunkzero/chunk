//! A minimal single-writer sync engine: serialized mutation commit with read-set
//! validation, then invalidation and re-evaluation of subscribed queries.
//! Mirrors `benches/bun/sync.ts`.
use std::collections::BTreeMap;

use chunk_js_baseline::Write;
use serde_json::{Value, json};

use crate::{Deps, Job, Outcome, Payload, Snapshot};

pub const SUBSCRIPTIONS: [(&str, &str); 5] = [
    ("0000", "0020"),
    ("0000", "0005"),
    ("0005", "0010"),
    ("0010", "0015"),
    ("0015", "0020"),
];

struct Subscription {
    arguments: Value,
    deps: Deps,
    result: Payload,
}

pub struct SyncEngine {
    rows: BTreeMap<String, Value>,
    row_revision: BTreeMap<String, usize>,
    pub revision: usize,
    subscriptions: Vec<Subscription>,
    pub evaluations: usize,
    pub published: usize,
    outcomes: BTreeMap<usize, Payload>,
}

fn covers(start: Option<&str>, end: Option<&str>, id: &str) -> bool {
    start.is_none_or(|s| id >= s) && end.is_none_or(|e| id < e)
}

impl SyncEngine {
    pub fn new() -> Self {
        Self {
            rows: (0..20)
                .map(|i| {
                    (
                        format!("{i:04}"),
                        json!({"score": i, "name": format!("player-{i}"), "online": true}),
                    )
                })
                .collect(),
            row_revision: BTreeMap::new(),
            revision: 0,
            subscriptions: Vec::new(),
            evaluations: 0,
            published: 0,
            outcomes: BTreeMap::new(),
        }
    }

    fn call(&mut self, run: &mut dyn FnMut(Job) -> Outcome, export: &str, arguments: Value) -> Outcome {
        self.evaluations += 1;
        run(Job {
            export: export.into(),
            arguments,
            snapshot: Snapshot::from_rows(self.rows.clone()),
        })
    }

    pub fn subscribe(&mut self, run: &mut dyn FnMut(Job) -> Outcome) {
        for (start, end) in SUBSCRIPTIONS {
            let arguments = json!({"start": start, "end": end});
            let outcome = self.call(run, "query", arguments.clone());
            self.subscriptions.push(Subscription {
                arguments,
                deps: outcome.deps,
                result: outcome.sample.value,
            });
        }
    }

    fn validate(&self, deps: &Deps, snapshot: usize) {
        for id in &deps.points {
            assert!(self.row_revision.get(id).copied().unwrap_or(0) <= snapshot, "conflict");
        }
        for (_, start, end) in &deps.scans {
            for (id, revision) in &self.row_revision {
                assert!(
                    !(covers(start.as_deref(), end.as_deref(), id) && *revision > snapshot),
                    "conflict"
                );
            }
        }
    }

    fn affected(deps: &Deps, writes: &[Write]) -> bool {
        writes.iter().any(|write| {
            deps.points.contains(&write.key.id)
                || deps
                    .scans
                    .iter()
                    .any(|(_, start, end)| covers(start.as_deref(), end.as_deref(), &write.key.id))
        })
    }

    /// One operation: speculative mutation, validation, atomic apply, invalidation.
    pub fn operate(&mut self, run: &mut dyn FnMut(Job) -> Outcome, operation: usize, id: &str) -> (Payload, usize) {
        let snapshot = self.revision;
        let outcome = self.call(run, "bump", json!({"id": id}));
        self.validate(&outcome.deps, snapshot);
        self.revision += 1;
        for Write { key, value } in &outcome.writes {
            match value {
                Some(value) => self.rows.insert(key.id.clone(), value.clone()),
                None => self.rows.remove(&key.id),
            };
            self.row_revision.insert(key.id.clone(), self.revision);
        }
        self.outcomes.insert(operation, outcome.sample.value.clone());
        let mut reevaluated = 0;
        for index in 0..self.subscriptions.len() {
            if !Self::affected(&self.subscriptions[index].deps, &outcome.writes) {
                continue;
            }
            reevaluated += 1;
            let arguments = self.subscriptions[index].arguments.clone();
            let fresh = self.call(run, "query", arguments);
            let subscription = &mut self.subscriptions[index];
            subscription.deps = fresh.deps;
            if fresh.sample.value != subscription.result {
                self.published += 1;
            }
            subscription.result = fresh.sample.value;
        }
        (outcome.sample.value, reevaluated)
    }

    pub fn leaderboard(&self) -> Value {
        let mut rows: Vec<_> = self
            .rows
            .iter()
            .map(|(id, doc)| (id.clone(), doc["score"].as_i64().unwrap()))
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        json!(
            rows.iter()
                .take(10)
                .map(|(id, score)| json!({"id": id, "score": score}))
                .collect::<Vec<_>>()
        )
    }

    pub fn summary(&self) -> Value {
        let last = self.subscriptions[0].result.value();
        assert_eq!(last, self.leaderboard(), "subscription drifted");
        json!({"evaluations": self.evaluations, "published": self.published,
            "revision": self.revision, "leaderboard": last})
    }
}
