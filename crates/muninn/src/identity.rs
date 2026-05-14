//! Phase 5.7 — Per-dev attribution.
//!
//! Resolves an author label by shelling out to `git config user.name`.
//! Name only — never email — to keep PII out of shared remote storage.
//! Result cached for the lifetime of the process. Fail-silent: a missing
//! git binary, an unset name, or a non-zero exit all yield `None` and
//! the caller leaves the column NULL.

use std::process::Command;
use std::sync::OnceLock;

static CACHED: OnceLock<Option<String>> = OnceLock::new();

pub fn resolve_author() -> Option<String> {
    CACHED.get_or_init(query_git_name).clone()
}

fn query_git_name() -> Option<String> {
    let output = Command::new("git")
        .args(["config", "user.name"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Test-only helper: resolve identity without consulting the cache. Lets
/// unit tests exercise the `git` shell-out without polluting the global
/// `OnceLock` (which would leak state across tests).
#[cfg(test)]
pub fn resolve_uncached() -> Option<String> {
    query_git_name()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncached_resolution_is_some_or_none() {
        // Every CI environment we care about has git installed; the
        // returned value must either be a non-empty string or None when
        // user.name is unset. Anything else (empty Some, panic) is wrong.
        if let Some(name) = resolve_uncached() {
            assert!(!name.is_empty());
        }
    }
}
