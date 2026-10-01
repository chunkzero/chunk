//! Protocol errors, and how backend and control failures map onto their codes.

use chunk_proto::sync::v1::{Error, error::Code};

/// The longest error message sent; a longer one is cut here and marked.
const MESSAGE_BYTES: usize = 64 * 1024;

pub(super) fn error(code: Code, message: impl Into<String>) -> Error {
    let mut message = message.into();
    if message.len() > MESSAGE_BYTES {
        message.truncate(message.floor_char_boundary(MESSAGE_BYTES));
        message.push_str("… (truncated)");
    }
    Error { code: code.into(), message }
}

pub(super) fn invalid(message: impl Into<String>) -> Error {
    error(Code::Invalid, message)
}

pub(super) fn denied(message: impl Into<String>) -> Error {
    error(Code::Denied, message)
}

pub(super) fn backend(failure: &chunk_backend::Error) -> Error {
    use chunk_backend::Error as Backend;
    let code = match failure {
        Backend::Overloaded(_) => Code::Overloaded,
        Backend::Storage(inner) if matches!(inner.as_ref(), chunk_store::Error::Capacity) => Code::Overloaded,
        Backend::Storage(inner) if matches!(inner.as_ref(), chunk_store::Error::OperationMismatch) => {
            Code::OperationMismatch
        }
        Backend::OperationMismatch => Code::OperationMismatch,
        Backend::Invalid(_) | Backend::Json(_) => Code::Invalid,
        Backend::Contract | Backend::Unknown => Code::Contract,
        Backend::ActionOutcomeUnknown => Code::OutcomeUnknown,
        Backend::JavaScript(inner) => match inner.as_ref() {
            chunk_js::Error::JavaScript(thrown) => return error(Code::Application, thrown.clone()),
            chunk_js::Error::Deadline | chunk_js::Error::Heap => Code::Application,
            chunk_js::Error::Invalid(_) | chunk_js::Error::UnknownDeployment => Code::Contract,
            chunk_js::Error::Cancelled | chunk_js::Error::Io(_) => Code::Unavailable,
        },
        Backend::Busy
        | Backend::NotReady
        | Backend::Retired
        | Backend::Retry
        | Backend::Closed
        | Backend::Cancelled
        | Backend::CommitFailed
        | Backend::Storage(_)
        | Backend::Io(_) => Code::Unavailable,
    };
    error(code, failure.to_string())
}

/// Maps a failed caller check: requests control rejects are denied.
pub(super) fn control(failure: &chunk_control::Error) -> Error {
    match failure {
        chunk_control::Error::Invalid(message) => denied(*message),
        chunk_control::Error::Stopped => error(Code::Stopped, failure.to_string()),
        _ => error(Code::Unavailable, failure.to_string()),
    }
}

/// Maps a failed control operation: a reused operation ID with another request mismatches, a JVM that differs from its
/// host's launch or names another host's player is denied, a request control rejects or refuses is invalid, and a
/// failure that may pass is unavailable.
pub(super) fn operation(failure: &chunk_control::Error) -> Error {
    use chunk_control::Error as Control;
    let code = match failure {
        Control::Invalid(
            chunk_control::OPERATION_CHANGED
            | chunk_control::DRAIN_CHANGED
            | chunk_control::MOVE_CHANGED
            | chunk_control::MOVE_NAMES_CLAIM
            | chunk_control::OPERATOR_CALL_CHANGED,
        ) => Code::OperationMismatch,
        Control::Invalid(super::super::runner::LAUNCH_MISMATCH | chunk_control::NOT_HOSTED) => Code::Denied,
        Control::Invalid(_) | Control::Refused(_) => Code::Invalid,
        Control::Capacity | Control::Busy => Code::Overloaded,
        Control::Stopped => Code::Stopped,
        _ => Code::Unavailable,
    };
    error(code, failure.to_string())
}
