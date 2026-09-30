use std::fmt;

use reqwest::StatusCode;
use serde::Deserialize;

/// A failed call: the status the service answered with, or a failure to reach it or to read its answer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Status(#[from] Status),
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("malformed response: {0}")]
    Protocol(String),
}

impl Error {
    /// The Connect code that best describes the failure: an unreachable service is `Unavailable` and a malformed
    /// answer `Internal`.
    #[must_use]
    pub fn code(&self) -> Code {
        match self {
            Self::Status(status) => status.code,
            Self::Transport(_) => Code::Unavailable,
            Self::Protocol(_) => Code::Internal,
        }
    }
}

/// An error status from the service.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct Status {
    pub code: Code,
    pub message: String,
}

/// The Connect error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Code {
    Canceled,
    Unknown,
    InvalidArgument,
    DeadlineExceeded,
    NotFound,
    AlreadyExists,
    PermissionDenied,
    ResourceExhausted,
    FailedPrecondition,
    Aborted,
    OutOfRange,
    Unimplemented,
    Internal,
    Unavailable,
    DataLoss,
    Unauthenticated,
}

const CODES: [(Code, &str); 16] = [
    (Code::Canceled, "canceled"),
    (Code::Unknown, "unknown"),
    (Code::InvalidArgument, "invalid_argument"),
    (Code::DeadlineExceeded, "deadline_exceeded"),
    (Code::NotFound, "not_found"),
    (Code::AlreadyExists, "already_exists"),
    (Code::PermissionDenied, "permission_denied"),
    (Code::ResourceExhausted, "resource_exhausted"),
    (Code::FailedPrecondition, "failed_precondition"),
    (Code::Aborted, "aborted"),
    (Code::OutOfRange, "out_of_range"),
    (Code::Unimplemented, "unimplemented"),
    (Code::Internal, "internal"),
    (Code::Unavailable, "unavailable"),
    (Code::DataLoss, "data_loss"),
    (Code::Unauthenticated, "unauthenticated"),
];

impl Code {
    /// The code's name on the wire, such as `failed_precondition`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        CODES.iter().find(|(code, _)| *code == self).map_or("unknown", |(_, name)| name)
    }

    /// The code named `name`; None for names Connect does not define.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        CODES.iter().find(|(_, candidate)| *candidate == name).map(|(code, _)| *code)
    }

    /// The code Connect assigns an HTTP status that carries no Connect error, for example from a proxy.
    #[must_use]
    pub fn from_http(status: StatusCode) -> Self {
        match status.as_u16() {
            400 => Self::Internal,
            401 => Self::Unauthenticated,
            403 => Self::PermissionDenied,
            404 => Self::Unimplemented,
            429 | 502..=504 => Self::Unavailable,
            _ => Self::Unknown,
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Deserialize)]
pub(crate) struct WireError {
    code: String,
    #[serde(default)]
    message: String,
}

impl WireError {
    pub(crate) fn into_status(self, token: Option<&str>) -> Status {
        Status { code: Code::from_name(&self.code).unwrap_or(Code::Unknown), message: redact(&self.message, token) }
    }
}

/// The error an unsuccessful HTTP response carries: its Connect error body, or else a code from its HTTP status.
pub(crate) fn from_response(status: StatusCode, body: &[u8], token: Option<&str>) -> Error {
    if let Ok(error) = serde_json::from_slice::<WireError>(body) {
        return error.into_status(token).into();
    }
    let body = redact(&String::from_utf8_lossy(body), token);
    let body: String = body.chars().take(512).collect();
    Status { code: Code::from_http(status), message: format!("HTTP {status}: {}", body.trim()) }.into()
}

/// A protocol error whose description, which may quote the response, never shows `token`.
pub(crate) fn protocol(problem: &str, token: Option<&str>) -> Error {
    Error::Protocol(redact(problem, token))
}

const REDACTED: &str = "<redacted>";

/// `text` without `token`, `Authorization` values, or long bearer tokens, which a proxy's error page may echo from the
/// request.
fn redact(text: &str, token: Option<&str>) -> String {
    let text = match token {
        Some(token) if !token.is_empty() => text.replace(token, REDACTED),
        _ => text.to_owned(),
    };
    let text = redact_after(&text, "authorization", header_value);
    redact_after(&text, "bearer", bearer_value)
}

/// Replaces the value `value` finds after each case-insensitive `keyword`, as a range of the text after it.
fn redact_after(text: &str, keyword: &str, value: fn(&str) -> Option<(usize, usize)>) -> String {
    let lower = text.to_ascii_lowercase();
    let (mut redacted, mut kept, mut from) = (String::with_capacity(text.len()), 0, 0);
    while let Some(found) = lower[from..].find(keyword) {
        from += found + keyword.len();
        if let Some((start, end)) = value(&text[from..]) {
            redacted.push_str(&text[kept..from + start]);
            redacted.push_str(REDACTED);
            kept = from + end;
            from = kept;
        }
    }
    redacted.push_str(&text[kept..]);
    redacted
}

/// A header's value after `: ` or `=`, as plain text or quoted JSON, up to the end of its line or string.
fn header_value(rest: &str) -> Option<(usize, usize)> {
    let quoting = |c: char| matches!(c, '"' | '\'' | '\\' | ' ' | '\t');
    let value = rest.trim_start_matches(quoting).strip_prefix([':', '='])?.trim_start_matches(quoting);
    let start = rest.len() - value.len();
    let length = value.find(['"', '\'', '\\', '\r', '\n', ',', ';', '&', '}']).unwrap_or(value.len());
    (length > 0).then_some((start, start + length))
}

/// A token-like word of at least 16 characters after `bearer `, so prose such as "a bearer token" stays readable.
fn bearer_value(rest: &str) -> Option<(usize, usize)> {
    let value = rest.trim_start_matches([' ', '\t']);
    let start = rest.len() - value.len();
    let length = value.find(|c: char| !(c.is_ascii_alphanumeric() || "-._~+/=".contains(c))).unwrap_or(value.len());
    (start > 0 && length >= 16).then_some((start, start + length))
}
