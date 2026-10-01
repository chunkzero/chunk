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

    /// Keeps both shapes of each active expand migration in step on a write from `contract`. A writer that declares
    /// the old shape but not the new one gets the added fields from `to`; one that declares the new shape but not
    /// the old one gets the removed fields from `back` where the migration has it, and otherwise keeps their stored
    /// values. Without a resident deployment carrying a migration, its fields are left as they are.
    pub(super) fn sync(&mut self, contract: &Deployment, table: &str, id: &str, value: &mut Value) -> Result<()> {
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
        let Some(writer) = contract.tables.get(table).filter(|_| !steps.is_empty()) else {
            return Ok(());
        };
        let mut known: BTreeSet<_> = writer.fields.keys().cloned().collect();
        let declares = |known: &BTreeSet<String>, fields: &[String]| fields.iter().all(|field| known.contains(field));
        let lacks = |known: &BTreeSet<String>, fields: &[String]| !fields.iter().any(|field| known.contains(field));
        for (migration, change, [input, _]) in &steps {
            if !change.added.is_empty() && declares(&known, &change.removed) && lacks(&known, &change.added) {
                self.apply((migration, table, Direction::To), (&change.added, input), id, value)?;
                known.extend(change.added.iter().cloned());
            }
        }
        for (migration, change, [_, input]) in steps.iter().rev() {
            if change.back
                && !change.removed.is_empty()
                && declares(&known, &change.added)
                && lacks(&known, &change.removed)
            {
                self.apply((migration, table, Direction::Back), (&change.removed, input), id, value)?;
                known.extend(change.removed.iter().cloned());
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
        let output = transform(&mut self.js, &carrier, (migration, table, direction), &[Value::Object(row)])
            .map_err(|error| failed(error.to_string()))?
            .pop()
            .ok_or_else(|| failed("the transform returned no row".into()))?;
        for field in fields {
            match output.get(field) {
                Some(computed) => object.insert(field.clone(), computed.clone()),
                None => object.remove(field),
            };
        }
        Ok(())
    }
}
