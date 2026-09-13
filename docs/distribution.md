# SDK distribution

The initial SDK targets Linux x64 on Ubuntu 24.04 or a compatible system with glibc and OpenSSL 3. It contains the
`chunk` CLI, native TypeScript compiler and license. The CLI embeds the version pins and standard Gradle wrapper files
used to create projects. Gradle downloads the matching JVM libraries, plugin markers, sources and API documentation from
`maven.chunkzero.com`; those artifacts are not bundled in the CLI archive. Users need Java to run Gradle; projects
select their gameplay JDK, with the current Minestom adapter requiring Java 25. Building an application does not require
Rust, Node, pnpm or the Chunk source repository.

Each JVM library and the Gradle plugin publish `-sources.jar` and `-javadoc.jar` artifacts. The documentation JARs
contain Dokka HTML for Java and Kotlin; the Gradle plugin markers remain POM-only.

The CLI and JVM versions must match. `Cargo.toml` supplies the release version; packaging checks it against the Gradle
catalog and the prepared CLI. The CLI embeds that catalog for the Chunk, Kotlin and Foojay project version pins. All
Maven dependencies use explicit versions; the repository does not publish mutable version indexes or support snapshots
and dynamic versions.

Gradle generates the repository's checked-in wrapper files. When updating the pinned Gradle version, regenerate them
with Gradle's `wrapper` task and rebuild the CLI. Project creation copies the embedded files without invoking Gradle or
requiring Java. The project's wrapper downloads and caches the selected Gradle distribution on its first build.

## Build and verify before publishing

```sh
just package-cli
python3 scripts/check-sdk.py target/dist/chunk-0.1.0-linux-x64.tar.gz target/dist/maven
```

Packaging refuses to replace existing outputs. Use a fresh `--output DIRECTORY` with `scripts/package-sdk.py` when
assembling another candidate of the same version. The script accepts `--chunk PATH` for a prepared CLI. It publishes the
JVM modules and all three plugin markers to `maven/` beside the CLI archive and its SHA-256 checksum. The workflow keeps
these Maven files as separate publication inputs; they are never installed with the CLI. Packaging does not publish
externally.

The verification script installs into a temporary prefix and runs `create`, `codegen` and `build` for Java and Kotlin.
It serves the unpublished Maven artifacts over HTTP and explicitly overrides the test projects' repository. Neither
consumer includes the Chunk build or accesses its source tree. It checks sources and documentation artifacts, resulting
executable JARs and release archives, then removes the installation and stops the HTTP server.

The `SDK distribution` workflow builds on a 2-vCPU Blacksmith runner, then verifies the archive in a separate Ubuntu
24.04 container with Java and only the verification scripts. The container has no build-machine Cargo cache or Chunk
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

The installer verifies the archive checksum and CLI version, stores the CLI and native compiler under
`~/.local/share/chunk/VERSION`, and points `~/.local/bin/chunk` to it. `CHUNK_INSTALL_DIR` overrides the `~/.local`
prefix. It refuses to replace an existing version or an unrelated executable. Keep the installation together: the CLI
finds its native type checker next to its executable.

Project support is generated inside the project: `.chunk/sdk/` contains the TypeScript SDK, `.chunk/generated/` contains
its typed bindings, and `.chunk/gradle/` contains Gradle build support. These directories are ignored by Git and
recreated by the CLI and Gradle plugin. The standard `gradlew`, `gradlew.bat` and `gradle/wrapper/` files are created
with the project and should be committed. Gradle manages its own distribution and dependency caches in
`GRADLE_USER_HOME` (normally `~/.gradle`).

Run `chunk create my-server` to generate a Kotlin project, or add `--language java`. The generated Gradle settings pin
the SDK version and use `https://maven.chunkzero.com`. That version's JVM artifacts must be published before a normal
consumer build can resolve them. For local validation of an unpublished candidate, set the generated project's
`chunk.mavenRepository` Gradle property to `file:///absolute/path/to/target/dist/maven`. This uses the separate
publication output; it does not require rebuilding the framework. The
[Gradle plugin guide](../jvm/gradle-plugin/README.md) shows the complete repository declarations.

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
