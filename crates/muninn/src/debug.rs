//! Debug telemetry producers. Writes go to the `debug_log` table (schema
//! 005) and are read back via the `muninn_debug` MCP tool.
//!
//! Gated behind `RUNAR_DEBUG=true`: injection fires on every PreToolUse
//! hook and search scoring on every retrieval, so always-on writes would be
//! pure amplification on the hot path. Reading stays ungated.

use std::sync::Arc;
use std::sync::OnceLock;

use crate::storage::MemoryStorage;
use crate::types::DebugLogInput;

/// Cached `RUNAR_DEBUG == "true"` check. Process-wide: toggling requires a
/// restart of the MCP server (hook processes are short-lived anyway).
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RUNAR_DEBUG")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Best-effort debug write: awaited (hook processes die before spawned
/// tasks run), errors swallowed — telemetry must never fail the operation
/// it observes. Callers check `enabled()`/`MemoryLibrarian::debug_enabled`
/// before building the payload so the hot path pays nothing when off.
pub async fn log(storage: &Arc<dyn MemoryStorage>, input: DebugLogInput) {
    let _ = storage.write_debug_log(input).await;
}
