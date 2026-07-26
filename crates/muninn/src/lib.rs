//! Runar Muninn library crate.
//!
//! `main.rs` (the `runar` binary) imports every module from here instead of
//! re-declaring them. That keeps dead-code analysis scoped to a single crate
//! and avoids false positives from items used only by MCP tool dispatch.

pub mod breaker;
pub mod capture;
pub mod config_cmd;
pub mod curator;
pub mod debug;
pub mod doctor;
pub mod embedding;
pub mod extract;
pub mod hooks_runtime;
pub mod huginn;
pub mod identity;
pub mod librarian;
pub mod maintenance;
pub mod mcp;
pub mod protocol;
pub mod redact;
pub mod setup;
pub mod storage;
pub mod summarizer;
pub mod sync;
pub mod text;
pub mod tokens;
pub mod types;
pub mod update;
pub mod wizard;

#[cfg(test)]
pub(crate) mod test_support;
