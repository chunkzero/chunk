set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# Format Rust, Kotlin, protobuf and dashboard sources.
fmt:
    cargo fmt --all
    ktlint --format "jvm/**/*.kt" "jvm/**/*.kts" "buildSrc/**/*.kts" "*.kts" "!**/build/**"
    buf format --write proto
    pnpm format

# Verify formatting without modifying files.
fmt-check:
    cargo fmt --all --check
    ktlint "jvm/**/*.kt" "jvm/**/*.kts" "buildSrc/**/*.kts" "*.kts" "!**/build/**"
    buf format --diff --exit-code proto
    pnpm format:check

# Run linters.
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    buf lint proto
    pnpm lint

# Type-check the TypeScript packages.
typecheck:
    pnpm install --frozen-lockfile
    pnpm typecheck

# Run the test suites.
test:
    cargo test --workspace
    cargo test -p chunk-proxy --no-default-features
    ./gradlew test

# Build everything.
build:
    cargo build --workspace
    ./gradlew assemble
    pnpm dashboard:build

# Build the static dashboard served by chunk edge --dashboard-dir.
dashboard-build:
    pnpm dashboard:build

# Run the dashboard dev server; /api forwards to the local chunk backend.
dashboard-dev:
    pnpm --filter @chunk/dashboard dev

# Everything CI runs. Run before opening a PR.
ready: fmt-check lint typecheck test build
