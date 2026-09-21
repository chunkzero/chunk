use std::{collections::BTreeMap, sync::Arc, time::Instant};

use chunk_js::{Cancellation, HttpOutcome, HttpRequest};
use reqwest::{
    Client, Request, Url,
    header::{HeaderName, HeaderValue},
};
use tokio::sync::Semaphore;

use super::{ActionGrants, HttpBinding};

const REQUEST_BYTES: usize = 64 * 1024;
const RESPONSE_BYTES: usize = 128 * 1024;
const HEADER_BYTES: usize = 8 * 1024;

pub(crate) struct ScopedEffects {
    pub invocation: String,
    pub grants: Arc<ActionGrants>,
    pub slots: Arc<Semaphore>,
    pub cancellation: Cancellation,
    pub deadline: Instant,
}

fn path(binding: &HttpBinding, relative: &str) -> Result<Url, &'static str> {
    if relative.len() > 2048 || relative.starts_with('/') || relative.contains(['\\', '#', '\0']) {
        return Err("Invalid binding-relative path");
    }
    let path = relative.split('?').next().unwrap_or_default();
    if path.split('/').any(|segment| segment == "." || segment == "..") || path.contains(':') {
        return Err("Invalid binding-relative path");
    }
    // Restrict path syntax so downstream decoders cannot reinterpret encoded
    // separators or matrix parameters as traversal. Queries may still be encoded.
    if !path.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/')) {
        return Err("HTTP paths require unreserved ASCII segments");
    }
    let url = binding.base.join(relative).map_err(|_| "Invalid binding-relative path")?;
    if url.origin() != binding.base.origin()
        || !url.path().starts_with(binding.base.path())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("HTTP binding escape denied");
    }
    Ok(url)
}

fn prepare(binding: &HttpBinding, request: HttpRequest) -> Result<Request, &'static str> {
    if !binding.methods.contains(&request.method) {
        return Err("HTTP method denied");
    }
    if request.body.as_ref().is_some_and(|body| body.len() > REQUEST_BYTES)
        || request.headers.len() > 32
        || request.headers.iter().map(|(key, value)| key.len() + value.len()).sum::<usize>() > HEADER_BYTES
    {
        return Err("HTTP request size limit");
    }
    let url = path(binding, &request.path)?;
    let method = reqwest::Method::from_bytes(request.method.as_str().as_bytes()).map_err(|_| "HTTP method denied")?;
    let mut builder = binding.client.request(method, url).timeout(binding.timeout);
    for (key, value) in request.headers {
        let key = HeaderName::from_bytes(key.as_bytes()).map_err(|_| "Invalid HTTP header")?;
        if matches!(
            key.as_str(),
            "host" | "content-length" | "transfer-encoding" | "connection" | "upgrade" | "te" | "trailer"
        ) || key.as_str().starts_with("proxy-")
        {
            return Err("HTTP transport header denied");
        }
        let value = HeaderValue::from_str(&value).map_err(|_| "Invalid HTTP header")?;
        builder = builder.header(key, value);
    }
    if let Some(body) = request.body {
        builder = builder.body(body);
    }
    builder.build().map_err(|_| "Invalid HTTP request")
}

async fn response(client: &Client, request: Request) -> Result<(u16, BTreeMap<String, String>, String), &'static str> {
    let mut response = client.execute(request).await.map_err(|_| "HTTP response unavailable after dispatch")?;
    if response.content_length().is_some_and(|size| size > RESPONSE_BYTES as u64) {
        return Err("HTTP response size limit after dispatch");
    }
    let mut headers = BTreeMap::new();
    let mut header_bytes = 0;
    for (key, value) in response.headers() {
        header_bytes += key.as_str().len() + value.as_bytes().len();
        if header_bytes > HEADER_BYTES || headers.len() >= 32 {
            return Err("HTTP response header limit after dispatch");
        }
        let value = value.to_str().map_err(|_| "HTTP response headers are not text")?;
        headers.insert(key.to_string(), value.into());
    }
    let status = response.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| "HTTP response interrupted after dispatch")? {
        if body.len() + chunk.len() > RESPONSE_BYTES {
            return Err("HTTP response size limit after dispatch");
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8(body).map_err(|_| "HTTP response body is not UTF-8")?;
    Ok((status, headers, body))
}

impl ScopedEffects {
    pub async fn http(&self, sequence: u32, request: HttpRequest) -> HttpOutcome {
        let effect_id = format!("{}/http/{sequence}", self.invocation);
        let rejected = |reason: &str| HttpOutcome::Rejected { effect_id: effect_id.clone(), reason: reason.into() };
        if self.cancellation.is_cancelled() || Instant::now() >= self.deadline {
            return rejected("Action scope expired");
        }
        let Some(binding) = self.grants.http.get(&request.binding) else {
            return rejected("HTTP binding denied");
        };
        let Ok(_permit) = self.slots.clone().try_acquire_owned() else {
            return rejected("HTTP concurrency limit");
        };
        let request = match prepare(binding, request) {
            Ok(request) => request,
            Err(reason) => return rejected(reason),
        };
        if self.cancellation.is_cancelled() || Instant::now() >= self.deadline {
            return rejected("Action scope expired");
        }
        let expired = async {
            loop {
                if self.cancellation.is_cancelled() || Instant::now() >= self.deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        };
        let result = tokio::select! {
            biased;
            ()=expired=>Err("Action scope expired after HTTP dispatch"),
            result=response(&binding.client,request)=>result,
        };
        match result {
            Ok((status, headers, body)) => HttpOutcome::Completed { effect_id, status, headers, body },
            Err(reason) => HttpOutcome::Unknown { effect_id, reason: reason.into() },
        }
    }
}
