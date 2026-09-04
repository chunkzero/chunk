/**
 * `@chunk/edge`: the only module edge code imports from the platform.
 *
 * Declarations (`defineSchema`, `defineTable`, `v`, `query`, `mutation`,
 * `action`, the `internal*` variants, `on`, `command`, `defineQueues`,
 * `cronJobs`, `defineSessions`, `defineApp`) are pure: they build descriptors
 * the contract compiler reads and the runtime registry invokes. Handlers run
 * only when Rust invokes them with the `ctx` allowed for their kind.
 *
 * `lib` is ES2023 with no DOM and no Node types on purpose: the QuickJS
 * runtime supplies only the language plus what chunk installs on `ctx`.
 */
export {}
