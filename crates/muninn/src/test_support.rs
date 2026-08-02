//! Shared test utilities for serializing access to process-global state.
//!
//! `RUNAR_HOME` is a single process-wide env var. Cargo's default test runner
//! schedules tests in parallel across modules, so a single mutex here is the
//! only safe way to mutate it. Both `setup` and `hooks_runtime` tests hold
//! this lock to avoid clobbering each other's `RUNAR_HOME` mid-assertion.

use std::sync::Mutex;

pub(crate) static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with `RUNAR_HOME` pointed at a fresh tempdir. Restores the prior
/// value (or unsets it) on exit. Holds `HOME_LOCK` for the whole call so
/// concurrent tests can't observe each other's directory.
pub(crate) fn with_runar_home<F: FnOnce()>(f: F) {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let prev = std::env::var("RUNAR_HOME").ok();
    std::env::set_var("RUNAR_HOME", tmp.path());
    f();
    match prev {
        Some(v) => std::env::set_var("RUNAR_HOME", v),
        None => std::env::remove_var("RUNAR_HOME"),
    }
}

/// RAII form of the same idea for one arbitrary env var, so async tests
/// can hold it across `.await` (a closure taking `FnOnce()` cannot).
/// Restores the previous value on drop and holds `HOME_LOCK` throughout.
pub(crate) struct EnvGuard {
    key: &'static str,
    prev: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Set `key` to `val` until the returned guard drops.
pub(crate) fn with_env(key: &'static str, val: &str) -> EnvGuard {
    let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var(key).ok();
    std::env::set_var(key, val);
    EnvGuard {
        key,
        prev,
        _lock: lock,
    }
}
