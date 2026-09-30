//! The runner's configuration, read from its environment.

use crate::Failure;
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

pub(crate) struct Config {
    /// Core's endpoint, `http://<address>:<port>` at a private address.
    pub endpoint: String,
    pub core: SocketAddr,
    /// The machine credential core minted for this host.
    pub credential: String,
    /// The host the credential names.
    pub host: String,
    pub cache: PathBuf,
    /// Where players reach the JVM; detected from the route to core when unset.
    pub player_address: Option<IpAddr>,
    /// How long the JVM may take to exit after SIGTERM or SIGINT before it is killed.
    pub stop_grace: Duration,
    pub java_home: PathBuf,
    /// What the machine's environment expects core to launch.
    pub expected: Expected,
    /// How long calls to core are retried while it is unavailable.
    pub retry: Duration,
    /// The proc filesystem, whose cgroup membership, mounts and memory report bound the heap.
    pub proc: PathBuf,
    /// Where the JVM's working directory is created.
    pub work_root: PathBuf,
    /// The CPUs this process may run on.
    pub cpus: usize,
}

#[derive(Default)]
pub(crate) struct Expected {
    pub release: Option<String>,
    pub app: Option<String>,
    pub profile: Option<String>,
}

impl Config {
    /// Reads the configuration through `var`, which treats empty values as unset.
    pub fn load(var: impl Fn(&str) -> Option<String>) -> Result<Self, Failure> {
        let var = |name: &str| var(name).filter(|value| !value.is_empty());
        let required = |name: &str| var(name).ok_or_else(|| Failure::env(format!("{name} is required")));
        let endpoint = required("CHUNK_CORE_ENDPOINT")?;
        let core = endpoint
            .strip_prefix("http://")
            .and_then(|address| address.parse::<SocketAddr>().ok())
            .filter(|address| chunk_service::net::private(address.ip()))
            .ok_or_else(|| Failure::env("CHUNK_CORE_ENDPOINT must be http://<address>:<port> at a private address"))?;
        let credential = required("CHUNK_JVM_CREDENTIAL")?;
        let environment = required("CHUNK_ENVIRONMENT_ID")?;
        let host = host(&credential, &environment)?;
        let player_address = match var("CHUNK_PLAYER_ADDRESS") {
            Some(value) => Some(
                value
                    .parse::<IpAddr>()
                    .ok()
                    .filter(|address| chunk_service::net::private(*address))
                    .ok_or_else(|| Failure::env("CHUNK_PLAYER_ADDRESS must be a loopback or private IP literal"))?,
            ),
            None => None,
        };
        let stop_grace = match var("CHUNK_STOP_GRACE") {
            Some(value) => Duration::from_secs(
                value.parse().map_err(|_| Failure::env("CHUNK_STOP_GRACE must be a whole number of seconds"))?,
            ),
            None => Duration::from_secs(10),
        };
        Ok(Self {
            endpoint,
            core,
            credential,
            host,
            cache: var("CHUNK_CACHE").unwrap_or_else(|| "/var/cache/chunk".into()).into(),
            player_address,
            stop_grace,
            java_home: required("JAVA_HOME")?.into(),
            expected: Expected {
                release: var("CHUNK_RELEASE_ID"),
                app: var("CHUNK_APP_ID"),
                profile: var("CHUNK_MACHINE_PROFILE"),
            },
            retry: Duration::from_secs(120),
            proc: "/proc".into(),
            work_root: std::env::temp_dir(),
            cpus: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
        })
    }
}

/// The host a JVM machine credential, `machine/v1/<environment>/jvm/<host>/<mac>`, names, once it names
/// `environment`. Core decides whether the credential holds.
fn host(credential: &str, environment: &str) -> Result<String, Failure> {
    let invalid = || Failure::env("CHUNK_JVM_CREDENTIAL is not a JVM machine credential");
    if !credential.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid());
    }
    let (scope, _) = credential.rsplit_once('/').ok_or_else(invalid)?;
    let (scope, id) = scope.strip_prefix("machine/v1/").and_then(|scope| scope.rsplit_once('/')).ok_or_else(invalid)?;
    let named = scope.strip_suffix("/jvm").filter(|_| !id.is_empty()).ok_or_else(invalid)?;
    if named != environment {
        return Err(Failure::env(format!(
            "the JVM credential belongs to environment {named:?}, not CHUNK_ENVIRONMENT_ID {environment:?}"
        )));
    }
    Ok(id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn load(vars: &[(&str, &str)]) -> Result<Config, Failure> {
        let vars: BTreeMap<_, _> = vars.iter().map(|(name, value)| ((*name).to_owned(), (*value).to_owned())).collect();
        Config::load(|name| vars.get(name).cloned())
    }

    const VALID: [(&str, &str); 4] = [
        ("CHUNK_CORE_ENDPOINT", "http://10.0.0.1:7000"),
        ("CHUNK_JVM_CREDENTIAL", "machine/v1/env/jvm/host-1/mac"),
        ("CHUNK_ENVIRONMENT_ID", "env"),
        ("JAVA_HOME", "/opt/java"),
    ];

    #[test]
    fn a_valid_environment_names_the_credentials_host_and_defaults_the_rest() {
        let config = load(&VALID).unwrap();
        assert_eq!(config.host, "host-1");
        assert_eq!(config.cache, PathBuf::from("/var/cache/chunk"));
        assert_eq!(config.stop_grace, Duration::from_secs(10));
        assert!(config.player_address.is_none());
    }

    #[test]
    fn invalid_environments_are_refused_with_64() {
        for (name, value) in [
            ("CHUNK_CORE_ENDPOINT", "http://8.8.8.8:7000"),
            ("CHUNK_CORE_ENDPOINT", "10.0.0.1:7000"),
            ("CHUNK_JVM_CREDENTIAL", "machine/v1/other/jvm/host-1/mac"),
            ("CHUNK_JVM_CREDENTIAL", "machine/v1/env/gateway/host-1/mac"),
            ("CHUNK_JVM_CREDENTIAL", "machine/v1/env/jvm//mac"),
            ("CHUNK_PLAYER_ADDRESS", "203.0.113.1"),
            ("CHUNK_STOP_GRACE", "10s"),
            ("JAVA_HOME", ""),
        ] {
            let mut vars = VALID.to_vec();
            vars.retain(|(existing, _)| *existing != name);
            vars.push((name, value));
            assert_eq!(load(&vars).err().map(|failure| failure.code), Some(64), "{name}={value}");
        }
    }
}
