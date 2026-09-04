set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Format Rust, Kotlin and protobuf sources.
fmt:
    cargo fmt --all
    ktlint --format "jvm/**/*.kt" "jvm/**/*.kts" "buildSrc/**/*.kts" "*.kts" "!**/build/**"
    buf format --write proto

# Verify formatting without modifying files.
fmt-check:
    cargo fmt --all --check
    ktlint "jvm/**/*.kt" "jvm/**/*.kts" "buildSrc/**/*.kts" "*.kts" "!**/build/**"
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
test:
    cargo test --workspace
    ./gradlew test

# Build everything.
build:
    cargo build --workspace
    ./gradlew assemble

# Everything CI runs. Run before opening a PR.
ready: fmt-check lint typecheck test build
