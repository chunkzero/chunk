//! Application toolchain orchestration. Scaffold only.
//!
//! Intended responsibilities include JavaScript type checking and bundling,
//! contract/client generation, server JAR builds, immutable asset publication
//! and deployment manifests. Rolldown is the likely bundler, not yet integrated.
//! The initial runtime is `deno_core`/V8; contract extraction remains proposed.
//!
//! Each app has app.toml metadata and its own Gradle build. Source declarations
//! and annotated JVM classes supply contracts; server code owns routing/demand.
//! Chunk owns session creation and machine provisioning. Domain command/hook
//! manifests and precompile backend/config bindings are toolchain outputs.
