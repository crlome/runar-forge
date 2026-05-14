//! `runar sync` — hybrid local + remote storage replication.
//!
//! Phase 5.6 split into 5 sub-phases. This module is the entry point
//! for the CLI dispatcher; each sub-phase adds a submodule.
//!
//! 5.6.1 (this commit): handshake-only via `cmd_init`. No row movement.

pub mod auto;
pub mod bootstrap;
pub mod conflict;
pub mod gc;
pub mod heartbeat;
pub mod init;
pub mod pull;
pub mod push;
pub mod status;

pub use bootstrap::cmd_bootstrap;
pub use gc::cmd_gc;
pub use init::cmd_init;
pub use pull::cmd_pull;
pub use push::cmd_push;
pub use status::cmd_status;

use anyhow::Result;

/// Toggle auto-sync via `~/.runar-forge/.env`. Atomic write through
/// the Phase 5.5 config helper.
pub fn cmd_enable() -> Result<()> {
    crate::config_cmd::cmd_set("RUNAR_SYNC_AUTO", "true")?;
    println!();
    println!("Auto-sync enabled. Restart any running mcp-muninn server to apply.");
    Ok(())
}

pub fn cmd_disable() -> Result<()> {
    crate::config_cmd::cmd_set("RUNAR_SYNC_AUTO", "false")?;
    println!();
    println!("Auto-sync disabled. Manual `runar sync push|pull` still works.");
    Ok(())
}
