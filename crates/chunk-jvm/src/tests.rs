use super::*;
use chunk_proto::sync::v1::{
    CallRequest, CallResponse, Error, JvmArchiveChunk, JvmArchiveRead, SubscribeRequest, Update, call_response,
    core_server, error::Code,
};
use config::Expected;
use prost::Message;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_stream::{Stream, wrappers::TcpListenerStream};
use tonic::{Request, Response, Status};

const CREDENTIAL: &str = "machine/v1/env/jvm/host-1/mac";
const CHUNK: usize = 256;

/// A one-app release archive, as `chunk build` publishes it, and its release ID.
static RELEASE: LazyLock<(Vec<u8>, String)> = LazyLock::new(|| {
    let root = tempfile::tempdir().unwrap();
    let archive = publish(root.path());
    let id = archive.file_name().unwrap().to_str().unwrap().trim_end_matches(".tar.gz").to_owned();
    (fs::read(archive).unwrap(), id)
});

fn publish(root: &Path) -> PathBuf {
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
    for (name, bytes) in [
        ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\r\nMain-Class: sample.lobby.Provider\r\n\r\n".to_vec()),
        ("sample/lobby/Provider.class", vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 69, 1]),
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

/// How the fake core answers.
#[derive(Default)]
struct Script {
    /// Launch calls answered as unavailable before the first real answer, alternating a gRPC status and an error.
    unavailable: usize,
    unauthenticated: bool,
    denied: bool,
    /// Bytes served past the archive's end.
    extra: usize,
    /// The digest core declares instead of the archive's.
    sha256: Option<String>,
}

struct FakeCore {
    script: Script,
    launches: AtomicUsize,
    reads: AtomicUsize,
}

impl FakeCore {
    fn launch(&self) -> JvmLaunch {
        let (archive, id) = &*RELEASE;
        JvmLaunch {
            deployment: "deployment-1".into(),
            release_id: id.clone(),
            archive_size: archive.len() as u64,
            archive_sha256: self.script.sha256.clone().unwrap_or_else(|| format!("{:x}", Sha256::digest(archive))),
            app: "lobby".into(),
            profile: "small".into(),
            process_id: "process-1".into(),
            generation: 3,
        }
    }
}

fn error(code: Code) -> Response<CallResponse> {
    let error = Error { code: code.into(), message: format!("{code:?}") };
    Response::new(CallResponse { outcome: Some(call_response::Outcome::Error(error)), ..CallResponse::default() })
}

struct Served(Arc<FakeCore>);

#[tonic::async_trait]
impl core_server::Core for Served {
    async fn call(&self, request: Request<CallRequest>) -> Result<Response<CallResponse>, Status> {
        let core = &self.0;
        if request.metadata().get("authorization").and_then(|value| value.to_str().ok())
            != Some(&format!("Bearer {CREDENTIAL}"))
            || core.script.unauthenticated
        {
            return Err(Status::unauthenticated("rejected"));
        }
        let request = request.into_inner();
        let result = match request.method.as_str() {
            "chunk:launch" => {
                let attempt = core.launches.fetch_add(1, Ordering::SeqCst);
                if attempt < core.script.unavailable {
                    return if attempt.is_multiple_of(2) {
                        Err(Status::unavailable("starting"))
                    } else {
                        Ok(error(Code::Unavailable))
                    };
                }
                if core.script.denied {
                    return Ok(error(Code::Denied));
                }
                core.launch().encode_to_vec()
            }
            "chunk:archive" => {
                core.reads.fetch_add(1, Ordering::SeqCst);
                let read = JvmArchiveRead::decode(request.arguments.as_slice()).unwrap();
                let mut archive = RELEASE.0.clone();
                archive.resize(archive.len() + core.script.extra, 0);
                let start = usize::try_from(read.offset).unwrap();
                let data = archive[start..archive.len().min(start + CHUNK)].to_vec();
                JvmArchiveChunk { data }.encode_to_vec()
            }
            _ => return Ok(error(Code::Invalid)),
        };
        Ok(Response::new(CallResponse { outcome: Some(call_response::Outcome::Result(result)), ..Default::default() }))
    }

    type SubscribeStream = Pin<Box<dyn Stream<Item = Result<Update, Status>> + Send>>;

    async fn subscribe(&self, _: Request<SubscribeRequest>) -> Result<Response<Self::SubscribeStream>, Status> {
        Err(Status::unimplemented("subscribe"))
    }
}

/// A machine with a cache, a Java 25 image whose `java` records how it ran, and 1 GiB of memory.
struct Machine {
    directory: tempfile::TempDir,
}

impl Machine {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let java_home = directory.path().join("java");
        fs::create_dir_all(java_home.join("bin")).unwrap();
        fs::write(java_home.join("release"), "IMPLEMENTOR=\"Test\"\nJAVA_VERSION=\"25.0.1\"\n").unwrap();
        let java = java_home.join("bin/java");
        fs::write(&java, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nenv > \"$0.env\"\npwd > \"$0.pwd\"\n")
            .unwrap();
        fs::set_permissions(&java, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(directory.path().join("memory.max"), "1073741824\n").unwrap();
        fs::create_dir(directory.path().join("work")).unwrap();
        Self { directory }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn config(&self, endpoint: &str) -> Config {
        Config {
            endpoint: endpoint.into(),
            core: endpoint.strip_prefix("http://").unwrap().parse().unwrap(),
            credential: CREDENTIAL.into(),
            environment: "env".into(),
            host: "host-1".into(),
            cache: self.path("cache"),
            player_address: None,
            stop_grace: Duration::from_secs(1),
            java_home: self.path("java"),
            expected: Expected::default(),
            retry: Duration::from_secs(10),
            memory_max: self.path("memory.max"),
            meminfo: self.path("meminfo"),
            work_root: self.path("work"),
        }
    }

    /// Runs the runner against a fake core following `script`, returning its exit code and the core.
    async fn run(&self, script: Script) -> (Result<i32, u8>, Arc<FakeCore>) {
        let core = Arc::new(FakeCore { script, launches: AtomicUsize::new(0), reads: AtomicUsize::new(0) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server =
            tonic::transport::Server::builder().add_service(core_server::CoreServer::new(Served(core.clone())));
        let handle = tokio::spawn(server.serve_with_incoming(TcpListenerStream::new(listener)));
        let (_sender, signals) = mpsc::unbounded_channel();
        let exit = super::run(self.config(&endpoint), signals).await.map_err(|failure| failure.code);
        handle.abort();
        (exit, core)
    }

    /// The entries of the release cache.
    fn cached(&self) -> Vec<String> {
        let entries = fs::read_dir(self.path("cache/releases")).unwrap();
        let mut names: Vec<_> = entries.map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        names
    }
}

#[tokio::test]
async fn a_runner_retries_an_unavailable_core_then_starts_the_verified_release() {
    let machine = Machine::new();
    let (exit, core) = machine.run(Script { unavailable: 2, ..Script::default() }).await;
    assert_eq!(exit, Ok(0));
    assert_eq!(core.launches.load(Ordering::SeqCst), 3);
    let args = fs::read_to_string(machine.path("java/bin/java.args")).unwrap();
    let args: Vec<_> = args.lines().collect();
    assert_eq!(args[..4], ["-Xmx261m", "-XX:+UseG1GC", "-XX:+ExitOnOutOfMemoryError", "-jar"]);
    let jar = Path::new(args[4]);
    assert!(jar.starts_with(machine.path("cache/releases").join(&RELEASE.1).canonicalize().unwrap()));
    let env = fs::read_to_string(machine.path("java/bin/java.env")).unwrap();
    let env: BTreeMap<_, _> = env.lines().filter_map(|line| line.split_once('=')).collect();
    assert!(env["CHUNK_CORE_ENDPOINT"].starts_with("http://127.0.0.1:"));
    for (name, value) in [
        ("CHUNK_PROCESS_TOKEN", CREDENTIAL),
        ("CHUNK_DEPLOYMENT", "deployment-1"),
        ("CHUNK_PROCESS_ID", "process-1"),
        ("CHUNK_PROCESS_GENERATION", "3"),
        ("CHUNK_MACHINE_PROFILE", "small"),
        ("CHUNK_APP_ID", "lobby"),
        ("CHUNK_ARTIFACT_DIGEST", &format!("{:x}", Sha256::digest(fs::read(jar).unwrap()))),
        ("CHUNK_PLAYER_ADDRESS", "127.0.0.1"),
        ("CHUNK_ENVIRONMENT", "env"),
        ("CHUNK_INSTANCE_ID", "host-1"),
    ] {
        assert_eq!(env.get(name), Some(&value), "{name}");
    }
    let working = fs::read_to_string(machine.path("java/bin/java.pwd")).unwrap();
    assert!(Path::new(working.trim()).starts_with(machine.path("work")));
    assert!(!Path::new(working.trim()).exists());
    assert_eq!(machine.cached(), [RELEASE.1.as_str()]);
}

#[tokio::test]
async fn a_verified_cache_is_reused_and_a_tampered_one_downloaded_again() {
    let machine = Machine::new();
    let (exit, core) = machine.run(Script::default()).await;
    assert_eq!(exit, Ok(0));
    assert!(core.reads.load(Ordering::SeqCst) > 1);
    let (exit, core) = machine.run(Script::default()).await;
    assert_eq!((exit, core.reads.load(Ordering::SeqCst)), (Ok(0), 0));
    let release = machine.path("cache/releases").join(&RELEASE.1);
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(release.join("release.json")).unwrap()).unwrap();
    let jar = release.join(manifest["apps"][0]["jar"].as_str().unwrap());
    let mut tampered = fs::read(&jar).unwrap();
    tampered.push(0);
    fs::write(&jar, tampered).unwrap();
    let (exit, core) = machine.run(Script::default()).await;
    assert_eq!(exit, Ok(0));
    assert!(core.reads.load(Ordering::SeqCst) > 1);
    assert!(chunk_build::verify_release(&release).is_ok());
}

#[tokio::test]
async fn an_archive_past_its_size_or_off_its_digest_fails_verification_and_is_not_kept() {
    let machine = Machine::new();
    let (exit, _) = machine.run(Script { extra: 1, ..Script::default() }).await;
    assert_eq!(exit, Err(65));
    let (exit, _) = machine.run(Script { sha256: Some("0".repeat(64)), ..Script::default() }).await;
    assert_eq!(exit, Err(65));
    assert!(machine.cached().is_empty());
    assert!(!machine.path("java/bin/java.args").exists());
}

#[tokio::test]
async fn a_rejected_credential_or_refused_boot_is_permanent() {
    let machine = Machine::new();
    let (exit, _) = machine.run(Script { unauthenticated: true, ..Script::default() }).await;
    assert_eq!(exit, Err(77));
    let (exit, core) = machine.run(Script { denied: true, ..Script::default() }).await;
    assert_eq!((exit, core.launches.load(Ordering::SeqCst)), (Err(77), 1));
}

#[tokio::test]
async fn a_core_that_stays_unavailable_exhausts_the_retries() {
    let machine = Machine::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let config = Config { retry: Duration::from_millis(500), ..machine.config(&endpoint) };
    let (_sender, signals) = mpsc::unbounded_channel();
    assert_eq!(super::run(config, signals).await.unwrap_err().code, 69);
}

#[tokio::test]
async fn a_release_newer_than_the_images_java_is_refused() {
    let machine = Machine::new();
    fs::write(machine.path("java/release"), "JAVA_VERSION=\"21.0.4\"\n").unwrap();
    let (exit, _) = machine.run(Script::default()).await;
    assert_eq!(exit, Err(78));
    assert!(!machine.path("java/bin/java.args").exists());
}
