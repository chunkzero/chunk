use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use chunk_build::Release;
use chunk_control::DrainPolicy;
use notify::{EventKind, RecursiveMode, Watcher as _};
use tokio::sync::mpsc;

const IGNORED: [&str; 8] = ["build", ".gradle", ".chunk", ".git", ".idea", ".kotlin", "node_modules", "dist"];

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Change {
    Unchanged,
    /// Only backend code or contracts changed; every app JAR is identical.
    Backend,
    Jvm,
}

pub(super) fn classify(current: &Release, next: &Release) -> Change {
    if current.id == next.id {
        Change::Unchanged
    } else if current.apps == next.apps {
        Change::Backend
    } else {
        Change::Jvm
    }
}

/// A replaced release draining in control, which stops it once its sessions have no players, or at its deadline.
pub(super) struct Drain {
    deadline: Option<Instant>,
}

impl Drain {
    /// Keeps existing sessions on the old version until their players leave, or until `deadline`.
    pub fn until(deadline: Option<Instant>) -> Self {
        Self { deadline }
    }

    /// Applies `deadline` unless an earlier one is already set; used when a JVM change supersedes pinned sessions.
    pub fn drain_by(&mut self, deadline: Instant) {
        self.deadline = Some(self.deadline.map_or(deadline, |current| current.min(deadline)));
    }

    /// Control's policy for the rest of the drain from `now`.
    pub fn policy(&self, now: Instant) -> DrainPolicy {
        DrainPolicy { max_age: None, deadline: self.deadline.map(|deadline| deadline.saturating_duration_since(now)) }
    }

    pub fn describe(&self, now: Instant) -> String {
        match self.deadline {
            Some(deadline) if deadline <= now => "stopping".into(),
            Some(deadline) => format!("drains in {}s", deadline.saturating_duration_since(now).as_secs()),
            None => "pinned".into(),
        }
    }
}

/// Project source watches: the root itself plus each relevant top-level directory, recursively.
pub(super) struct Watcher {
    notify: notify::RecommendedWatcher,
    root: PathBuf,
    ignored: Vec<PathBuf>,
}

impl Watcher {
    /// Watches a changed path when it is a relevant top-level directory, so directories created or
    /// recreated after startup are followed too.
    pub fn cover(&mut self, path: &Path) {
        if path.parent() == Some(self.root.as_path()) && path.is_dir() && relevant(&self.root, &self.ignored, path) {
            let _ = self.notify.watch(path, RecursiveMode::Recursive);
        }
    }
}

/// Watches project sources, sending each relevant changed path.
pub(super) fn watch(root: &Path, ignored: &[PathBuf]) -> notify::Result<(Watcher, mpsc::UnboundedReceiver<PathBuf>)> {
    let (sender, receiver) = mpsc::unbounded_channel();
    let (project, excluded) = (root.to_owned(), ignored.to_vec());
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        for path in event.paths.into_iter().filter(|path| relevant(&project, &excluded, path)) {
            let _ = sender.send(path);
        }
    })?;
    watcher.watch(root, RecursiveMode::NonRecursive)?;
    let mut watcher = Watcher { notify: watcher, root: root.to_owned(), ignored: ignored.to_vec() };
    for entry in std::fs::read_dir(root)?.flatten() {
        watcher.cover(&entry.path());
    }
    Ok((watcher, receiver))
}

/// Source paths and `.dev.vars` only: build outputs, tool state, other hidden files and editor temporaries never
/// trigger a rebuild.
pub(super) fn relevant(root: &Path, ignored: &[PathBuf], path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else { return false };
    if relative == Path::new(super::dev_vars::FILE) {
        return true;
    }
    if relative.as_os_str().is_empty() || ignored.iter().any(|ignored| path.starts_with(ignored)) {
        return false;
    }
    let generated = relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        IGNORED.contains(&name.as_ref()) || name.starts_with('.')
    });
    let name = relative.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
    !generated && !name.ends_with('~') && !name.ends_with(".swp") && !name.ends_with(".tmp") && name != "4913"
}

#[cfg(test)]
mod tests;
