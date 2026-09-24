use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

/// Held by `chunk dev` for the whole session, whatever its state directory.
pub(crate) const PROJECT_LOCK: &str = ".chunk/dev.lock";

#[derive(clap::Args)]
pub(crate) struct Options {
    #[arg(default_value = ".")]
    pub project: PathBuf,
    /// Also delete local backend data (the tables `chunk dev` writes).
    #[arg(long)]
    pub data: bool,
}

struct Cleaned {
    removed: Vec<String>,
    kept: Vec<String>,
}

pub(crate) fn run(options: &Options) -> io::Result<()> {
    let Cleaned { removed, kept } = clean(options)?;
    if removed.is_empty() {
        cliclack::log::info("Nothing to clean")?;
    } else {
        cliclack::log::success(format!("Removed {}", removed.join(", ")))?;
    }
    if !kept.is_empty() {
        cliclack::log::info(format!("Kept local backend data in {}; pass --data to delete it", kept.join(", ")))?;
    }
    Ok(())
}

/// Deletes `dist/` and `.chunk/`, keeping local backend data unless `data` is set.
/// Refuses while `chunk dev` runs for the project.
fn clean(options: &Options) -> io::Result<Cleaned> {
    let root = options.project.canonicalize()?;
    if !root.join("chunk.toml").is_file() {
        return Err(io::Error::other(format!("{} has no chunk.toml", root.display())));
    }
    let _lock = if is_directory(&root.join(".chunk"))? {
        let lock = crate::local::runner_lock(&root.join(PROJECT_LOCK))
            .map_err(|error| io::Error::other(format!("stop chunk dev first: {error}")))?;
        Some(lock)
    } else {
        None
    };
    remove_generated(&root, options.data)
}

/// Prunes `.chunk/` except state directories, which lose everything but their lock and, unless `data` is set,
/// their backend data.
fn remove_generated(root: &Path, data: bool) -> io::Result<Cleaned> {
    let chunk = root.join(".chunk");
    let states = state_directories(&chunk)?;
    let mut keep: BTreeSet<_> = states.iter().map(String::as_str).collect();
    keep.insert("dev.lock");
    let mut keep_state = BTreeSet::from(["runner.lock"]);
    if !data {
        keep_state.insert("backend");
    }
    let relative = |path: PathBuf| path.strip_prefix(root).unwrap_or(&path).display().to_string();
    let mut removed: Vec<_> = prune(&chunk, &keep)?.into_iter().map(relative).collect();
    let mut kept = Vec::new();
    for state in &states {
        let state = chunk.join(state);
        removed.extend(prune(&state, &keep_state)?.into_iter().map(relative));
        if !data && state.join("backend").exists() {
            kept.push(relative(state.join("backend")));
        }
    }
    if remove(&root.join("dist"))? {
        removed.push("dist".into());
    }
    Ok(Cleaned { removed, kept })
}

/// Names the `chunk dev` state directories directly under `chunk`, recognized by their runner lock.
fn state_directories(chunk: &Path) -> io::Result<Vec<String>> {
    if !is_directory(chunk)? {
        return Ok(Vec::new());
    }
    let mut states = Vec::new();
    for entry in fs::read_dir(chunk)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry.path().join("runner.lock").is_file()
            && let Some(name) = entry.file_name().to_str()
        {
            states.push(name.to_owned());
        }
    }
    Ok(states)
}

/// Removes every entry of `directory` whose name is not in `keep`, returning the removed paths.
pub(crate) fn prune(directory: &Path, keep: &BTreeSet<&str>) -> io::Result<Vec<PathBuf>> {
    if !is_directory(directory)? {
        return Ok(Vec::new());
    }
    let mut removed = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(|name| keep.contains(name)) {
            continue;
        }
        if remove(&entry.path())? {
            removed.push(entry.path());
        }
    }
    Ok(removed)
}

/// Returns whether `path` is a directory, refusing a symlink so pruning never reaches outside the project.
fn is_directory(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_symlink() => {
            Err(io::Error::other(format!("{} is a symlink; not cleaning through it", path.display())))
        }
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Removes a file or directory tree, returning whether it existed.
pub(crate) fn remove(path: &Path) -> io::Result<bool> {
    let removed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests;
