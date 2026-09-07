# dev.chunkzero.chunk

Registered Gradle plugin scaffold; `apply` currently performs no wiring.
Each app will apply the plugin from its own `build.gradle.kts`; root settings
registers modules and an optional root build provides plugin orchestration.
The plugin supplies toolchains, generated client/config sources, framework
dependencies, and artifact builds. App metadata lives in `app.toml`; routing,
queues, and matchmaking stay in server code. No gameplay DSL belongs in Gradle.
Generate types needed by gameplay before compilation, then inspect session
annotations and validate contracts. Exact APIs and wiring remain unimplemented.
