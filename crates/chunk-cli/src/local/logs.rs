use std::{
    collections::HashMap,
    fmt::{Debug, Write as _},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::Duration,
};

use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing_subscriber::{EnvFilter, Layer, layer::Context, prelude::*};

use super::report::{Reporter, Source};

const READ_LIMIT: u64 = 256 * 1024;

/// Service logs stay at warnings in plain output; `RUST_LOG` restores detail.
pub(super) fn plain() {
    tracing_subscriber::fmt().with_env_filter(filter("warn")).init();
}

/// Routes service logs to the TUI, grouped by the crate that emitted them.
pub(super) fn tui(reporter: Reporter) {
    tracing_subscriber::registry().with(Forward(reporter).with_filter(filter("info"))).init();
}

fn filter(default: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into())
}

struct Forward(Reporter);

impl<S: tracing::Subscriber> Layer<S> for Forward {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let metadata = event.metadata();
        let mut line = Line::default();
        event.record(&mut line);
        self.0.log(Source::of(metadata.target()), format!("{:<5} {}{}", metadata.level(), line.message, line.fields));
    }
}

#[derive(Default)]
struct Line {
    message: String,
    fields: String,
}

impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
}

/// Follows every `<control>/*/nodes/*.jvm.log`, skipping output written before the session started.
pub(super) async fn follow_jvms(control: PathBuf, reporter: Reporter, stop: CancellationToken) {
    let mut offsets = HashMap::new();
    let mut first = true;
    loop {
        for path in jvm_logs(&control) {
            let offset = offsets.entry(path.clone()).or_insert_with(|| {
                if first { std::fs::metadata(&path).map_or(0, |metadata| metadata.len()) } else { 0 }
            });
            let host = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            for line in read_lines(&path, offset) {
                reporter.log(Source::Jvm, format!("{} {line}", &host[..host.len().min(8)]));
            }
        }
        first = false;
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
}

fn jvm_logs(control: &Path) -> Vec<PathBuf> {
    let Ok(releases) = std::fs::read_dir(control) else { return Vec::new() };
    releases
        .flatten()
        .filter_map(|release| std::fs::read_dir(release.path().join("nodes")).ok())
        .flat_map(|nodes| nodes.flatten().map(|entry| entry.path()))
        .filter(|path| path.to_str().is_some_and(|path| path.ends_with(".jvm.log")))
        .collect()
}

/// Reads complete lines appended since `offset`, leaving a partial final line for the next read.
fn read_lines(path: &Path, offset: &mut u64) -> Vec<String> {
    let mut buffer = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        file.seek(SeekFrom::Start(*offset))?;
        file.take(READ_LIMIT).read_to_end(&mut buffer)
    });
    if read.is_err() {
        return Vec::new();
    }
    let end = match buffer.iter().rposition(|byte| *byte == b'\n') {
        Some(end) => end + 1,
        None if buffer.len() as u64 == READ_LIMIT => buffer.len(),
        None => return Vec::new(),
    };
    *offset += end as u64;
    String::from_utf8_lossy(&buffer[..end]).lines().map(str::to_owned).collect()
}
