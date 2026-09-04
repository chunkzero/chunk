//! The edge tier.
//!
//! Loads an app from its manifest, runs its bundle in `chunk-js` over its
//! `chunk-store`, and implements the platform primitives behind `ctx`:
//! database, scheduler, crons, sessions, players, queues, packs, presence,
//! storage, vars and bindings. Dispatches the four proxy events (ping, login,
//! disconnect, chat) and edge commands to listeners in module-then-app order.
//! Serves `EdgeCall` to session processes so sessions can invoke functions and
//! subscribe to queries, and serves resource packs.
//!
//! Composes `chunk-proxy` for connection ownership. Session placement and
//! delivery go through the runtime supervisor over the internal transport;
//! this crate never speaks to a JVM directly and never sees a Minestom type.
