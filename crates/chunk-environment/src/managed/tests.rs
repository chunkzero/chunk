use super::release;
use crate::{Config, CoreConfig, GatewayConfig, ManagementConfig, core::Archives};
use bytes::Bytes;
use chunk_contract::ControlConnection;
use chunk_management::v1::{
    AttachRequest, AttachResponse, DeploymentProgress, DeploymentState, ReleaseArtifact, ReportStatusRequest,
};
use chunk_proto::sync::v1::{CallRequest, call_response::Outcome, core_client::CoreClient, error::Code};
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
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
    records: Mutex<Records>,
    desired: watch::Sender<AttachResponse>,
    /// The lease of the latest core attach, which fences every core attached before it.
    lease: watch::Sender<u64>,
    reports: mpsc::UnboundedSender<ReportStatusRequest>,
    archives: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Notified when a download of an archive whose path names it `stalled` starts. That download sends half of the
    /// archive, then nothing.
    stalled: Notify,
    /// ACTIVE reports for this deployment are refused as unavailable, notifying `refusal`.
    refused: Mutex<Option<String>>,
    refusal: Notify,
}

/// Management's record of the environment's deployments, oldest first, kept by the rules of `packages/management`.
#[derive(Default)]
struct Records {
    revision: u64,
    deployments: Vec<(String, ReleaseArtifact, DeploymentState)>,
}

impl Management {
    /// Supersedes the unfinished deployments with a new one.
    fn deploy(&self, deployment: &str, release: ReleaseArtifact) {
        let mut records = self.records.lock().unwrap();
        for (_, _, state) in &mut records.deployments {
            if matches!(state, DeploymentState::Pending | DeploymentState::InProgress) {
                *state = DeploymentState::Superseded;
            }
        }
        records.deployments.push((deployment.into(), release, DeploymentState::Pending));
        self.publish(&mut records);
    }

    fn record(&self, progress: &DeploymentProgress) {
        let mut records = self.records.lock().unwrap();
        let deployments = &mut records.deployments;
        let active = deployments.iter().position(|(_, _, state)| *state == DeploymentState::Active);
        let Some(index) = deployments.iter().position(|(id, _, _)| *id == progress.deployment_id) else { return };
        let state = deployments[index].2;
        let unfinished = matches!(state, DeploymentState::Pending | DeploymentState::InProgress);
        match progress.state() {
            DeploymentState::InProgress if state == DeploymentState::Pending => {
                deployments[index].2 = DeploymentState::InProgress;
            }
            // A superseded deployment the environment activated still replaces an older active one.
            DeploymentState::Active
                if unfinished
                    || (state == DeploymentState::Superseded && active.is_none_or(|active| active < index)) =>
            {
                if let Some(active) = active {
                    deployments[active].2 = DeploymentState::Superseded;
                }
                deployments[index].2 = DeploymentState::Active;
            }
            DeploymentState::Failed if unfinished => {
                deployments[index].2 = DeploymentState::Failed;
                self.publish(&mut records);
            }
            _ => {}
        }
    }

    /// Desires the newest deployment that neither failed nor was superseded, as a new revision.
    fn publish(&self, records: &mut Records) {
        records.revision += 1;
        let served = records.deployments.iter().rev().find(|(_, _, state)| {
            matches!(state, DeploymentState::Pending | DeploymentState::InProgress | DeploymentState::Active)
        });
        self.desired.send_replace(AttachResponse {
            revision: records.revision,
            environment_id: "env_test".into(),
            project_id: "prj_test".into(),
            deployment_id: served.map(|(id, _, _)| id.clone()).unwrap_or_default(),
            release: served.map(|(_, release, _)| release.clone()),
            ..Default::default()
        });
    }
}

fn envelope(flags: u8, payload: &[u8]) -> Bytes {
    let mut frame = vec![flags];
    frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(payload);
    frame.into()
}

const FENCED: &str = r#"{"code":"failed_precondition","message":"a newer core attached"}"#;
const UNAVAILABLE: &str = r#"{"code":"unavailable","message":"try again"}"#;

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
            let progress = report.deployment.clone().unwrap_or_default();
            let refused = progress.state() == DeploymentState::Active
                && management.refused.lock().unwrap().as_ref() == Some(&progress.deployment_id);
            let error = if report.lease < *management.lease.borrow() {
                Some((400, FENCED))
            } else if refused {
                management.refusal.notify_one();
                Some((503, UNAVAILABLE))
            } else {
                management.record(&progress);
                management.reports.send(report).unwrap();
                None
            };
            match error {
                Some((status, error)) => response
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Full::new(error.into()).boxed())
                    .unwrap(),
                None => response.header("content-type", "application/proto").body(Full::default().boxed()).unwrap(),
            }
        }
        _ if path.contains("unavailable") => response.status(503).body(Full::default().boxed()).unwrap(),
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

/// A one-app release, as `chunk build` publishes it, whose `status` query returns `status`.
fn publish(root: &Path, status: u8) -> std::path::PathBuf {
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
    fs::write(backend.join("source.mjs"), format!("export function status() {{ return {status}; }}")).unwrap();
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

/// Whether the backend holds `deployment`: a CLI call to the `status` query every test release declares fails its
/// contract only once the deployment isn't resident. `None` while the backend releases it, since it refuses calls to a
/// deployment it is releasing as unavailable.
async fn serves(record: &Path, deployment: &str) -> Option<bool> {
    let control: ControlConnection = chunk_service::read(record).unwrap();
    let call = CallRequest {
        method: "status".into(),
        arguments: b"null".to_vec(),
        deployment: deployment.into(),
        ..CallRequest::default()
    };
    let mut request = tonic::Request::new(call);
    request.metadata_mut().insert("authorization", format!("Bearer {}", control.token).parse().unwrap());
    let response = CoreClient::connect(control.endpoint).await.unwrap().call(request).await.unwrap().into_inner();
    match response.outcome {
        Some(Outcome::Result(_)) => Some(true),
        Some(Outcome::Error(error)) if error.code() == Code::Contract => Some(false),
        Some(Outcome::Error(error)) if error.code() == Code::Unavailable => None,
        outcome => panic!("{deployment}: {outcome:?}"),
    }
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
        let archive = publish(directory.path(), 1);
        let release_id = archive.file_name().unwrap().to_str().unwrap().trim_end_matches(".tar.gz").to_owned();
        let (reports, reported) = mpsc::unbounded_channel();
        let management = Arc::new(Management {
            records: Mutex::default(),
            desired: watch::Sender::new(AttachResponse::default()),
            lease: watch::Sender::new(0),
            reports,
            archives: Mutex::default(),
            stalled: Notify::new(),
            refused: Mutex::default(),
            refusal: Notify::new(),
        });
        let url = serve(management.clone()).await;
        Self { directory, management, url, reported, release: (release_id, fs::read(&archive).unwrap()) }
    }

    fn valid(&self) -> ReleaseArtifact {
        artifact(&self.management, &self.url, &self.release.0, self.release.1.clone())
    }

    /// The published release, downloaded from a path under `/<route>/`.
    fn valid_at(&self, route: &str) -> ReleaseArtifact {
        let path = format!("/{route}/{}.tar.gz", self.release.0);
        self.management.archives.lock().unwrap().insert(path.clone(), self.release.1.clone());
        ReleaseArtifact { url: format!("{}{path}", self.url), ..self.valid() }
    }

    fn stalled(&self) -> ReleaseArtifact {
        artifact(&self.management, &self.url, "stalled", self.release.1.clone())
    }

    fn deploy(&self, deployment: &str, release: ReleaseArtifact) {
        self.management.deploy(deployment, release);
    }

    fn state(&self) -> std::path::PathBuf {
        self.directory.path().join("state")
    }

    fn archive(&self, release_id: &str) -> std::path::PathBuf {
        self.state().join("archives").join(format!("{release_id}.tar.gz"))
    }

    fn core(&self) -> CoreConfig {
        let state = self.state();
        CoreConfig {
            bundle: None,
            environment: "env_test".into(),
            control_record: state.join("control.json"),
            state,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            core_bind: None,
            private_address: None,
            java: "java".into(),
            environment_token: None,
            fresh: false,
        }
    }

    /// Leaves `count` backend versions resident that control never ran, as a run that crashed after each commit would.
    async fn abandon(&self, count: usize) {
        let core = crate::Core::start(self.core(), || {}).await.unwrap();
        let status = chunk_contract::Function {
            kind: chunk_contract::FunctionKind::Query,
            visibility: chunk_contract::Visibility::Public,
            export: "status".into(),
            arguments: chunk_contract::Schema::Null,
            result: chunk_contract::Schema::Integer,
        };
        for index in 0..count {
            let bundle = chunk_contract::Deployment {
                contracts: chunk_contract::Contracts::default(),
                contract_version: 2,
                runtime_profile: chunk_contract::RuntimeProfile::TransactionalV1,
                id: format!("dep_abandoned_{index}"),
                source: "export function status() { return 1; }".into(),
                tables: BTreeMap::new(),
                functions: BTreeMap::from([("status".into(), status.clone())]),
            };
            core.deploy(bundle).await.unwrap();
        }
        core.stop(|| {}).await.unwrap();
    }

    fn start(&self) -> (CancellationToken, tokio::task::JoinHandle<std::io::Result<()>>) {
        let config = Config::Core {
            core: Box::new(self.core()),
            gateway: Some(GatewayConfig::new("127.0.0.1:0".parse().unwrap())),
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

    /// Waits until management refuses an ACTIVE report.
    async fn refused(&self) {
        tokio::time::timeout(Duration::from_secs(60), self.management.refusal.notified()).await.unwrap();
    }

    async fn serves(&self, deployment: &str) -> bool {
        let serves = serves(&self.state().join("control.json"), deployment).await;
        serves.unwrap_or_else(|| panic!("{deployment} is unavailable"))
    }

    /// Waits until the backend no longer holds `deployment`.
    async fn released(&self, deployment: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while serves(&self.state().join("control.json"), deployment).await != Some(false) {
            assert!(tokio::time::Instant::now() < deadline, "{deployment} was never released");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keeps_the_replaced_deployment_until_management_accepts_the_next_and_falls_back_to_it() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let (stop, running) = harness.start();
    let (first, _) = harness.expect(1, "dep_a", DeploymentState::InProgress).await;
    let (second, _) = harness.expect(1, "dep_a", DeploymentState::Active).await;
    assert!(first < second);
    let kept = fs::read(harness.archive(&harness.release.0)).unwrap();
    assert_eq!(Sha256::digest(&kept), Sha256::digest(&harness.release.1));

    // dep_b activates, but its report fails until dep_c has superseded it.
    *harness.management.refused.lock().unwrap() = Some("dep_b".into());
    harness.deploy("dep_b", harness.valid());
    harness.expect(2, "dep_b", DeploymentState::InProgress).await;
    harness.refused().await;
    harness.deploy("dep_c", artifact(&harness.management, &harness.url, "rejected", invalid()));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(harness.serves("dep_a").await && harness.serves("dep_b").await);
    *harness.management.refused.lock().unwrap() = None;

    // Attached again, the core reports dep_b before it starts on dep_c.
    harness.expect(3, "dep_b", DeploymentState::Active).await;
    harness.expect(3, "dep_c", DeploymentState::InProgress).await;
    let (_, DeploymentProgress { message, .. }) = harness.expect(3, "dep_c", DeploymentState::Failed).await;
    assert!(
        message.starts_with("release rejected fails verification") && message.len() <= super::MAX_MESSAGE_BYTES,
        "{message}"
    );

    // Management falls back to dep_b, which keeps serving without deploying again, and dep_a retires.
    harness.expect(4, "dep_b", DeploymentState::Active).await;
    harness.released("dep_a").await;
    assert!(harness.serves("dep_b").await);
    assert!(!harness.state().join("releases/rejected").exists() && !harness.archive("rejected").exists());
    assert!(!running.is_finished());

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deployment_superseded_mid_download_never_activates_and_a_fenced_core_stops() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let (_stop, running) = harness.start();
    harness.expect(1, "dep_a", DeploymentState::InProgress).await;
    harness.expect(1, "dep_a", DeploymentState::Active).await;

    harness.deploy("dep_b", harness.stalled());
    harness.expect(2, "dep_b", DeploymentState::InProgress).await;
    harness.management.stalled.notified().await;
    harness.deploy("dep_c", harness.valid());
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
async fn restarts_retire_only_what_management_no_longer_needs() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let (stop, running) = harness.start();
    harness.expect(1, "dep_a", DeploymentState::InProgress).await;
    harness.expect(1, "dep_a", DeploymentState::Active).await;
    harness.deploy("dep_b", harness.stalled());
    harness.expect(2, "dep_b", DeploymentState::InProgress).await;
    harness.management.stalled.notified().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
    // With dep_a, these fill the backend's 16 versions, so dep_c deploys only once they retire.
    harness.abandon(15).await;

    let abandoned = harness.state().join("releases/.unpack-abandoned");
    fs::create_dir_all(&abandoned).unwrap();
    let unused = harness.state().join("releases/unused");
    fs::create_dir_all(&unused).unwrap();
    let (abandoned_archive, unused_archive) = (harness.archive(".abandoned"), harness.archive("unused"));
    fs::write(&abandoned_archive, b"partial").unwrap();
    fs::write(&unused_archive, b"unused").unwrap();
    let installed = harness.state().join("releases").join(&harness.release.0);
    fs::write(installed.join("source.mjs"), "tampered").unwrap();
    // Its download unavailable, dep_c's release is installed again from the archive kept for it.
    harness.deploy("dep_c", harness.valid_at("unavailable"));
    let (stop, running) = harness.start();
    harness.expect(3, "dep_c", DeploymentState::InProgress).await;
    harness.expect(3, "dep_c", DeploymentState::Active).await;
    harness.released("dep_a").await;
    harness.released("dep_abandoned_0").await;
    assert!(harness.serves("dep_c").await);
    assert!(!abandoned.exists() && !unused.exists());
    assert!(!abandoned_archive.exists() && !unused_archive.exists());
    assert_eq!(chunk_build::verify_release(&installed).unwrap().id, harness.release.0);

    // dep_d activates, but the core stops before management accepts it, and dep_e supersedes it meanwhile.
    *harness.management.refused.lock().unwrap() = Some("dep_d".into());
    harness.deploy("dep_d", harness.valid());
    harness.expect(4, "dep_d", DeploymentState::InProgress).await;
    harness.refused().await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
    harness.deploy("dep_e", artifact(&harness.management, &harness.url, "rejected", invalid()));

    // Restarted, the core keeps dep_c until management accepts dep_d, which it falls back to once dep_e fails.
    let (stop, running) = harness.start();
    harness.refused().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(harness.serves("dep_c").await && harness.serves("dep_d").await);
    *harness.management.refused.lock().unwrap() = None;
    harness.expect(5, "dep_d", DeploymentState::Active).await;
    harness.expect(5, "dep_e", DeploymentState::InProgress).await;
    harness.expect(5, "dep_e", DeploymentState::Failed).await;
    harness.expect(6, "dep_d", DeploymentState::InProgress).await;
    harness.expect(6, "dep_d", DeploymentState::Active).await;
    harness.released("dep_c").await;
    assert!(harness.serves("dep_d").await);
    assert!(!harness.state().join("managed.json").exists());

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unfinished_archive_repair_leaves_the_serving_release_installed() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let (stop, running) = harness.start();
    harness.expect(1, "dep_a", DeploymentState::InProgress).await;
    harness.expect(1, "dep_a", DeploymentState::Active).await;

    // dep_b runs the same release, whose kept archive is corrupt, and dep_c supersedes it mid-download.
    fs::write(harness.archive(&harness.release.0), b"corrupt").unwrap();
    harness.deploy("dep_b", harness.valid_at("stalled"));
    harness.expect(2, "dep_b", DeploymentState::InProgress).await;
    harness.management.stalled.notified().await;
    harness.deploy("dep_c", artifact(&harness.management, &harness.url, "rejected", invalid()));
    harness.expect(3, "dep_c", DeploymentState::InProgress).await;
    harness.expect(3, "dep_c", DeploymentState::Failed).await;

    // Management falls back to dep_a, which serves from its untouched install.
    harness.expect(4, "dep_a", DeploymentState::Active).await;
    let installed = harness.state().join("releases").join(&harness.release.0);
    assert_eq!(chunk_build::verify_release(&installed).unwrap().id, harness.release.0);
    assert!(harness.serves("dep_a").await);
    assert!(!harness.archive(&harness.release.0).exists());

    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn reclaiming_a_release_forgets_its_archive_and_a_restart_restores_the_retained_one() {
    let harness = Harness::new().await;
    let second = publish(&harness.directory.path().join("second"), 2);
    let id = second.file_name().unwrap().to_str().unwrap().trim_end_matches(".tar.gz").to_owned();
    let second = artifact(&harness.management, &harness.url, &id, fs::read(second).unwrap());
    let first = harness.valid();
    assert_ne!(first.release_id, second.release_id);
    let client = chunk_management::Client::new(harness.url.clone()).with_token("secret");
    let lookup = Arc::new(Archives::default());
    let store = release::Store::new(&harness.state(), lookup.clone());
    for artifact in [&first, &second] {
        release::load(&client, &store, artifact, &CancellationToken::new()).await.unwrap().unwrap();
    }
    let kept = lookup.get(&second.release_id).unwrap();
    assert_eq!((kept.sha256.as_str(), kept.size), (second.sha256.as_str(), second.size_bytes));
    assert_eq!(format!("{:x}", Sha256::digest(fs::read(&kept.path).unwrap())), second.sha256);

    let aside = release::set_aside(&store, BTreeSet::from([second.release_id.clone()]));
    release::remove(aside).await.unwrap();
    assert!(lookup.get(&first.release_id).is_none() && !harness.archive(&first.release_id).exists());
    let releases = harness.state().join("releases");
    assert!(!releases.join(&first.release_id).exists() && releases.join(&second.release_id).exists());
    assert_eq!(lookup.get(&second.release_id).as_ref(), Some(&kept));

    // Restarted, core looks the retained release's archive up again only while it matches the recorded digest.
    let restart = || async {
        let lookup = Arc::new(Archives::default());
        let retained = BTreeSet::from([first.release_id.clone(), second.release_id.clone()]);
        release::restore(&release::Store::new(&harness.state(), lookup.clone()), retained).await.unwrap();
        (lookup.get(&first.release_id), lookup.get(&second.release_id))
    };
    assert_eq!(restart().await, (None, Some(kept.clone())));
    let mut corrupt = fs::read(&kept.path).unwrap();
    corrupt[0] ^= 1;
    fs::write(&kept.path, corrupt).unwrap();
    assert_eq!(restart().await, (None, None));
}

mod runner_image;
