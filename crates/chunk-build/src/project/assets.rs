//! Worlds and resource packs declared in `app.ts` and `scope.ts`, and the asset contract a release derives from them.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use chunk_contract::{AssetContract, PackDeclaration};
use oxc_ast::ast::Expression;
use serde::Serialize;

use super::{
    Inventory,
    authoring::{literal_string, object_entries, object_fields},
    invalid,
};
use crate::valid_id;

#[derive(Debug, Serialize)]
pub struct World {
    /// Project-relative path with forward slashes to a `.polar` file.
    pub source: String,
}

#[derive(Debug)]
pub struct Pack {
    /// Project-relative path with forward slashes: a directory with `pack.mcmeta`, or a `.zip`.
    pub source: String,
    pub declaration: PackDeclaration,
    /// Its position among the packs its file declares; later packs override earlier ones.
    pub position: usize,
}

/// The `assets/` directory a declaration's sources are relative to, and its project-relative path. Sources must
/// exist unless `require_sources` is false.
pub(super) struct Root<'a> {
    pub directory: &'a Path,
    pub prefix: &'a str,
    pub require_sources: bool,
}

pub(super) fn worlds(file: &Path, expression: &Expression<'_>, root: &Root<'_>) -> io::Result<BTreeMap<String, World>> {
    let mut worlds = BTreeMap::new();
    for (name, value) in object_fields(file, expression)? {
        if !valid_id(name) {
            return Err(invalid(file, "world names must be ASCII identifiers"));
        }
        let fields = object_fields(file, value)?;
        if fields.keys().any(|key| *key != "source") {
            return Err(invalid(file, format!("unsupported option of world {name}")));
        }
        let (source, found) = source(file, fields.get("source").copied(), root)?;
        if !extension(&source, "polar") || found.is_some_and(|(_, metadata)| !metadata.is_file()) {
            return Err(invalid(
                file,
                format!("world {name} must be a .polar file; Chunk no longer converts Anvil saves"),
            ));
        }
        worlds.insert(name.into(), World { source });
    }
    Ok(worlds)
}

pub(super) fn packs(file: &Path, expression: &Expression<'_>, root: &Root<'_>) -> io::Result<BTreeMap<String, Pack>> {
    let mut packs = BTreeMap::new();
    for (position, (name, value)) in object_entries(file, expression)?.into_iter().enumerate() {
        if !valid_id(name) {
            return Err(invalid(file, "pack names must be ASCII identifiers"));
        }
        let fields = object_fields(file, value)?;
        if fields.keys().any(|key| !["source", "required", "prompt"].contains(key)) {
            return Err(invalid(file, format!("unsupported option of pack {name}")));
        }
        let (source, found) = source(file, fields.get("source").copied(), root)?;
        if let Some((path, metadata)) = found {
            let directory =
                metadata.is_dir() && fs::symlink_metadata(path.join("pack.mcmeta")).is_ok_and(|meta| meta.is_file());
            if !(directory || metadata.is_file() && extension(&source, "zip")) {
                return Err(invalid(file, format!("pack {name} requires a directory with pack.mcmeta or a .zip")));
            }
        }
        let required = match fields.get("required") {
            Some(Expression::BooleanLiteral(value)) => value.value,
            Some(_) => return Err(invalid(file, "required must be a literal boolean")),
            None => false,
        };
        let prompt = fields.get("prompt").map(|value| literal_string(file, value)).transpose()?;
        packs.insert(name.into(), Pack { source, declaration: PackDeclaration { required, prompt }, position });
    }
    Ok(packs)
}

/// Resolves a literal `source` inside `root` without following symlinks, returning its project-relative path and,
/// when sources are required, its path and metadata.
fn source(
    file: &Path,
    value: Option<&Expression<'_>>,
    root: &Root<'_>,
) -> io::Result<(String, Option<(PathBuf, fs::Metadata)>)> {
    let source = value
        .map(|value| literal_string(file, value))
        .transpose()?
        .ok_or_else(|| invalid(file, "worlds and packs require a literal source"))?;
    let prefix = root.prefix;
    crate::publication::relative_name(&source)
        .map_err(|_| invalid(file, format!("source {source:?} must be a relative path inside {prefix}/")))?;
    if !root.require_sources {
        return Ok((format!("{prefix}/{source}"), None));
    }
    let missing = |error: io::Error| invalid(file, format!("source {prefix}/{source}: {error}"));
    let mut path = root.directory.to_path_buf();
    let mut metadata = fs::symlink_metadata(&path).map_err(missing)?;
    for segment in source.split('/') {
        if !metadata.is_dir() {
            return Err(invalid(file, format!("source {prefix}/{source} must be inside a directory, not a symlink")));
        }
        path.push(segment);
        metadata = fs::symlink_metadata(&path).map_err(missing)?;
    }
    if metadata.is_symlink() {
        return Err(invalid(file, format!("source {prefix}/{source} cannot be a symlink")));
    }
    Ok((format!("{prefix}/{source}"), Some((path, metadata))))
}

fn extension(source: &str, expected: &str) -> bool {
    Path::new(source).extension().is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

/// Rejects pack names declared twice anywhere in the project.
pub(super) fn check_unique_packs(root: &Path, inventory: &Inventory) -> io::Result<()> {
    let mut names = BTreeSet::new();
    let scopes = inventory.packs.values().flat_map(BTreeMap::keys);
    for name in scopes.chain(inventory.apps.iter().flat_map(|app| app.packs.keys())) {
        if !names.insert(name) {
            return Err(invalid(&root.join("apps"), format!("pack {name} is declared more than once")));
        }
    }
    Ok(())
}

/// The worlds and packs the project's apps declare, with each app's packs from its outermost scope down to its own,
/// each file's in declaration order.
pub(crate) fn contract(inventory: &Inventory) -> AssetContract {
    let mut contract = AssetContract::default();
    for packs in inventory.packs.values().chain(inventory.apps.iter().map(|app| &app.packs)) {
        for (name, pack) in packs {
            contract.packs.insert(name.clone(), pack.declaration.clone());
        }
    }
    for app in &inventory.apps {
        if !app.worlds.is_empty() {
            contract.worlds.insert(app.id.clone(), app.worlds.keys().cloned().collect());
        }
        let mut scopes = vec![String::new()];
        for segment in app.domain.split('/').filter(|segment| !segment.is_empty()) {
            let parent = scopes.last().expect("the root scope");
            scopes.push(if parent.is_empty() { segment.into() } else { format!("{parent}/{segment}") });
        }
        let names: Vec<_> = scopes
            .iter()
            .filter_map(|scope| inventory.packs.get(scope))
            .chain([&app.packs])
            .flat_map(|packs| {
                let mut declared: Vec<_> = packs.iter().collect();
                declared.sort_by_key(|(_, pack)| pack.position);
                declared.into_iter().map(|(name, _)| name.clone())
            })
            .collect();
        if !names.is_empty() {
            contract.app_packs.insert(app.id.clone(), names);
        }
    }
    contract
}
