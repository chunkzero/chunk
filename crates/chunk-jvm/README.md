# chunk-jvm

The runner on a JVM machine: the main process of the `chunk-jvm` image, which
[management](../../packages/management/README.md) starts when core asks for JVM capacity. It asks core what its host
runs (`chunk:launch`), downloads that release's archive from core (`chunk:archive`), checks its size and SHA-256, and
unpacks and verifies it with [`chunk-build`](../chunk-build/README.md). It then starts the app's JVM as a child process
and stays in front of it: it forwards SIGTERM, SIGINT and SIGQUIT, sends SIGKILL once the stop grace runs out after
SIGTERM or SIGINT, and reaps orphaned processes when it runs as PID 1.

Each run of the runner makes up a new boot ID, and core binds the host to the first boot it sees. A stopped machine is
replaced, not restarted: a restarted runner exits with code 77.

## Configuration

| Variable                                                    | Required | Meaning                                                                                                         |
| ----------------------------------------------------------- | -------- | --------------------------------------------------------------------------------------------------------------- |
| `CHUNK_CORE_ENDPOINT`                                       | yes      | Core, as `http://<private IP>:<port>`.                                                                          |
| `CHUNK_JVM_CREDENTIAL`                                      | yes      | The machine credential core minted for this host, `machine/v1/<environment>/jvm/<host>/<mac>`.                  |
| `CHUNK_ENVIRONMENT_ID`                                      | yes      | The environment, which the credential must name.                                                                |
| `JAVA_HOME`                                                 | yes      | The Java that runs the app; `$JAVA_HOME/release` gives its version.                                             |
| `CHUNK_CACHE`                                               | no       | Where verified releases (`releases/<release id>`) and AOT caches (`aot/`) are kept; default `/var/cache/chunk`. |
| `CHUNK_PLAYER_ADDRESS`                                      | no       | The private IP players reach the JVM at; default: the local address of the connection to core.                  |
| `CHUNK_STOP_GRACE`                                          | no       | Whole seconds the JVM gets to exit after SIGTERM or SIGINT; default 10.                                         |
| `CHUNK_RELEASE_ID`, `CHUNK_APP_ID`, `CHUNK_MACHINE_PROFILE` | no       | Cross-checks: the runner exits if core launches something else.                                                 |

Empty values count as unset. Cached releases and AOT caches are verified again before reuse.

The JVM runs as `java -Xmx<heap>m -XX:+UseG1GC -XX:+ExitOnOutOfMemoryError [AOT flags] -jar <app jar>` in a fresh
working directory under `/tmp`. The heap is the lowest of the cgroup v2 memory limit, the machine's memory and the
launch profile's memory, less 200 MiB and a tenth of that memory for everything outside the heap. Besides the runner's
own environment, the JVM gets what the Java runtime reads: `CHUNK_PROCESS_TOKEN` (the machine credential),
`CHUNK_DEPLOYMENT`, `CHUNK_CORE_ENDPOINT`, `CHUNK_PROCESS_ID`, `CHUNK_PROCESS_GENERATION`, `CHUNK_MACHINE_PROFILE`,
`CHUNK_APP_ID`, `CHUNK_ARTIFACT_DIGEST` and `CHUNK_PLAYER_ADDRESS`.

## AOT cache

The runner reports its Java runtime to core in `chunk:launch` (`IMPLEMENTOR`, `JAVA_RUNTIME_VERSION` and `OS_ARCH` from
`$JAVA_HOME/release`). Core keeps one Leyden AOT cache per release, app and runtime, and its answer says what to do:

- **Use:** the runner downloads the cache (`chunk:aot-read`), checks its size and SHA-256, keeps it at
  `$CHUNK_CACHE/aot/<release id>/<app>.aot`, and adds `-XX:AOTCache=<file>`. A kept cache that still matches is used
  without a download. If the download fails or takes over 20 seconds, the JVM runs without a cache.
- **Record:** no cache exists yet, and core picked this host to make it. The JVM runs with
  `-XX:AOTMode=record -XX:AOTConfiguration=<file>`. Once it exits cleanly (0, or on a forwarded SIGTERM or SIGINT), the
  runner creates the cache with `-XX:AOTMode=create`, for up to 45 seconds, and uploads it (`chunk:aot-write`) for up to
  30 seconds more; core holds the host's release for up to 90 seconds meanwhile. If anything fails, the runner tells
  core no cache came, and another host may record it. A machine with less than 768 MiB of memory doesn't record, since
  creating the cache needs a few hundred MiB beyond the heap, but it still uses a cache a larger machine made.

The cache never changes the runner's exit code. Java checks the JAR's path, size and modification time against the
cache; unpacked releases keep the archive's zero timestamps under the same `CHUNK_CACHE`, so a cache fits every machine
on the same image.

## Exit codes

| Code        | Meaning                                                                                                |
| ----------- | ------------------------------------------------------------------------------------------------------ |
| 0           | The JVM exited cleanly, or stopped on a forwarded SIGTERM or SIGINT, or one arrived before it started. |
| Java's code | The JVM's own exit code, or 128 plus the signal that ended it (137 after the grace's SIGKILL).         |
| 64          | Bad configuration, or too little memory for a heap.                                                    |
| 65          | The archive or app failed verification, or core launched something the cross-checks don't allow.       |
| 69          | Core stayed unreachable or silent for 2 minutes, or failed the request.                                |
| 74          | A local I/O error, such as an unwritable cache.                                                        |
| 77          | Core rejected the credential, refused this boot, or has nothing for the host to run. Don't restart.    |
| 78          | The image's Java is older than the release requires, or can't be run.                                  |

Any exit other than the JVM's prints one JSON line on stderr: `{"level":"error","code":<code>,"message":"..."}`.

## Image

`just jvm-image` builds `chunk-jvm:25` from `crates/chunk-jvm/Dockerfile`; `just jvm-image <java>` builds on another
Java version. The image is `eclipse-temurin:<java>-jre` with the runner as its entrypoint, so the runner is PID 1. It
runs as uid 65532 and keeps its cache in the `/var/cache/chunk` volume. Management picks the image for each release's
Java version through `CHUNK_JVM_IMAGE`.

## Testing

`just jvm-e2e` checks the image end to end with Podman. It builds a release of `examples/local` and starts a managed
core on this machine's LAN address with a fake management service. A claim makes core launch a host whose runner runs in
`podman run --network host`, recording the AOT cache; stopping that node uploads the cache, and the next host's JVM
starts with it. The test removes the `chunk-jvm-e2e-*` containers it started, even when it fails.
