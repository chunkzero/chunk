use std::{collections::VecDeque, sync::Mutex};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

const RETAINED: usize = 2000;
const EXCERPT: usize = 80;

/// Bounded, interleaved capture of a build's stdout and stderr.
#[derive(Default)]
pub(super) struct Capture(Mutex<VecDeque<String>>);

impl Capture {
    pub async fn read(&self, stream: impl AsyncRead + Unpin) {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line).await.is_ok_and(|read| read > 0) {
            let text = String::from_utf8_lossy(&line).trim_end().to_owned();
            line.clear();
            let Ok(mut lines) = self.0.lock() else { return };
            if lines.len() == RETAINED {
                lines.pop_front();
            }
            lines.push_back(text);
        }
    }

    pub fn excerpt(&self) -> String {
        self.0.lock().map(|lines| excerpt(lines.iter().map(String::as_str))).unwrap_or_default()
    }
}

/// Keeps compiler diagnostics and Gradle's failure summary, dropping progress and advice.
pub(super) fn excerpt<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut advice = false;
    for line in lines {
        if advice {
            advice = !line.trim().is_empty() && !line.starts_with("BUILD FAILED");
            if advice {
                continue;
            }
        }
        if line.starts_with("* Try:") || line.starts_with("Deprecated Gradle features were used") {
            advice = true;
            continue;
        }
        if noise(line) || (line.trim().is_empty() && kept.last().is_none_or(|last| last.trim().is_empty())) {
            continue;
        }
        kept.push(line);
    }
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }
    kept[kept.len().saturating_sub(EXCERPT)..].join("\n")
}

fn noise(line: &str) -> bool {
    const PREFIXES: [&str; 17] = [
        "> Configure project",
        "> Transform ",
        "Starting a Gradle Daemon",
        "To honour the JVM settings",
        "Daemon will be stopped",
        "Welcome to Gradle",
        "Downloading https://",
        "Calculating task graph",
        "Configuration cache ",
        "Reusing configuration cache",
        "You can use '--warning-mode all'",
        "For more on this, please refer to",
        "BUILD SUCCESSFUL",
        "w: ",
        "[Incubating] Problems report",
        "●  ",
        "◆  ",
    ];
    if let Some(task) = line.strip_prefix("> Task ") {
        return !task.ends_with("FAILED");
    }
    line.trim() == "│" || PREFIXES.iter().any(|prefix| line.starts_with(prefix)) || line.contains(" actionable task")
}
