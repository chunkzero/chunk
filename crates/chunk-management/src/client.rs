use prost::Message;
use reqwest::header::{CONTENT_TYPE, HeaderName, HeaderValue};

use crate::error::{self, Error};
use crate::stream::{Stream, envelope};

/// A client for one management service. Cloning is cheap and shares connections.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl Client {
    /// A client for the service at `base_url`, such as `https://manage.example.net`, without credentials.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_http(reqwest::Client::new(), base_url)
    }

    /// Like `new`, over an existing HTTP client.
    pub fn with_http(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        Self { http, base_url, token: None }
    }

    /// Sends `token` as the bearer token of every call.
    #[must_use]
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub(crate) async fn unary<I: Message, O: Message + Default>(&self, path: &str, request: &I) -> Result<O, Error> {
        let response = self.post(path, "application/proto", request.encode_to_vec()).send().await?;
        let status = response.status();
        let body = response.bytes().await?;
        if !status.is_success() {
            return Err(error::from_response(status, &body));
        }
        O::decode(body).map_err(|error| Error::Protocol(error.to_string()))
    }

    pub(crate) async fn server_stream<I: Message, O: Message + Default>(
        &self,
        path: &str,
        request: &I,
    ) -> Result<Stream<O>, Error> {
        let body = envelope(0, &request.encode_to_vec());
        let response = self.post(path, "application/connect+proto", body).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(error::from_response(status, &response.bytes().await?));
        }
        Ok(Stream::new(response))
    }

    /// Sends a release archive where `UploadRelease` said to. The target is presigned, so no bearer token is sent.
    ///
    /// # Errors
    /// The store's refusal, with a code from its HTTP status, or a transport failure.
    pub async fn upload_archive(
        &self,
        target: &crate::v1::UploadTarget,
        archive: impl Into<reqwest::Body>,
    ) -> Result<(), Error> {
        let method = if target.method.is_empty() { "PUT" } else { &target.method };
        let method =
            reqwest::Method::from_bytes(method.as_bytes()).map_err(|error| Error::Protocol(error.to_string()))?;
        let mut request = self.http.request(method, &target.url).body(archive);
        for (name, value) in &target.headers {
            let name = HeaderName::try_from(name).map_err(|error| Error::Protocol(error.to_string()))?;
            let value = HeaderValue::try_from(value).map_err(|error| Error::Protocol(error.to_string()))?;
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        Err(error::from_response(status, &response.bytes().await?))
    }

    fn post(&self, path: &str, content_type: &'static str, body: Vec<u8>) -> reqwest::RequestBuilder {
        let request = self
            .http
            .post(format!("{}/{path}", self.base_url))
            .header(CONTENT_TYPE, content_type)
            .header("connect-protocol-version", "1")
            .body(body);
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }
}
