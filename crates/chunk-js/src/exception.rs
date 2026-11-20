use std::fmt::Write;

use deno_core::{JsRuntime, op2, v8};

const MESSAGE_BYTES: usize = 16 * 1024;
const FRAMES: usize = 16;

/// The first application error of a run, which fails it.
#[derive(Default)]
pub(crate) struct Reported(pub Option<String>);

/// Formats a thrown value from V8's message and the frames captured with it. Neither runs
/// JavaScript, so getters, proxies and hooks on the value can't stall or enlarge the result.
pub(crate) fn describe(scope: &mut v8::PinScope, exception: v8::Local<v8::Value>) -> String {
    let mut text = v8::Exception::create_message(scope, exception).get(scope).to_rust_string_lossy(scope);
    if text.starts_with("Uncaught ") {
        text.drain(.."Uncaught ".len());
    }
    if let Some(trace) = v8::Exception::get_stack_trace(scope, exception) {
        for index in 0..trace.get_frame_count().min(FRAMES) {
            let Some(frame) = trace.get_frame(scope, index) else { break };
            let script = frame.get_script_name_or_source_url(scope).map(|name| name.to_rust_string_lossy(scope));
            let script = script.as_deref().unwrap_or("<anonymous>");
            let (line, column) = (frame.get_line_number(), frame.get_column());
            match frame.get_function_name(scope).map(|name| name.to_rust_string_lossy(scope)) {
                Some(function) if !function.is_empty() => {
                    write!(text, "\n    at {function} ({script}:{line}:{column})")
                }
                _ => write!(text, "\n    at {script}:{line}:{column}"),
            }
            .expect("write to String");
        }
    }
    text.truncate(text.floor_char_boundary(MESSAGE_BYTES));
    text
}

/// Fails the run with `error` and ends it. The exception `deno_core` then converts is the
/// dispatched `undefined`, never the application's value.
#[op2(fast)]
fn op_chunk_report(scope: &mut v8::PinScope, error: v8::Local<v8::Value>, promise: bool) {
    let message = describe(scope, error);
    if let Some(reported) = JsRuntime::op_state_from(scope).borrow_mut().try_borrow_mut::<Reported>() {
        reported.0.get_or_insert(message);
    }
    let undefined = v8::undefined(scope);
    deno_core::error::dispatch_exception(scope, undefined.into(), promise);
}

deno_core::extension!(chunk_exceptions, ops = [op_chunk_report]);
