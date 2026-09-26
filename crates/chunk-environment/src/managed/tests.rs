use crate::{Config, CoreConfig, GatewayConfig, ManagementConfig, Services};
use bytes::Bytes;
use chunk_contract::BackendConnection;
use chunk_management::v1::{
    AttachRequest, AttachResponse, DeploymentProgress, DeploymentState, ReleaseArtifact, ReportStatusRequest,
};
use chunk_proto::v1::backend_client::BackendClient;
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    convert::Infallible,
    fs,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

type Body = BoxBody<Bytes, Infallible>;

/// The parts of the management service a core uses to deploy.
struct Management {
    desired: watch::Sender<AttachResponse>,
    reports: mpsc::UnboundedSender<ReportStatusRequest>,
    archives: Mutex<BTreeMap<String, Vec<u8>>>,
}

fn envelope(payload: &[u8]) -> Bytes {
    let mut frame = vec![0];
    frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(payload);
    frame.into()
}

async fn handle(
    management: Arc<Management>,
    request: hyper::Request<Incoming>,
) -> Result<hyper::Response<Body>, Infallible> {
    let path = request.uri().path().to_owned();
    assert_eq!(request.headers()["authorization"], "Bearer secret");
    let body = request.into_body().collect().await.unwrap().to_bytes();
    let response = hyper::Response::builder();
    Ok(match path.as_str() {
        "/chunk.management.v1.EnvironmentService/Attach" => {
            let attach = AttachRequest::decode(&body[5..]).unwrap();
            assert!(attach.core && !attach.instance_id.is_empty() && !attach.version.is_empty());
            let (sender, receiver) = mpsc::channel(4);
            let mut desired = management.desired.subscribe();
            tokio::spawn(async move {
                loop {
                    let message = desired.borrow_and_update().encode_to_vec();
                    if sender.send(Ok(Frame::data(envelope(&message)))).await.is_err()
                        || desired.changed().await.is_err()
                    {
                        return;
                    }
                }
            });
            let stream = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(receiver));
            response.header("content-type", "application/connect+proto").body(stream.boxed()).unwrap()
        }
        "/chunk.management.v1.EnvironmentService/ReportStatus" => {
            management.reports.send(ReportStatusRequest::decode(body).unwrap()).unwrap();
            response.header("content-type", "application/proto").body(Full::default().boxed()).unwrap()
        }
        _ => {
            let archive = management.archives.lock().unwrap().get(&path).cloned().unwrap();
            response.body(Full::new(archive.into()).boxed()).unwrap()
        }
    })
}

async fn serve(management: Arc<Management>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let management = management.clone();
            let service = hyper::service::service_fn(move |request| handle(management.clone(), request));
            let io = hyper_util::rt::TokioIo::new(stream);
            tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(io, service));
        }
    });
    format!("http://{address}")
}

/// A one-app release, as `chunk build` publishes it.
fn publish(root: &Path) -> std::path::PathBuf {
    let (project, backend) = (root.join("project"), root.join("backend"));
    fs::create_dir_all(project.join("apps/lobby")).unwrap();
    fs::create_dir_all(&backend).unwrap();
    fs::write(
        project.join("chunk.toml"),
        "[local]\nenvironment='local'\nmachine_profile='small'\ncapacity=16\nmax_processes=4\n\
         [local.profiles.small]\nmemory_mib=512\nmax_sessions=2\n",
    )
    .unwrap();
    fs::write(project.join("apps/lobby/app.toml"), "").unwrap();
    fs::write(project.join("apps/lobby/build.gradle.kts"), "").unwrap();
    fs::write(backend.join("source.mjs"), "export function status() { return 1; }").unwrap();
    fs::write(backend.join("contract.json"), r#"{"contract_version":2,"runtime_profile":"transactional_v1","tables":{},"functions":{"status":{"kind":"query","visibility":"public","export":"status","arguments":{"type":"null"},"result":{"type":"integer"}}}}"#).unwrap();
    let mut jar = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let class = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 69, 1];
    for (name, bytes) in [
        ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\nMain-Class: sample.lobby.Provider\r\n\r\n".to_vec()),
        ("sample/lobby/Provider.class", class),
    ] {
        jar.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        jar.write_all(&bytes).unwrap();
    }
    fs::write(root.join("lobby.jar"), jar.finish().unwrap().into_inner()).unwrap();
    let descriptor = serde_json::json!({
        "version": 4, "java": {"version": 25, "executable": root.join("jdk/bin/java")},
        "apps": [{"id": "lobby", "jar": root.join("lobby.jar"), "classpath": [], "java_version": 25, "sessions": ["default"]}]
    });
    let jvm_descriptor = root.join("artifacts.json");
    fs::write(&jvm_descriptor, descriptor.to_string()).unwrap();
    let inputs = chunk_build::ReleaseInputs { project, backend, jvm_descriptor, archive: true };
    chunk_build::publish_release(&inputs, &root.join("dist")).unwrap().archive.unwrap()
}

/// An intact archive whose release fails verification.
fn invalid() -> Vec<u8> {
    let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()));
    let mut header = tar::Header::new_gnu();
    header.set_size(2);
    header.set_mode(0o644);
    archive.append_data(&mut header, "release.json", &b"{}"[..]).unwrap();
    archive.into_inner().unwrap().finish().unwrap()
}

fn artifact(management: &Management, url: &str, release_id: &str, bytes: Vec<u8>) -> ReleaseArtifact {
    let path = format!("/releases/{release_id}.tar.gz");
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let size_bytes = bytes.len() as u64;
    management.archives.lock().unwrap().insert(path.clone(), bytes);
    ReleaseArtifact { release_id: release_id.into(), url: format!("{url}{path}"), sha256, size_bytes }
}

fn desired(revision: u64, deployment: &str, release: ReleaseArtifact) -> AttachResponse {
    AttachResponse {
        revision,
        environment_id: "env_test".into(),
        project_id: "prj_test".into(),
        deployment_id: deployment.into(),
        release: Some(release),
        lease: 7,
        ..Default::default()
    }
}

async fn serves(record: &Path, deployment: &str) -> bool {
    let backend: BackendConnection = chunk_service::read(record).unwrap();
    let mut request = tonic::Request::new(());
    let metadata = request.metadata_mut();
    metadata.insert("authorization", format!("Bearer {}", backend.token).parse().unwrap());
    metadata.insert("x-chunk-environment", "env_test".parse().unwrap());
    metadata.insert("x-chunk-deployment", deployment.parse().unwrap());
    BackendClient::connect(backend.endpoint).await.unwrap().check_deployment(request).await.is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploys_a_valid_release_and_keeps_serving_it_when_a_later_one_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let release = publish(directory.path());
    let release_id = release.file_name().unwrap().to_str().unwrap().trim_end_matches(".tar.gz").to_owned();
    let (reports, mut reported) = mpsc::unbounded_channel();
    let management = Arc::new(Management {
        desired: watch::Sender::new(AttachResponse::default()),
        reports,
        archives: Mutex::default(),
    });
    let url = serve(management.clone()).await;
    let valid = artifact(&management, &url, &release_id, fs::read(&release).unwrap());
    management.desired.send_replace(desired(1, "dep_first", valid.clone()));
    let state = directory.path().join("state");
    let config = Config {
        services: Services::default(),
        core: CoreConfig {
            bundle: None,
            environment: "env_test".into(),
            backend_record: state.join("backend.json"),
            control_record: state.join("control.json"),
            state: state.clone(),
            backend_bind: "127.0.0.1:0".parse().unwrap(),
            control_bind: "127.0.0.1:0".parse().unwrap(),
            fresh: false,
        },
        gateway: GatewayConfig::new("127.0.0.1:0".parse().unwrap()),
        management: Some(ManagementConfig { url: url.clone(), token: "secret".into() }),
    };
    let stop = CancellationToken::new();
    let running = tokio::spawn(crate::run(config, stop.clone()));
    let mut expect = async |revision, deployment: &str, state: DeploymentState| {
        let report = tokio::time::timeout(Duration::from_secs(60), reported.recv()).await.unwrap().unwrap();
        assert_eq!((report.lease, report.desired_revision), (7, revision));
        let progress = report.deployment.unwrap();
        assert_eq!((progress.deployment_id.as_str(), progress.state()), (deployment, state));
        (report.sequence, progress)
    };

    let (first, _) = expect(1, "dep_first", DeploymentState::InProgress).await;
    let (second, _) = expect(1, "dep_first", DeploymentState::Active).await;
    assert!(first < second);
    assert!(serves(&state.join("backend.json"), "dep_first").await);

    let rejected = artifact(&management, &url, "rejected", invalid());
    management.desired.send_replace(desired(2, "dep_second", rejected));
    expect(2, "dep_second", DeploymentState::InProgress).await;
    let (_, DeploymentProgress { message, .. }) = expect(2, "dep_second", DeploymentState::Failed).await;
    assert!(
        message.starts_with("release rejected fails verification") && message.len() <= super::MAX_MESSAGE_BYTES,
        "{message}"
    );

    // Management records the failure and falls back to the deployment that kept serving.
    management.desired.send_replace(desired(3, "dep_first", valid));
    expect(3, "dep_first", DeploymentState::Active).await;
    assert!(serves(&state.join("backend.json"), "dep_first").await);
    assert!(!state.join("releases/rejected").exists());
    assert!(!running.is_finished());

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
}
