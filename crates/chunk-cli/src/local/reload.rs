use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chunk_build::Release;
use chunk_proto::v1::{NodePhase, NodeStatus};
use notify::{EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// How long a retiring release must stay empty before it stops.
const SETTLE: Duration = Duration::from_secs(10);

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

/// When a replaced release stops: once it is empty, or at a drain deadline that disconnects remaining players.
pub(super) struct Retirement {
    deadline: Option<Instant>,
    empty_since: Option<Instant>,
}

impl Retirement {
    /// Keeps existing sessions on the old version until their players leave.
    pub fn pinned() -> Self {
        Self { deadline: None, empty_since: None }
    }

    pub fn until(deadline: Instant) -> Self {
        Self { deadline: Some(deadline), empty_since: None }
    }

    /// Whether the release can stop now; `nodes` is `None` while its control is unreachable.
    pub fn due(&mut self, nodes: Option<&[NodeStatus]>, now: Instant) -> bool {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            return true;
        }
        let Some(nodes) = nodes else { return false };
        let live: Vec<_> = nodes.iter().filter(|node| node.phase != i32::from(NodePhase::Stopped)).collect();
        if live.is_empty() {
            return true;
        }
        if live.iter().any(|node| {
            node.phase == i32::from(NodePhase::Starting)
                || node.health.as_ref().is_some_and(|health| health.players > 0)
        }) {
            self.empty_since = None;
            return false;
        }
        now.duration_since(*self.empty_since.get_or_insert(now)) >= SETTLE
    }

    pub fn describe(&self, now: Instant) -> String {
        match self.deadline {
            Some(deadline) => format!("drains in {}s", deadline.saturating_duration_since(now).as_secs()),
            None => "pinned".into(),
        }
    }
}

/// Watches project sources, sending each relevant changed path.
pub(super) fn watch(
    root: &Path,
    ignored: &[PathBuf],
) -> notify::Result<(notify::RecommendedWatcher, mpsc::UnboundedReceiver<PathBuf>)> {
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
    for entry in std::fs::read_dir(root)?.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) && relevant(root, ignored, &path) {
            watcher.watch(&path, RecursiveMode::Recursive)?;
        }
    }
    Ok((watcher, receiver))
}

/// Source paths only: build outputs, tool state, hidden files and editor temporaries never trigger a rebuild.
pub(super) fn relevant(root: &Path, ignored: &[PathBuf], path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else { return false };
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
