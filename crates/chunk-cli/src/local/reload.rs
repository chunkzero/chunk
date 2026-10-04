use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
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

/// How `next` differs from `current`, each a release and the ID of the asset revision built with it. New assets need
/// new JVMs, as changed app JARs do.
pub(super) fn classify((current, current_assets): (&Release, &str), (next, next_assets): (&Release, &str)) -> Change {
    if current_assets != next_assets || current.apps != next.apps {
        Change::Jvm
    } else if current.id == next.id {
        Change::Unchanged
    } else {
        Change::Backend
    }
}

/// A replaced release draining in control, which stops it once its sessions have no players, or at its deadline.
pub(super) struct Drain {
    since: Instant,
    /// How long after `since` the release stops, whoever remains.
    deadline: Option<Duration>,
}

impl Drain {
    /// Keeps existing sessions on the old version until their players leave, or until `deadline` after now.
    pub fn until(deadline: Option<Duration>) -> Self {
        Self { since: Instant::now(), deadline }
    }

    /// Stops the release `within` from now unless it stops sooner anyway; used when a JVM change supersedes pinned
    /// sessions.
    pub fn drain_within(&mut self, within: Duration) {
        let deadline = self.since.elapsed() + within;
        self.deadline = Some(self.deadline.map_or(deadline, |current| current.min(deadline)));
    }

    /// Control's policy: the deadline as a duration from when the drain started.
    pub fn policy(&self) -> DrainPolicy {
        DrainPolicy { max_age: None, deadline: self.deadline }
    }

    pub fn describe(&self, now: Instant) -> String {
        match self.deadline.map(|deadline| (self.since + deadline).saturating_duration_since(now)) {
            Some(remaining) if remaining.is_zero() => "stopping".into(),
            Some(remaining) => format!("drains in {}s", remaining.as_secs()),
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
