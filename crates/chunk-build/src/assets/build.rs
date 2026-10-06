use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Cursor, Read},
    path::Path,
};

use chunk_contract::{
    ASSET_REVISION_VERSION, AssetBlob, AssetRevision, MAX_ASSET_ENTRIES, MAX_FILE_BYTES, MAX_PACK_BYTES,
    MAX_REVISION_BYTES, MAX_WORLD_BYTES, PackBlob,
};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

use super::Store;
use crate::{
    project::{self, Pack},
    publication::MAX_COMPONENTS,
};

/// The bytes every Polar world starts with.
const POLAR_MAGIC: &[u8; 4] = b"Polr";

/// Builds the asset revision of the project at `root` into `store` and writes it there: the files of `assets/` and of
/// each app's `assets/`, except declared world and pack sources; each app's Polar worlds; and every pack, directories
/// zipped reproducibly.
/// # Errors
/// Rejects invalid declarations, symlinks, nonportable paths, sources of the wrong format, revisions over the limits of
/// [`chunk_contract`], and filesystem failures.
pub fn build_revision(root: &Path, store: &Store) -> io::Result<AssetRevision> {
    let inventory = project::inspect_inventory(root)?;
    let mut builder = Builder { store, entries: 0 };
    let mut revision = AssetRevision { version: ASSET_REVISION_VERSION, ..AssetRevision::default() };
    let scope_packs: Vec<(&String, &Pack)> = inventory.packs.values().flatten().collect();
    let excluded = scope_packs.iter().map(|(_, pack)| pack.source.as_str()).collect();
    builder.files(&root.join("assets"), "assets", "", &excluded, &mut revision.shared)?;
    let mut packs = scope_packs;
    for app in &inventory.apps {
        let mut assets = chunk_contract::AppAssets::default();
        for (name, world) in &app.worlds {
            let path = root.join(&world.source);
            let mut magic = [0; 4];
            regular(&path)?.read_exact(&mut magic).ok();
            if &magic != POLAR_MAGIC {
                return Err(io::Error::other(format!("{} is not a Polar world", path.display())));
            }
            assets.worlds.insert(name.clone(), builder.blob(&path, MAX_WORLD_BYTES)?);
        }
        let worlds = app.worlds.values().map(|world| world.source.as_str());
        let excluded = worlds.chain(app.packs.values().map(|pack| pack.source.as_str())).collect();
        let prefix = format!("{}/assets", app.directory);
        builder.files(&root.join(&prefix), &prefix, "", &excluded, &mut assets.files)?;
        if assets != chunk_contract::AppAssets::default() {
            revision.apps.insert(app.id.clone(), assets);
        }
        packs.extend(&app.packs);
    }
    for (name, pack) in packs {
        let path = root.join(&pack.source);
        let bytes = if fs::symlink_metadata(&path)?.is_dir() { zip_directory(&path)? } else { read_zip(&path)? };
        let (sha256, sha1) = (format!("{:x}", Sha256::digest(&bytes)), format!("{:x}", Sha1::digest(&bytes)));
        store.insert(&sha256, &bytes)?;
        revision.packs.insert(name.clone(), PackBlob { sha256, sha1, size: bytes.len() as u64 });
        builder.count()?;
    }
    revision.validate().map_err(io::Error::other)?;
    store.write_revision(&revision)?;
    Ok(revision)
}

struct Builder<'a> {
    store: &'a Store,
    entries: usize,
}

impl Builder<'_> {
    /// Adds the regular files under `directory` to `files`, by their path relative to it, skipping the
    /// project-relative paths in `excluded`.
    fn files(
        &mut self,
        directory: &Path,
        prefix: &str,
        relative: &str,
        excluded: &BTreeSet<&str>,
        files: &mut BTreeMap<String, AssetBlob>,
    ) -> io::Result<()> {
        if relative.split('/').count() >= MAX_COMPONENTS {
            return Err(io::Error::other(format!("{} nests too deeply", directory.display())));
        }
        for child in project::children(directory, "asset", |_| false)? {
            let path = if relative.is_empty() { child.name } else { format!("{relative}/{}", child.name) };
            if excluded.contains(format!("{prefix}/{path}").as_str()) {
                continue;
            }
            if child.kind.is_dir() {
                self.files(&child.path, prefix, &path, excluded, files)?;
            } else if child.kind.is_file() {
                files.insert(path, self.blob(&child.path, MAX_FILE_BYTES)?);
            } else {
                return Err(io::Error::other(format!("{} is not a regular file", child.path.display())));
            }
        }
        Ok(())
    }

    /// Stores the file at `path`, of at most `limit` bytes, unless the store holds it already.
    fn blob(&mut self, path: &Path, limit: u64) -> io::Result<AssetBlob> {
        self.count()?;
        let mut digest = Sha256::new();
        let size = io::copy(&mut regular(path)?.take(limit + 1), &mut digest)?;
        if size > limit {
            return Err(io::Error::other(format!("{} exceeds the {limit} byte asset limit", path.display())));
        }
        let sha256 = format!("{:x}", digest.finalize());
        self.store.insert_file(&sha256, path)?;
        Ok(AssetBlob { sha256, size })
    }

    fn count(&mut self) -> io::Result<()> {
        self.entries += 1;
        if self.entries > MAX_ASSET_ENTRIES {
            return Err(io::Error::other(format!("an asset revision holds at most {MAX_ASSET_ENTRIES} entries")));
        }
        Ok(())
    }
}

fn regular(path: &Path) -> io::Result<fs::File> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other(format!("{} must be a regular file, not a symlink", path.display())));
    }
    fs::File::open(path)
}

/// Zips a pack directory reproducibly: entries in path order, fixed timestamps and permissions, deflated.
fn zip_directory(directory: &Path) -> io::Result<Vec<u8>> {
    let mut files = Vec::new();
    pack_files(directory, "", &mut files)?;
    let mut input = 0;
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(0o644);
    for (name, path) in files {
        input += fs::symlink_metadata(&path)?.len();
        if input > MAX_REVISION_BYTES {
            return Err(io::Error::other(format!("{} exceeds the asset revision size limit", directory.display())));
        }
        zip.start_file(name, options).map_err(io::Error::other)?;
        io::copy(&mut fs::File::open(&path)?, &mut zip)?;
    }
    let bytes = zip.finish().map_err(io::Error::other)?.into_inner();
    if bytes.len() as u64 > MAX_PACK_BYTES {
        return Err(io::Error::other(format!("{} zips to more than {MAX_PACK_BYTES} bytes", directory.display())));
    }
    Ok(bytes)
}

fn pack_files(directory: &Path, relative: &str, files: &mut Vec<(String, std::path::PathBuf)>) -> io::Result<()> {
    if relative.split('/').count() >= MAX_COMPONENTS {
        return Err(io::Error::other(format!("{} nests too deeply", directory.display())));
    }
    for child in project::children(directory, "pack", |_| false)? {
        let name = if relative.is_empty() { child.name } else { format!("{relative}/{}", child.name) };
        if child.kind.is_dir() {
            pack_files(&child.path, &name, files)?;
        } else if child.kind.is_file() {
            files.push((name, child.path));
        } else {
            return Err(io::Error::other(format!("{} is not a regular file", child.path.display())));
        }
    }
    Ok(())
}

fn read_zip(path: &Path) -> io::Result<Vec<u8>> {
    let bytes = crate::read_limited(path, MAX_PACK_BYTES)?;
    let mut archive = ZipArchive::new(Cursor::new(bytes.as_slice()))
        .map_err(|error| io::Error::other(format!("{} is not a zip: {error}", path.display())))?;
    if archive.by_name("pack.mcmeta").is_err() {
        return Err(io::Error::other(format!("{} has no pack.mcmeta", path.display())));
    }
    Ok(bytes)
}
