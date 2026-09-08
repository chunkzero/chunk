/// <reference path="./web.d.ts" />
/**
 * `@chunk/server`: scaffold for backend functions, commands and hooks.
 *
 * The initial runtime is deno_core/V8 with explicit chunk capabilities, not
 * ambient Node or browser APIs. No public SDK API is implemented yet.
 *
 * App TOML carries metadata; Gradle builds JVM gameplay. Server code owns
 * routing and optional queue/matching policies. File-based command domains
 * scope named commands and createHook exports. Chunk provisions sessions.
 */
export {}
