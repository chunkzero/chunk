use deno_core::{JsRuntime, OpState, op2};
use deno_error::JsErrorBox;

use crate::Error;

struct Context {
    timestamp: f64,
    random: u64,
    logs: Vec<crate::Log>,
    log_bytes: usize,
}

pub(crate) fn initialize(runtime: &mut JsRuntime) {
    runtime.op_state().borrow_mut().put(None::<Context>);
}

#[allow(clippy::cast_precision_loss)]
pub(crate) fn begin(runtime: &mut JsRuntime, timestamp: i64, seed: u64) -> Result<(), Error> {
    // Date's range fits within the integers exactly represented by a JS number.
    if !(-8_640_000_000_000_000..=8_640_000_000_000_000).contains(&timestamp) {
        return Err(Error::Invalid("snapshot timestamp outside Date range"));
    }
    runtime.op_state().borrow_mut().put(Some(Context {
        timestamp: timestamp as f64,
        random: seed,
        logs: Vec::new(),
        log_bytes: 0,
    }));
    Ok(())
}

pub(crate) fn end(runtime: &mut JsRuntime) -> Vec<crate::Log> {
    runtime
        .op_state()
        .borrow_mut()
        .borrow_mut::<Option<Context>>()
        .take()
        .map_or_else(Vec::new, |context| context.logs)
}

fn context(state: &mut OpState) -> Result<&mut Context, JsErrorBox> {
    state
        .borrow_mut::<Option<Context>>()
        .as_mut()
        .ok_or_else(|| JsErrorBox::generic("Invocation time and randomness unavailable during initialization"))
}

#[op2(fast)]
fn op_chunk_now(state: &mut OpState) -> Result<f64, JsErrorBox> {
    Ok(context(state)?.timestamp)
}

#[op2(fast)]
#[allow(clippy::cast_precision_loss)]
fn op_chunk_random(state: &mut OpState) -> Result<f64, JsErrorBox> {
    let context = context(state)?;
    // SplitMix64 with the top 53 bits mapped exactly into [0, 1).
    context.random = context.random.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = context.random;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^= value >> 31;
    Ok((value >> 11) as f64 / 9_007_199_254_740_992.0)
}

#[op2(fast)]
fn op_chunk_log(state: &mut OpState, #[string] level: &str, #[string] message: &str) -> Result<(), JsErrorBox> {
    let context = context(state)?;
    if context.logs.len() >= 32 || context.log_bytes + message.len() > 16 * 1024 {
        return Err(JsErrorBox::range_error("Invocation log limit exceeded"));
    }
    context.log_bytes += message.len();
    context.logs.push(crate::Log {
        level: level.into(),
        message: message.into(),
    });
    Ok(())
}

#[op2(fast)]
fn op_chunk_digest(
    #[string] algorithm: &str,
    #[buffer] input: &[u8],
    #[buffer] output: &mut [u8],
) -> Result<(), JsErrorBox> {
    use sha2::Digest;
    if input.len() > 1024 * 1024 {
        return Err(JsErrorBox::range_error("Digest input limit exceeded"));
    }
    let bytes = match algorithm {
        "SHA-1" => sha1::Sha1::digest(input).to_vec(),
        "SHA-256" => sha2::Sha256::digest(input).to_vec(),
        "SHA-384" => sha2::Sha384::digest(input).to_vec(),
        "SHA-512" => sha2::Sha512::digest(input).to_vec(),
        _ => return Err(JsErrorBox::type_error("Unsupported digest algorithm")),
    };
    if output.len() != bytes.len() {
        return Err(JsErrorBox::type_error("Invalid digest output"));
    }
    output.copy_from_slice(&bytes);
    Ok(())
}

deno_core::extension!(
    chunk_profile,
    ops = [op_chunk_now, op_chunk_random, op_chunk_log, op_chunk_digest]
);
