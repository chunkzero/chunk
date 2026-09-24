use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(clap::Args)]
pub(crate) struct Options {
    #[arg(default_value = ".")]
    pub project: PathBuf,
    /// Also delete local backend data (the tables `chunk dev` writes).
    #[arg(long)]
    pub data: bool,
}

pub(crate) fn run(options: &Options) -> io::Result<()> {
    let removed = clean(options)?;
    if removed.is_empty() {
        cliclack::log::info("Nothing to clean")?;
    } else {
        cliclack::log::success(format!("Removed {}", removed.join(", ")))?;
    }
    if !options.data && options.project.join(".chunk/local/backend").exists() {
        cliclack::log::info("Kept local backend data in .chunk/local/backend; pass --data to delete it")?;
    }
    Ok(())
}

/// Deletes `dist/` and `.chunk/`, keeping local backend data unless `data` is set, and returns the removed paths.
/// Refuses while `chunk dev` owns the project's local state.
fn clean(options: &Options) -> io::Result<Vec<String>> {
    let root = options.project.canonicalize()?;
    if !root.join("chunk.toml").is_file() {
        return Err(io::Error::other(format!("{} has no chunk.toml", root.display())));
    }
    let local = root.join(".chunk/local");
    let _lock = if local.is_dir() {
        let lock = crate::local::runner_lock(&local.join("runner.lock"))
            .map_err(|error| io::Error::other(format!("stop chunk dev first: {error}")))?;
        Some(lock)
    } else {
        None
    };
    remove_generated(&root, options.data)
}

fn remove_generated(root: &Path, data: bool) -> io::Result<Vec<String>> {
    let mut keep_local = BTreeSet::from(["runner.lock"]);
    if !data {
        keep_local.insert("backend");
    }
    let mut removed = Vec::new();
    for (directory, keep) in [(root.join(".chunk"), BTreeSet::from(["local"])), (root.join(".chunk/local"), keep_local)]
    {
        for path in prune(&directory, &keep)? {
            removed.push(path.strip_prefix(root).unwrap_or(&path).display().to_string());
        }
    }
    if remove(&root.join("dist"))? {
        removed.push("dist".into());
    }
    Ok(removed)
}

/// Removes every entry of `directory` whose name is not in `keep`, returning the removed paths.
pub(crate) fn prune(directory: &Path, keep: &BTreeSet<&str>) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut removed = Vec::new();
    for entry in entries {
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
