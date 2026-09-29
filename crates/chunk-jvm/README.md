# chunk-jvm

The main process of a remote JVM machine. It asks core what its host runs (`chunk:launch`), downloads that release's
archive from core (`chunk:archive`), checks the archive's size and SHA-256, and unpacks and verifies it with
`chunk-build`. It then starts the app's JVM as a child process. The runner stays in front of the JVM: it forwards
SIGTERM, SIGINT and SIGQUIT, sends SIGKILL once the stop grace runs out after SIGTERM or SIGINT, and reaps orphaned
processes when it runs as PID 1.

Each process run makes up a new boot ID, and core binds the host to the first boot it sees. After a full stop, the
machine must be replaced; restarting the runner gets exit code 77.

## AOT cache

The runner reports its Java runtime to core with `chunk:launch`: `IMPLEMENTOR`, `JAVA_RUNTIME_VERSION` and `OS_ARCH`
from `$JAVA_HOME/release`. Core keeps one Leyden AOT cache per release, app and runtime, and its answer tells the runner
what to do about it:

- **Use:** the runner downloads the cache (`chunk:aot-read`), checks its size and SHA-256, keeps it at
  `$CHUNK_CACHE/aot/<release_id>/<app>.aot`, and adds `-XX:AOTCache=<file>`. A kept cache that still matches is used
  without a download. If the download fails, the JVM runs without a cache.
- **Record:** no cache exists yet, and core picked this host to make it. The JVM runs with
  `-XX:AOTMode=record -XX:AOTConfiguration=<file>`, the file in a fresh directory under `/tmp`. Once the JVM exits
  cleanly (0, or on a forwarded SIGTERM or SIGINT), the runner runs Java again with the same flags and JAR plus
  `-XX:AOTMode=create -XX:AOTConfiguration=<file> -XX:AOTCache=<cache>`, for up to 60 seconds, and uploads the cache
  (`chunk:aot-write`) for up to 60 seconds more. Core holds the host's release until that upload ends, for up to 90
  seconds. If anything fails, or a SIGTERM or SIGINT arrives meanwhile, the runner tells core that no cache came, and
  another host may record it.

The cache never changes the runner's exit code. Java checks the JAR's path, size and mtime against the cache and runs
without one that doesn't fit; the unpacked release keeps the archive's zero mtimes and the default `CHUNK_CACHE`, so a
cache fits every machine on the same image.

## Environment

| Variable                                                    | Required | Meaning                                                                                                                                                        |
| ----------------------------------------------------------- | -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `CHUNK_CORE_ENDPOINT`                                       | yes      | Core, as `http://<address>:<port>` at a private address                                                                                                        |
| `CHUNK_JVM_CREDENTIAL`                                      | yes      | The machine credential core minted for this host, `machine/v1/<environment>/jvm/<host>/<mac>`                                                                  |
| `CHUNK_ENVIRONMENT_ID`                                      | yes      | The environment, which the credential must name                                                                                                                |
| `JAVA_HOME`                                                 | yes      | The image's Java; `$JAVA_HOME/release` gives its major version and `$JAVA_HOME/bin/java` runs the app                                                          |
| `CHUNK_CACHE`                                               | no       | Where verified releases are kept, under `releases/<release_id>`, and AOT caches, under `aot/`; default `/var/cache/chunk`. Each is verified again before reuse |
| `CHUNK_PLAYER_ADDRESS`                                      | no       | The private IP players reach the JVM at; default: the local address of a connection to core                                                                    |
| `CHUNK_STOP_GRACE`                                          | no       | Whole seconds the JVM gets to exit after SIGTERM or SIGINT; default 10                                                                                         |
| `CHUNK_RELEASE_ID`, `CHUNK_APP_ID`, `CHUNK_MACHINE_PROFILE` | no       | Cross-checks: the runner stops if core launches something else                                                                                                 |

The JVM runs as `java -Xmx<heap>m -XX:+UseG1GC -XX:+ExitOnOutOfMemoryError [AOT flags] -jar <app jar>` in a fresh
working directory under `/tmp`. The heap is the lower of the lowest cgroup v2 `memory.max` from the runner's cgroup up
to the hierarchy's mount, the machine's `MemTotal` and the launch profile's memory, less 200 MiB and a tenth of that
memory for everything outside the heap. The JVM gets the environment `RuntimeEnvironment` reads: `CHUNK_PROCESS_TOKEN`
(the machine credential), `CHUNK_DEPLOYMENT`, `CHUNK_CORE_ENDPOINT`, `CHUNK_PROCESS_ID`, `CHUNK_PROCESS_GENERATION`,
`CHUNK_MACHINE_PROFILE`, `CHUNK_APP_ID`, `CHUNK_ARTIFACT_DIGEST` and `CHUNK_PLAYER_ADDRESS`.

## Exit codes

| Code        | Meaning                                                                                               |
| ----------- | ----------------------------------------------------------------------------------------------------- |
| 0           | The JVM exited cleanly, or stopped on a forwarded SIGTERM or SIGINT, or one arrived before it started |
| Java's code | The JVM's own exit code, or 128 plus the signal that ended it (137 after the grace's SIGKILL)         |
| 64          | Bad environment, or too little memory for a heap                                                      |
| 65          | The archive, the cached release or the app JAR failed verification                                    |
| 69          | Core stayed unreachable or silent for 2 minutes, or failed the request                                |
| 74          | A local I/O error, such as an unwritable cache                                                        |
| 77          | Core rejected the credential or refused the boot; permanent, so don't restart                         |
| 78          | The image's Java is older than the release's, or can't be run                                         |

Any exit other than the JVM's prints one JSON line on stderr: `{"level":"error","code":<code>,"message":"..."}`.

## Image

`just jvm-image` builds `chunk-jvm:25` from `crates/chunk-jvm/Dockerfile`; `just jvm-image <java>` builds on another
Java major version. The image is `eclipse-temurin:<java>-jre` with the runner as its entrypoint, so the runner is PID 1.
It runs as uid 65532 and keeps its cache in the `/var/cache/chunk` volume. Started without its environment, it exits 64.

`just jvm-e2e` checks the image end to end with podman. It builds a release of `examples/local` and starts a managed
core on this machine's LAN address with a fake management service. A claim makes core launch a host whose runner runs in
`podman run --network host` and whose JVM serves players at that address, recording the AOT cache. Stopping that node
makes its runner upload the cache and exit 0, and the next host's JVM starts with the cache. Stopping core stops that
JVM, and its container exits 0. The test removes the `chunk-jvm-e2e-*` containers it started, even when it fails.
