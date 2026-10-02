//! Worlds and resource packs declared in `app.ts` and `scope.ts`, and the asset contract a release derives from them.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use chunk_contract::{AssetContract, PackDeclaration};
use oxc_ast::ast::{Expression, UnaryOperator};
use serde::Serialize;

use super::{
    Inventory,
    authoring::{literal_string, object_fields},
    invalid,
};
use crate::valid_id;

#[derive(Debug, Serialize)]
pub struct World {
    /// Project-relative path with forward slashes.
    pub source: String,
    pub format: WorldFormat,
    /// The inclusive chunk range an Anvil world is cropped to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks: Option<ChunkRange>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorldFormat {
    /// A save directory with `region/`, converted to Polar by the Gradle build.
    Anvil,
    Polar,
}

#[derive(Debug, Serialize)]
pub struct ChunkRange {
    pub from: [i32; 2],
    pub to: [i32; 2],
}

#[derive(Debug)]
pub struct Pack {
    /// Project-relative path with forward slashes: a directory with `pack.mcmeta`, or a `.zip`.
    pub source: String,
    pub declaration: PackDeclaration,
}

/// The `assets/` directory a declaration's sources are relative to, and its project-relative path.
pub(super) struct Root<'a> {
    pub directory: &'a Path,
    pub prefix: &'a str,
}

pub(super) fn worlds(file: &Path, expression: &Expression<'_>, root: &Root<'_>) -> io::Result<BTreeMap<String, World>> {
    let mut worlds = BTreeMap::new();
    for (name, value) in object_fields(file, expression)? {
        if !valid_id(name) {
            return Err(invalid(file, "world names must be ASCII identifiers"));
        }
        let fields = object_fields(file, value)?;
        if fields.keys().any(|key| !["source", "chunks"].contains(key)) {
            return Err(invalid(file, format!("unsupported option of world {name}")));
        }
        let (source, path, metadata) = source(file, fields.get("source").copied(), root)?;
        let format = if metadata.is_file() && extension(&source, "polar") {
            WorldFormat::Polar
        } else if metadata.is_dir() && fs::symlink_metadata(path.join("region")).is_ok_and(|region| region.is_dir()) {
            WorldFormat::Anvil
        } else {
            return Err(invalid(file, format!("world {name} requires a .polar file or an Anvil save directory")));
        };
        let chunks = fields.get("chunks").map(|value| chunk_range(file, value)).transpose()?;
        if chunks.is_some() && format == WorldFormat::Polar {
            return Err(invalid(file, format!("world {name} can only crop Anvil saves to chunks")));
        }
        worlds.insert(name.into(), World { source, format, chunks });
    }
    Ok(worlds)
}

pub(super) fn packs(file: &Path, expression: &Expression<'_>, root: &Root<'_>) -> io::Result<BTreeMap<String, Pack>> {
    let mut packs = BTreeMap::new();
    for (name, value) in object_fields(file, expression)? {
        if !valid_id(name) {
            return Err(invalid(file, "pack names must be ASCII identifiers"));
        }
        let fields = object_fields(file, value)?;
        if fields.keys().any(|key| !["source", "required", "prompt"].contains(key)) {
            return Err(invalid(file, format!("unsupported option of pack {name}")));
        }
        let (source, path, metadata) = source(file, fields.get("source").copied(), root)?;
        let directory =
            metadata.is_dir() && fs::symlink_metadata(path.join("pack.mcmeta")).is_ok_and(|meta| meta.is_file());
        if !(directory || metadata.is_file() && extension(&source, "zip")) {
            return Err(invalid(file, format!("pack {name} requires a directory with pack.mcmeta or a .zip")));
        }
        let required = match fields.get("required") {
            Some(Expression::BooleanLiteral(value)) => value.value,
            Some(_) => return Err(invalid(file, "required must be a literal boolean")),
            None => false,
        };
        let prompt = fields.get("prompt").map(|value| literal_string(file, value)).transpose()?;
        packs.insert(name.into(), Pack { source, declaration: PackDeclaration { required, prompt } });
    }
    Ok(packs)
}

/// Resolves a literal `source` inside `root` without following symlinks, returning its project-relative path, its
/// path and its metadata.
fn source(file: &Path, value: Option<&Expression<'_>>, root: &Root<'_>) -> io::Result<(String, PathBuf, fs::Metadata)> {
    let source = value
        .map(|value| literal_string(file, value))
        .transpose()?
        .ok_or_else(|| invalid(file, "worlds and packs require a literal source"))?;
    let prefix = root.prefix;
    crate::publication::relative_name(&source)
        .map_err(|_| invalid(file, format!("source {source:?} must be a relative path inside {prefix}/")))?;
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
    Ok((format!("{prefix}/{source}"), path, metadata))
}

fn extension(source: &str, expected: &str) -> bool {
    Path::new(source).extension().is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn chunk_range(file: &Path, expression: &Expression<'_>) -> io::Result<ChunkRange> {
    let error = || invalid(file, "chunks requires literal { from: [x, z], to: [x, z] } with from <= to");
    let fields = object_fields(file, expression)?;
    if fields.len() != 2 {
        return Err(error());
    }
    let corner = |key| -> io::Result<[i32; 2]> {
        let Some(Expression::ArrayExpression(array)) = fields.get(key) else { return Err(error()) };
        let values = array
            .elements
            .iter()
            .map(|element| element.as_expression().and_then(integer))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(error)?;
        values.try_into().map_err(|_| error())
    };
    let (from, to) = (corner("from")?, corner("to")?);
    if from[0] > to[0] || from[1] > to[1] {
        return Err(error());
    }
    Ok(ChunkRange { from, to })
}

fn integer(expression: &Expression<'_>) -> Option<i32> {
    match expression {
        Expression::NumericLiteral(number) => number.raw.as_ref()?.parse().ok(),
        Expression::UnaryExpression(unary) if unary.operator == UnaryOperator::UnaryNegation => {
            integer(&unary.argument)?.checked_neg()
        }
        _ => None,
    }
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

/// The worlds and packs the project's apps declare, with each app's packs from its outermost scope down to its own.
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
            .flat_map(BTreeMap::keys)
            .cloned()
            .collect();
        if !names.is_empty() {
            contract.app_packs.insert(app.id.clone(), names);
        }
    }
    contract
}
