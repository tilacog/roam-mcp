//! MCP tool implementations.
//!
//! Each tool lives in its own submodule and is wired into the
//! `RoamServer` tool router in `server.rs`. Tools are kept narrow
//! and side-effect-free on the index — writes are guarded by
//! `Config::can_write` and produce no DB mutations.

pub mod content;
pub mod query;
pub mod sync_tool;
pub mod validation_tools;
pub mod write;
