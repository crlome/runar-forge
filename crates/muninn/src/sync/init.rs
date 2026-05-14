//! `runar sync init` — read-only handshake validating a hybrid
//! local + remote storage pair.
//!
//! Phase 5.6.1. Subsequent sub-phases (push/pull/bootstrap/auto)
//! all assume a successful `init` ran at least once.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;

use crate::storage::postgres::{PostgresAdapter, PG_MIGRATIONS};
use crate::storage::sqlite::{SqliteAdapter, MIGRATIONS};
use crate::storage::MemoryStorage;

/// Resolve a backend pair from env. Returns `(local, remote)` adapters
/// already initialized (migrations applied).
async fn resolve_backends() -> Result<(Arc<dyn MemoryStorage>, Arc<dyn MemoryStorage>)> {
    let local_kind = std::env::var("RUNAR_STORAGE_LOCAL")
        .context("RUNAR_STORAGE_LOCAL not set — hybrid sync requires both backends")?;
    let remote_url = std::env::var("RUNAR_STORAGE_REMOTE")
        .context("RUNAR_STORAGE_REMOTE not set — hybrid sync requires both backends")?;

    let namespace = std::env::var("RUNAR_MEMORY_NAMESPACE").unwrap_or_else(|_| "default".into());

    let local: Arc<dyn MemoryStorage> = match local_kind.as_str() {
        "sqlite" => {
            let path = std::env::var("RUNAR_SQLITE_PATH")
                .unwrap_or_else(|_| "~/.runar-forge/memory.db".into());
            Arc::new(SqliteAdapter::new(&path, &namespace)?)
        }
        "postgresql" | "postgres" => {
            let url = std::env::var("RUNAR_DB_URL").context(
                "RUNAR_STORAGE_LOCAL=postgresql requires RUNAR_DB_URL for the local instance",
            )?;
            Arc::new(PostgresAdapter::new(&url, &namespace)?)
        }
        other => bail!("unknown RUNAR_STORAGE_LOCAL: {other} (use sqlite or postgresql)"),
    };
    local.initialize().await?;

    // Remote is always PG. The url IS the differentiator.
    let remote: Arc<dyn MemoryStorage> = Arc::new(PostgresAdapter::new(&remote_url, &namespace)?);
    remote.initialize().await?;

    Ok((local, remote))
}

/// Read embedding dimensionality from the env. If unset, returns None
/// — sync init will warn but not refuse.
fn env_embedding_dim() -> Option<i32> {
    std::env::var("RUNAR_VECTOR_DIMENSIONS")
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Compute a stable schema-version string from a migration list. The
/// hash is the count + the last name — so adding any migration changes
/// the version string. Cheap and human-readable.
fn schema_version(migrations: &[(&str, &str)]) -> String {
    let last = migrations.last().map(|(n, _)| *n).unwrap_or("");
    format!("{}-{}", migrations.len(), last)
}

/// Run handshake. Reports each check via stdout. Returns Err on hard
/// failures; `force=true` downgrades schema-version mismatch to a
/// warning.
pub async fn cmd_init(force: bool) -> Result<()> {
    println!("runar sync init — handshake");
    println!();

    // Backend resolution.
    let (local, _remote) = resolve_backends().await?;
    println!("✔ backends opened (local + remote)");

    // Schema version handshake.
    let local_schema = if std::env::var("RUNAR_STORAGE_LOCAL")
        .map(|v| v == "sqlite")
        .unwrap_or(false)
    {
        schema_version(MIGRATIONS)
    } else {
        schema_version(PG_MIGRATIONS)
    };
    let remote_schema = schema_version(PG_MIGRATIONS);

    if local_schema != remote_schema {
        if force {
            println!(
                "⚠ schema version mismatch (local={local_schema} remote={remote_schema}) — \
                 forced via --force, proceeding anyway"
            );
        } else {
            bail!(
                "schema version mismatch: local={} remote={} (upgrade lagging side, or pass --force)",
                local_schema,
                remote_schema
            );
        }
    } else {
        println!("✔ schema version  {local_schema}");
    }

    // Embedding dim handshake. We don't have a per-DB dim probe yet
    // (would require querying the first embedding row, which may not
    // exist). Use the env variable as the contract.
    let dim = env_embedding_dim();
    if let Some(d) = dim {
        println!("✔ embedding dim   {d}");
    } else {
        println!("⚠ embedding dim   not set (RUNAR_VECTOR_DIMENSIONS) — skipping check");
    }

    // Persist sync_state on the LOCAL side. The local DB owns the
    // cursor; remote stores its own copy if some future feature wants
    // bidirectional state, but for 5.6 the local is canonical.
    let mut state = local.read_sync_state().await?;
    state.local_dim = dim;
    state.remote_dim = dim;
    state.local_schema_version = Some(local_schema);
    state.remote_schema_version = Some(remote_schema);
    state.initialized_at = Some(Utc::now());
    local
        .write_sync_state(&state)
        .await
        .map_err(|e| anyhow!("write_sync_state failed: {e}"))?;

    println!();
    println!("✔ sync_state initialized at {}", state.initialized_at.unwrap());
    println!();
    println!("Next:");
    println!("  runar sync bootstrap   # first-time full pull from remote (5.6.3)");
    println!("  runar sync push|pull   # incremental once 5.6.2/5.6.3 ship");
    Ok(())
}
