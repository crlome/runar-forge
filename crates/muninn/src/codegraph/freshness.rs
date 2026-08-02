//! Does this project's graph still describe what is on disk?
//!
//! The graph only changes when someone runs a crawl, so every consumer — the
//! `graph` queries, the session code map, the search-hint hook — can be reading
//! a picture of the repository as it was hours ago. Answering confidently from
//! an old picture is worse than answering nothing, because nothing is visibly
//! nothing while a stale symbol looks exactly like a current one. So each index
//! records what the repository looked like at the moment it ran, and any reader
//! can compare without opening the graph writable.
//!
//! Two signals, both cheap:
//!
//! * the commit at `HEAD`, which moves on commit, checkout, pull and rebase;
//! * a signature over `git status --porcelain -uall -z`, with each listed
//!   path's size and mtime folded in so that editing an already-dirty file
//!   still moves it.
//!
//! The signature is compared for *change*, never for "the tree is dirty". A
//! repository that stays dirty all afternoon is not getting staler every
//! minute, and a signal that fires the whole time someone is working is a
//! signal nobody reads.
//!
//! Everything degrades to [`Verdict::Unknown`]: no git, no baseline, an
//! unreadable root, a blob that will not parse. `Fresh` is returned only on
//! positive evidence that both signals match, so the failure direction is
//! always "cannot say", never a false all-clear.
//!
//! One consequence worth knowing: the graph file must live outside the tree it
//! describes, which the default location does. Point `RUNAR_CODEGRAPH_PATH`
//! inside a working tree and the index becomes an untracked change of its own,
//! so every pass leaves the project stale by its own measurement.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use super::store::CodeGraphStore;

/// What the repository looked like when the graph was last built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    /// Commit at `HEAD`, or the literal `no-git` when there was no repository —
    /// the same convention `huginn::git::build_state` uses.
    pub head: String,
    /// Working-tree signature, `"0"` when clean. Empty means it could not be
    /// taken, which reads as "cannot judge" rather than "clean".
    pub dirty: String,
    pub recorded_at: String,
}

/// What the repository looks like now. A `None` field is a signal that could
/// not be read at all, which is different from a signal that read as empty.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub head: Option<String>,
    pub dirty: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Fresh,
    Stale { reason: String },
    Unknown { reason: String },
}

impl Verdict {
    pub fn is_stale(&self) -> bool {
        matches!(self, Verdict::Stale { .. })
    }

    /// The human half of every verdict, so callers render one way.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Verdict::Fresh => None,
            Verdict::Stale { reason } | Verdict::Unknown { reason } => Some(reason),
        }
    }
}

/// Reserved for a clean working tree, so "clean" and "could not be read" are
/// never the same string.
const CLEAN: &str = "0";

/// Read both signals for `root`. Never fails: a project without git, or with
/// git broken, simply cannot be judged, and indexing must not start failing
/// because of it.
pub fn snapshot(root: &Path) -> Snapshot {
    if !root.is_dir() {
        return Snapshot::default();
    }
    let head = git_output(root, &["rev-parse", "HEAD"])
        .map(|out| String::from_utf8_lossy(&out).trim().to_string())
        .filter(|h| !h.is_empty());
    // Without a commit there is no repository to compare a working tree
    // against, and `git status` outside one is an error anyway.
    if head.is_none() {
        return Snapshot::default();
    }
    let toplevel = git_output(root, &["rev-parse", "--show-toplevel"])
        .map(|out| PathBuf::from(String::from_utf8_lossy(&out).trim().to_string()))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| root.to_path_buf());
    let dirty = git_output(root, &["status", "--porcelain", "-uall", "-z"]).map(|out| {
        dirty_signature(&out, |rel| {
            std::fs::metadata(toplevel.join(rel))
                .ok()
                .map(|m| (m.len(), mtime_secs(&m)))
        })
    });
    Snapshot { head, dirty }
}

/// Record `snap` as the baseline for `project`. Call it only after an index
/// has fully succeeded: a baseline written for a pass that then failed claims
/// freshness the graph does not have.
pub fn stamp(store: &CodeGraphStore, project: &str, snap: &Snapshot) -> super::store::Result<()> {
    let baseline = Baseline {
        head: snap.head.clone().unwrap_or_else(|| "no-git".to_string()),
        dirty: snap.dirty.clone().unwrap_or_default(),
        recorded_at: chrono::Utc::now().to_rfc3339(),
    };
    let json = serde_json::to_string(&baseline).unwrap_or_default();
    store.set_freshness(project, &json)
}

/// The recorded baseline, or `None` if there is none or it will not parse.
///
/// A blob that fails to parse reads as no baseline, which is the safe
/// direction here: the verdict becomes `Unknown` and the reader is told the
/// index cannot be judged. It can never turn into a false `Fresh`.
pub fn baseline(store: &CodeGraphStore, project: &str) -> Option<Baseline> {
    let json = store.freshness(project).ok()??;
    serde_json::from_str(&json).ok()
}

/// Compare a baseline against the current signals.
///
/// Pure on purpose: every interesting case is reachable without a repository,
/// a store or a clock.
pub fn judge(baseline: Option<&Baseline>, current: &Snapshot) -> Verdict {
    let unknown = |reason: &str| Verdict::Unknown {
        reason: reason.to_string(),
    };

    let Some(base) = baseline else {
        return unknown("no freshness baseline — this graph was built by an older version");
    };
    if base.head == "no-git" {
        return unknown("indexed outside a git repository, so changes cannot be detected");
    }
    if base.dirty.is_empty() {
        return unknown("no working-tree signature was recorded at index time");
    }
    let (Some(head), Some(dirty)) = (current.head.as_deref(), current.dirty.as_deref()) else {
        return unknown("git is unavailable here, so changes cannot be detected");
    };

    if base.head != head {
        return Verdict::Stale {
            reason: format!(
                "HEAD moved {} -> {} since this index",
                short(&base.head),
                short(head)
            ),
        };
    }
    if base.dirty != dirty {
        return Verdict::Stale {
            reason: "the working tree changed since this index".to_string(),
        };
    }
    Verdict::Fresh
}

/// The verdict for a project the store already knows about, root and all.
///
/// Read-only throughout: the caller's handle may well be a read-only one.
pub fn verdict(store: &CodeGraphStore, project: &str) -> Verdict {
    let base = baseline(store, project);
    let root = store.project_root(project).ok().flatten();
    let current = match root.as_deref() {
        Some(r) => snapshot(Path::new(r)),
        None => Snapshot::default(),
    };
    match (root, judge(base.as_ref(), &current)) {
        // A root that no longer exists cannot be compared against, and saying
        // "git is unavailable" would send the reader looking in the wrong place.
        (None, Verdict::Unknown { .. }) => Verdict::Unknown {
            reason: "no project root recorded, so the tree cannot be compared".to_string(),
        },
        (_, v) => v,
    }
}

/// One line for the session code map, and only when the graph is genuinely
/// behind. `Fresh` needs no words, and `Unknown` would be a warning the reader
/// can do nothing with on every session of a non-git project.
pub fn stale_annotation(store: &CodeGraphStore, project: &str) -> Option<String> {
    match verdict(store, project) {
        Verdict::Stale { reason } => Some(format!(
            " STALE: {reason}, so symbols and line numbers below may be wrong."
        )),
        _ => None,
    }
}

// ── Internals ──────────────────────────────────────────────────────

fn git_output(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// First 8 characters of a commit hash. Characters, not bytes: `head` is
/// whatever git printed, and slicing a byte range can land inside a character.
fn short(head: &str) -> &str {
    crate::text::char_prefix(head, 8)
}

/// Size and modification time of one path git listed.
type Stat = (u64, i64);

/// Fold `git status --porcelain -uall -z` and the size and mtime of every path
/// it names into one signature.
///
/// The status output alone is not enough — editing a file that is already
/// listed as modified does not change a single byte of it — so the metadata is
/// what makes an in-place edit visible. `stat` is injected rather than called
/// directly so every case is reachable without a working tree, including the
/// ones a test cannot stage reliably (an mtime change within the same second).
fn dirty_signature<F>(porcelain: &[u8], stat: F) -> String
where
    F: Fn(&str) -> Option<Stat>,
{
    let mut hash = Fnv1a::new();
    let mut entries = 0usize;
    let mut chunks = porcelain.split(|b| *b == 0).filter(|c| !c.is_empty());

    while let Some(chunk) = chunks.next() {
        // `XY <path>`: two status bytes, a space, then the path. Anything
        // shorter is not an entry git produced.
        if chunk.len() < 4 {
            continue;
        }
        entries += 1;
        let (status, rest) = chunk.split_at(2);
        let path = &rest[1..];
        hash.write(status);
        hash.write(path);

        // A rename or copy carries its origin path as the next NUL-separated
        // field. It has to be consumed here or it would be read as the next
        // entry, whose first two bytes are then part of a filename.
        if status.contains(&b'R') || status.contains(&b'C') {
            if let Some(origin) = chunks.next() {
                hash.write(origin);
            }
        }

        // Lossy only for the lookup: the raw bytes are already in the hash, so
        // a path that is not valid UTF-8 still contributes its identity, and
        // the failed `stat` below contributes a marker rather than nothing.
        match stat(&String::from_utf8_lossy(path)) {
            Some((size, mtime)) => {
                hash.write(&size.to_le_bytes());
                hash.write(&mtime.to_le_bytes());
            }
            None => hash.write(b"\x01absent"),
        }
    }

    if entries == 0 {
        return CLEAN.to_string();
    }
    hash.finish()
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Incremental FNV-1a. The crawler hashes whole file bodies in one call; this
/// one folds a stream of unrelated fields, so it keeps its state.
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }

    fn finish(&self) -> String {
        format!("{:016x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn base(head: &str, dirty: &str) -> Baseline {
        Baseline {
            head: head.to_string(),
            dirty: dirty.to_string(),
            recorded_at: "2026-08-02T00:00:00Z".to_string(),
        }
    }

    fn now(head: &str, dirty: &str) -> Snapshot {
        Snapshot {
            head: Some(head.to_string()),
            dirty: Some(dirty.to_string()),
        }
    }

    /// The whole verdict space, asserted as a set rather than a sample: a
    /// bound ("at least one case is Unknown") would still pass with a case
    /// silently reclassified.
    #[test]
    fn judge_covers_every_input_shape() {
        let cases: Vec<(&str, Verdict, Verdict)> = vec![
            (
                "no baseline",
                judge(None, &now("abc", "0")),
                Verdict::Unknown {
                    reason: String::new(),
                },
            ),
            (
                "baseline from a non-git project",
                judge(Some(&base("no-git", "0")), &now("abc", "0")),
                Verdict::Unknown {
                    reason: String::new(),
                },
            ),
            (
                "baseline with no signature",
                judge(Some(&base("abc", "")), &now("abc", "0")),
                Verdict::Unknown {
                    reason: String::new(),
                },
            ),
            (
                "git unreadable now",
                judge(Some(&base("abc", "0")), &Snapshot::default()),
                Verdict::Unknown {
                    reason: String::new(),
                },
            ),
            (
                "only the signature is unreadable now",
                judge(
                    Some(&base("abc", "0")),
                    &Snapshot {
                        head: Some("abc".into()),
                        dirty: None,
                    },
                ),
                Verdict::Unknown {
                    reason: String::new(),
                },
            ),
            (
                "head moved",
                judge(Some(&base("abc", "0")), &now("def", "0")),
                Verdict::Stale {
                    reason: String::new(),
                },
            ),
            (
                "working tree moved",
                judge(Some(&base("abc", "0")), &now("abc", "ff00")),
                Verdict::Stale {
                    reason: String::new(),
                },
            ),
            (
                "both signals match",
                judge(Some(&base("abc", "ff00")), &now("abc", "ff00")),
                Verdict::Fresh,
            ),
        ];

        for (name, got, want) in cases {
            assert_eq!(
                std::mem::discriminant(&got),
                std::mem::discriminant(&want),
                "{name}: got {got:?}"
            );
            if !matches!(got, Verdict::Fresh) {
                assert!(
                    got.reason().is_some_and(|r| !r.is_empty()),
                    "{name}: a non-fresh verdict must say why"
                );
            }
        }
    }

    /// The failure that would make this feature worse than useless: claiming a
    /// graph is current when there is no way to know.
    #[test]
    fn nothing_unjudgeable_is_ever_fresh() {
        for verdict in [
            judge(None, &now("abc", "0")),
            judge(Some(&base("no-git", "0")), &now("abc", "0")),
            judge(Some(&base("abc", "")), &now("abc", "0")),
            judge(Some(&base("abc", "0")), &Snapshot::default()),
        ] {
            assert!(
                !matches!(verdict, Verdict::Fresh),
                "an unjudgeable state reported Fresh: {verdict:?}"
            );
        }
    }

    #[test]
    fn a_moved_head_is_named_with_short_hashes() {
        let v = judge(
            Some(&base("0123456789abcdef", "0")),
            &now("fedcba9876543210", "0"),
        );
        let reason = v.reason().unwrap();
        assert!(reason.contains("01234567"), "got {reason}");
        assert!(reason.contains("fedcba98"), "got {reason}");
        assert!(
            !reason.contains("0123456789abcdef"),
            "the full hash is noise in a one-line notice: {reason}"
        );
    }

    #[test]
    fn a_clean_tree_is_the_reserved_signature() {
        assert_eq!(dirty_signature(b"", |_| None), CLEAN);
    }

    /// Both halves of the injected metadata are load-bearing, and each is
    /// asserted alone: an edit that keeps a file the same size still moves its
    /// mtime, and an edit inside one second still moves its size.
    #[test]
    fn the_signature_moves_with_size_and_with_mtime_independently() {
        let porcelain = b" M a.rs\0".to_vec();
        let sig = |size, mtime| dirty_signature(&porcelain, |_| Some((size, mtime)));

        let first = sig(100, 1_800_000_000);
        assert_ne!(first, CLEAN, "a listed entry is not a clean tree");
        assert_eq!(
            first,
            sig(100, 1_800_000_000),
            "the same tree must hash the same twice"
        );
        assert_ne!(
            first,
            sig(101, 1_800_000_000),
            "a size change went unnoticed"
        );
        assert_ne!(
            first,
            sig(100, 1_800_000_001),
            "an mtime change at the same size went unnoticed"
        );
    }

    #[test]
    fn the_signature_distinguishes_paths_and_statuses() {
        let stat = |_: &str| Some((10u64, 20i64));
        let modified = dirty_signature(b" M a.rs\0", stat);
        assert_ne!(
            modified,
            dirty_signature(b" M b.rs\0", stat),
            "two different paths collided"
        );
        assert_ne!(
            modified,
            dirty_signature(b"?? a.rs\0", stat),
            "the status code is part of the state of the tree"
        );
        assert_ne!(
            modified,
            dirty_signature(b" M a.rs\0 M b.rs\0", stat),
            "a second dirty file went unnoticed"
        );
    }

    #[test]
    fn a_vanished_path_still_changes_the_signature() {
        let porcelain = b" D gone.rs\0".to_vec();
        let absent = dirty_signature(&porcelain, |_| None);
        assert_ne!(absent, CLEAN);
        assert_ne!(
            absent,
            dirty_signature(b" D other.rs\0", |_| None),
            "two different missing paths must not collide"
        );
    }

    /// A rename entry carries a second NUL-separated field: the path it came
    /// from. Leaving it in the stream means the *next* read treats it as an
    /// entry, so its first two bytes become a status code and the rest a
    /// filename that was never on disk.
    ///
    /// Asserted by watching which paths get looked up rather than by comparing
    /// two signatures: both signatures come from the same function, so a
    /// mis-parse shifts them together and the comparison still holds.
    #[test]
    fn a_rename_consumes_its_origin_path() {
        let seen = std::cell::RefCell::new(Vec::new());
        let watch = |rel: &str| {
            seen.borrow_mut().push(rel.to_string());
            None
        };

        dirty_signature(b"R  new.rs\0old.rs\0", watch);
        assert_eq!(
            *seen.borrow(),
            vec!["new.rs"],
            "a rename is one entry: only the destination is a real path"
        );

        // The entry after a rename must still be read as an entry.
        seen.borrow_mut().clear();
        dirty_signature(b"R  new.rs\0old.rs\0 M after.rs\0", watch);
        assert_eq!(
            *seen.borrow(),
            vec!["new.rs", "after.rs"],
            "the origin path displaced the entry that followed it"
        );

        // A copy carries the same second field.
        seen.borrow_mut().clear();
        dirty_signature(b"C  copy.rs\0from.rs\0", watch);
        assert_eq!(*seen.borrow(), vec!["copy.rs"]);
    }

    #[test]
    fn a_root_that_is_not_a_repository_reads_as_unjudgeable() {
        let dir = tempfile::tempdir().unwrap();
        let snap = snapshot(dir.path());
        assert_eq!(snap, Snapshot::default(), "got {snap:?}");
        assert!(!matches!(
            judge(Some(&base("abc", "0")), &snap),
            Verdict::Fresh
        ));
    }

    #[test]
    fn a_missing_root_reads_as_unjudgeable() {
        let snap = snapshot(Path::new("/no/such/directory/anywhere"));
        assert_eq!(snap, Snapshot::default());
    }

    fn run_git(args: &[&str], dir: &Path) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn commit(dir: &Path, message: &str) -> bool {
        run_git(&["add", "-A"], dir)
            && run_git(
                &[
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "-m",
                    message,
                ],
                dir,
            )
    }

    /// Against a real repository, through the real `git` the crawl will run:
    /// the parse above is only worth having if git's actual output reaches it
    /// in the shape it expects.
    #[test]
    fn a_real_repository_reports_both_signals_and_both_move() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if !run_git(&["init", "--quiet"], root) {
            return; // no git on this machine; the pure tests still cover the logic
        }
        fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        if !commit(root, "init") {
            return;
        }

        let clean = snapshot(root);
        let head = clean.head.clone().expect("HEAD of a real repository");
        assert_eq!(head.len(), 40, "expected a full commit hash, got {head}");
        assert_eq!(
            clean.dirty.as_deref(),
            Some(CLEAN),
            "a just-committed tree is clean"
        );
        assert_eq!(
            judge(
                Some(&base(&head, CLEAN)),
                &Snapshot {
                    head: Some(head.clone()),
                    dirty: Some(CLEAN.to_string())
                }
            ),
            Verdict::Fresh
        );

        // Working tree moves, HEAD does not.
        fs::write(root.join("b.rs"), "pub fn beta() {}\n").unwrap();
        let dirty = snapshot(root);
        assert_eq!(dirty.head, clean.head, "HEAD has not moved");
        assert_ne!(
            dirty.dirty, clean.dirty,
            "an untracked file left the tree looking clean"
        );
        let v = judge(Some(&base(&head, CLEAN)), &dirty);
        assert!(v.is_stale(), "got {v:?}");

        // The edit is committed: HEAD moves too, and the tree is clean again.
        if !commit(root, "add beta") {
            return;
        }
        let after = snapshot(root);
        assert_ne!(after.head, clean.head, "HEAD did not move on a commit");
        assert_eq!(after.dirty.as_deref(), Some(CLEAN));
        let v = judge(Some(&base(&head, CLEAN)), &after);
        assert!(v.is_stale(), "got {v:?}");
        assert!(
            v.reason().unwrap().contains("HEAD moved"),
            "the reason should name the signal that moved: {v:?}"
        );
    }

    /// `stamp` -> `baseline` -> `judge` against the same tree, with a real
    /// repository at both ends. The round trip is where a serialization change
    /// would silently turn every project unjudgeable.
    #[test]
    fn a_stamped_repository_reads_back_as_fresh_until_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if !run_git(&["init", "--quiet"], root) {
            return;
        }
        fs::write(root.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        if !commit(root, "init") {
            return;
        }

        let store = CodeGraphStore::in_memory().unwrap();
        store.begin_project("p", root, true).unwrap();
        stamp(&store, "p", &snapshot(root)).unwrap();

        assert_eq!(verdict(&store, "p"), Verdict::Fresh);
        assert_eq!(stale_annotation(&store, "p"), None);

        fs::write(root.join("a.rs"), "pub fn alpha() { beta(); }\n").unwrap();
        let v = verdict(&store, "p");
        assert!(v.is_stale(), "got {v:?}");
        let line = stale_annotation(&store, "p").expect("a stale graph says so");
        assert!(line.contains("STALE"), "got {line}");
        assert!(
            line.contains("working tree"),
            "the line should name what moved: {line}"
        );
    }

    #[test]
    fn a_baseline_round_trips_through_the_store() {
        let store = CodeGraphStore::in_memory().unwrap();
        let snap = Snapshot {
            head: Some("abc123".into()),
            dirty: Some("ff00".into()),
        };
        stamp(&store, "p", &snap).unwrap();

        let got = baseline(&store, "p").expect("a stamped baseline reads back");
        assert_eq!(got.head, "abc123");
        assert_eq!(got.dirty, "ff00");
        assert!(!got.recorded_at.is_empty());
        assert_eq!(judge(Some(&got), &snap), Verdict::Fresh);
        assert!(baseline(&store, "other").is_none(), "keyed per project");
    }

    #[test]
    fn a_snapshot_without_git_stamps_as_no_git_and_stays_unjudgeable() {
        let store = CodeGraphStore::in_memory().unwrap();
        stamp(&store, "p", &Snapshot::default()).unwrap();

        let got = baseline(&store, "p").expect("even an unjudgeable baseline is recorded");
        assert_eq!(got.head, "no-git");
        assert!(got.dirty.is_empty());
        assert!(!matches!(
            judge(Some(&got), &now("abc", "0")),
            Verdict::Fresh
        ));
    }

    #[test]
    fn an_unparseable_baseline_reads_as_none_not_as_fresh() {
        let store = CodeGraphStore::in_memory().unwrap();
        store.set_freshness("p", "{not json").unwrap();
        assert!(baseline(&store, "p").is_none());
        assert!(!matches!(
            judge(baseline(&store, "p").as_ref(), &now("abc", "0")),
            Verdict::Fresh
        ));
    }

    #[test]
    fn stale_annotation_speaks_only_when_the_graph_is_behind() {
        let store = CodeGraphStore::in_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        store
            .begin_project("p", dir.path(), true)
            .expect("a project row carries the root the verdict needs");

        // No git in a bare tempdir: unjudgeable, and an unjudgeable graph must
        // not put a warning nobody can act on into every session.
        stamp(&store, "p", &Snapshot::default()).unwrap();
        assert_eq!(stale_annotation(&store, "p"), None);

        // A baseline that cannot match anything the tree can produce.
        stamp(
            &store,
            "p",
            &Snapshot {
                head: Some("abc123".into()),
                dirty: Some("ff00".into()),
            },
        )
        .unwrap();
        let v = verdict(&store, "p");
        assert!(
            matches!(v, Verdict::Unknown { .. }),
            "a non-repository root cannot be judged, got {v:?}"
        );
        assert_eq!(stale_annotation(&store, "p"), None);
    }
}
