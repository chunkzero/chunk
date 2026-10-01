use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use chunk_js::{Cancellation, HttpOutcome, HttpRequest};
use reqwest::{
    Client, Method, Response, StatusCode, Url,
    dns::{Addrs, Name, Resolve, Resolving},
    header::{self, HeaderMap, HeaderName, HeaderValue},
    redirect,
};
use tokio::sync::Semaphore;

use crate::{Error, Result};

const REQUEST_BYTES: usize = 64 * 1024;
const RESPONSE_BYTES: usize = 128 * 1024;
const HEADER_BYTES: usize = 8 * 1024;
const URL_BYTES: usize = 8 * 1024;
const REDIRECTS: usize = 10;
/// Bounds one fetch, redirects included. The action deadline also applies.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Which addresses fetches may connect to.
type Policy = fn(IpAddr) -> bool;

/// One HTTP client for every action of the environment. It connects only to addresses its policy admits, checked after
/// DNS resolution and again on every redirect, so neither a hostname nor a redirect reaches a refused address.
pub(crate) struct Fetcher {
    client: Client,
    policy: Policy,
}

#[derive(Debug)]
struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("address refused")
    }
}

impl std::error::Error for Refused {}

/// Resolves through the system, keeping only admitted addresses.
struct Resolver(Policy);

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let policy = self.0;
        Box::pin(async move {
            let resolved = tokio::net::lookup_host((name.as_str(), 0)).await?;
            let admitted: Vec<SocketAddr> = resolved.filter(|address| policy(address.ip())).collect();
            if admitted.is_empty() {
                return Err(Box::new(Refused) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(admitted.into_iter()) as Addrs)
        })
    }
}

/// Why a fetch produced no response: refused before anything was sent, or failed once a request may have arrived.
enum Failure {
    Rejected(&'static str),
    Unknown(&'static str),
}

struct Prepared {
    url: Url,
    method: Method,
    headers: HeaderMap,
    body: Option<String>,
}

impl Prepared {
    /// Moves to `next` as fetch does: 303, and 301 or 302 after a POST, continue as a GET without the body, and
    /// credentials don't follow to another origin.
    fn follow(&mut self, status: StatusCode, next: Url) {
        let get = status == StatusCode::SEE_OTHER && self.method != Method::HEAD
            || matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND) && self.method == Method::POST;
        if get {
            self.method = Method::GET;
            self.body = None;
            for name in
                [header::CONTENT_TYPE, header::CONTENT_ENCODING, header::CONTENT_LANGUAGE, header::CONTENT_LOCATION]
            {
                self.headers.remove(name);
            }
        }
        if next.origin() != self.url.origin() {
            self.headers.remove(header::AUTHORIZATION);
            self.headers.remove(header::COOKIE);
        }
        self.url = next;
    }
}

impl Fetcher {
    pub fn new(policy: Policy) -> Result<Self> {
        let client = Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .http1_only()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .connect_timeout(TIMEOUT)
            .dns_resolver(Arc::new(Resolver(policy)))
            .build()
            .map_err(|_| Error::Invalid("HTTP client unavailable"))?;
        Ok(Self { client, policy })
    }

    /// Requires an HTTP(S) URL without credentials whose host, when an IP address, is admitted. Hostnames are checked
    /// once resolved.
    fn check(&self, url: &mut Url) -> std::result::Result<(), &'static str> {
        if !matches!(url.scheme(), "http" | "https") || !url.username().is_empty() || url.password().is_some() {
            return Err("HTTP requests require an http or https URL without credentials");
        }
        let host = url.host_str().ok_or("Invalid URL")?;
        // The parser normalizes IP literals, so any host that isn't one is a domain name.
        let literal = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>();
        if literal.is_ok_and(|address| !(self.policy)(address)) {
            return Err("HTTP destination address refused");
        }
        url.set_fragment(None);
        Ok(())
    }

    fn prepare(&self, request: HttpRequest) -> std::result::Result<Prepared, &'static str> {
        if request.url.len() > URL_BYTES
            || request.body.as_ref().is_some_and(|body| body.len() > REQUEST_BYTES)
            || request.headers.len() > 32
            || request.headers.iter().map(|(key, value)| key.len() + value.len()).sum::<usize>() > HEADER_BYTES
        {
            return Err("HTTP request size limit");
        }
        let mut url = Url::parse(&request.url).map_err(|_| "Invalid URL")?;
        self.check(&mut url)?;
        let method = Method::from_bytes(request.method.as_str().as_bytes()).map_err(|_| "HTTP method denied")?;
        let mut headers = HeaderMap::new();
        for (key, value) in request.headers {
            let key = HeaderName::from_bytes(key.as_bytes()).map_err(|_| "Invalid HTTP header")?;
            if matches!(
                key.as_str(),
                "host" | "content-length" | "transfer-encoding" | "connection" | "upgrade" | "te" | "trailer"
            ) || key.as_str().starts_with("proxy-")
            {
                return Err("HTTP transport header denied");
            }
            headers.insert(key, HeaderValue::from_str(&value).map_err(|_| "Invalid HTTP header")?);
        }
        Ok(Prepared { url, method, headers, body: request.body })
    }

    /// Sends `request`, following redirects itself so each hop's destination is checked.
    async fn send(&self, mut request: Prepared) -> std::result::Result<(Url, Response), Failure> {
        for hop in 0..=REDIRECTS {
            let mut builder = self.client.request(request.method.clone(), request.url.clone());
            builder = builder.headers(request.headers.clone());
            if let Some(body) = &request.body {
                builder = builder.body(body.clone());
            }
            let response = match builder.send().await {
                Ok(response) => response,
                Err(error) if refused(&error) && hop == 0 => {
                    return Err(Failure::Rejected("HTTP destination address refused"));
                }
                Err(error) if refused(&error) => {
                    return Err(Failure::Unknown("HTTP redirect to a refused address after dispatch"));
                }
                Err(_) => return Err(Failure::Unknown("HTTP response unavailable after dispatch")),
            };
            let status = response.status();
            let location = response.headers().get(header::LOCATION);
            if !status.is_redirection() || status == StatusCode::NOT_MODIFIED || location.is_none() {
                return Ok((request.url, response));
            }
            let next =
                location.and_then(|location| location.to_str().ok()).and_then(|path| request.url.join(path).ok());
            let Some(mut next) = next else {
                return Err(Failure::Unknown("Invalid HTTP redirect after dispatch"));
            };
            if self.check(&mut next).is_err() {
                return Err(Failure::Unknown("HTTP redirect to a refused address after dispatch"));
            }
            request.follow(status, next);
        }
        Err(Failure::Unknown("HTTP redirect limit after dispatch"))
    }

    async fn fetch(
        &self,
        request: Prepared,
    ) -> std::result::Result<(String, u16, BTreeMap<String, String>, String), Failure> {
        let (url, mut response) = self.send(request).await?;
        if response.content_length().is_some_and(|size| size > RESPONSE_BYTES as u64) {
            return Err(Failure::Unknown("HTTP response size limit after dispatch"));
        }
        let mut headers = BTreeMap::new();
        let mut header_bytes = 0;
        for (key, value) in response.headers() {
            header_bytes += key.as_str().len() + value.as_bytes().len();
            if header_bytes > HEADER_BYTES || headers.len() >= 32 {
                return Err(Failure::Unknown("HTTP response header limit after dispatch"));
            }
            let value = value.to_str().map_err(|_| Failure::Unknown("HTTP response headers are not text"))?;
            headers.insert(key.to_string(), value.into());
        }
        let status = response.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) =
            response.chunk().await.map_err(|_| Failure::Unknown("HTTP response interrupted after dispatch"))?
        {
            if body.len() + chunk.len() > RESPONSE_BYTES {
                return Err(Failure::Unknown("HTTP response size limit after dispatch"));
            }
            body.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(body).map_err(|_| Failure::Unknown("HTTP response body is not UTF-8"))?;
        Ok((url.into(), status, headers, body))
    }
}

/// Whether the resolver refused every address `error`'s request could connect to.
fn refused(error: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(error) = source {
        if error.is::<Refused>() {
            return true;
        }
        source = error.source();
    }
    false
}

pub(crate) struct ScopedEffects {
    pub invocation: String,
    /// Unset for hooks, which can't fetch.
    pub fetcher: Option<Arc<Fetcher>>,
    pub slots: Arc<Semaphore>,
    pub cancellation: Cancellation,
    pub deadline: Instant,
}

impl ScopedEffects {
    pub async fn fetch(&self, sequence: u32, request: HttpRequest) -> HttpOutcome {
        let effect_id = format!("{}/http/{sequence}", self.invocation);
        let rejected = |reason: &str| HttpOutcome::Rejected { effect_id: effect_id.clone(), reason: reason.into() };
        if self.cancellation.is_cancelled() || Instant::now() >= self.deadline {
            return rejected("Action scope expired");
        }
        let Some(fetcher) = &self.fetcher else {
            return rejected("HTTP effects unavailable");
        };
        let Ok(_permit) = self.slots.clone().try_acquire_owned() else {
            return rejected("HTTP concurrency limit");
        };
        let request = match fetcher.prepare(request) {
            Ok(request) => request,
            Err(reason) => return rejected(reason),
        };
        if self.cancellation.is_cancelled() || Instant::now() >= self.deadline {
            return rejected("Action scope expired");
        }
        let result = tokio::select! {
            biased;
            () = self.cancellation.expired(self.deadline) => Err(Failure::Unknown("Action scope expired after HTTP dispatch")),
            result = tokio::time::timeout(TIMEOUT, fetcher.fetch(request)) => {
                result.unwrap_or(Err(Failure::Unknown("HTTP timeout after dispatch")))
            }
        };
        match result {
            Ok((url, status, headers, body)) => HttpOutcome::Completed { effect_id, url, status, headers, body },
            Err(Failure::Rejected(reason)) => rejected(reason),
            Err(Failure::Unknown(reason)) => HttpOutcome::Unknown { effect_id, reason: reason.into() },
        }
    }
}
