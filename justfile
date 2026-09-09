set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Format Rust, Kotlin and protobuf sources.
fmt:
    cargo fmt --all
    ktlint --format "jvm/**/*.kt" "jvm/**/*.kts" "buildSrc/**/*.kt" "buildSrc/**/*.kts" "*.kts" "!**/build/**"
    buf format --write proto

# Verify formatting without modifying files.
fmt-check:
    cargo fmt --all --check
    ktlint "jvm/**/*.kt" "jvm/**/*.kts" "buildSrc/**/*.kt" "buildSrc/**/*.kts" "*.kts" "!**/build/**"
    buf format --diff --exit-code proto

# Run linters.
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    buf lint proto

# Type-check the TypeScript packages.
typecheck:
    pnpm install --frozen-lockfile
    pnpm typecheck

# Run the test suites.
test: toolchain
    cargo test --workspace
    cargo test -p chunk-proxy --no-default-features
    ./gradlew test

# Build everything.
build:
    cargo build --workspace
    ./gradlew assemble

# Everything CI runs. Run before opening a PR.
ready: fmt-check lint typecheck test build

# Build and run the complete local example. Ctrl-C stops its services and gameplay JVMs.
local *args:
    pnpm install --frozen-lockfile
    node packages/compiler/install-toolchain.mjs
    ./gradlew :jvm:example:installDist :jvm:example:writeJavaExecutable
    cargo run -p chunk-cli -- local --java "$(cat jvm/example/build/java-executable.txt)" {{args}}

# Operate on players connected to the local example.
players *args:
    cargo run -p chunk-cli -- players --control-file .chunk/local/control.json {{args}}

# Install the pinned native TypeScript toolchain beside development executables.
toolchain:
    pnpm install --frozen-lockfile
    node packages/compiler/install-toolchain.mjs

# Assemble a host-platform CLI distribution, including its native type checker.
package-cli:
    pnpm install --frozen-lockfile
    cargo build --release -p chunk-cli
    mkdir -p target/dist
    cp target/release/chunk target/dist/chunk
    node packages/compiler/install-toolchain.mjs target/dist
