//! Minimal JavaScript execution for the environment sync engine. Scaffold only.
//!
//! Start with `deno_core`/V8. V1 supports language APIs and pure-JS packages;
//! filesystem, network, process and Node APIs are not ambient capabilities.
//! The sync engine supplies capabilities appropriate to queries, mutations and
//! actions. Transactional execution must support safe validation and retries.
//!
//! One environment backend retains code for multiple immutable deployments.
//! Calls identify a deployment and function path; nested calls retain that
//! version. Runtime lifetime, isolation, limits and cancellation are open.
