set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Format repository sources, configuration and documentation.
fmt:
    cargo fmt --all
    ktlint --format "jvm/**/*.kt" "jvm/**/*.kts" "buildSrc/**/*.kt" "buildSrc/**/*.kts" "examples/**/*.kt" "examples/**/*.kts" "crates/chunk-cli/templates/**/*.kt" "crates/chunk-cli/templates/**/*.kts" "*.kts" "!**/build/**" "!**/.chunk/**"
    buf format --write proto
    git ls-files -z --cached --others --exclude-standard -- '*.java' | xargs -0 google-java-format --aosp --replace
    pnpm fmt
    just --unstable --fmt

# Verify formatting without modifying files.
fmt-check:
    cargo fmt --all --check
    ktlint "jvm/**/*.kt" "jvm/**/*.kts" "buildSrc/**/*.kt" "buildSrc/**/*.kts" "examples/**/*.kt" "examples/**/*.kts" "crates/chunk-cli/templates/**/*.kt" "crates/chunk-cli/templates/**/*.kts" "*.kts" "!**/build/**" "!**/.chunk/**"
    buf format --diff --exit-code proto
    git ls-files -z --cached --others --exclude-standard -- '*.java' | xargs -0 google-java-format --aosp --dry-run --set-exit-if-changed
    pnpm fmt:check
    just --unstable --fmt --check

# Run linters.
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    buf lint proto
    pnpm lint

# Type-check the embedded TypeScript SDK.
typecheck:
    pnpm install --frozen-lockfile
    pnpm typecheck

# Run the test suites.
test: toolchain
    pnpm test
    cargo test --workspace
    cargo test -p chunk-proxy --no-default-features
    ./gradlew test
    examples/local/gradlew test

# Build everything.
build: toolchain
    cargo build --workspace
    ./gradlew assemble
    examples/local/gradlew assemble

# Everything CI runs. Run before opening a PR.
ready: fmt-check lint typecheck test build consumers

# Run an opt-in local workload benchmark (see crates/chunk-bench/README.md).
bench *args:
    cargo run --release --locked -p chunk-bench -- {{ args }}

# Create Java/Kotlin projects and build consumers from source-only scratch copies.
consumers: toolchain
    python3 scripts/check-consumers.py target/debug/chunk

# Build and run the complete local example. Ctrl-C stops its services and gameplay JVMs.
local *args: toolchain
    cargo run -p chunk-cli -- dev examples/local {{ args }}

# Operate on players connected to the local example.
players *args:
    cargo run -p chunk-cli -- players --control-file examples/local/.chunk/local/control.json {{ args }}

# Build the development CLI and install its pinned native TypeScript toolchain.
toolchain:
    cargo build -p chunk-cli
    pnpm install --frozen-lockfile
    node scripts/install-typescript.mjs

# Build a Linux x64 CLI SDK archive and separate JVM publications.
package-cli:
    pnpm install --frozen-lockfile
    cargo build --release --locked -p chunk-cli
    python3 scripts/package-sdk.py

# Build the environment container image with podman or docker, tagged with the workspace version.
image:
    #!/usr/bin/env bash
    set -euo pipefail
    engine=$(command -v podman || command -v docker)
    version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
    "$engine" build -f crates/chunk-environment/Dockerfile -t "chunk-environment:$version" \
        --build-arg VERSION="$version" --build-arg REVISION="$(git rev-parse HEAD)" .

# Build the edge image with podman or docker, tagged with the workspace version.
edge-image:
    #!/usr/bin/env bash
    set -euo pipefail
    engine=$(command -v podman || command -v docker)
    version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
    "$engine" build -f crates/chunk-edge/Dockerfile -t "chunk-edge:$version" \
        --build-arg VERSION="$version" --build-arg REVISION="$(git rev-parse HEAD)" .

# Build the management image, dashboard included, with podman or docker, tagged with the workspace version.
management-image:
    #!/usr/bin/env bash
    set -euo pipefail
    engine=$(command -v podman || command -v docker)
    version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
    "$engine" build -f packages/management/Dockerfile -t "chunk-management:$version" \
        --build-arg VERSION="$version" --build-arg REVISION="$(git rev-parse HEAD)" .

# Validate the self-hosting compose bundle against a throwaway `init.sh` environment.
compose-check:
    #!/usr/bin/env bash
    set -euo pipefail
    env_dir=$(mktemp -d)
    trap 'rm -rf "$env_dir"' EXIT
    deploy/compose/init.sh "$env_dir/.env" >/dev/null
    if command -v docker >/dev/null; then compose=(docker compose); else compose=(podman compose); fi
    "${compose[@]}" --env-file "$env_dir/.env" -f deploy/compose/compose.yaml config --quiet

# Build the remote JVM runner image on a Java `java` runtime with podman or docker, tagged `chunk-jvm:<java>`.
jvm-image java="25":
    #!/usr/bin/env bash
    set -euo pipefail
    engine=$(command -v podman || command -v docker)
    version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
    "$engine" build -f crates/chunk-jvm/Dockerfile -t "chunk-jvm:{{ java }}" --build-arg JAVA_VERSION="{{ java }}" \
        --build-arg VERSION="$version" --build-arg REVISION="$(git rev-parse HEAD)" .

# Run a managed core whose JVM runs in the chunk-jvm image under podman, from a release of the local example.
jvm-e2e: toolchain (jvm-image "25")
    rm -rf target/jvm-e2e
    target/debug/chunk build examples/local --output target/jvm-e2e
    CHUNK_E2E_IMAGE=chunk-jvm:25 CHUNK_E2E_RELEASE="$(realpath target/jvm-e2e/*.tar.gz)" \
        cargo test -p chunk-environment --lib runner_image -- --ignored --nocapture
