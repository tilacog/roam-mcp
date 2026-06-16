//! org-roam-mcp: a Model Context Protocol server for org-roam knowledge bases.
//!
//! See `server::RoamServer` for the main entry point and the `index` module for
//! the data layer (`SQLite` + filesystem scanner).
//!
//! The crate is library-first so integration tests can spin up a server in
//! process over an in-memory transport. `main.rs` wires it to stdio/HTTP.

pub mod config;
pub mod index;
pub mod org;
pub mod server;
pub mod sync;
pub mod tools;
pub mod util;
pub mod validation;

pub use config::Config;
pub use server::RoamServer;
