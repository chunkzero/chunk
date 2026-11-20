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

/// Parsed components of a JVM log line emitted by [`follow_jvms`].
#[derive(Debug, PartialEq, Eq)]
pub(super) struct JvmLine<'a> {
    pub host: &'a str,
    pub session: Option<&'a str>,
    pub body: &'a str,
}

/// Splits `<short-host> [[session:<id>] ]<body>` into its host prefix, optional session ID, and message body.
pub(super) fn parse_jvm_line(line: &str) -> JvmLine<'_> {
    let Some((host, rest)) = line.split_once(' ') else {
        return JvmLine { host: "", session: None, body: line };
    };
    if let Some((session, body)) = strip_session_tag(rest) {
        JvmLine { host, session: Some(session), body }
    } else {
        JvmLine { host, session: None, body: rest }
    }
}

fn strip_session_tag(rest: &str) -> Option<(&str, &str)> {
    let after_prefix = rest.strip_prefix("[session:")?;
    let end = after_prefix.find(']')?;
    let id = &after_prefix[..end];
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
        return None;
    }
    let trailing = &after_prefix[end + 1..];
    if trailing.is_empty() { Some((id, "")) } else { trailing.strip_prefix(' ').map(|body| (id, body)) }
}

/// Follows every `<nodes>/*.jvm.log`, skipping output written before the session started.
pub(super) async fn follow_jvms(nodes: PathBuf, reporter: Reporter, stop: CancellationToken) {
    let mut offsets = HashMap::new();
    let mut first = true;
    loop {
        for path in jvm_logs(&nodes) {
            let offset = offsets.entry(path.clone()).or_insert_with(|| {
                if first { std::fs::metadata(&path).map_or(0, |metadata| metadata.len()) } else { 0 }
            });
            let file = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
            let host = file.strip_suffix(".jvm.log").unwrap_or(file);
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

fn jvm_logs(nodes: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(nodes) else { return Vec::new() };
    entries
        .flatten()
        .map(|entry| entry.path())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tagged_and_untagged_jvm_lines() {
        assert_eq!(
            parse_jvm_line("5a9e4aba [session:f47ac10b-58cc] player joined"),
            JvmLine { host: "5a9e4aba", session: Some("f47ac10b-58cc"), body: "player joined" }
        );
        assert_eq!(
            parse_jvm_line("5a9e4aba [session:sess_1]"),
            JvmLine { host: "5a9e4aba", session: Some("sess_1"), body: "" }
        );
        assert_eq!(
            parse_jvm_line("5a9e4aba JVM started"),
            JvmLine { host: "5a9e4aba", session: None, body: "JVM started" }
        );
        assert_eq!(
            parse_jvm_line("5a9e4aba [session:invalid id] hi"),
            JvmLine { host: "5a9e4aba", session: None, body: "[session:invalid id] hi" }
        );
        assert_eq!(
            parse_jvm_line("5a9e4aba [session:] hi"),
            JvmLine { host: "5a9e4aba", session: None, body: "[session:] hi" }
        );
    }
}
