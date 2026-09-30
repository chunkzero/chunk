//! This process's own log lines, kept in a bounded buffer for a managed core to ship to management.

use chunk_management::v1::{LogSeverity, LogSource};
use std::{
    collections::VecDeque,
    fmt::{self, Write as _},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};
use tokio::time::Instant;
use tracing::{Event, Level, Subscriber, field::Field};
use tracing_subscriber::{EnvFilter, Layer, layer::Context, prelude::*};

/// A longer line is cut to this many bytes.
const MAX_LINE_BYTES: usize = 8 * 1024;
/// What the buffer holds at most, counting each line's text and [`LINE_OVERHEAD`]. The oldest lines go first.
const MAX_BUFFERED_BYTES: usize = 4 * 1024 * 1024;
const LINE_OVERHEAD: usize = 64;

/// The lines this process logs once [`Lines::capture`] starts capturing them.
pub(crate) static LINES: Lines = Lines::new();

/// Logs lines at or above `RUST_LOG` (default `info`) to standard output, and captures those at `info` and above for
/// management once a managed core starts.
pub fn logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::registry().with(filter).with(tracing_subscriber::fmt::layer()).with(Capture).init();
}

struct Capture;

impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let metadata = event.metadata();
        let severity = match *metadata.level() {
            Level::ERROR => LogSeverity::Error,
            Level::WARN => LogSeverity::Warn,
            Level::INFO => LogSeverity::Info,
            _ => return,
        };
        if !LINES.capturing.load(Ordering::Relaxed) {
            return;
        }
        let source = if metadata.target().starts_with("chunk_proxy") { LogSource::Gateway } else { LogSource::Core };
        let mut text = Text::default();
        event.record(&mut text);
        LINES.push(source, severity, text.0);
    }
}

/// An event's message, then its other fields as `name=value`.
#[derive(Default)]
struct Text(String);

impl tracing::field::Visit for Text {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        _ = if field.name() == "message" {
            write!(self.0, "{value:?}")
        } else {
            write!(self.0, "{}={value:?}", field.name())
        };
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_debug(field, &format_args!("{value}"));
    }
}

/// One captured line, numbered in the order it was logged.
#[derive(Clone)]
pub(crate) struct Line {
    pub sequence: u64,
    pub time: SystemTime,
    /// When it was captured, to tell how long it has waited.
    pub at: Instant,
    pub source: LogSource,
    pub severity: LogSeverity,
    pub message: String,
    /// The deployment the process served then, or empty.
    pub deployment: Arc<str>,
}

pub(crate) struct Lines {
    capturing: AtomicBool,
    state: Mutex<State>,
}

struct State {
    lines: VecDeque<Line>,
    bytes: usize,
    /// The sequence of the next line.
    next: u64,
    dropped: u64,
    deployment: Option<Arc<str>>,
}

impl Lines {
    pub(crate) const fn new() -> Self {
        Self {
            capturing: AtomicBool::new(false),
            state: Mutex::new(State { lines: VecDeque::new(), bytes: 0, next: 1, dropped: 0, deployment: None }),
        }
    }

    /// Captures the lines logged from now on.
    pub(crate) fn capture(&self) {
        self.capturing.store(true, Ordering::Relaxed);
    }

    /// Tags later lines with `deployment`.
    pub(crate) fn serving(&self, deployment: &str) {
        self.lock().deployment = Some(deployment.into());
    }

    /// Keeps `message`, cut to [`MAX_LINE_BYTES`], dropping the oldest lines once the buffer is full.
    pub(crate) fn push(&self, source: LogSource, severity: LogSeverity, mut message: String) {
        message.truncate(message.floor_char_boundary(MAX_LINE_BYTES));
        let mut state = self.lock();
        let sequence = state.next;
        state.next += 1;
        state.bytes += message.len() + LINE_OVERHEAD;
        let deployment = state.deployment.clone().unwrap_or_else(|| "".into());
        state.lines.push_back(Line {
            sequence,
            time: SystemTime::now(),
            at: Instant::now(),
            source,
            severity,
            message,
            deployment,
        });
        while state.bytes > MAX_BUFFERED_BYTES
            && let Some(oldest) = state.lines.pop_front()
        {
            state.bytes -= oldest.message.len() + LINE_OVERHEAD;
            state.dropped += 1;
        }
    }

    /// The oldest lines, up to `count` of them and as many as fit `bytes` when each also costs `overhead`.
    pub(crate) fn oldest(&self, count: usize, bytes: usize, overhead: usize) -> Vec<Line> {
        let state = self.lock();
        let mut total = 0;
        let fits = |line: &&Line| {
            total += line.message.len() + line.deployment.len() + overhead;
            total <= bytes
        };
        state.lines.iter().take(count).take_while(fits).cloned().collect()
    }

    /// Forgets the lines up to and including `sequence`, which management holds.
    pub(crate) fn delivered(&self, sequence: u64) {
        let mut state = self.lock();
        while let Some(line) = state.lines.pop_front_if(|line| line.sequence <= sequence) {
            state.bytes -= line.message.len() + LINE_OVERHEAD;
        }
    }

    /// The sequence of the newest line held.
    pub(crate) fn newest(&self) -> Option<u64> {
        self.lock().lines.back().map(|line| line.sequence)
    }

    /// When the oldest line still held was captured.
    pub(crate) fn oldest_at(&self) -> Option<Instant> {
        self.lock().lines.front().map(|line| line.at)
    }

    /// Lines dropped since the process started because the buffer was full.
    pub(crate) fn dropped(&self) -> u64 {
        self.lock().dropped
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
