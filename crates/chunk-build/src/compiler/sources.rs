use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub(super) struct Source {
    pub path: PathBuf,
    pub namespace: String,
}

pub(super) fn discover(root: &Path) -> io::Result<Vec<Source>> {
    let mut files = Vec::new();
    collect(&root.join("server"), "shared", &mut files, 0)?;
    for app in entries(&root.join("apps"))? {
        if app.file_type()?.is_symlink() {
            return Err(io::Error::other("app symlinks are unsupported"));
        }
        if app.file_type()?.is_dir() {
            let name = app
                .file_name()
                .into_string()
                .map_err(|_| io::Error::other("source paths must be UTF-8"))?;
            collect(&app.path().join("server"), &format!("apps/{name}"), &mut files, 0)?;
        }
    }
    if files.iter().try_fold(0_u64, |total, source| {
        Ok::<_, io::Error>(total + fs::metadata(&source.path)?.len())
    })? > 8 * 1024 * 1024
    {
        return Err(io::Error::other("backend source size limit"));
    }
    if !files
        .iter()
        .any(|source| source.path == root.join("server/schema/index.ts"))
    {
        return Err(io::Error::other("missing explicitly composed server/schema/index.ts"));
    }
    Ok(files)
}
fn entries(directory: &Path) -> io::Result<Vec<fs::DirEntry>> {
    match fs::symlink_metadata(directory) {
        Ok(meta) if meta.file_type().is_symlink() => return Err(io::Error::other("source symlinks are unsupported")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
        _ => {}
    }
    let mut entries = fs::read_dir(directory)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}
fn collect(directory: &Path, namespace: &str, files: &mut Vec<Source>, depth: usize) -> io::Result<()> {
    if depth > 32 {
        return Err(io::Error::other("source nesting limit"));
    }
    for entry in entries(directory)? {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| io::Error::other("source paths must be UTF-8"))?;
        if ["node_modules", "_generated", ".chunk"].contains(&name.as_str()) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(io::Error::other("source symlinks are unsupported"));
        }
        if kind.is_dir() {
            collect(&entry.path(), &format!("{namespace}/{name}"), files, depth + 1)?;
        } else if kind.is_file()
            && let Some(stem) = name.strip_suffix(".ts").or_else(|| name.strip_suffix(".mts"))
        {
            files.push(Source {
                path: entry.path(),
                namespace: format!("{namespace}/{stem}"),
            });
            if files.len() > 512 {
                return Err(io::Error::other("too many backend source files"));
            }
        }
    }
    Ok(())
}
