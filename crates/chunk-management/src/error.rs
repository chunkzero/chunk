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
    pub(crate) fn into_status(self) -> Status {
        Status { code: Code::from_name(&self.code).unwrap_or(Code::Unknown), message: self.message }
    }
}

/// The error an unsuccessful HTTP response carries: its Connect error body, or else a code from its HTTP status.
pub(crate) fn from_response(status: StatusCode, body: &[u8]) -> Error {
    match serde_json::from_slice::<WireError>(body) {
        Ok(error) => error.into_status().into(),
        Err(_) => Status {
            code: Code::from_http(status),
            message: format!("HTTP {status}: {}", String::from_utf8_lossy(&body[..body.len().min(512)]).trim()),
        }
        .into(),
    }
}
