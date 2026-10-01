//! A remote runner's `chunk:launch`, `chunk:archive` and AOT cache calls, and the JVM machine credential it and its JVM
//! present.

mod host;

use super::{jvm::registration, *};
use crate::core::ReleaseArchive;
use chunk_control::{Launch, MachineKind, Progress, Registration, RuntimeConnection};
use chunk_proto::sync::v1::{
    JvmAotRecord, JvmAotUse, JvmAotWrite, JvmArchiveChunk, JvmArchiveRead, JvmBoot, JvmLaunch, JvmRegistered,
    JvmRegistration, jvm_launch::Aot,
};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

const HOST: &str = "runner-1";
const RELEASE: &str = "release-1";
const CHUNK: usize = 4 * 1024 * 1024 - 1024;
const RUNTIME: &str = "Eclipse Adoptium 25.0.1+8-LTS x86_64";

/// Runs `HOST` remotely, whose JVM registers with its machine credential. `JVM` is `host-1`'s process credential.
#[derive(Default)]
struct Remote(Mutex<Option<(String, Registration)>>);

#[tonic::async_trait]
impl chunk_control::Host for Remote {
    async fn ensure(&self, _: &str, _: &chunk_control::Release, _: &str, _: &str) -> chunk_control::Result<Progress> {
        Ok(Progress::Pending)
    }
    async fn release(&self, _: &str) -> chunk_control::Result<bool> {
        Ok(true)
    }
    fn stopped(&self, _: &str) -> bool {
        true
    }
    fn register(&self, token: &str, registration: Registration) -> chunk_control::Result<()> {
        let token = token.strip_prefix("Bearer ").unwrap_or_default();
        if registration.identity.host != HOST || !token.starts_with("machine/v1/test/jvm/runner-1/") {
            return Err(chunk_control::Error::Invalid("unknown machine"));
        }
        *self.0.lock().unwrap() = Some((token.into(), registration));
        Ok(())
    }
    fn connection(&self, id: &str) -> Option<RuntimeConnection> {
        let (token, registration) = self.0.lock().unwrap().clone().filter(|_| id == HOST)?;
        Some(RuntimeConnection {
            token,
            identity: registration.identity,
            player_endpoint: registration.player_endpoint,
        })
    }
    fn authenticate(&self, credential: &str) -> Option<String> {
        (credential == JVM).then(|| "host-1".into())
    }
}

fn launch() -> Launch {
    Launch {
        deployment: "test".into(),
        release: RELEASE.into(),
        app: "bridge".into(),
        profile: "small".into(),
        process_id: "process-1".into(),
        generation: 1,
        boot: None,
    }
}

/// A core whose `HOST` runs `RELEASE`, whose kept archive is two full chunks and a partial one, with `HOST`'s machine
/// credential and the archive's bytes.
async fn runner() -> (Fixture, String, Vec<u8>) {
    let fixture = Fixture::with_host(Arc::new(Remote::default())).await;
    let credential = Issuer::new("test", None, &fixture.cli).machine(MachineKind::Jvm, HOST);
    let archive: Vec<u8> = (0..2 * CHUNK + 7).map(|index| u8::try_from(index % 251).unwrap()).collect();
    let path = fixture.directory.path().join("release.tar.gz");
    std::fs::write(&path, &archive).unwrap();
    let kept = ReleaseArchive {
        path,
        sha256: auth::hex(&Sha256::digest(&archive)),
        size: u64::try_from(archive.len()).unwrap(),
    };
    fixture.archives.insert(RELEASE.into(), kept);
    fixture.control.record_launch(HOST, launch()).unwrap();
    (fixture, credential, archive)
}

impl Fixture {
    async fn runner_call(&self, credential: &str, method: &str, arguments: &impl Message) -> CallResponse {
        let message =
            CallRequest { method: method.into(), arguments: arguments.encode_to_vec(), ..CallRequest::default() };
        self.client.clone().call(authorized(message, credential)).await.unwrap().into_inner()
    }

    async fn launch(&self, credential: &str, boot: &str) -> CallResponse {
        self.runner_call(credential, "chunk:launch", &JvmBoot { boot: boot.into(), ..JvmBoot::default() }).await
    }

    async fn read(&self, credential: &str, boot: &str, offset: u64) -> CallResponse {
        self.runner_call(credential, "chunk:archive", &JvmArchiveRead { boot: boot.into(), offset }).await
    }

    /// `host`'s machine credential, once core launches `RELEASE`'s app on it too.
    fn another_host(&self, host: &str) -> String {
        self.control.record_launch(host, Launch { process_id: format!("{host}-process"), ..launch() }).unwrap();
        Issuer::new("test", None, &self.cli).machine(MachineKind::Jvm, host)
    }

    /// The AOT cache plan core gives the runner booted as `boot` with `RUNTIME`.
    async fn plan(&self, credential: &str, boot: &str) -> Option<Aot> {
        let arguments = JvmBoot { boot: boot.into(), runtime: RUNTIME.into() };
        result::<JvmLaunch>(&self.runner_call(credential, "chunk:launch", &arguments).await).aot
    }

    /// Uploads `cache` as `boot`, declaring `declared` as its size and digest, in chunks of `chunk` bytes, returning
    /// each response.
    async fn upload(
        &self,
        credential: &str,
        boot: &str,
        cache: &[u8],
        declared: (u64, &str),
        chunk: usize,
    ) -> Vec<CallResponse> {
        let mut responses = Vec::new();
        for (index, data) in cache.chunks(chunk).enumerate() {
            let write = JvmAotWrite {
                boot: boot.into(),
                offset: u64::try_from(index * chunk).unwrap(),
                data: data.to_vec(),
                size: declared.0,
                sha256: declared.1.into(),
                abandon: false,
            };
            responses.push(self.runner_call(credential, "chunk:aot-write", &write).await);
        }
        responses
    }
}

fn written(response: &CallResponse) -> bool {
    matches!(&response.outcome, Some(Outcome::Result(result)) if result.is_empty())
}

fn result<T: Message + Default>(response: &CallResponse) -> T {
    match &response.outcome {
        Some(Outcome::Result(result)) => T::decode(result.as_slice()).unwrap(),
        outcome => panic!("expected a result, got {outcome:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_boots_its_host_once_and_downloads_the_bound_release() {
    let (fixture, credential, archive) = runner().await;
    let launched = fixture.launch(&credential, "boot-1").await;
    let expected = JvmLaunch {
        deployment: "test".into(),
        release_id: RELEASE.into(),
        archive_size: u64::try_from(archive.len()).unwrap(),
        archive_sha256: auth::hex(&Sha256::digest(&archive)),
        app: "bridge".into(),
        profile: "small".into(),
        process_id: "process-1".into(),
        generation: 1,
        aot: None,
        environment_name: "prod".into(),
    };
    assert_eq!(result::<JvmLaunch>(&launched), expected);
    // A retry after a lost response gets the same launch; a machine that booted again is refused.
    assert_eq!(fixture.launch(&credential, "boot-1").await, launched);
    assert_eq!(code(&fixture.launch(&credential, "boot-2").await), Code::Denied);
    assert_eq!(code(&fixture.read(&credential, "boot-2", 0).await), Code::Denied);

    let mut downloaded = Vec::new();
    while downloaded.len() < archive.len() {
        let offset = u64::try_from(downloaded.len()).unwrap();
        let chunk = result::<JvmArchiveChunk>(&fixture.read(&credential, "boot-1", offset).await).data;
        assert_eq!(chunk.len(), CHUNK.min(archive.len() - downloaded.len()));
        downloaded.extend(chunk);
    }
    assert_eq!(auth::hex(&Sha256::digest(&downloaded)), expected.archive_sha256);
    for offset in [expected.archive_size, expected.archive_size + 1] {
        assert_eq!(code(&fixture.read(&credential, "boot-1", offset).await), Code::Invalid);
    }
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosts_boot_stays_bound_across_a_core_restart() {
    let (fixture, credential, _) = runner().await;
    let launched = fixture.launch(&credential, "boot-1").await;
    let kept = fixture.archives.get(RELEASE).unwrap();
    let fixture = fixture.restart(Arc::new(Remote::default())).await;
    // The restarted core keeps the archive again, and its runner host records the same launch.
    fixture.archives.insert(RELEASE.into(), kept);
    fixture.control.record_launch(HOST, launch()).unwrap();

    assert_eq!(fixture.launch(&credential, "boot-1").await, launched);
    result::<JvmArchiveChunk>(&fixture.read(&credential, "boot-1", 0).await);
    assert_eq!(code(&fixture.launch(&credential, "boot-2").await), Code::Denied);
    assert_eq!(code(&fixture.read(&credential, "boot-2", 0).await), Code::Denied);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosts_archive_read_holds_it_until_the_read_ends_even_if_its_caller_left() {
    let (fixture, credential, archive) = runner().await;
    result::<JvmLaunch>(&fixture.launch(&credential, "boot-1").await);
    let stalled = fixture.archives.stall.held.lock().await;

    let message = CallRequest {
        method: "chunk:archive".into(),
        arguments: JvmArchiveRead { boot: "boot-1".into(), offset: 0 }.encode_to_vec(),
        ..CallRequest::default()
    };
    let (mut client, request) = (fixture.client.clone(), authorized(message, &credential));
    let first = tokio::spawn(async move { client.call(request).await });
    // A read that got past the held one would stall too, so each gives up rather than hang.
    let second = || async {
        let read = tokio::time::timeout(Duration::from_secs(5), fixture.read(&credential, "boot-1", 0)).await;
        code(&read.expect("the read did not stall"))
    };
    let entered = tokio::time::timeout(Duration::from_secs(10), fixture.archives.stall.entered.notified()).await;
    entered.expect("the first read reached its blocking I/O");
    assert_eq!(second().await, Code::Overloaded);
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    // Gives core time to drop the cancelled call.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(second().await, Code::Overloaded);

    drop(stalled);
    let resumed = async {
        loop {
            let response = fixture.read(&credential, "boot-1", 0).await;
            if !matches!(&response.outcome, Some(Outcome::Error(error)) if error.code() == Code::Overloaded) {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let response = tokio::time::timeout(Duration::from_secs(10), resumed).await.expect("the host read again");
    assert_eq!(result::<JvmArchiveChunk>(&response).data, archive[..CHUNK]);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_hosts_jvm_machine_credential_launches_it_until_revoked() {
    let (fixture, credential, _) = runner().await;
    fixture.control.add_machine("gateway-1", MachineKind::Gateway).unwrap();
    let gateway = Issuer::new("test", None, &fixture.cli).machine(MachineKind::Gateway, "gateway-1");
    // `host-1` has a launch too, so its process credential is refused for what it is.
    fixture.control.record_launch("host-1", launch()).unwrap();
    for other in [fixture.gateway.as_str(), &gateway, &fixture.cli, JVM] {
        assert_eq!(code(&fixture.launch(other, "boot-1").await), Code::Denied);
        assert_eq!(code(&fixture.read(other, "boot-1", 0).await), Code::Denied);
    }
    result::<JvmLaunch>(&fixture.launch(&credential, "boot-1").await);

    fixture.control.revoke_machine(HOST, MachineKind::Jvm).unwrap();
    let message = CallRequest { method: "chunk:launch".into(), ..CallRequest::default() };
    let status = fixture.client.clone().call(authorized(message, &credential)).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvm_machine_credential_registers_and_follows_its_topic_until_revoked() {
    let (fixture, credential, _) = runner().await;
    // Over loopback, the JVM runs on core's machine, so it can't serve players at another machine's address.
    let foreign = JvmRegistration { player_endpoint: "10.0.0.9:25565".into(), ..registration() };
    assert_eq!(code(&fixture.runner_call(&credential, "chunk:register", &foreign).await), Code::Denied);
    let registered = fixture.runner_call(&credential, "chunk:register", &registration()).await;
    assert_eq!(result::<JvmRegistered>(&registered).host, HOST);

    let mut foreign = fixture.follow_jvm(&credential, "host-1").await;
    assert_eq!(next(&mut foreign).await.error.map(|error| error.code()), Some(Code::Denied));
    let mut updates = fixture.follow_jvm(&credential, HOST).await;
    let snapshot = next(&mut updates).await;
    assert!(snapshot.snapshot && snapshot.error.is_none());

    fixture.control.revoke_machine(HOST, MachineKind::Jvm).unwrap();
    assert_eq!(next(&mut updates).await.error.map(|error| error.code()), Some(Code::Stopped));
    drop((foreign, updates));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_host_records_each_aot_cache_and_later_hosts_use_it_once_it_checks_out() {
    let (fixture, credential, _) = runner().await;
    let cache: Vec<u8> = (0..2500).map(|index| u8::try_from(index % 251).unwrap()).collect();
    let (size, sha256) = (u64::try_from(cache.len()).unwrap(), auth::hex(&Sha256::digest(&cache)));
    let record = Some(Aot::Record(JvmAotRecord {}));
    assert_eq!(fixture.plan(&credential, "boot-1").await, record);
    // A retry after a lost response records again; another host of the same release, app and runtime doesn't.
    assert_eq!(fixture.plan(&credential, "boot-1").await, record);
    let other = fixture.another_host("runner-2");
    assert_eq!(fixture.plan(&other, "boot-2").await, None);
    // Only the recording host's bound boot writes.
    for (credential, boot) in [(&other, "boot-2"), (&credential, "boot-2")] {
        let responses = fixture.upload(credential, boot, &cache, (size, &sha256), 1000).await;
        assert_eq!(code(&responses[0]), Code::Denied);
    }

    // A cache that differs from its digest isn't installed, and ends the recording for good.
    let responses = fixture.upload(&credential, "boot-1", &cache, (size, &"0".repeat(64)), 1000).await;
    assert!(written(&responses[0]) && written(&responses[1]));
    assert_eq!(code(&responses[2]), Code::Invalid);
    assert_eq!(code(&fixture.upload(&credential, "boot-1", &cache, (size, &sha256), 1000).await[0]), Code::Denied);
    assert_eq!(fixture.plan(&credential, "boot-1").await, None);
    // The next host to launch records instead, and one over the size cap ends its recording too.
    let third = fixture.another_host("runner-3");
    assert_eq!(fixture.plan(&third, "boot-3").await, record);
    let oversized = fixture.upload(&third, "boot-3", &cache, (512 * 1024 * 1024 + 1, &sha256), 1000).await;
    assert_eq!(code(&oversized[0]), Code::Invalid);

    let fourth = fixture.another_host("runner-4");
    assert_eq!(fixture.plan(&fourth, "boot-4").await, record);
    let (first, rest) = cache.split_at(1000);
    assert!(written(&fixture.upload(&fourth, "boot-4", first, (size, &sha256), 1000).await[0]));
    // A repeat of the last chunk changes nothing.
    assert!(written(&fixture.upload(&fourth, "boot-4", first, (size, &sha256), 1000).await[0]));
    let rest = JvmAotWrite {
        boot: "boot-4".into(),
        offset: 1000,
        data: rest.to_vec(),
        size,
        sha256: sha256.clone(),
        abandon: false,
    };
    assert!(written(&fixture.runner_call(&fourth, "chunk:aot-write", &rest).await));
    let installed = fixture.directory.path().join("aot").join(RELEASE).join("bridge");
    let installed = installed.join(auth::hex(&Sha256::digest(RUNTIME)));
    assert_eq!(std::fs::read(installed).unwrap(), cache);

    // Now it exists, a host that launches uses it, and reads it as it reads the archive.
    let fifth = fixture.another_host("runner-5");
    assert_eq!(fixture.plan(&fifth, "boot-5").await, Some(Aot::Use(JvmAotUse { size, sha256 })));
    let read = |credential, boot| {
        let read = JvmArchiveRead { boot: String::from(boot), offset: 0 };
        let fixture = &fixture;
        async move { fixture.runner_call(credential, "chunk:aot-read", &read).await }
    };
    assert_eq!(result::<JvmArchiveChunk>(&read(&fifth, "boot-5").await).data, cache);
    assert_eq!(code(&read(&other, "boot-2").await), Code::Contract);
    fixture.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_release_wait_never_extends_the_next() {
    let directory = tempfile::tempdir().unwrap();
    let aot = crate::core::AotCaches::new(directory.path().to_owned());
    let record = Some(Aot::Record(JvmAotRecord {}));
    assert_eq!(aot.plan(HOST, "boot-1", RELEASE, "bridge", RUNTIME).await, record);
    // A release cancelled a minute into its wait for an upload that never comes, then tried again, waits only the rest.
    let start = tokio::time::Instant::now();
    assert!(tokio::time::timeout(Duration::from_secs(60), aot.settle(HOST)).await.is_err());
    aot.settle(HOST).await;
    assert_eq!(start.elapsed(), Duration::from_secs(90));
    // The released host never records again, and leaves the key to the next host.
    assert_eq!(aot.plan(HOST, "boot-1", RELEASE, "bridge", RUNTIME).await, None);
    assert_eq!(aot.plan("runner-2", "boot-2", RELEASE, "bridge", RUNTIME).await, record);
}
