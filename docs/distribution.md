# SDK distribution

The initial SDK targets Linux x64 on Ubuntu 24.04 or a compatible system with glibc and OpenSSL 3. It contains the
`chunk` CLI, native TypeScript compiler, Gradle wrapper, prebuilt JVM libraries, Gradle plugin markers, sources and API
documentation JARs, and `sdk.json`. Users need Java to run Gradle; projects select their gameplay JDK, with the current
Minestom adapter requiring Java 25. Building an application does not require Rust, Node, pnpm or the Chunk source
repository.

Each JVM library and the Gradle plugin publish `-sources.jar` and `-javadoc.jar` artifacts. The documentation JARs
contain Dokka HTML for Java and Kotlin; the Gradle plugin markers remain POM-only.

The CLI and JVM versions must match. `Cargo.toml` supplies the release version; packaging checks it against the Gradle
catalog and the prepared CLI. `sdk.json` carries that version and the Kotlin/Foojay versions for project tooling. All
Maven dependencies use explicit versions; the repository does not publish mutable version indexes or support snapshots
and dynamic versions.

## Build and verify before publishing

```sh
just package-cli
python3 scripts/check-sdk.py target/dist/chunk-0.1.0-linux-x64.tar.gz
```

Packaging refuses to replace an existing archive. Use a fresh `--output DIRECTORY` with `scripts/package-sdk.py` when
assembling another candidate of the same version. The script accepts `--chunk PATH` for a prepared CLI. It publishes the
JVM modules and all three plugin markers to a fresh local Maven repository, then creates the archive and its SHA-256
checksum. It does not publish externally.

The verification script installs into a temporary prefix and builds the real Java and Kotlin example sources against the
archive's Maven repository served over HTTP. Neither consumer includes the Chunk build or accesses its source tree. It
checks the sources and documentation artifacts, resulting executable JARs and release archives, then removes the
installation and stops the HTTP server.

The `SDK distribution` workflow builds on a 2-vCPU Blacksmith runner, then verifies the archive in a separate Ubuntu
24.04 container with Java and only the consumer fixtures. The container has no build-machine Cargo cache or Chunk
implementation sources. Publishing requires this check to pass. Pull requests that change distribution inputs run it
automatically. A manual run with `publish` disabled produces downloadable workflow artifacts without creating a release
or uploading to R2.

## Install

Before a public release, download the archive, checksum and installer from the workflow artifacts, or use a local build:

```sh
sh scripts/install.sh 0.1.0 target/dist/chunk-0.1.0-linux-x64.tar.gz
export PATH="$HOME/.local/bin:$PATH"
chunk --version
```

After that version is publicly released:

```sh
curl --fail --location https://github.com/chunkzero/chunk/releases/download/v0.1.0/install.sh -o install.sh
sh install.sh 0.1.0
```

The installer verifies the archive checksum and CLI version, stores the complete SDK under
`~/.local/share/chunk/VERSION`, and points `~/.local/bin/chunk` to it. `CHUNK_INSTALL_DIR` overrides the `~/.local`
prefix. It refuses to replace an existing version or an unrelated executable. Keep the SDK directory together: the CLI
finds its native type checker next to its executable.

For a consumer build before publication, use `file:///absolute/path/to/SDK/sdk/maven` in both Gradle's
`pluginManagement.repositories` and `dependencyResolutionManagement.repositories`, alongside the plugin portal and Maven
Central. After publication, use `https://maven.chunkzero.com` in both places. Pin the Chunk settings plugin to the SDK
version; it resolves matching runtime libraries. The [Gradle plugin guide](../jvm/gradle-plugin/README.md) shows the
complete repository declarations. Project creation is handled separately from SDK distribution.

## Configure publishing once

1. Enable R2 in the Cloudflare dashboard and create a dedicated bucket, such as `chunk-maven`.
2. Connect the bucket's custom domain to `maven.chunkzero.com` in the `chunkzero.com` zone. The public domain serves the
   bucket root as the Maven repository. No Worker or registry server is required.
3. Create an R2 S3 access key restricted to object read/write on that bucket.
4. Create the GitHub environment `sdk-release`, restrict it to `main`, and configure its release reviewer. Add
   environment variables `CLOUDFLARE_ACCOUNT_ID` and `R2_MAVEN_BUCKET`, plus secrets `R2_ACCESS_KEY_ID` and
   `R2_SECRET_ACCESS_KEY`.

Wrangler's local OAuth login does not provide credentials to GitHub Actions. The workflow uses the runner's AWS CLI to
upload through R2's S3 API. See Cloudflare's
[custom domain](https://developers.cloudflare.com/r2/buckets/public-buckets/) and
[S3 credentials](https://developers.cloudflare.com/r2/api/tokens/) instructions.

## Publish a version

Update the Cargo workspace version and Gradle catalog together and merge the release changes. Run `SDK distribution`
from `main` with `publish` enabled when ready to make the SDK public. The workflow verifies the package, prepares a
draft GitHub release, uploads the versioned Maven files, checks the public Maven endpoint, and then publishes the CLI
release. It uses the exact archive that passed consumer verification.

Maven uploads compare SHA-256 metadata before writing and use conditional creation to prevent overwriting existing
objects. Identical files are skipped on retry; different bytes at an existing version fail. Publication failures leave
the GitHub release in draft. Re-run the failed publishing job with the same workflow artifact to finish a partial
upload; do not rebuild or reuse a published version for different artifacts. The workflow never deletes Maven objects.
