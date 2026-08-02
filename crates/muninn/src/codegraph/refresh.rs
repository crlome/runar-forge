//! Bring an already-built graph back in line with the tree, and nothing else.
//!
//! A crawl does two independent jobs: it writes memory entries about the
//! project, and it indexes the symbol graph. Only the second is worth repeating
//! every time a file changes — it costs well under a second on a repository
//! this size, where the memory pass costs tens of seconds — so this is that
//! half on its own, reachable without a librarian, a runtime or a network.
//!
//! Three properties it deliberately does not have:
//!
//! * **It never builds.** A project with no graph, or a graph this binary
//!   cannot read, is an error naming `runar crawl`, not a silent from-scratch
//!   index. Opening the store writable creates the file and, on a schema
//!   mismatch, discards *every* project's graph to rebuild — a cost a crawl
//!   takes knowingly and a refresh must never impose by surprise, least of all
//!   because someone mistyped `--project`.
//! * **It never narrows the inventory.** The index forgets any file missing
//!   from the list it is handed, so "only re-index what changed" would delete
//!   the rest of the project. The whole tree is always scanned; what makes it
//!   cheap is that unchanged files are not re-parsed.
//! * **It never queues.** A second refresh while one is running is not work
//!   worth doing twice, so it reports the holder and exits. There is no queue
//!   to grow and no burst to absorb.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::freshness::{self, Snapshot, Verdict};
use super::index::{self, IndexOutcome};
use super::store::CodeGraphStore;
use crate::huginn::scanner;

/// A crashed process leaves its lock file behind, so the lock is only honoured
/// while it is plausibly alive. Generous: a refresh here takes about half a
/// second, and the cost of waiting too long is a delayed refresh, while the
/// cost of stealing too early is two writers in the same file.
const LOCK_STALE: Duration = Duration::from_secs(600);

#[derive(Debug)]
pub enum RefreshError {
    NoGraph(PathBuf),
    SchemaMismatch { found: String, expected: i64 },
    UnknownProject { project: String, known: Vec<String> },
    RootMissing { project: String, root: PathBuf },
    Store(super::store::Error),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefreshError::NoGraph(path) => write!(
                f,
                "no code graph at {} — build one with `runar crawl <path> --project <id>`",
                path.display()
            ),
            RefreshError::SchemaMismatch { found, expected } => write!(
                f,
                "the graph is schema {found} and this build writes v{expected}; \
                 rebuild it with `runar crawl <path> --project <id>` \
                 (refreshing would discard every project's graph)"
            ),
            RefreshError::UnknownProject { project, known } => write!(
                f,
                "project '{project}' has no graph to refresh — build one with \
                 `runar crawl <path> --project {project}`. Projects that do: {}",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ),
            RefreshError::RootMissing { project, root } => write!(
                f,
                "the root recorded for '{project}' is gone ({}) — pass --path, \
                 or recrawl from where the project lives now",
                root.display()
            ),
            RefreshError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl From<super::store::Error> for RefreshError {
    fn from(e: super::store::Error) -> Self {
        RefreshError::Store(e)
    }
}

#[derive(Debug)]
pub struct RefreshReport {
    pub outcome: IndexOutcome,
    pub root: PathBuf,
    pub files_scanned: usize,
    pub duration: Duration,
}

#[derive(Debug)]
pub enum Refreshed {
    Done(Box<RefreshReport>),
    /// Another refresh holds the lock. Not an error: the work is already
    /// happening, and doing it twice would only contend for the same file.
    AlreadyRunning {
        since: String,
    },
}

/// Everything the caller may vary. `full` re-parses every file rather than
/// only the changed ones; it is the slow path and widens the window in which a
/// concurrent reader sees a partially rebuilt graph, so it is opt-in.
pub struct RefreshOptions {
    pub project: String,
    pub path: Option<PathBuf>,
    pub full: bool,
}

/// Re-index `project` in place.
pub fn run(opts: &RefreshOptions) -> Result<Refreshed, RefreshError> {
    let started = Instant::now();
    let root = {
        // Scoped so the read-only handle is closed before the writable one is
        // opened: two handles on one file in one process is a lock waiting to
        // happen, and nothing here needs both.
        let store = open_verified(&opts.project)?;
        resolve_root(&store, opts)?
    };

    let Some(_lock) = RefreshLock::try_acquire(&opts.project) else {
        return Ok(Refreshed::AlreadyRunning {
            since: RefreshLock::holder(&opts.project).unwrap_or_else(|| "unknown".to_string()),
        });
    };

    let (observed, scan) = observe_then_scan(&root);
    let files_scanned = scan.files.len();

    let store = CodeGraphStore::open_default()?;
    let outcome = index::index_project(
        &store,
        &opts.project,
        &root,
        &scan.files,
        opts.full,
        Some(observed),
    )?;

    Ok(Refreshed::Done(Box::new(RefreshReport {
        outcome,
        root,
        files_scanned,
        duration: started.elapsed(),
    })))
}

/// Read the freshness signals, *then* walk the tree — in that order, which is
/// why they are produced together rather than at two call sites.
///
/// A file created while the walk is in progress may miss the inventory. A
/// baseline taken afterwards would include it and so claim it was indexed;
/// taken before, the next check reads stale and the file is picked up on the
/// following pass. The window is small and the ordering only ever errs toward
/// re-doing work, never toward a graph that lies about being current.
///
/// Note for anyone changing this: the two orderings are indistinguishable to
/// any test that does not mutate the tree mid-walk, so this one is held by
/// construction rather than by an assertion.
fn observe_then_scan(root: &Path) -> (Snapshot, scanner::ScanResult) {
    let observed = freshness::snapshot(root);
    (observed, scanner::scan_project(root))
}

/// What `run` would do, without doing any of it. Opens the graph read-only and
/// writes nothing at all.
pub fn check(project: &str, path: Option<&Path>) -> Result<Verdict, RefreshError> {
    let store = open_verified(project)?;
    let opts = RefreshOptions {
        project: project.to_string(),
        path: path.map(|p| p.to_path_buf()),
        full: false,
    };
    let root = resolve_root(&store, &opts)?;
    Ok(freshness::judge(
        freshness::baseline(&store, project).as_ref(),
        &freshness::snapshot(&root),
    ))
}

/// A read-only handle to a graph that exists, is readable by this build, and
/// knows this project. Every refusal happens here, before anything is opened
/// writable.
fn open_verified(project: &str) -> Result<CodeGraphStore, RefreshError> {
    let path = CodeGraphStore::default_path();
    if !path.exists() {
        return Err(RefreshError::NoGraph(path));
    }
    let store = CodeGraphStore::open_readonly(&path)?;

    // A writable open on a mismatched schema drops and rebuilds every project.
    // The crawl accepts that; refreshing one project must not quietly cost the
    // others theirs.
    let expected = CodeGraphStore::expected_schema_version();
    match store.stored_schema_version()? {
        Some(v) if v == expected => {}
        found => {
            return Err(RefreshError::SchemaMismatch {
                found: found
                    .map(|v| format!("v{v}"))
                    .unwrap_or("unversioned".into()),
                expected,
            })
        }
    }

    let known = store.projects()?;
    if !known.iter().any(|p| p == project) {
        return Err(RefreshError::UnknownProject {
            project: project.to_string(),
            known,
        });
    }
    Ok(store)
}

/// An explicit path wins; otherwise the root the index recorded. Read from the
/// graph rather than from crawl state, because the memory store has no
/// read-only handle and this path must never migrate it.
fn resolve_root(store: &CodeGraphStore, opts: &RefreshOptions) -> Result<PathBuf, RefreshError> {
    let recorded = store.project_root(&opts.project)?.map(PathBuf::from);
    let root = match opts.path.clone().or(recorded) {
        Some(r) => r,
        None => {
            return Err(RefreshError::RootMissing {
                project: opts.project.clone(),
                root: PathBuf::new(),
            })
        }
    };
    if !root.is_dir() {
        return Err(RefreshError::RootMissing {
            project: opts.project.clone(),
            root,
        });
    }
    Ok(root.canonicalize().unwrap_or(root))
}

// ── Locking ────────────────────────────────────────────────────────

/// A whole-file advisory lock, held for as long as the guard lives.
///
/// `create_new` is the whole mechanism: it succeeds for exactly one caller.
/// There is no dependency here that would give a real OS lock, and the blast
/// radius does not warrant one — the worst outcome of getting this wrong is a
/// refresh that does not happen, which the next one will.
pub struct RefreshLock {
    path: PathBuf,
}

impl RefreshLock {
    pub fn try_acquire(project: &str) -> Option<Self> {
        let path = lock_path(project);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if Self::create(&path).is_ok() {
            return Some(Self { path });
        }
        // Someone holds it, or someone died holding it.
        if is_stale(&path) {
            let _ = std::fs::remove_file(&path);
            if Self::create(&path).is_ok() {
                return Some(Self { path });
            }
        }
        None
    }

    fn create(path: &Path) -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        // The holder's identity, so "already running" can say who.
        writeln!(
            f,
            "pid {} since {}",
            std::process::id(),
            chrono::Utc::now().to_rfc3339()
        )
    }

    /// Whatever the current holder wrote about itself.
    ///
    /// Creating the file and writing to it are two steps, so a caller that
    /// loses the race by microseconds can read it while it is still empty.
    /// Falling back to its age keeps the message true rather than "unknown",
    /// which reads like something went wrong when nothing did.
    pub fn holder(project: &str) -> Option<String> {
        let path = lock_path(project);
        let written = std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if written.is_some() {
            return written;
        }
        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())?;
        Some(format!("started {}s ago", age.as_secs()))
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_path(project: &str) -> PathBuf {
    // Same sanitising as the hint hook's per-session files: a project id
    // reaches this from a command line and must not be able to name a path.
    let safe: String = project
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    crate::setup::runar_dir().join(format!("graph-refresh-{safe}.lock"))
}

/// A lock file older than [`LOCK_STALE`] is assumed abandoned. Judged by
/// modification time rather than by asking whether the pid is alive: pids are
/// reused, and a wrong answer there steals a live lock.
fn is_stale(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > LOCK_STALE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huginn::scanner::FileEntry;
    use crate::test_support::with_env;

    /// A graph with one project, rooted at a real git repository, in an
    /// isolated file. Returns the guard that keeps `RUNAR_CODEGRAPH_PATH`
    /// pointed at it.
    ///
    /// The repository matters: freshness is judged against git, so a fixture
    /// without one can only ever be `Unknown`. The graph file is deliberately
    /// outside the tree — inside it, the index is itself an untracked change
    /// and every pass would leave the project stale by its own measurement.
    fn fixture(dir: &Path, db: &Path, project: &str) -> crate::test_support::EnvGuard {
        let guard = with_env("RUNAR_CODEGRAPH_PATH", db.to_str().unwrap());
        std::fs::write(dir.join("a.rs"), "pub fn alpha() {}\n").unwrap();
        git_init(dir);
        let store = CodeGraphStore::open_default().unwrap();
        index::index_project(&store, project, dir, &entries(dir, &["a.rs"]), true, None).unwrap();
        guard
    }

    /// Make `dir` a repository with one commit. Silent when git is absent —
    /// the tests that need a verdict check [`have_git`] and skip.
    fn git_init(dir: &Path) {
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if run(&["init", "--quiet"]) && run(&["add", "-A"]) {
            run(&[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "init",
            ]);
        }
    }

    fn have_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn entries(root: &Path, names: &[&str]) -> Vec<FileEntry> {
        names
            .iter()
            .map(|n| FileEntry {
                path: root.join(n),
                relative_path: n.to_string(),
                size: 0,
                line_count: 1,
                last_modified: None,
                extension: "rs".to_string(),
            })
            .collect()
    }

    fn opts(project: &str) -> RefreshOptions {
        RefreshOptions {
            project: project.to_string(),
            path: None,
            full: false,
        }
    }

    /// Through the real entry point, asserting on what a consumer reads back:
    /// a symbol written after the last index has to become findable.
    #[test]
    fn a_refresh_picks_up_a_symbol_written_since_the_last_index() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _g = fixture(dir.path(), &home.path().join("cg.db"), "p");

        std::fs::write(dir.path().join("b.rs"), "pub fn beta_was_added() {}\n").unwrap();
        let store = CodeGraphStore::open_default().unwrap();
        assert!(
            store
                .search("p", "beta_was_added", None, 5)
                .unwrap()
                .is_empty(),
            "the fixture must not already know the new symbol"
        );
        drop(store);

        match run(&opts("p")).unwrap() {
            Refreshed::Done(report) => {
                assert!(report.files_scanned >= 2, "the whole tree is scanned");
                assert!(
                    report.outcome.files_reused >= 1,
                    "an unchanged file must be reused, not re-parsed: {:?}",
                    report.outcome
                );
            }
            Refreshed::AlreadyRunning { .. } => panic!("nothing else holds the lock"),
        }

        let store = CodeGraphStore::open_default().unwrap();
        assert_eq!(
            store.search("p", "beta_was_added", None, 5).unwrap().len(),
            1,
            "the new symbol is still not in the graph"
        );
    }

    /// The most dangerous thing this could do: hand the index a narrowed list
    /// and have it forget everything else.
    #[test]
    fn a_refresh_keeps_the_files_it_did_not_touch() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _g = with_env(
            "RUNAR_CODEGRAPH_PATH",
            home.path().join("cg.db").to_str().unwrap(),
        );

        std::fs::write(dir.path().join("a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "pub fn beta() {}\n").unwrap();
        let store = CodeGraphStore::open_default().unwrap();
        index::index_project(
            &store,
            "p",
            dir.path(),
            &entries(dir.path(), &["a.rs", "b.rs"]),
            true,
            None,
        )
        .unwrap();
        assert_eq!(store.coverage("p").unwrap().symbols, 2);
        drop(store);

        // Only a.rs changes.
        std::fs::write(dir.path().join("a.rs"), "pub fn alpha() { beta(); }\n").unwrap();
        run(&opts("p")).unwrap();

        let store = CodeGraphStore::open_default().unwrap();
        assert_eq!(
            store.search("p", "beta", None, 5).unwrap().len(),
            1,
            "the untouched file's symbol was dropped"
        );
        assert_eq!(store.coverage("p").unwrap().symbols, 2);
    }

    /// A deleted file must still leave the graph, which is the same code path
    /// as the one above and the reason it cannot simply skip the scan.
    #[test]
    fn a_refresh_forgets_a_file_that_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _g = with_env(
            "RUNAR_CODEGRAPH_PATH",
            home.path().join("cg.db").to_str().unwrap(),
        );
        std::fs::write(dir.path().join("a.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "pub fn beta() {}\n").unwrap();
        let store = CodeGraphStore::open_default().unwrap();
        index::index_project(
            &store,
            "p",
            dir.path(),
            &entries(dir.path(), &["a.rs", "b.rs"]),
            true,
            None,
        )
        .unwrap();
        drop(store);

        std::fs::remove_file(dir.path().join("b.rs")).unwrap();
        run(&opts("p")).unwrap();

        let store = CodeGraphStore::open_default().unwrap();
        assert!(
            store.search("p", "beta", None, 5).unwrap().is_empty(),
            "a deleted file kept contributing symbols"
        );
    }

    #[test]
    fn a_refresh_leaves_the_graph_current() {
        if !have_git() {
            return; // freshness is a git comparison; nothing to assert without one
        }
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _g = fixture(dir.path(), &home.path().join("cg.db"), "p");

        std::fs::write(dir.path().join("b.rs"), "pub fn beta() {}\n").unwrap();
        run(&opts("p")).unwrap();

        let store = CodeGraphStore::open_default().unwrap();
        let base = freshness::baseline(&store, "p").expect("a refresh records a baseline");
        assert_eq!(
            freshness::judge(Some(&base), &freshness::snapshot(dir.path())),
            Verdict::Fresh,
            "the graph should describe the tree it just read"
        );
    }

    #[test]
    fn a_refresh_never_creates_a_graph() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join("cg.db");
        let _g = with_env("RUNAR_CODEGRAPH_PATH", db.to_str().unwrap());

        let err = run(&opts("p")).expect_err("there is nothing to refresh");
        assert!(matches!(err, RefreshError::NoGraph(_)), "got {err:?}");
        assert!(
            err.to_string().contains("runar crawl"),
            "the error must name the way out: {err}"
        );
        assert!(
            !db.exists(),
            "refusing to refresh still created the graph file"
        );
    }

    #[test]
    fn a_refresh_refuses_a_project_it_does_not_know() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _g = fixture(dir.path(), &home.path().join("cg.db"), "p");

        let err = run(&opts("typo")).expect_err("an unknown project is an error");
        match &err {
            RefreshError::UnknownProject { known, .. } => {
                assert_eq!(
                    known,
                    &vec!["p".to_string()],
                    "the error should list what exists"
                )
            }
            other => panic!("got {other:?}"),
        }
        // And it must not have quietly indexed one under the wrong name.
        let store = CodeGraphStore::open_default().unwrap();
        assert_eq!(store.projects().unwrap(), vec!["p".to_string()]);
    }

    /// A refresh on a mismatched schema would drop every project's graph as a
    /// side effect of opening the file writable.
    #[test]
    fn a_refresh_refuses_a_graph_this_build_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join("cg.db");
        let _g = fixture(dir.path(), &db, "p");
        // Straight to the file: a graph written by a future build is exactly
        // what this refuses, and the store offers no way to fake one.
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute(
                "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();

        let err = run(&opts("p")).expect_err("a mismatched schema is an error");
        assert!(
            matches!(err, RefreshError::SchemaMismatch { .. }),
            "got {err:?}"
        );

        // The graph is still there: refusing cost nothing.
        let store = CodeGraphStore::open_readonly(&db).unwrap();
        assert_eq!(
            store.coverage("p").unwrap().symbols,
            1,
            "the graph was discarded anyway"
        );
    }

    #[test]
    fn a_refresh_refuses_a_root_that_is_gone() {
        let home = tempfile::tempdir().unwrap();
        let _g = with_env(
            "RUNAR_CODEGRAPH_PATH",
            home.path().join("cg.db").to_str().unwrap(),
        );
        let store = CodeGraphStore::open_default().unwrap();
        index::index_project(&store, "p", Path::new("/no/such/root"), &[], true, None).unwrap();
        drop(store);

        let err = run(&opts("p")).expect_err("a vanished root is an error");
        assert!(
            matches!(err, RefreshError::RootMissing { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("--path"),
            "no way out named: {err}"
        );
    }

    #[test]
    fn a_second_refresh_stands_aside_rather_than_queueing() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let runar = tempfile::tempdir().unwrap();
        let _g = fixture(dir.path(), &home.path().join("cg.db"), "p");
        // The lock lives under the runar dir, so that has to be isolated too.
        std::env::set_var("RUNAR_HOME", runar.path());

        let held = RefreshLock::try_acquire("p").expect("first caller takes the lock");
        match run(&opts("p")).unwrap() {
            Refreshed::AlreadyRunning { since } => {
                assert!(since.contains("pid"), "the holder should be named: {since}")
            }
            Refreshed::Done(_) => panic!("two refreshes ran against one graph"),
        }

        // A lock file that exists but has not been written to yet is the
        // microsecond-wide state a losing caller can observe; it must still
        // describe the holder rather than give up.
        std::fs::write(lock_path("p"), "").unwrap();
        let described = RefreshLock::holder("p").expect("an empty lock file still says something");
        assert!(
            described.contains("ago"),
            "expected an age, got {described}"
        );

        drop(held);

        // With the lock released it proceeds.
        assert!(matches!(run(&opts("p")).unwrap(), Refreshed::Done(_)));
        std::env::remove_var("RUNAR_HOME");
    }

    #[test]
    fn a_lock_left_by_a_dead_process_is_reclaimed() {
        let runar = tempfile::tempdir().unwrap();
        let _g = with_env("RUNAR_HOME", runar.path().to_str().unwrap());

        let held = RefreshLock::try_acquire("p").expect("first take");
        assert!(
            RefreshLock::try_acquire("p").is_none(),
            "a live lock must hold"
        );
        // Age it past the staleness window without waiting for one.
        let path = lock_path("p");
        let old = SystemTime::now() - LOCK_STALE - Duration::from_secs(60);
        set_mtime(&path, old);
        assert!(
            RefreshLock::try_acquire("p").is_some(),
            "an abandoned lock must not block refreshes forever"
        );
        drop(held);
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        // No filetime dependency in this workspace: rewrite the file through a
        // handle whose times we then set via the standard library.
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    #[test]
    fn check_reports_without_writing_anything() {
        if !have_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join("cg.db");
        let _g = fixture(dir.path(), &db, "p");

        let before = std::fs::metadata(&db).unwrap().len();
        let indexed_at = {
            let s = CodeGraphStore::open_readonly(&db).unwrap();
            s.indexed_at("p").unwrap()
        };

        assert_eq!(check("p", None).unwrap(), Verdict::Fresh);
        std::fs::write(dir.path().join("b.rs"), "pub fn beta() {}\n").unwrap();
        assert!(check("p", None).unwrap().is_stale());

        let s = CodeGraphStore::open_readonly(&db).unwrap();
        assert_eq!(
            s.indexed_at("p").unwrap(),
            indexed_at,
            "check re-indexed the project"
        );
        assert_eq!(
            std::fs::metadata(&db).unwrap().len(),
            before,
            "check wrote to the graph"
        );
    }
}
