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

/// An embedder that is always available and returns the same vector for
/// every input, so semantic search ranks every stored copy of that vector as
/// a perfect match.
pub(crate) struct FixedEmbeddings(pub Vec<f32>);

#[async_trait::async_trait]
impl crate::embedding::EmbeddingProvider for FixedEmbeddings {
    fn name(&self) -> &str {
        "fixed"
    }
    fn dimensions(&self) -> usize {
        self.0.len()
    }
    fn is_available(&self) -> bool {
        true
    }
    async fn embed(&self, _text: &str) -> Option<Vec<f32>> {
        Some(self.0.clone())
    }
    async fn health_check(&self) -> bool {
        true
    }
}

/// An available embedder that records every input and returns no vector,
/// so callers fall back to keyword search: what an embeddings API would
/// have been sent.
#[derive(Default)]
pub(crate) struct RecordingEmbeddings(pub std::sync::Mutex<Vec<String>>);

impl RecordingEmbeddings {
    pub(crate) fn inputs(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl crate::embedding::EmbeddingProvider for RecordingEmbeddings {
    fn name(&self) -> &str {
        "recording"
    }
    fn dimensions(&self) -> usize {
        4
    }
    fn is_available(&self) -> bool {
        true
    }
    async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        self.0.lock().unwrap().push(text.to_string());
        None
    }
    async fn health_check(&self) -> bool {
        true
    }
}

/// An available embedder whose `embed` never completes: a provider stuck
/// behind a dead connection.
pub(crate) struct StalledEmbeddings;

#[async_trait::async_trait]
impl crate::embedding::EmbeddingProvider for StalledEmbeddings {
    fn name(&self) -> &str {
        "stalled"
    }
    fn dimensions(&self) -> usize {
        4
    }
    fn is_available(&self) -> bool {
        true
    }
    async fn embed(&self, _text: &str) -> Option<Vec<f32>> {
        std::future::pending().await
    }
    async fn health_check(&self) -> bool {
        false
    }
}

/// A local HTTP endpoint that accepts connections and never answers: a
/// stalled API. Returns its base URL. Binds 127.0.0.1 only, so no test
/// needs the network.
pub(crate) async fn silent_http_listener() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket); // keep it open, never reply
        }
    });
    format!("http://{addr}")
}

/// The session-text scenario shared by the SQLite and live-Postgres tests:
/// every session write path drops private blocks before storage. `project`
/// must be unique to the test on a shared database.
pub(crate) async fn session_paths_drop_private_blocks(
    lib: &crate::librarian::MemoryLibrarian,
    project: &str,
) {
    use crate::types::{EntryType, ListFilters, SessionInput, SessionSummary, SessionUpdate};

    let session = lib
        .propose_session(SessionInput {
            goal: Some("ship <private>PRIV-S1</private> plan".into()),
            project_id: Some(project.into()),
            tool: None,
        })
        .await
        .unwrap();
    assert_eq!(session.goal.as_deref(), Some("ship [redacted] plan"));

    lib.end_session(
        session.id,
        SessionSummary {
            summary: "done <private>PRIV-S2</private>".into(),
            goal: Some("g <private>PRIV-S3</private>".into()),
            instructions: vec!["<private>PRIV-S4</private> rule".into()],
            accomplished: vec!["a <private>PRIV-S5</private>".into()],
            discoveries: vec!["d <private>PRIV-S6</private>".into()],
            ..Default::default()
        },
        Some(project),
    )
    .await
    .unwrap();
    let stored = lib.get_session(session.id).await.unwrap();
    assert_eq!(stored.goal.as_deref(), Some("g [redacted]"));
    assert_eq!(stored.summary.as_deref(), Some("done [redacted]"));
    assert_eq!(stored.discoveries, ["d [redacted]"]);

    // The call the prompt hook makes with a session's first prompt.
    let next = lib
        .propose_session(SessionInput {
            goal: None,
            project_id: Some(project.into()),
            tool: None,
        })
        .await
        .unwrap();
    lib.update_session(
        next.id,
        SessionUpdate {
            goal: Some("<private>PRIV-S7</private> deploy the api".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let stored = lib.get_session(next.id).await.unwrap();
    assert_eq!(stored.goal.as_deref(), Some("[redacted] deploy the api"));

    let entries = lib
        .list(ListFilters {
            entry_type: Some(EntryType::Session),
            namespace: Some(project.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!entries.is_empty(), "the session-summary entry was saved");
    for entry in entries {
        assert!(
            !entry.title.contains("PRIV-") && !entry.content.contains("PRIV-"),
            "private text in the session entry: {}",
            entry.content
        );
    }
}

/// The context packet names only its own project. Shared by the SQLite and
/// live-Postgres tests; on a shared database `project` and `other` must be
/// unique to the test.
pub(crate) async fn context_packet_names_no_other_namespace(
    lib: &crate::librarian::MemoryLibrarian,
    project: &str,
    other: &str,
) {
    use crate::types::{EntryType, MemoryEntryInput};

    for (namespace, title) in [(project, "own entry"), (other, "other entry")] {
        lib.propose(MemoryEntryInput {
            title: title.into(),
            content: "context packet scope check".into(),
            entry_type: EntryType::Decision,
            project_id: Some(namespace.into()),
            ..Default::default()
        })
        .await
        .unwrap();
    }
    let packet = lib.get_context(None, Some(project), 3).await.unwrap();
    let text = &packet.formatted;
    assert!(!text.contains("Namespaces:"), "{text}");
    assert!(!text.contains(other), "another namespace is named: {text}");
    assert!(text.contains("own entry"), "{text}");
    assert!(
        text.contains("memory entries |"),
        "the stats line stays: {text}"
    );
}

/// Claim every queued outbox row and decode its payload the way
/// `sync::push` does before sending it: exactly what the remote would
/// receive.
pub(crate) async fn drain_outbox(
    storage: &dyn crate::storage::MemoryStorage,
) -> Vec<crate::types::MemoryEntry> {
    storage
        .claim_outbox(10_000, i32::MAX)
        .await
        .expect("claim the outbox")
        .into_iter()
        .map(|row| {
            serde_json::from_value(row.row_payload)
                .expect("an outbox payload decodes as a MemoryEntry, as push does")
        })
        .collect()
}

/// `count` texts mixing runs of hostile atoms, ASCII words, `<private>` tag
/// fragments and markdown lines, drawn from a xorshift generator seeded
/// with `seed`: random coverage that is identical on every OS, with no
/// dependency.
pub(crate) fn hostile_mixes(count: usize, seed: u64) -> Vec<String> {
    const WORDS: &[&str] = &[
        "alpha",
        "pin 4821",
        " ",
        "\n",
        "## Key Learnings\n",
        "- item\n",
    ];
    const TAGS: &[&str] = &[
        "<private>",
        "</private>",
        "<PRIVATE>",
        "</Private>",
        "<priv",
        "ate>",
    ];
    let mut state = seed | 1;
    let mut next = move |bound: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % bound as u64) as usize
    };
    (0..count)
        .map(|_| {
            let mut text = String::new();
            for _ in 0..1 + next(40) {
                match next(3) {
                    0 => {
                        let atom = HOSTILE_ATOMS[next(HOSTILE_ATOMS.len())];
                        text.push_str(&atom.repeat(1 + next(12)));
                    }
                    1 => text.push_str(WORDS[next(WORDS.len())]),
                    _ => text.push_str(TAGS[next(TAGS.len())]),
                }
            }
            text
        })
        .collect()
}
