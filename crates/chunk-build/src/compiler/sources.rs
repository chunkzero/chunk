use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::project::{Child, GENERATED, Inventory, authoring::Module, children};

pub(super) struct Source<'a> {
    pub path: PathBuf,
    pub namespace: String,
    pub authoring: Option<&'a Module>,
}

pub(super) fn discover<'a>(root: &Path, inventory: &'a Inventory) -> io::Result<Vec<Source<'a>>> {
    let mut files = Vec::new();
    collect(&root.join("server"), "shared", &mut files, 0)?;
    for app in &inventory.apps {
        collect(&root.join(&app.directory).join("server"), &format!("apps/{}", app.id), &mut files, 0)?;
    }
    for module in &inventory.modules {
        files.push(Source { path: module.path.clone(), namespace: module.namespace.clone(), authoring: Some(module) });
    }
    if files.len() > 512 {
        return Err(io::Error::other("too many backend source files"));
    }
    if files.iter().try_fold(0_u64, |total, source| Ok::<_, io::Error>(total + fs::metadata(&source.path)?.len()))?
        > 8 * 1024 * 1024
    {
        return Err(io::Error::other("backend source size limit"));
    }
    if !files.iter().any(|source| source.path == root.join("server/schema/index.ts")) {
        return Err(io::Error::other("missing explicitly composed server/schema/index.ts"));
    }
    Ok(files)
}
fn collect(directory: &Path, namespace: &str, files: &mut Vec<Source<'_>>, depth: usize) -> io::Result<()> {
    if depth > 32 {
        return Err(io::Error::other("source nesting limit"));
    }
    for Child { name, path, kind } in children(directory, "source", |name| GENERATED.contains(&name))? {
        if kind.is_dir() {
            collect(&path, &format!("{namespace}/{name}"), files, depth + 1)?;
        } else if kind.is_file()
            && let Some(stem) = name.strip_suffix(".ts").or_else(|| name.strip_suffix(".mts"))
        {
            files.push(Source { path, namespace: format!("{namespace}/{stem}"), authoring: None });
            if files.len() > 512 {
                return Err(io::Error::other("too many backend source files"));
            }
        }
    }
    Ok(())
}
