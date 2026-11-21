//! Shared test utilities.
//!
//! **Process-global state.** `RUNAR_HOME` is a single process-wide env var.
//! Cargo's default test runner schedules tests in parallel across modules, so
//! a single mutex here is the only safe way to mutate it. Both `setup` and
//! `hooks_runtime` tests hold this lock to avoid clobbering each other's
//! `RUNAR_HOME` mid-assertion. `HOME_LOCK` is not reentrant: never call
//! `with_env` inside `with_runar_home`, or the test deadlocks.
//!
//! **Hostile text.** [`hostile_corpus`] is the fixture every text
//! transformer runs against: characters whose byte length changes when
//! lowercased, multibyte characters straddling a byte budget, CRLF, BOM and
//! NUL. Code that finds an offset in one string and cuts another with it, or
//! cuts at a byte count, breaks on these.

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

/// Characters and sequences that break text code written for ASCII.
pub(crate) const HOSTILE_ATOMS: &[&str] = &[
    // Lowercase to a different byte length: an offset found in the
    // lowercased copy is wrong in the original.
    "\u{0130}", // İ, 2 bytes -> "i̇", 3 bytes
    "\u{212A}", // Kelvin sign, 3 bytes -> "k", 1 byte
    "\u{2126}", // Ohm sign, 3 bytes -> "ω", 2 bytes
    "\u{1E9E}", // capital sharp s, 3 bytes -> "ß", 2 bytes
    "\u{023A}", // Ⱥ, 2 bytes -> "ⱥ", 3 bytes
    // Multibyte without a case change: a byte budget can land inside them.
    "配",                        // CJK, 3 bytes
    "\u{2500}",                  // box drawing, this repo's divider character
    "\u{1F426}\u{200D}\u{2B1B}", // ZWJ emoji sequence
    "e\u{0301}",                 // e + combining acute
    // Control and format characters.
    "\r\n",
    "\u{FEFF}", // BOM
    "\0",
];

/// `at - 1` ASCII bytes, then `atom` four times, so byte `at` falls inside
/// the first `atom` when it is multibyte.
pub(crate) fn straddling(at: usize, atom: &str) -> String {
    "a".repeat(at.saturating_sub(1)) + &atom.repeat(4)
}

/// The shared hostile-text fixture: every atom alone, and repeated 3, 10
/// and 40 times at the start, middle and end of ASCII text; each multibyte
/// atom straddling byte 500; a 20,000-char text cycling through every atom;
/// and lone `<private>` tag fragments. Only the fragment strings contain
/// `<`, so a caller that wraps its own block around a corpus string skips
/// those.
pub(crate) fn hostile_corpus() -> Vec<String> {
    let mut corpus = Vec::new();
    for atom in HOSTILE_ATOMS {
        corpus.push((*atom).to_string());
        for n in [3, 10, 40] {
            let run = atom.repeat(n);
            corpus.push(format!("{run} plain words"));
            corpus.push(format!("plain {run} words"));
            corpus.push(format!("plain words {run}"));
        }
        if !atom.is_ascii() {
            corpus.push(straddling(500, atom));
        }
    }
    corpus.push(
        HOSTILE_ATOMS
            .iter()
            .flat_map(|atom| atom.chars().chain(" word ".chars()))
            .cycle()
            .take(20_000)
            .collect(),
    );
    for fragment in ["<private>", "<PRIVATE>", "</private>", "<priv"] {
        corpus.push(format!("before {fragment} after"));
    }
    corpus
}
