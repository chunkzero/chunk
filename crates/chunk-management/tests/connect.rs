//! The client against a tiny in-process Connect server.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use chunk_management::v1::{
    AttachRequest, AttachResponse, DeployRequest, DeployResponse, Deployment, FailedAuth, GetDeploymentRequest,
    ListAppsRequest, ListProjectsRequest, LogEntry, ObjectStore, ReadLogsRequest, ReadLogsResponse,
    ReportFailedAuthRequest, ReportStatusRequest, RevokeTokenRequest, SetWakeAlarmRequest, UploadTarget, WakeReason,
    WakeRequest, WatchRoutesRequest, WatchRoutesResponse,
};
use chunk_management::{Client, Code, Error};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Request, Response};
use prost::Message;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

type Body = BoxBody<Bytes, Infallible>;

/// Requests that reached `/landed` with a bearer token.
static CREDENTIALED_LANDINGS: AtomicUsize = AtomicUsize::new(0);

fn full(status: u16, content_type: &str, body: impl Into<Bytes>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Full::new(body.into()).boxed())
        .expect("response")
}

fn envelope(flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![flags];
    frame.extend_from_slice(&u32::try_from(payload.len()).expect("small").to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Sends `bytes` in pieces of at most `size` bytes, so frames arrive split across chunks.
fn chunked(bytes: Vec<u8>, size: usize) -> Response<Body> {
    let (sender, receiver) = mpsc::channel(4);
    tokio::spawn(async move {
        for piece in bytes.chunks(size) {
            if sender.send(Ok::<_, Infallible>(Frame::data(Bytes::copy_from_slice(piece)))).await.is_err() {
                return;
            }
        }
    });
    Response::builder()
        .header("content-type", "application/connect+proto")
        .body(StreamBody::new(ReceiverStream::new(receiver)).boxed())
        .expect("response")
}

async fn handle(request: Request<Incoming>) -> Result<Response<Body>, Infallible> {
    let path = request.uri().path().to_owned();
    let uri = request.uri().to_string();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let authorized = request.headers().get("authorization").is_some_and(|value| value == "Bearer secret");
    let content_type = request.headers().get("content-type").cloned();
    let credentialed = request.headers().contains_key("authorization");
    let body = request.into_body().collect().await.expect("request body").to_bytes();
    if path == "/redirect" {
        return Ok(Response::builder()
            .status(307)
            .header("location", query)
            .body(Full::default().boxed())
            .expect("response"));
    }
    if path == "/upload" && uri.contains("refuse") {
        let body = format!("<Error><Code>AccessDenied</Code><Resource>{uri}</Resource></Error>");
        return Ok(full(403, "application/xml", body));
    }
    if path == "/landed" {
        if credentialed {
            CREDENTIALED_LANDINGS.fetch_add(1, Ordering::SeqCst);
        }
        return Ok(full(200, "application/octet-stream", "archive"));
    }
    if path == "/blob" {
        let status = if credentialed { 403 } else { 200 };
        return Ok(full(status, "application/octet-stream", "blob"));
    }
    if path == "/upload" {
        let status = if credentialed || body.as_ref() != b"archive" { 403 } else { 200 };
        return Ok(full(status, "text/plain", ""));
    }
    if !authorized {
        return Ok(full(
            401,
            "application/json",
            r#"{"code":"unauthenticated","message":"a valid bearer token is required"}"#,
        ));
    }
    let response = match path.as_str() {
        "/chunk.management.v1.DeploymentService/Deploy" => {
            assert_eq!(content_type.expect("content type"), "application/proto");
            let request = DeployRequest::decode(body).expect("deploy request");
            let deployment = Deployment { id: format!("dep_{}", request.request_id), ..Deployment::default() };
            full(200, "application/proto", DeployResponse { deployment: Some(deployment) }.encode_to_vec())
        }
        "/chunk.management.v1.DeploymentService/GetDeployment" => {
            full(404, "application/json", r#"{"code":"not_found","message":"deployment not found"}"#)
        }
        "/chunk.management.v1.EdgeService/Wake" => full(503, "text/plain", "upstream is down"),
        "/chunk.management.v1.ProjectService/ListProjects" => {
            full(502, "text/html", "<pre>POST / HTTP/1.1\r\nAuthorization: Bearer secret\r\nAccept: */*</pre>")
        }
        "/chunk.management.v1.DeploymentService/ListApps" => full(
            400,
            "application/json",
            r#"{"code":"invalid_argument","message":"{\"authorization\":\"Basic dXNlcg==\"} bearer sentinel0123456789abcdef"}"#,
        ),
        "/chunk.management.v1.AuthService/RevokeToken" => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            full(200, "application/proto", Vec::new())
        }
        "/chunk.management.v1.EnvironmentService/ReportStatus" => {
            Response::builder().status(204).body(Full::new(Bytes::new()).boxed()).expect("response")
        }
        "/chunk.management.v1.EnvironmentService/SetWakeAlarm" => {
            full(200, "text/html; echoed-authorization=Bearer secret", "<p>sign in</p>")
        }
        "/chunk.management.v1.EnvironmentService/ReportFailedAuth" => {
            let request = ReportFailedAuthRequest::decode(body).expect("report failed auth request");
            assert_eq!(request.failures[0].client_address, "192.0.2.1");
            full(200, "application/proto; charset=binary", Vec::new())
        }
        "/chunk.management.v1.LogService/ReadLogs" => {
            let request = ReadLogsRequest::decode(&body[5..]).expect("read logs request");
            let entry = envelope(0, &ReadLogsResponse { entries: vec![LogEntry::default()] }.encode_to_vec());
            let stream = match request.environment_id.as_str() {
                "malformed" => [envelope(0, &[0xff, 0xff]), entry, envelope(2, b"{}")].concat(),
                "no_end" => entry,
                "mid_frame" => [entry.clone(), entry[..entry.len() - 1].to_vec()].concat(),
                "echoed_end" => envelope(2, br#"{"error":"Authorization: Bearer secret"}"#),
                other => panic!("unexpected environment {other}"),
            };
            chunked(stream, 1024)
        }
        "/chunk.management.v1.EnvironmentService/Attach" => {
            assert_eq!(content_type.expect("content type"), "application/connect+proto");
            let request = AttachRequest::decode(&body[5..]).expect("attach request");
            assert_eq!(usize::try_from(u32::from_be_bytes(body[1..5].try_into().expect("length"))), Ok(body.len() - 5));
            let mut stream = Vec::new();
            for revision in [1, 2] {
                let message = AttachResponse {
                    revision,
                    environment_id: "env_1".into(),
                    lease: u64::from(request.core),
                    log_store: Some(ObjectStore { prefix: "environments/env_1/".into(), ..ObjectStore::default() }),
                    ..AttachResponse::default()
                };
                stream.extend(envelope(0, &message.encode_to_vec()));
            }
            let end = br#"{"error":{"code":"failed_precondition","message":"lease 1 was superseded by lease 2"}}"#;
            stream.extend(envelope(2, end));
            chunked(stream, 3)
        }
        "/chunk.management.v1.EdgeService/WatchRoutes" => {
            let mut stream =
                envelope(0, &WatchRoutesResponse { revision: 1, reset: true, ..Default::default() }.encode_to_vec());
            stream.extend(envelope(2, b"{}"));
            chunked(stream, 1024)
        }
        _ => full(404, "text/plain", "no such route"),
    };
    Ok(response)
}

async fn serve() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, hyper::service::service_fn(handle))
                    .await;
            });
        }
    });
    address
}

async fn client() -> Client {
    Client::new(format!("http://{}/", serve().await)).with_token("secret")
}

#[tokio::test]
async fn unary_calls_round_trip_binary_protobuf() {
    let client = client().await;
    let response = client
        .deploy(&DeployRequest {
            request_id: "abc".into(),
            environment_id: "env_1".into(),
            release_id: "r1".into(),
            stop_previous: false,
            asset_revision_id: "a".repeat(64),
        })
        .await
        .expect("deploy");
    assert_eq!(response.deployment.expect("deployment").id, "dep_abc");
}

#[tokio::test]
async fn errors_map_connect_codes_and_plain_http_statuses() {
    let client = client().await;
    let error = client.get_deployment(&GetDeploymentRequest { deployment_id: "x".into() }).await.unwrap_err();
    assert_eq!(error.code(), Code::NotFound);
    assert_eq!(error.to_string(), "not_found: deployment not found");

    let error = client
        .wake(&WakeRequest {
            environment_id: "env_1".into(),
            reason: WakeReason::Login.into(),
            client_address: "192.0.2.1".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), Code::Unavailable);

    let anonymous = Client::new(format!("http://{}", serve().await));
    let error = anonymous.deploy(&DeployRequest::default()).await.unwrap_err();
    assert_eq!(error.code(), Code::Unauthenticated);

    let unreachable = Client::new("http://127.0.0.1:9").with_token("secret");
    let error = unreachable.deploy(&DeployRequest::default()).await.unwrap_err();
    assert!(matches!(error, Error::Transport(_)));
    assert_eq!(error.code(), Code::Unavailable);
}

#[tokio::test]
async fn errors_never_show_bearer_tokens_the_server_echoes() {
    let client = client().await;
    let plain = client.list_projects(&ListProjectsRequest::default()).await.unwrap_err().to_string();
    let json = client.list_apps(&ListAppsRequest::default()).await.unwrap_err().to_string();
    assert_eq!(
        plain,
        "unavailable: HTTP 502 Bad Gateway: <pre>POST / HTTP/1.1\r\nAuthorization: <redacted>\r\nAccept: */*</pre>"
    );
    assert_eq!(json, r#"invalid_argument: {"authorization":"<redacted>"} bearer <redacted>"#);

    let anonymous = Client::new(format!("http://{}", serve().await));
    let error = anonymous.deploy(&DeployRequest::default()).await.unwrap_err();
    assert_eq!(error.to_string(), "unauthenticated: a valid bearer token is required");

    let content_type = client.set_wake_alarm(&SetWakeAlarmRequest::default()).await.unwrap_err().to_string();
    let end = read_logs("echoed_end").await.message().await.unwrap_err().to_string();
    for output in [content_type, end] {
        assert!(!output.contains("secret"), "{output}");
    }
}

#[tokio::test]
async fn unary_calls_time_out_as_unavailable() {
    let client = client().await.with_timeout(Duration::from_millis(100));
    let error = client.revoke_token(&RevokeTokenRequest::default()).await.unwrap_err();
    assert_eq!(error.code(), Code::Unavailable);
}

#[tokio::test]
async fn server_streams_reassemble_split_frames_and_surface_the_end_of_stream_error() {
    let client = client().await;
    let mut stream = client
        .attach(&AttachRequest { instance_id: "i".into(), core: true, ..Default::default() })
        .await
        .expect("attach");
    let first = stream.message().await.expect("first").expect("a message");
    assert_eq!((first.revision, first.lease), (1, 1));
    assert_eq!(first.log_store.expect("log store").prefix, "environments/env_1/");
    assert_eq!(stream.message().await.expect("second").expect("a message").revision, 2);
    let error = stream.message().await.unwrap_err();
    assert_eq!(error.code(), Code::FailedPrecondition);
    assert!(error.to_string().contains("superseded"));
    assert!(stream.message().await.expect("after the end").is_none());
}

#[tokio::test]
async fn a_clean_end_of_stream_ends_the_stream() {
    let client = client().await;
    let mut stream = client.watch_routes(&WatchRoutesRequest {}).await.expect("watch");
    assert!(stream.message().await.expect("first").expect("a message").reset);
    assert!(stream.message().await.expect("end").is_none());
}

#[tokio::test]
async fn debug_output_never_shows_the_token() {
    let client = Client::new("http://127.0.0.1:9").with_token("sentinel-token-7f3a");
    let debug = format!("{client:?} {client:#?}");
    assert!(!debug.contains("sentinel-token-7f3a"), "{debug}");
    assert!(debug.contains("<redacted>"));
}

#[tokio::test]
async fn archive_uploads_carry_no_credentials_of_the_service_client() {
    let address = serve().await;
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("authorization", "Bearer secret".parse().expect("header"));
    let http = reqwest::Client::builder().default_headers(headers);
    let client = Client::with_http(http, format!("http://{address}")).with_token("secret");

    client.deploy(&DeployRequest::default()).await.expect("service calls keep the default header");
    let target = UploadTarget { url: format!("http://{address}/upload"), ..UploadTarget::default() };
    client.upload_archive(&target, "archive").await.expect("an upload without credentials");
}

#[tokio::test]
async fn only_http_200_with_the_connect_content_type_succeeds() {
    let client = client().await;
    let error = client.report_status(&ReportStatusRequest::default()).await.unwrap_err();
    assert_eq!(error.code(), Code::Unknown);

    let error = client.set_wake_alarm(&SetWakeAlarmRequest::default()).await.unwrap_err();
    assert!(matches!(error, Error::Protocol(_)), "{error}");

    let failures = vec![FailedAuth { client_address: "192.0.2.1".into(), ..FailedAuth::default() }];
    client.report_failed_auth(&ReportFailedAuthRequest { failures }).await.expect("a parameterized content type");
}

async fn read_logs(environment_id: &str) -> chunk_management::Stream<ReadLogsResponse> {
    let request = ReadLogsRequest { environment_id: environment_id.into(), ..ReadLogsRequest::default() };
    client().await.read_logs(&request).await.expect("read logs")
}

#[tokio::test]
async fn a_malformed_message_ends_the_stream() {
    let mut stream = read_logs("malformed").await;
    assert!(matches!(stream.message().await, Err(Error::Protocol(_))));
    assert!(stream.message().await.expect("after the error").is_none());
}

#[tokio::test]
async fn a_truncated_stream_fails_then_ends() {
    for environment_id in ["no_end", "mid_frame"] {
        let mut stream = read_logs(environment_id).await;
        assert_eq!(stream.message().await.expect("a whole message").expect("a message").entries.len(), 1);
        assert!(matches!(stream.message().await, Err(Error::Protocol(_))), "{environment_id}");
        assert!(stream.message().await.expect("after the error").is_none(), "{environment_id}");
    }
}

#[tokio::test]
async fn upload_errors_never_show_the_presigned_url() {
    let refusing = format!("http://{}/upload?refuse=1&X-Amz-Signature=sentinel-signature-51c2", serve().await);
    let unreachable = "http://127.0.0.1:9/upload?X-Amz-Signature=sentinel-signature-51c2".to_owned();
    for url in [refusing, unreachable] {
        let target = UploadTarget { url, ..UploadTarget::default() };
        let error = Client::new("http://127.0.0.1:9").upload_archive(&target, "archive").await.unwrap_err();
        let output = format!("{error} {error:?} {error:#?}");
        assert!(!output.contains("sentinel-signature-51c2"), "{output}");
        match error {
            Error::Status(status) => {
                assert_eq!(status.code, Code::PermissionDenied);
                assert_eq!(status.message, "upload refused with HTTP 403 Forbidden (AccessDenied)");
            }
            error => assert!(matches!(error, Error::Transport(_)), "{error}"),
        }
    }
}

#[tokio::test]
async fn the_token_follows_a_redirect_only_within_the_origin() {
    let (address, other) = (serve().await, serve().await);
    let client = Client::new(format!("http://{address}")).with_token("secret");
    client.download_archive(&format!("http://{address}/redirect?/landed")).await.expect("a redirect within the origin");
    assert_eq!(CREDENTIALED_LANDINGS.load(Ordering::SeqCst), 1);

    let (port, host, scheme) = (other.port(), format!("localhost:{}", address.port()), format!("https://{address}"));
    for target in [format!("http://127.0.0.1:{port}"), format!("http://{host}"), scheme] {
        let url = format!("http://{address}/redirect?{target}/landed");
        let Err(error) = client.download_archive(&url).await else { panic!("{target} was followed") };
        assert!(matches!(&error, Error::Status(status) if status.message.contains("307")), "{target}: {error}");
    }
    assert_eq!(CREDENTIALED_LANDINGS.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn blob_downloads_follow_a_redirect_elsewhere_without_the_token() {
    let (address, other) = (serve().await, serve().await);
    let client = Client::new(format!("http://{address}")).with_token("secret");
    let url = format!("http://{address}/redirect?http://127.0.0.1:{}/blob", other.port());
    let mut download = client.download_blob(&url).await.expect("the presigned blob");
    assert_eq!(download.chunk().await.expect("a chunk").expect("bytes").as_ref(), b"blob");
}
