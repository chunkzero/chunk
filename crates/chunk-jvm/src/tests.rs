use super::*;
use chunk_proto::sync::v1::{
    CallRequest, CallResponse, Error, JvmAotRecord, JvmAotUse, JvmAotWrite, JvmArchiveChunk, JvmArchiveRead,
    SubscribeRequest, Update, call_response, core_server, error::Code,
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
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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
    /// Archive reads are never answered.
    stall: bool,
    /// What the launch says about the AOT cache.
    aot: Option<Aot>,
}

/// The AOT cache the fake core keeps.
const AOT_CACHE: &[u8] = b"an AOT cache";

struct FakeCore {
    script: Script,
    launches: AtomicUsize,
    reads: AtomicUsize,
    aot_reads: AtomicUsize,
    /// The AOT cache the runner uploaded, as it declared it, and whether it abandoned its recording.
    uploaded: Mutex<(Vec<u8>, u64, String)>,
    abandoned: AtomicBool,
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
            aot: self.script.aot.clone(),
            environment_name: "prod".into(),
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
                if core.script.stall {
                    std::future::pending::<()>().await;
                }
                let read = JvmArchiveRead::decode(request.arguments.as_slice()).unwrap();
                let mut archive = RELEASE.0.clone();
                archive.resize(archive.len() + core.script.extra, 0);
                let start = usize::try_from(read.offset).unwrap();
                let data = archive[start..archive.len().min(start + CHUNK)].to_vec();
                JvmArchiveChunk { data }.encode_to_vec()
            }
            "chunk:aot-read" => {
                core.aot_reads.fetch_add(1, Ordering::SeqCst);
                let read = JvmArchiveRead::decode(request.arguments.as_slice()).unwrap();
                let start = usize::try_from(read.offset).unwrap();
                JvmArchiveChunk { data: AOT_CACHE[start..AOT_CACHE.len().min(start + 5)].to_vec() }.encode_to_vec()
            }
            "chunk:aot-write" => {
                let write = JvmAotWrite::decode(request.arguments.as_slice()).unwrap();
                if write.abandon {
                    core.abandoned.store(true, Ordering::SeqCst);
                } else {
                    let mut uploaded = core.uploaded.lock().unwrap();
                    assert_eq!(write.offset, uploaded.0.len() as u64);
                    uploaded.0.extend(write.data);
                    (uploaded.1, uploaded.2) = (write.size, write.sha256);
                }
                Vec::new()
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
    cpus: usize,
}

impl Machine {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let java_home = directory.path().join("java");
        fs::create_dir_all(java_home.join("bin")).unwrap();
        fs::write(java_home.join("release"), "IMPLEMENTOR=\"Test\"\nJAVA_VERSION=\"25.0.1\"\n").unwrap();
        let java = java_home.join("bin/java");
        // Java's AOT create step records its arguments apart and writes a cache, unless `java.fail` exists.
        let script = r#"#!/bin/sh
case "$*" in *-XX:AOTMode=create*)
    printf '%s\n' "$@" > "$0.create"
    [ -e "$0.fail" ] && exit 1
    for arg; do case "$arg" in -XX:AOTCache=*) printf created > "${arg#-XX:AOTCache=}";; esac; done
    exit 0;;
esac
printf '%s\n' "$@" > "$0.args"
env > "$0.env"
pwd > "$0.pwd"
"#;
        fs::write(&java, script).unwrap();
        fs::set_permissions(&java, fs::Permissions::from_mode(0o700)).unwrap();
        let proc = directory.path().join("proc/self");
        fs::create_dir_all(&proc).unwrap();
        fs::write(proc.join("cgroup"), "0::/\n").unwrap();
        let cgroup = directory.path().join("cgroup");
        fs::write(proc.join("mountinfo"), format!("35 24 0:30 / {} rw - cgroup2 cgroup2 rw\n", cgroup.display()))
            .unwrap();
        fs::create_dir(&cgroup).unwrap();
        fs::write(cgroup.join("memory.max"), "1073741824\n").unwrap();
        fs::create_dir(directory.path().join("work")).unwrap();
        Self { directory, cpus: 2 }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn config(&self, endpoint: &str) -> Config {
        Config {
            endpoint: endpoint.into(),
            core: endpoint.strip_prefix("http://").unwrap().parse().unwrap(),
            credential: CREDENTIAL.into(),
            host: "host-1".into(),
            cache: self.path("cache"),
            player_address: None,
            stop_grace: Duration::from_secs(1),
            java_home: self.path("java"),
            expected: Expected::default(),
            retry: Duration::from_secs(10),
            proc: self.path("proc"),
            work_root: self.path("work"),
            cpus: self.cpus,
        }
    }

    /// Runs the runner against a fake core following `script`, returning its exit code and the core.
    async fn run(&self, script: Script) -> (Result<i32, u8>, Arc<FakeCore>) {
        self.run_within(script, Duration::from_secs(10)).await
    }

    /// Like `run`, retrying calls to core for up to `retry`.
    async fn run_within(&self, script: Script, retry: Duration) -> (Result<i32, u8>, Arc<FakeCore>) {
        let core = Arc::new(FakeCore {
            script,
            launches: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            aot_reads: AtomicUsize::new(0),
            uploaded: Mutex::default(),
            abandoned: AtomicBool::new(false),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server =
            tonic::transport::Server::builder().add_service(core_server::CoreServer::new(Served(core.clone())));
        let handle = tokio::spawn(server.serve_with_incoming(TcpListenerStream::new(listener)));
        let (_sender, signals) = mpsc::unbounded_channel();
        let exit =
            super::run(Config { retry, ..self.config(&endpoint) }, signals).await.map_err(|failure| failure.code);
        handle.abort();
        (exit, core)
    }

    /// The entries of the release cache.
    /// The lines of what the fake Java recorded in `java/bin/java.<name>`.
    fn java(&self, name: &str) -> Vec<String> {
        let recorded = fs::read_to_string(self.path(&format!("java/bin/java.{name}"))).unwrap();
        recorded.lines().map(str::to_owned).collect()
    }

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
    assert_eq!(args[..5], ["-Xms261m", "-Xmx261m", "-XX:+UseG1GC", "-XX:+ExitOnOutOfMemoryError", "-jar"]);
    let jar = Path::new(args[5]);
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
        ("CHUNK_ENVIRONMENT_NAME", "prod"),
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
async fn a_core_that_never_answers_exhausts_the_retries() {
    let machine = Machine::new();
    let (exit, core) =
        machine.run_within(Script { stall: true, ..Script::default() }, Duration::from_millis(500)).await;
    assert_eq!((exit, core.reads.load(Ordering::SeqCst)), (Err(69), 1));
}

#[tokio::test]
async fn a_release_newer_than_the_images_java_is_refused() {
    let machine = Machine::new();
    fs::write(machine.path("java/release"), "JAVA_VERSION=\"21.0.4\"\n").unwrap();
    let (exit, _) = machine.run(Script::default()).await;
    assert_eq!(exit, Err(78));
    assert!(!machine.path("java/bin/java.args").exists());
}

#[tokio::test]
async fn a_recording_run_creates_and_uploads_the_aot_cache_once_the_jvm_exits_cleanly() {
    let machine = Machine::new();
    let record = || Script { aot: Some(Aot::Record(JvmAotRecord {})), ..Script::default() };
    let (exit, core) = machine.run(record()).await;
    assert_eq!(exit, Ok(0));
    let args = machine.java("args");
    let configuration = args[4].strip_prefix("-XX:AOTConfiguration=").unwrap();
    assert_eq!(args[3], "-XX:AOTMode=record");
    assert!(Path::new(configuration).starts_with(machine.path("work")));
    // Creating the cache repeats the run's Java flags and JAR.
    let create = machine.java("create");
    assert_eq!((&create[..3], create[3].as_str()), (&args[..3], "-XX:AOTMode=create"));
    assert_eq!(create[4], format!("-XX:AOTConfiguration={configuration}"));
    assert!(create[5].starts_with("-XX:AOTCache="));
    assert_eq!(create[6..], args[5..]);
    let uploaded = core.uploaded.lock().unwrap().clone();
    assert_eq!(uploaded, (b"created".to_vec(), 7, format!("{:x}", Sha256::digest(b"created"))));
    assert!(!core.abandoned.load(Ordering::SeqCst));
    assert!(!Path::new(configuration).parent().unwrap().exists());

    // A create step that fails uploads nothing, and the runner still exits as the JVM did.
    fs::write(machine.path("java/bin/java.fail"), "").unwrap();
    let (exit, core) = machine.run(record()).await;
    assert_eq!(exit, Ok(0));
    assert!(core.uploaded.lock().unwrap().0.is_empty() && core.abandoned.load(Ordering::SeqCst));
}

#[tokio::test]
async fn a_machine_too_small_to_record_runs_without_recording_and_tells_core_but_uses_a_cache() {
    let machine = Machine::new();
    fs::write(machine.path("cgroup/memory.max"), format!("{}\n", 512 * 1024 * 1024)).unwrap();
    let (exit, core) = machine.run(Script { aot: Some(Aot::Record(JvmAotRecord {})), ..Script::default() }).await;
    assert_eq!(exit, Ok(0));
    assert!(!machine.java("args").iter().any(|arg| arg.starts_with("-XX:AOTMode")));
    assert!(core.uploaded.lock().unwrap().0.is_empty() && core.abandoned.load(Ordering::SeqCst));

    let using = JvmAotUse { size: AOT_CACHE.len() as u64, sha256: format!("{:x}", Sha256::digest(AOT_CACHE)) };
    let (exit, _) = machine.run(Script { aot: Some(Aot::Use(using)), ..Script::default() }).await;
    assert_eq!(exit, Ok(0));
    assert!(machine.java("args")[4].starts_with("-XX:AOTCache="));
}

#[tokio::test]
async fn a_one_cpu_512_mib_machine_records_with_the_serial_collector_and_a_growing_heap() {
    let machine = Machine { cpus: 1, ..Machine::new() };
    fs::write(machine.path("cgroup/memory.max"), format!("{}\n", 512 * 1024 * 1024)).unwrap();
    let (exit, core) = machine.run(Script { aot: Some(Aot::Record(JvmAotRecord {})), ..Script::default() }).await;
    assert_eq!(exit, Ok(0));
    let args = machine.java("args");
    assert_eq!(args[..4], ["-Xmx261m", "-XX:+UseSerialGC", "-XX:+ExitOnOutOfMemoryError", "-XX:AOTMode=record"]);
    assert!(!core.uploaded.lock().unwrap().0.is_empty());
}

#[tokio::test]
async fn the_jvm_starts_with_a_fetched_aot_cache_or_without_one_that_fails_its_digest() {
    let machine = Machine::new();
    let sha256 = format!("{:x}", Sha256::digest(AOT_CACHE));
    let using = |sha256: &str| Script {
        aot: Some(Aot::Use(JvmAotUse { size: AOT_CACHE.len() as u64, sha256: sha256.into() })),
        ..Script::default()
    };
    let (exit, core) = machine.run(using(&sha256)).await;
    assert_eq!(exit, Ok(0));
    assert!(core.aot_reads.load(Ordering::SeqCst) > 1);
    let args = machine.java("args");
    let cache = args[4].strip_prefix("-XX:AOTCache=").unwrap();
    assert_eq!(Path::new(cache), machine.path("cache/aot").join(&RELEASE.1).join("lobby.aot"));
    assert_eq!(fs::read(cache).unwrap(), AOT_CACHE);
    assert_eq!(args[5], "-jar");
    // A kept cache that still matches is used again without a download.
    let (exit, core) = machine.run(using(&sha256)).await;
    assert_eq!((exit, core.aot_reads.load(Ordering::SeqCst)), (Ok(0), 0));
    assert!(machine.java("args")[4].starts_with("-XX:AOTCache="));

    let (exit, _) = machine.run(using(&"0".repeat(64))).await;
    assert_eq!(exit, Ok(0));
    assert_eq!(machine.java("args")[4], "-jar");
}
