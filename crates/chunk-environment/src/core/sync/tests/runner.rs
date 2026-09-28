//! A remote runner's `chunk:launch` and `chunk:archive`, and the JVM machine credential it and its JVM present.

use super::{jvm::registration, *};
use crate::core::ReleaseArchive;
use chunk_control::{Launch, MachineKind, Progress, Registration, RuntimeConnection};
use chunk_proto::sync::v1::{JvmArchiveChunk, JvmArchiveRead, JvmBoot, JvmLaunch, JvmRegistered, JvmRegistration};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

const HOST: &str = "runner-1";
const RELEASE: &str = "release-1";
const CHUNK: usize = 4 * 1024 * 1024;

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
    fixture.control.add_machine(HOST, MachineKind::Jvm).unwrap();
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
        // A full chunk and its framing exceed tonic's default 4 MiB, but not the protocol's message limit.
        let mut client = self.client.clone().max_decoding_message_size(MESSAGE_BYTES);
        client.call(authorized(message, credential)).await.unwrap().into_inner()
    }

    async fn launch(&self, credential: &str, boot: &str) -> CallResponse {
        self.runner_call(credential, "chunk:launch", &JvmBoot { boot: boot.into() }).await
    }

    async fn read(&self, credential: &str, boot: &str, offset: u64) -> CallResponse {
        self.runner_call(credential, "chunk:archive", &JvmArchiveRead { boot: boot.into(), offset }).await
    }
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
        assert!(!chunk.is_empty() && chunk.len() <= CHUNK);
        downloaded.extend(chunk);
    }
    assert_eq!(auth::hex(&Sha256::digest(&downloaded)), expected.archive_sha256);
    for offset in [expected.archive_size, expected.archive_size + 1] {
        assert_eq!(code(&fixture.read(&credential, "boot-1", offset).await), Code::Invalid);
    }
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
