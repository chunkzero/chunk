//! In-memory snapshot of seeded player profiles, shared by both engines.
use std::{collections::HashMap, rc::Rc};

use chunk_contract::IndexQuery;
use chunk_js::{IndexRows, Key, ReadHost};
use serde_json::{Value, json};

pub const PLAYERS: u64 = 1000;

pub struct Data {
    documents: HashMap<String, Value>,
    by_player: HashMap<String, String>,
    /// Document IDs in `by_rank` order.
    by_rank: Vec<String>,
}

/// Mirrors the bundle's `seed` mutation.
pub fn seed() -> Rc<Data> {
    let mut documents = HashMap::new();
    let mut by_player = HashMap::new();
    let mut ranked = Vec::new();
    for index in 0..PLAYERS {
        let best = (index * 7919) % 100_000;
        let id = format!("profiles:{index:032x}");
        let inventory: Vec<_> =
            (0..8).map(|slot| json!({"item": format!("item-{}", (index + slot) % 64), "count": 1 + slot})).collect();
        let document = json!({"player": format!("p{index}"), "name": format!("Player {index}"), "coins": index % 1000,
            "xp": index * 10, "level": 1 + index % 50, "best": best, "rank": -i64::try_from(best).unwrap(),
            "inventory": inventory, "lastSeen": 0, "saves": 0});
        by_player.insert(format!("p{index}"), id.clone());
        ranked.push((id.clone(), document.clone()));
        documents.insert(id, document);
    }
    let fields = ["rank".to_owned()];
    ranked.sort_by(|a, b| IndexQuery::compare(&fields, a, b));
    Rc::new(Data { documents, by_player, by_rank: ranked.into_iter().map(|(id, _)| id).collect() })
}

pub struct Host(pub Rc<Data>);

impl Host {
    fn row(&self, id: &str) -> (String, Value) {
        (id.to_owned(), self.0.documents[id].clone())
    }
}

impl ReadHost for Host {
    fn get(&mut self, key: &Key) -> Result<Option<Value>, String> {
        Ok(self.0.documents.get(&key.id).filter(|_| key.table == "profiles").cloned())
    }

    fn scan(&mut self, _: &str, _: Option<&str>, _: Option<&str>) -> Result<Vec<(String, Value)>, String> {
        Err("the benchmark bundle does not scan tables".into())
    }

    fn scan_index(&mut self, query: &IndexQuery) -> Result<IndexRows, String> {
        match query.index.as_str() {
            "by_player" => {
                let player = query.prefix.first().and_then(Value::as_str).ok_or("player prefix required")?;
                let rows = self.0.by_player.get(player).map(|id| self.row(id)).into_iter().collect();
                Ok(IndexRows { fields: vec!["player".into()], rows })
            }
            "by_rank" => {
                let rows = self.0.by_rank.iter().take(query.limit).map(|id| self.row(id)).collect();
                Ok(IndexRows { fields: vec!["rank".into()], rows })
            }
            _ => Err("unknown index".into()),
        }
    }
}
