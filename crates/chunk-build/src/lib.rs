//! The build pipeline behind `chunk build` and `chunk dev`.
//!
//! Three separate edge responsibilities: type checking with the TypeScript
//! compiler and no emit, bundling with Rolldown into one neutral ESM module
//! for `QuickJS`, and contract compilation by evaluating declarations in a
//! capability-free `chunk-js` runtime. From the resulting contract this crate
//! generates Kotlin and Java document types and the `Edge` client, TypeScript
//! session stubs and `_generated/` types, and test fakes; then writes the
//! manifest into `dist/`.
//!
//! JVM compilation, bytecode indexing and asset building happen in Gradle
//! through the plugin in `jvm/gradle-plugin`; this crate drives them and
//! consumes their output. Developers never configure the bundler.
