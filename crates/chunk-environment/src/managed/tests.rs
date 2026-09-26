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
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

type Body = BoxBody<Bytes, Infallible>;

/// The parts of the management service a core uses to deploy.
struct Management {
    desired: watch::Sender<AttachResponse>,
    /// The lease of the latest core attach, which fences every core attached before it.
    lease: watch::Sender<u64>,
    reports: mpsc::UnboundedSender<ReportStatusRequest>,
    archives: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Notified when a download of an archive whose path names it `stalled` starts. That download sends half of the
    /// archive, then nothing.
    stalled: Notify,
}

fn envelope(flags: u8, payload: &[u8]) -> Bytes {
    let mut frame = vec![flags];
    frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(payload);
    frame.into()
}

const FENCED: &str = r#"{"code":"failed_precondition","message":"a newer core attached"}"#;

async fn handle(
    management: Arc<Management>,
    request: hyper::Request<Incoming>,
) -> Result<hyper::Response<Body>, Infallible> {
    let path = request.uri().path().to_owned();
    assert_eq!(request.headers()["authorization"], "Bearer secret");
    let body = request.into_body().collect().await.unwrap().to_bytes();
    let response = hyper::Response::builder();
    let (sender, receiver) = mpsc::channel(4);
    let streamed = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(receiver)).boxed();
    Ok(match path.as_str() {
        "/chunk.management.v1.EnvironmentService/Attach" => {
            let attach = AttachRequest::decode(&body[5..]).unwrap();
            assert!(attach.core && !attach.instance_id.is_empty() && !attach.version.is_empty());
            management.lease.send_modify(|lease| *lease += 1);
            let lease = *management.lease.borrow();
            let (mut desired, mut leases) = (management.desired.subscribe(), management.lease.subscribe());
            tokio::spawn(async move {
                loop {
                    let message = AttachResponse { lease, ..desired.borrow_and_update().clone() };
                    if sender.send(Ok(Frame::data(envelope(0, &message.encode_to_vec())))).await.is_err() {
                        return;
                    }
                    tokio::select! {
                        changed = desired.changed() => if changed.is_err() { return },
                        _ = async { leases.wait_for(|latest| *latest > lease).await.map(|_| ()) } => {
                            let end = format!(r#"{{"error":{FENCED}}}"#);
                            _ = sender.send(Ok(Frame::data(envelope(2, end.as_bytes())))).await;
                            return;
                        }
                    }
                }
            });
            response.header("content-type", "application/connect+proto").body(streamed).unwrap()
        }
        "/chunk.management.v1.EnvironmentService/ReportStatus" => {
            let report = ReportStatusRequest::decode(body).unwrap();
            if report.lease < *management.lease.borrow() {
                return Ok(response
                    .status(400)
                    .header("content-type", "application/json")
                    .body(Full::new(FENCED.into()).boxed())
                    .unwrap());
            }
            management.reports.send(report).unwrap();
            response.header("content-type", "application/proto").body(Full::default().boxed()).unwrap()
        }
        _ if path.contains("stalled") => {
            let archive = management.archives.lock().unwrap().get(&path).cloned().unwrap();
            management.stalled.notify_one();
            tokio::spawn(async move {
                _ = sender.send(Ok(Frame::data(Bytes::copy_from_slice(&archive[..archive.len() / 2])))).await;
                std::future::pending::<()>().await;
                drop(sender);
            });
            response.body(streamed).unwrap()
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

async fn serves(record: &Path, deployment: &str) -> bool {
    let backend: BackendConnection = chunk_service::read(record).unwrap();
    let mut request = tonic::Request::new(());
    let metadata = request.metadata_mut();
    metadata.insert("authorization", format!("Bearer {}", backend.token).parse().unwrap());
    metadata.insert("x-chunk-environment", "env_test".parse().unwrap());
    metadata.insert("x-chunk-deployment", deployment.parse().unwrap());
    BackendClient::connect(backend.endpoint).await.unwrap().check_deployment(request).await.is_ok()
}

/// A fake management service with one published release, and the state directory of the core it deploys.
struct Harness {
    directory: tempfile::TempDir,
    management: Arc<Management>,
    url: String,
    reported: mpsc::UnboundedReceiver<ReportStatusRequest>,
    release: (String, Vec<u8>),
}

impl Harness {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let archive = publish(directory.path());
        let release_id = archive.file_name().unwrap().to_str().unwrap().trim_end_matches(".tar.gz").to_owned();
        let (reports, reported) = mpsc::unbounded_channel();
        let management = Arc::new(Management {
            desired: watch::Sender::new(AttachResponse::default()),
            lease: watch::Sender::new(0),
            reports,
            archives: Mutex::default(),
            stalled: Notify::new(),
        });
        let url = serve(management.clone()).await;
        Self { directory, management, url, reported, release: (release_id, fs::read(&archive).unwrap()) }
    }

    fn valid(&self) -> ReleaseArtifact {
        artifact(&self.management, &self.url, &self.release.0, self.release.1.clone())
    }

    fn stalled(&self) -> ReleaseArtifact {
        artifact(&self.management, &self.url, "stalled", self.release.1.clone())
    }

    fn desire(&self, revision: u64, deployment: &str, release: ReleaseArtifact) {
        self.management.desired.send_replace(AttachResponse {
            revision,
            environment_id: "env_test".into(),
            project_id: "prj_test".into(),
            deployment_id: deployment.into(),
            release: Some(release),
            ..Default::default()
        });
    }

    fn state(&self) -> std::path::PathBuf {
        self.directory.path().join("state")
    }

    fn start(&self) -> (CancellationToken, tokio::task::JoinHandle<std::io::Result<()>>) {
        let state = self.state();
        let config = Config {
            services: Services::default(),
            core: CoreConfig {
                bundle: None,
                environment: "env_test".into(),
                backend_record: state.join("backend.json"),
                control_record: state.join("control.json"),
                state,
                backend_bind: "127.0.0.1:0".parse().unwrap(),
                control_bind: "127.0.0.1:0".parse().unwrap(),
                fresh: false,
            },
            gateway: GatewayConfig::new("127.0.0.1:0".parse().unwrap()),
            management: Some(ManagementConfig { url: self.url.clone(), token: "secret".into() }),
        };
        let stop = CancellationToken::new();
        (stop.clone(), tokio::spawn(crate::run(config, stop)))
    }

    /// The next report, which must be under the latest lease.
    async fn expect(&mut self, revision: u64, deployment: &str, state: DeploymentState) -> (u64, DeploymentProgress) {
        let report = tokio::time::timeout(Duration::from_secs(60), self.reported.recv()).await.unwrap().unwrap();
        assert_eq!((report.lease, report.desired_revision), (*self.management.lease.borrow(), revision));
        let progress = report.deployment.unwrap();
        assert_eq!((progress.deployment_id.as_str(), progress.state()), (deployment, state));
        (report.sequence, progress)
    }

    async fn serves(&self, deployment: &str) -> bool {
        serves(&self.state().join("backend.json"), deployment).await
    }

    /// Waits until the backend no longer holds `deployment`.
    async fn released(&self, deployment: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while self.serves(deployment).await {
            assert!(tokio::time::Instant::now() < deadline, "{deployment} was never released");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploys_a_valid_release_and_keeps_serving_it_when_a_later_one_is_rejected() {
    let mut harness = Harness::new().await;
    let valid = harness.valid();
    harness.desire(1, "dep_first", valid.clone());
    let (stop, running) = harness.start();

    let (first, _) = harness.expect(1, "dep_first", DeploymentState::InProgress).await;
    let (second, _) = harness.expect(1, "dep_first", DeploymentState::Active).await;
    assert!(first < second);
    assert!(harness.serves("dep_first").await);

    let rejected = artifact(&harness.management, &harness.url, "rejected", invalid());
    harness.desire(2, "dep_second", rejected);
    harness.expect(2, "dep_second", DeploymentState::InProgress).await;
    let (_, DeploymentProgress { message, .. }) = harness.expect(2, "dep_second", DeploymentState::Failed).await;
    assert!(
        message.starts_with("release rejected fails verification") && message.len() <= super::MAX_MESSAGE_BYTES,
        "{message}"
    );

    // Management records the failure and falls back to the deployment that kept serving.
    harness.desire(3, "dep_first", valid);
    harness.expect(3, "dep_first", DeploymentState::Active).await;
    assert!(harness.serves("dep_first").await);
    assert!(!harness.state().join("releases/rejected").exists());
    assert!(!running.is_finished());

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployment_superseded_mid_download_never_activates_and_a_fenced_core_stops() {
    let mut harness = Harness::new().await;
    harness.desire(1, "dep_a", harness.valid());
    let (_stop, running) = harness.start();
    harness.expect(1, "dep_a", DeploymentState::InProgress).await;
    harness.expect(1, "dep_a", DeploymentState::Active).await;

    harness.desire(2, "dep_b", harness.stalled());
    harness.expect(2, "dep_b", DeploymentState::InProgress).await;
    harness.management.stalled.notified().await;
    harness.desire(3, "dep_c", harness.valid());
    // Had dep_b activated, its report would arrive first.
    harness.expect(3, "dep_c", DeploymentState::InProgress).await;
    harness.expect(3, "dep_c", DeploymentState::Active).await;
    assert!(harness.serves("dep_c").await && !harness.serves("dep_b").await);
    harness.released("dep_a").await;

    harness.management.lease.send_modify(|lease| *lease += 1);
    let error = tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap_err();
    assert!(error.to_string().contains("fenced"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_after_an_interrupted_deployment_retires_what_it_no_longer_serves() {
    let mut harness = Harness::new().await;
    harness.desire(1, "dep_a", harness.valid());
    let (stop, running) = harness.start();
    harness.expect(1, "dep_a", DeploymentState::InProgress).await;
    harness.expect(1, "dep_a", DeploymentState::Active).await;
    harness.desire(2, "dep_b", harness.stalled());
    harness.expect(2, "dep_b", DeploymentState::InProgress).await;
    harness.management.stalled.notified().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();

    let abandoned = harness.state().join("releases/.unpack-abandoned");
    fs::create_dir_all(&abandoned).unwrap();
    let unused = harness.state().join("releases/unused");
    fs::create_dir_all(&unused).unwrap();
    harness.desire(3, "dep_c", harness.valid());
    let (stop, running) = harness.start();
    harness.expect(3, "dep_c", DeploymentState::InProgress).await;
    harness.expect(3, "dep_c", DeploymentState::Active).await;
    harness.released("dep_a").await;
    assert!(harness.serves("dep_c").await);
    assert!(!abandoned.exists() && !unused.exists());
    assert!(harness.state().join("releases").join(&harness.release.0).exists());

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
}
