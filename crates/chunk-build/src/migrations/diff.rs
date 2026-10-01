use std::collections::BTreeMap;

use chunk_contract::{DatabaseSchema, MigrationTable};

/// Tables whose change from `previous` to `current` needs a migration, with every added and removed field.
/// New tables, dropped tables, new optional fields and index changes apply without one.
pub(crate) fn changes(previous: &DatabaseSchema, current: &DatabaseSchema) -> BTreeMap<String, MigrationTable> {
    let mut changes = BTreeMap::new();
    for (name, table) in current {
        let Some(old) = previous.get(name) else { continue };
        let removed: Vec<_> =
            old.fields.iter().filter(|(field, definition)| table.fields.get(*field) != Some(definition)).collect();
        let added: Vec<_> =
            table.fields.iter().filter(|(field, definition)| old.fields.get(*field) != Some(definition)).collect();
        if removed.is_empty() && added.iter().all(|(_, definition)| definition.optional) {
            continue;
        }
        changes.insert(
            name.clone(),
            MigrationTable {
                added: added.into_iter().map(|(field, _)| field.clone()).collect(),
                removed: removed.into_iter().map(|(field, _)| field.clone()).collect(),
                back: false,
            },
        );
    }
    changes
}

/// Describes changes for messages, such as `fighters (removed name; added displayName)`.
pub(crate) fn describe(changes: &BTreeMap<String, MigrationTable>) -> String {
    changes
        .iter()
        .map(|(table, change)| {
            let mut parts = Vec::new();
            if !change.removed.is_empty() {
                parts.push(format!("removed {}", change.removed.join(", ")));
            }
            if !change.added.is_empty() {
                parts.push(format!("added {}", change.added.join(", ")));
            }
            format!("{table} ({})", parts.join("; "))
        })
        .collect::<Vec<_>>()
        .join(", ")
}
