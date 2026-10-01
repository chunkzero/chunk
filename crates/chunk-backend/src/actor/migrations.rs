use std::{collections::BTreeSet, sync::Arc};

use chunk_contract::{Deployment, MigrationTable};
use chunk_js::DeploymentId;
use serde_json::{Map, Value};

use super::Actor;
use crate::{
    Error, Result,
    migrations::{Direction, transform},
};

/// An active expand's ID, its change to a table, and the fields its `to` and `back` read.
type Step = (String, MigrationTable, [BTreeSet<String>; 2]);

impl Actor {
    /// The newest resident deployment whose journal contains `migration`; its bundle runs the migration.
    pub(super) fn carrier(&self, migration: &str) -> Option<Arc<Deployment>> {
        self.versions
            .values()
            .flatten()
            .filter(|deployment| deployment.contracts.migrations.iter().any(|entry| entry.id == migration))
            .max_by_key(|deployment| deployment.contracts.migrations.len())
            .cloned()
    }

    /// Keeps both shapes of each active expand migration in step on a write from `contract`, whose row `value`
    /// is the write merged with the stored fields it doesn't declare. A writer that lacks any added field gets
    /// them from `to`; one that lacks any removed field gets them from `back`, where the migration has it.
    /// Without a resident deployment carrying a migration, its fields are left as they are.
    pub(super) fn sync(&mut self, contract: &Deployment, table: &str, id: &str, value: &mut Value) -> Result<()> {
        let Some(writer) = contract.tables.get(table) else { return Ok(()) };
        let steps: Vec<Step> = self
            .work
            .migrations
            .iter()
            .filter_map(|migration| {
                let change = migration.tables.get(table)?;
                let inputs = |back| migration.input_fields(table, back).into_iter().map(str::to_owned).collect();
                Some((migration.id.clone(), change.clone(), [inputs(false), inputs(true)]))
            })
            .collect();
        let lacks = |fields: &[String]| fields.iter().any(|field| !writer.fields.contains_key(field));
        for (migration, change, [to, back]) in &steps {
            if lacks(&change.added) {
                self.apply((migration, table, Direction::To), (&change.added, to), id, value)?;
            }
            if change.back && lacks(&change.removed) {
                self.apply((migration, table, Direction::Back), (&change.removed, back), id, value)?;
            }
        }
        Ok(())
    }

    /// Replaces the first `fields` of `value` with those the transform computes from its `input` fields.
    fn apply(
        &mut self,
        (migration, table, direction): (&str, &str, Direction),
        (fields, input): (&[String], &BTreeSet<String>),
        id: &str,
        value: &mut Value,
    ) -> Result<()> {
        let Some(carrier) = self.carrier(migration) else {
            return Ok(());
        };
        let carrier = DeploymentId::new(&carrier.id)?;
        let object = value.as_object_mut().ok_or(Error::Contract)?;
        let mut row: Map<_, _> =
            object.iter().filter(|(name, _)| input.contains(*name)).map(|(k, v)| (k.clone(), v.clone())).collect();
        row.insert("_id".into(), Value::String(id.into()));
        let failed =
            |reason: String| Error::Migration(format!("migration {migration} failed on {table} row {id}: {reason}"));
        let output =
            transform(&mut self.js, &carrier, (migration, table, direction), &Value::Object(row)).map_err(failed)?;
        for field in fields {
            match output.get(field) {
                Some(computed) => object.insert(field.clone(), computed.clone()),
                None => object.remove(field),
            };
        }
        Ok(())
    }
}
