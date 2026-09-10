set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Format Rust, Kotlin and protobuf sources.
fmt:
    cargo fmt --all
    ktlint --format "jvm/**/*.kt" "jvm/**/*.kts" "buildSrc/**/*.kt" "buildSrc/**/*.kts" "examples/**/*.kt" "examples/**/*.kts" "*.kts" "!**/build/**" "!**/.chunk/**"
    buf format --write proto

# Verify formatting without modifying files.
fmt-check:
    cargo fmt --all --check
    ktlint "jvm/**/*.kt" "jvm/**/*.kts" "buildSrc/**/*.kt" "buildSrc/**/*.kts" "examples/**/*.kt" "examples/**/*.kts" "*.kts" "!**/build/**" "!**/.chunk/**"
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
    examples/local/gradlew test

# Build everything.
build: toolchain
    cargo build --workspace
    ./gradlew assemble
    examples/local/gradlew assemble

# Everything CI runs. Run before opening a PR.
ready: fmt-check lint typecheck test build

# Build and run the complete local example. Ctrl-C stops its services and gameplay JVMs.
local *args: toolchain
    cargo run -p chunk-cli -- dev examples/local {{args}}

# Operate on players connected to the local example.
players *args:
    cargo run -p chunk-cli -- players --control-file examples/local/.chunk/local/control.json {{args}}

# Build the development CLI and install its pinned native TypeScript toolchain.
toolchain:
    cargo build -p chunk-cli
    pnpm install --frozen-lockfile
    node scripts/install-typescript.mjs

# Assemble a host-platform CLI distribution, including its native type checker.
package-cli:
    pnpm install --frozen-lockfile
    cargo build --release -p chunk-cli
    mkdir -p target/dist
    cp target/release/chunk target/dist/chunk
    node scripts/install-typescript.mjs target/dist
