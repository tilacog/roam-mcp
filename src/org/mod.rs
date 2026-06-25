//! Org-mode parsing helpers built on `orgize`.
//!
//! The index layer (`SQLite` / scanner) handles *metadata*; this module
//! handles *content*. That includes:
//! - subtree extraction by byte offset,
//! - locating dedicated targets `<<name>>` and `CUSTOM_ID`s,
//! - rendering a section of an org file to text and char-offsets.

pub mod anchors;
pub mod edit;
pub mod filetags;
pub mod parse;

pub use anchors::AnchorResolver;
pub use parse::{OrgDoc, Section};
