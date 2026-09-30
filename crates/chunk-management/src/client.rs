use std::{fmt, time::Duration};

use prost::Message;
use reqwest::header::{CONTENT_TYPE, HeaderName, HeaderValue};
use reqwest::{Response, StatusCode};

use crate::error::{self, Code, Error, Status};
use crate::stream::{Stream, envelope};

const UNARY_CONTENT_TYPE: &str = "application/proto";
const STREAM_CONTENT_TYPE: &str = "application/connect+proto";

/// A client for one management service. Cloning is cheap and shares connections.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    /// Archive uploads and downloads use presigned object-store URLs, so they never share the service client's
    /// credentials.
    presigned: reqwest::Client,
    base_url: String,
    token: Option<String>,
    timeout: Option<Duration>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base_url", &self.base_url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client for the service at `base_url`, such as `https://manage.example.net`, without credentials.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_http(reqwest::Client::builder(), base_url)
    }

    /// Like `new`, calling the service over an HTTP client built from `http`. Since calls carry the bearer token, that
    /// client follows a redirect only within the origin (scheme, host and port) it was sent to. Archive transfers to
    /// other origins use a client of their own.
    ///
    /// # Panics
    /// When the HTTP client cannot be built, as `reqwest::Client::new` does.
    pub fn with_http(http: reqwest::ClientBuilder, base_url: impl Into<String>) -> Self {
        let http = http.redirect(reqwest::redirect::Policy::custom(same_origin)).build().expect("HTTP client");
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        Self { http, presigned: reqwest::Client::new(), base_url, token: None, timeout: None }
    }

    /// Sends `token` as the bearer token of every call to the service.
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Fails a unary call that takes longer than `timeout` as `Unavailable`. Streams have no deadline.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub(crate) async fn unary<I: Message, O: Message + Default>(&self, path: &str, request: &I) -> Result<O, Error> {
        let mut request = self.post(path, UNARY_CONTENT_TYPE, request.encode_to_vec());
        if let Some(timeout) = self.timeout {
            request = request.timeout(timeout);
        }
        let response = accept(request.send().await?, UNARY_CONTENT_TYPE, self.token.as_deref()).await?;
        O::decode(response.bytes().await?).map_err(|error| Error::Protocol(error.to_string()))
    }

    pub(crate) async fn server_stream<I: Message, O: Message + Default>(
        &self,
        path: &str,
        request: &I,
    ) -> Result<Stream<O>, Error> {
        let body = envelope(0, &request.encode_to_vec());
        let response = self.post(path, STREAM_CONTENT_TYPE, body).send().await?;
        let response = accept(response, STREAM_CONTENT_TYPE, self.token.as_deref()).await?;
        Ok(Stream::new(response, self.token.clone()))
    }

    /// Sends a release archive where `UploadRelease` said to, with only the headers it named: the URL is presigned,
    /// so neither the bearer token nor any default header of the service's HTTP client goes along.
    ///
    /// # Errors
    /// The store's refusal, with a code from its HTTP status, or a transport failure. Errors never include the URL,
    /// whose query carries the upload's signature, nor the store's response body, which may echo it.
    pub async fn upload_archive(
        &self,
        target: &crate::v1::UploadTarget,
        archive: impl Into<reqwest::Body>,
    ) -> Result<(), Error> {
        let method = if target.method.is_empty() { "PUT" } else { &target.method };
        let method =
            reqwest::Method::from_bytes(method.as_bytes()).map_err(|error| Error::Protocol(error.to_string()))?;
        let mut request = self.presigned.request(method, &target.url).body(archive);
        for (name, value) in &target.headers {
            let name = HeaderName::try_from(name).map_err(|error| Error::Protocol(error.to_string()))?;
            let value = HeaderValue::try_from(value).map_err(|error| Error::Protocol(error.to_string()))?;
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(without_url)?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(upload_refusal(status, &response.bytes().await.map_err(without_url)?))
    }

    /// Starts fetching a release archive from the URL an `AttachResponse` names. The bearer token goes along only to
    /// this service's own origin, whose redirects are followed only within it; other URLs are presigned, and any
    /// redirect of theirs is followed without credentials.
    ///
    /// # Errors
    /// The server's refusal, with a code from its HTTP status, or a transport failure. Errors never include the URL,
    /// which may carry a signature.
    pub async fn download_archive(&self, url: &str) -> Result<Download, Error> {
        let url = reqwest::Url::parse(url).map_err(|_| Error::Protocol("invalid archive URL".into()))?;
        let own = reqwest::Url::parse(&self.base_url).is_ok_and(|base| base.origin() == url.origin());
        let request = if own { self.authorized(self.http.get(url)) } else { self.presigned.get(url) };
        let response = request.send().await.map_err(without_url)?;
        let status = response.status();
        if !status.is_success() {
            let message = format!("archive download refused with HTTP {status}");
            return Err(Status { code: Code::from_http(status), message }.into());
        }
        Ok(Download(response))
    }

    fn post(&self, path: &str, content_type: &'static str, body: Vec<u8>) -> reqwest::RequestBuilder {
        let request = self
            .http
            .post(format!("{}/{path}", self.base_url))
            .header(CONTENT_TYPE, content_type)
            .header("connect-protocol-version", "1")
            .body(body);
        self.authorized(request)
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }
}

/// A release archive's bytes as they arrive. Dropping it ends the download.
pub struct Download(Response);

impl Download {
    /// The next piece of the archive, or None once all of it arrived.
    ///
    /// # Errors
    /// A transport failure, without the URL.
    pub async fn chunk(&mut self) -> Result<Option<bytes::Bytes>, Error> {
        self.0.chunk().await.map_err(without_url)
    }
}

/// Follows a redirect only to the origin the request was first sent to, and at most 10 of them. A refused redirect is
/// the response.
fn same_origin(attempt: reqwest::redirect::Attempt<'_>) -> reqwest::redirect::Action {
    let first = attempt.previous().first().map(reqwest::Url::origin);
    if attempt.previous().len() > 10 || first != Some(attempt.url().origin()) {
        attempt.stop()
    } else {
        attempt.follow()
    }
}

fn without_url(error: reqwest::Error) -> Error {
    Error::Transport(error.without_url())
}

fn upload_refusal(status: StatusCode, body: &[u8]) -> Error {
    let detail = s3_code(body).map(|code| format!(" ({code})")).unwrap_or_default();
    Status { code: Code::from_http(status), message: format!("upload refused with HTTP {status}{detail}") }.into()
}

/// The `<Code>` of an S3 error body, such as `AccessDenied`, when it is a plain identifier.
fn s3_code(body: &[u8]) -> Option<&str> {
    let body = std::str::from_utf8(body).ok()?;
    let start = body.find("<Code>")? + "<Code>".len();
    let code = &body[start..start + body[start..].find("</Code>")?];
    let plain = !code.is_empty() && code.len() <= 64 && code.bytes().all(|byte| byte.is_ascii_alphanumeric());
    plain.then_some(code)
}

/// A Connect success is HTTP 200 with the expected content type. Anything else is an error: the Connect error it
/// carries, a code from its HTTP status, or, for a 200 of another type such as a proxy's page, a protocol error. Its
/// message never shows `token`.
async fn accept(response: Response, content_type: &str, token: Option<&str>) -> Result<Response, Error> {
    let status = response.status();
    if status != StatusCode::OK {
        return Err(error::from_response(status, &response.bytes().await?, token));
    }
    let actual = response.headers().get(CONTENT_TYPE).and_then(|value| value.to_str().ok()).unwrap_or_default();
    let media_type = actual.split(';').next().unwrap_or_default().trim();
    if !media_type.eq_ignore_ascii_case(content_type) {
        return Err(error::protocol(&format!("expected {content_type}, got {actual:?}"), token));
    }
    Ok(response)
}
