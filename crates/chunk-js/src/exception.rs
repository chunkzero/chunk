use deno_core::{JsRuntime, op2, v8};

const MESSAGE_BYTES: usize = 16 * 1024;
const FRAMES: usize = 16;

/// The first application error of a run, which fails it.
#[derive(Default)]
pub(crate) struct Reported(pub Option<String>);

/// The report text, which copies at most [`MESSAGE_BYTES`] out of V8 however long its strings are.
struct Text(String);

impl Text {
    fn remaining(&self) -> usize {
        MESSAGE_BYTES - self.0.len()
    }

    fn push(&mut self, scope: &mut v8::PinScope, string: v8::Local<v8::String>) {
        let mut buffer = vec![0; self.remaining()];
        let written = string.write_utf8_v2(scope, &mut buffer, v8::WriteFlags::kReplaceInvalidUtf8, None);
        self.0.push_str(&String::from_utf8_lossy(&buffer[..written]));
    }

    fn push_str(&mut self, text: &str) {
        self.0.push_str(&text[..text.floor_char_boundary(self.remaining())]);
    }
}

/// Formats a thrown value from V8's message and the frames captured with it. Neither runs
/// JavaScript, so getters, proxies and hooks on the value can't stall or enlarge the result.
pub(crate) fn describe(scope: &mut v8::PinScope, exception: v8::Local<v8::Value>) -> String {
    let mut text = Text(String::with_capacity(MESSAGE_BYTES));
    let message = v8::Exception::create_message(scope, exception).get(scope);
    text.push(scope, message);
    if text.0.starts_with("Uncaught ") {
        text.0.drain(.."Uncaught ".len());
    }
    let Some(trace) = v8::Exception::get_stack_trace(scope, exception) else { return text.0 };
    for index in 0..trace.get_frame_count().min(FRAMES) {
        let Some(frame) = trace.get_frame(scope, index) else { break };
        text.push_str("\n    at ");
        let function = frame.get_function_name(scope).filter(|name| name.length() > 0);
        if let Some(function) = function {
            text.push(scope, function);
            text.push_str(" (");
        }
        match frame.get_script_name_or_source_url(scope) {
            Some(script) => text.push(scope, script),
            None => text.push_str("<anonymous>"),
        }
        text.push_str(&format!(":{}:{}", frame.get_line_number(), frame.get_column()));
        if function.is_some() {
            text.push_str(")");
        }
        if text.remaining() == 0 {
            break;
        }
    }
    text.0
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
