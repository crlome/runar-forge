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

// ── Automatic refresh ──────────────────────────────────────────────

/// Beyond this many files a refresh stops being something to do behind
/// someone's back. Override with `RUNAR_GRAPH_AUTOREFRESH_MAX_FILES`; running
/// `runar graph refresh` by hand ignores the ceiling entirely.
const AUTO_MAX_FILES: usize = 20_000;

/// Floor on the gap between automatic refreshes.
const AUTO_MIN_GAP_MS: i64 = 30_000;
/// Ceiling on it, so a slow project still refreshes eventually.
const AUTO_MAX_GAP_MS: i64 = 600_000;
/// How much of the time an automatic refresh may occupy: the next one is held
/// off for this many times its own duration, which keeps a 0.3s refresh to
/// roughly 1.5% duty cycle and stops a slow one from running back to back.
const AUTO_DUTY_FACTOR: i64 = 20;

/// Backoff for states that will not change on their own.
const AUTO_BACKOFF_SETTLED_MS: i64 = 3_600_000;
/// Backoff for states that need a person, but might change sooner.
const AUTO_BACKOFF_BLOCKED_MS: i64 = 600_000;
/// Backoff for states that are expected to clear by themselves.
const AUTO_BACKOFF_TRANSIENT_MS: i64 = 30_000;
/// Backoff after an error, which is neither expected nor permanent.
const AUTO_BACKOFF_ERROR_MS: i64 = 300_000;

/// What the last automatic attempt did, and when the next may start.
///
/// A file rather than a database row: the hook that reads it must not open any
/// store, and this has to be answerable in well under a millisecond.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stamp {
    pub last_attempt_ms: i64,
    pub not_before_ms: i64,
    pub last_outcome: String,
    pub last_duration_ms: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AutoOutcome {
    Refreshed {
        files: usize,
        scanned: usize,
        duration_ms: i64,
    },
    Skipped(String),
    Failed(String),
}

impl AutoOutcome {
    fn label(&self) -> String {
        match self {
            AutoOutcome::Refreshed { .. } => "done".to_string(),
            AutoOutcome::Skipped(reason) => format!("skip: {reason}"),
            AutoOutcome::Failed(reason) => format!("error: {reason}"),
        }
    }
}

/// Is another automatic refresh due?
///
/// Pure, so the debounce is testable without a clock. No stamp at all means
/// yes: a project that has never refreshed should not have to wait.
pub fn should_spawn(stamp: Option<&Stamp>, now_ms: i64) -> bool {
    stamp.is_none_or(|s| now_ms >= s.not_before_ms)
}

pub fn stamp_path(project: &str) -> PathBuf {
    crate::setup::runar_dir().join(format!("graph-refresh-{}.stamp", sanitize(project)))
}

pub fn read_stamp(project: &str) -> Option<Stamp> {
    let body = std::fs::read_to_string(stamp_path(project)).ok()?;
    serde_json::from_str(&body).ok()
}

/// Best-effort: a stamp that fails to write costs an extra refresh, not
/// correctness, and this runs on a path that must never fail a tool call.
pub fn write_stamp(project: &str, stamp: &Stamp) {
    let path = stamp_path(project);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_string(stamp) {
        // Write-then-rename: a reader must never see half a stamp and treat a
        // truncated one as "no stamp", which would defeat the debounce exactly
        // when writes are frequent.
        let tmp = path.with_extension("stamp.tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub fn stats_path() -> PathBuf {
    crate::setup::runar_dir().join("graph-refresh-stats.tsv")
}

/// One tab-separated line per decision, for judging later whether this hook
/// earns its keep. Mirrors what the search-hint hook records.
///
/// The cost columns are what make a soak worth running: an outcome alone says
/// the hook is alive, but not whether it is getting slower, how much work each
/// pass actually does, or what a bad day looks like. None of that can be
/// reconstructed afterwards from a stamp holding only the last run.
pub fn record_stat(project: &str, outcome: &str, cost: Option<RefreshCost>) {
    use std::io::Write;
    let path = stats_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let (ms, reparsed, scanned) = match cost {
            Some(c) => (
                c.duration_ms.to_string(),
                c.files_reparsed.to_string(),
                c.files_scanned.to_string(),
            ),
            None => ("-".into(), "-".into(), "-".into()),
        };
        let _ = writeln!(
            f,
            "{}\t{project}\t{outcome}\t{ms}\t{reparsed}\t{scanned}",
            crate::protocol::now_ms()
        );
    }
}

/// What one automatic pass cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshCost {
    pub duration_ms: i64,
    pub files_reparsed: usize,
    pub files_scanned: usize,
}

/// Everything the recorded rows can answer, for `runar doctor` and for
/// deciding whether this hook should stay opt-in.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RefreshStats {
    /// Hook fires that started a child.
    pub spawns: usize,
    /// Hook fires the debounce turned away.
    pub debounced: usize,
    /// Passes that actually re-indexed.
    pub refreshes: usize,
    /// Children that decided there was nothing to do, by reason.
    pub skipped: Vec<(String, usize)>,
    pub errors: usize,
    pub last_error: Option<String>,
    /// Durations of successful passes, ascending.
    durations_ms: Vec<i64>,
    pub files_reparsed: usize,
    pub projects: usize,
}

impl RefreshStats {
    /// Hook fires, however they ended.
    pub fn fires(&self) -> usize {
        self.spawns + self.debounced
    }

    /// Of the children that ran, how many found work. A number that falls
    /// towards zero means the hook is spawning processes for nothing.
    pub fn work_rate(&self) -> Option<usize> {
        let ran = self.refreshes + self.skipped.iter().map(|(_, n)| n).sum::<usize>();
        (self.refreshes * 100).checked_div(ran)
    }

    pub fn median_ms(&self) -> Option<i64> {
        self.percentile_ms(50)
    }

    /// Nearest-rank percentile. The tail is the number that matters here: a
    /// median refresh is always fast, and what would make this unacceptable is
    /// the occasional slow one.
    pub fn percentile_ms(&self, p: usize) -> Option<i64> {
        if self.durations_ms.is_empty() {
            return None;
        }
        let rank = (self.durations_ms.len() * p).div_ceil(100).max(1);
        self.durations_ms.get(rank - 1).copied()
    }

    /// Of the children that ran, how many failed — as a percentage.
    ///
    /// Deliberately a **rate**, not a count. These stats accumulate for the
    /// life of the file and are never reset, so any check phrased as
    /// "errors == 0" latches on the first transient failure and can never
    /// return to green, however healthy the following thousand runs are.
    /// One `database is locked` four days ago is not a reason to keep
    /// reporting a fault today.
    ///
    /// Denominator matches `work_rate`: every child that actually ran,
    /// which is refreshes plus skips plus errors. Debounced fires never
    /// started work and cannot have failed.
    pub fn error_rate(&self) -> Option<usize> {
        let ran = self.refreshes + self.skipped.iter().map(|(_, n)| n).sum::<usize>() + self.errors;
        (self.errors * 100).checked_div(ran)
    }
}

/// Error-rate percentage at which auto-refresh stops being worth its place.
///
/// The graduation bar recorded when the soak was scheduled: "kill criteria:
/// error rate over 5%". Named here so the doctor check and the soak runbook
/// cannot drift apart on the number.
pub const ERROR_RATE_KILL_PCT: usize = 5;

/// Read the recorded rows.
///
/// Tolerates the three-column rows written before the cost columns existed, so
/// a soak already in progress is not thrown away by an upgrade.
pub fn refresh_stats() -> RefreshStats {
    let Ok(body) = std::fs::read_to_string(stats_path()) else {
        return RefreshStats::default();
    };
    let mut out = RefreshStats::default();
    let mut skipped: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut projects: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in body.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        // Timestamp, project, outcome, then optionally the cost columns.
        let (Some(project), Some(outcome)) = (cols.get(1), cols.get(2)) else {
            continue;
        };
        projects.insert((*project).to_string());
        let duration = cols.get(3).and_then(|v| v.parse::<i64>().ok());
        let reparsed = cols.get(4).and_then(|v| v.parse::<usize>().ok());

        match *outcome {
            "spawn" => out.spawns += 1,
            "debounce" => out.debounced += 1,
            "done" => {
                out.refreshes += 1;
                if let Some(ms) = duration {
                    out.durations_ms.push(ms);
                }
                out.files_reparsed += reparsed.unwrap_or(0);
            }
            other if other.starts_with("error") => {
                out.errors += 1;
                out.last_error = Some(other.to_string());
            }
            other if other.starts_with("skip") => {
                let reason = other.strip_prefix("skip: ").unwrap_or(other);
                *skipped.entry(reason.to_string()).or_default() += 1;
            }
            // `spawn-failed` and anything a later version writes.
            other => {
                *skipped.entry(other.to_string()).or_default() += 1;
            }
        }
    }

    out.durations_ms.sort_unstable();
    out.projects = projects.len();
    let mut skipped: Vec<(String, usize)> = skipped.into_iter().collect();
    skipped.sort_by_key(|e| std::cmp::Reverse(e.1));
    out.skipped = skipped;
    out
}

/// Start a refresh that outlives this process.
///
/// The hook that calls this has a budget measured in milliseconds and must
/// never delay a tool call, so the work cannot happen inline. The child is put
/// in its own process group so that the group-kill Claude Code uses to enforce
/// a hook timeout cannot take the refresh with it, and its streams are closed
/// so nothing it prints can reach the transcript.
pub fn spawn_detached(project: &str) -> std::io::Result<u32> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["graph", "refresh", "--auto", "--project", project])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    // Never waited on: the parent exits immediately and init reaps the child.
    cmd.spawn().map(|c| c.id())
}

/// The detached child's whole job.
///
/// Every exit — including every refusal — writes a stamp, a breadcrumb and a
/// statistic. A background task that decides to do nothing and says nothing is
/// indistinguishable from one that is broken, and the only way anyone would
/// find out is by noticing the graph never changes.
pub fn run_auto(project: &str) -> AutoOutcome {
    let started = Instant::now();
    let (outcome, backoff_ms) = decide_and_run(project, started);

    let now = crate::protocol::now_ms();
    let duration_ms = started.elapsed().as_millis() as i64;
    write_stamp(
        project,
        &Stamp {
            last_attempt_ms: now,
            not_before_ms: now + backoff_ms,
            last_outcome: outcome.label(),
            last_duration_ms: duration_ms,
        },
    );
    // Cost columns only where there was a cost: a skip's duration is the cost
    // of deciding not to work, which would drag every percentile down towards
    // zero and make a slow refresh invisible.
    let cost = match &outcome {
        AutoOutcome::Refreshed {
            files,
            scanned,
            duration_ms,
        } => Some(RefreshCost {
            duration_ms: *duration_ms,
            files_reparsed: *files,
            files_scanned: *scanned,
        }),
        _ => None,
    };
    record_stat(project, &outcome.label(), cost);
    crate::hooks_runtime::append_hook_log(
        "graph-autorefresh",
        &format!("{} ({project}, {duration_ms}ms)", outcome.label()),
    );
    outcome
}

/// The gate chain, cheapest and most conclusive first. Returns how long to
/// wait before trying again alongside what happened.
fn decide_and_run(project: &str, started: Instant) -> (AutoOutcome, i64) {
    let skip = |reason: &str, backoff: i64| (AutoOutcome::Skipped(reason.to_string()), backoff);

    // The switch may have been thrown between the hook firing and this
    // starting, and this is the one that actually writes.
    if crate::hooks_runtime::hooks_disabled() {
        return skip("hooks disabled", AUTO_BACKOFF_TRANSIENT_MS);
    }

    // Read-only, and never creates the file: refreshing is maintenance, so a
    // project with no graph is not this hook's business.
    let Some(store) = CodeGraphStore::open_if_indexed(project) else {
        return skip(
            "no graph for this project — run `runar crawl` once",
            AUTO_BACKOFF_SETTLED_MS,
        );
    };
    let Some(root) = store
        .project_root(project)
        .ok()
        .flatten()
        .map(PathBuf::from)
    else {
        return skip("no root recorded", AUTO_BACKOFF_SETTLED_MS);
    };
    if !root.is_dir() {
        return skip("recorded root is gone", AUTO_BACKOFF_SETTLED_MS);
    }

    // Observed before anything is scanned or written, and committed only if
    // the index below succeeds.
    let observed = freshness::snapshot(&root);
    if observed.head.is_none() || observed.dirty.is_none() {
        // Without git there is no cheap way to tell whether anything changed,
        // so every trigger would pay for a full content-hash pass over the
        // tree. Refreshing by hand still works.
        return skip("not a git repository", AUTO_BACKOFF_SETTLED_MS);
    }

    // The gate that makes this safe to fire on every write: refresh when the
    // signals have *changed*, not merely when the tree is dirty. A repository
    // that stays dirty is not getting staler, and re-indexing it on a timer
    // would be a write amplifier with no reader.
    if let Some(base) = freshness::baseline(&store, project) {
        if matches!(
            freshness::judge(Some(&base), &observed),
            Verdict::Fresh | Verdict::Unknown { .. }
        ) {
            return skip(
                "nothing changed since the last index",
                AUTO_BACKOFF_TRANSIENT_MS,
            );
        }
    }
    drop(store);

    let Some(_lock) = RefreshLock::try_acquire(project) else {
        return skip("a refresh is already running", AUTO_BACKOFF_TRANSIENT_MS);
    };

    let scan = scanner::scan_project(&root);
    // An empty inventory would make the index forget the entire project. The
    // tree cannot really be empty — it was indexed once — so this means the
    // scan failed, and doing nothing is the only safe response.
    if scan.files.is_empty() {
        return skip(
            "scan found no files, refusing to index over a project",
            AUTO_BACKOFF_BLOCKED_MS,
        );
    }
    if scan.files.len() > auto_max_files() {
        return (
            AutoOutcome::Skipped(format!(
                "{} files is over the automatic ceiling — refresh by hand",
                scan.files.len()
            )),
            AUTO_BACKOFF_BLOCKED_MS,
        );
    }

    let store = match CodeGraphStore::open_default() {
        Ok(s) => s,
        Err(e) => return (AutoOutcome::Failed(e.to_string()), AUTO_BACKOFF_ERROR_MS),
    };
    match index::index_project(&store, project, &root, &scan.files, false, Some(observed)) {
        Ok(outcome) => {
            let duration_ms = started.elapsed().as_millis() as i64;
            (
                AutoOutcome::Refreshed {
                    files: outcome.files_indexed + outcome.files_partial,
                    scanned: scan.files.len(),
                    duration_ms,
                },
                next_gap_ms(duration_ms),
            )
        }
        Err(e) => (AutoOutcome::Failed(e.to_string()), AUTO_BACKOFF_ERROR_MS),
    }
}

/// Hold off the next automatic refresh in proportion to what this one cost.
fn next_gap_ms(duration_ms: i64) -> i64 {
    (duration_ms * AUTO_DUTY_FACTOR).clamp(AUTO_MIN_GAP_MS, AUTO_MAX_GAP_MS)
}

/// How long an explicit crawl waits for a background refresh to finish.
/// Tunable so tests do not have to spend it.
pub fn crawl_lock_wait() -> Duration {
    std::env::var("RUNAR_GRAPH_LOCK_WAIT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(120))
}

fn auto_max_files() -> usize {
    std::env::var("RUNAR_GRAPH_AUTOREFRESH_MAX_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(AUTO_MAX_FILES)
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
    /// Wait up to `limit` for the lock, then give up.
    ///
    /// For a crawl, which is explicit work someone is waiting on: a background
    /// refresh takes well under a second, so waiting is nearly always better
    /// than colliding. Giving up rather than blocking forever matters more —
    /// a crawl that hangs behind a stuck refresh would be a far worse failure
    /// than two writers briefly sharing a WAL database.
    pub fn acquire_bounded(project: &str, limit: Duration) -> Option<Self> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(lock) = Self::try_acquire(project) {
                return Some(lock);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

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
    crate::setup::runar_dir().join(format!("graph-refresh-{}.lock", sanitize(project)))
}

/// A project id reaches these paths straight off a command line, so it must
/// not be able to name a directory of its own choosing.
fn sanitize(project: &str) -> String {
    project
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
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

    // ── the automatic path ─────────────────────────────────────────

    fn stamp_at(not_before_ms: i64) -> Stamp {
        Stamp {
            last_attempt_ms: 0,
            not_before_ms,
            last_outcome: "done".into(),
            last_duration_ms: 10,
        }
    }

    #[test]
    fn the_debounce_holds_until_its_window_passes() {
        assert!(
            should_spawn(None, 1_000),
            "a project that has never refreshed must not wait"
        );
        assert!(!should_spawn(Some(&stamp_at(2_000)), 1_999));
        assert!(
            should_spawn(Some(&stamp_at(2_000)), 2_000),
            "boundary is due"
        );
        assert!(should_spawn(Some(&stamp_at(2_000)), 5_000));
    }

    #[test]
    fn an_unreadable_stamp_does_not_wedge_the_debounce() {
        let runar = tempfile::tempdir().unwrap();
        let _g = with_env("RUNAR_HOME", runar.path().to_str().unwrap());
        std::fs::create_dir_all(crate::setup::runar_dir()).unwrap();
        std::fs::write(stamp_path("p"), "{ truncated").unwrap();
        assert!(read_stamp("p").is_none());
        assert!(
            should_spawn(read_stamp("p").as_ref(), 0),
            "a corrupt stamp must read as 'never refreshed', not as 'wait forever'"
        );
    }

    #[test]
    fn a_stamp_round_trips() {
        let runar = tempfile::tempdir().unwrap();
        let _g = with_env("RUNAR_HOME", runar.path().to_str().unwrap());
        let s = stamp_at(1234);
        write_stamp("p", &s);
        assert_eq!(read_stamp("p").unwrap(), s);
    }

    /// The gap scales with what the refresh cost, so a slow project does not
    /// spend its life re-indexing, and a fast one is not throttled to a crawl.
    #[test]
    fn the_next_gap_scales_with_cost_and_stays_within_bounds() {
        assert_eq!(
            next_gap_ms(10),
            AUTO_MIN_GAP_MS,
            "a fast refresh gets the floor"
        );
        assert_eq!(next_gap_ms(5_000), 100_000);
        assert_eq!(
            next_gap_ms(120_000),
            AUTO_MAX_GAP_MS,
            "a very slow refresh is still retried eventually"
        );
    }

    /// The gate that lets this fire on every write: unchanged signals mean no
    /// work, so a tree that merely stays dirty is not re-indexed forever.
    #[test]
    fn an_unchanged_tree_is_not_re_indexed() {
        if !have_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let runar = tempfile::tempdir().unwrap();
        let db = home.path().join("cg.db");
        let _g = fixture(dir.path(), &db, "p");
        std::env::set_var("RUNAR_HOME", runar.path());

        let before = {
            let s = CodeGraphStore::open_readonly(&db).unwrap();
            s.indexed_at("p").unwrap()
        };
        match run_auto("p") {
            AutoOutcome::Skipped(reason) => assert!(reason.contains("nothing changed"), "{reason}"),
            other => panic!("an unchanged tree was re-indexed: {other:?}"),
        }
        let s = CodeGraphStore::open_readonly(&db).unwrap();
        assert_eq!(
            s.indexed_at("p").unwrap(),
            before,
            "the graph was rewritten"
        );
        std::env::remove_var("RUNAR_HOME");
    }

    #[test]
    fn a_changed_tree_is_re_indexed_and_the_baseline_moves() {
        if !have_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let runar = tempfile::tempdir().unwrap();
        let _g = fixture(dir.path(), &home.path().join("cg.db"), "p");
        std::env::set_var("RUNAR_HOME", runar.path());

        std::fs::write(dir.path().join("b.rs"), "pub fn beta_auto() {}\n").unwrap();
        assert!(matches!(run_auto("p"), AutoOutcome::Refreshed { .. }));

        let store = CodeGraphStore::open_default().unwrap();
        assert_eq!(store.search("p", "beta_auto", None, 5).unwrap().len(), 1);
        drop(store);

        // Through the real child, not a hand-made row: what a pass cost has to
        // survive into the record, or a week of soaking answers nothing about
        // how expensive this actually is.
        let stats = refresh_stats();
        assert_eq!(stats.refreshes, 1);
        assert!(
            stats.median_ms().is_some(),
            "the child recorded a refresh with no duration"
        );
        assert!(
            stats.files_reparsed >= 1,
            "the child recorded no work done: {stats:?}"
        );

        // And now it settles: the second run has nothing to do.
        assert!(matches!(run_auto("p"), AutoOutcome::Skipped(_)));
        std::env::remove_var("RUNAR_HOME");
    }

    /// Automatic refresh is maintenance, never construction.
    #[test]
    fn the_automatic_path_never_builds_a_graph() {
        let home = tempfile::tempdir().unwrap();
        let runar = tempfile::tempdir().unwrap();
        let db = home.path().join("cg.db");
        let _g = with_env("RUNAR_CODEGRAPH_PATH", db.to_str().unwrap());
        std::env::set_var("RUNAR_HOME", runar.path());

        match run_auto("never-indexed") {
            AutoOutcome::Skipped(reason) => assert!(reason.contains("no graph"), "{reason}"),
            other => panic!("got {other:?}"),
        }
        assert!(!db.exists(), "the automatic path created a graph");
        std::env::remove_var("RUNAR_HOME");
    }

    #[test]
    fn the_kill_switch_stops_the_child_too() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let runar = tempfile::tempdir().unwrap();
        let db = home.path().join("cg.db");
        let _g = fixture(dir.path(), &db, "p");
        std::env::set_var("RUNAR_HOME", runar.path());
        std::env::set_var("RUNAR_DISABLE_HOOKS", "1");

        std::fs::write(dir.path().join("b.rs"), "pub fn beta() {}\n").unwrap();
        let outcome = run_auto("p");
        std::env::remove_var("RUNAR_DISABLE_HOOKS");
        std::env::remove_var("RUNAR_HOME");

        match outcome {
            AutoOutcome::Skipped(reason) => assert!(reason.contains("disabled"), "{reason}"),
            other => panic!("the kill switch did not stop a write: {other:?}"),
        }
        let store = CodeGraphStore::open_readonly(&db).unwrap();
        assert!(store.search("p", "beta", None, 5).unwrap().is_empty());
    }

    /// A scan that comes back empty is the shape of a failure, not of a
    /// project. Indexing over it would hand the index an empty inventory,
    /// which forgets every file the project has — the worst thing a
    /// background writer could quietly do.
    #[test]
    fn an_empty_scan_is_refused_rather_than_indexed_over() {
        if !have_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let runar = tempfile::tempdir().unwrap();
        let db = home.path().join("cg.db");
        let _g = fixture(dir.path(), &db, "p");
        std::env::set_var("RUNAR_HOME", runar.path());

        // Everything the scanner would find is gone; only `.git` remains, and
        // that is ignored. The tree still *exists*, so this is not the
        // vanished-root case.
        std::fs::remove_file(dir.path().join("a.rs")).unwrap();
        let outcome = run_auto("p");
        std::env::remove_var("RUNAR_HOME");

        match outcome {
            AutoOutcome::Skipped(reason) => {
                assert!(reason.contains("no files"), "{reason}")
            }
            other => panic!("an empty scan was indexed: {other:?}"),
        }
        let store = CodeGraphStore::open_readonly(&db).unwrap();
        assert_eq!(
            store.coverage("p").unwrap().symbols,
            1,
            "the project was erased by an empty scan"
        );
    }

    #[test]
    fn a_project_over_the_ceiling_is_left_to_a_person() {
        if !have_git() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let runar = tempfile::tempdir().unwrap();
        let _g = fixture(dir.path(), &home.path().join("cg.db"), "p");
        std::env::set_var("RUNAR_HOME", runar.path());
        std::env::set_var("RUNAR_GRAPH_AUTOREFRESH_MAX_FILES", "1");

        std::fs::write(dir.path().join("b.rs"), "pub fn beta() {}\n").unwrap();
        let outcome = run_auto("p");
        std::env::remove_var("RUNAR_GRAPH_AUTOREFRESH_MAX_FILES");
        std::env::remove_var("RUNAR_HOME");

        match outcome {
            AutoOutcome::Skipped(reason) => assert!(reason.contains("ceiling"), "{reason}"),
            other => panic!("got {other:?}"),
        }
    }

    /// The recorded rows have to answer the questions a soak is run to
    /// answer, and the cost of a pass cannot be reconstructed later: the stamp
    /// keeps only the last one.
    #[test]
    fn the_record_answers_what_a_soak_needs_to_know() {
        let runar = tempfile::tempdir().unwrap();
        let _g = with_env("RUNAR_HOME", runar.path().to_str().unwrap());

        record_stat("p", "spawn", None);
        record_stat("p", "debounce", None);
        for ms in [100, 200, 300, 400, 5_000] {
            record_stat(
                "p",
                "done",
                Some(RefreshCost {
                    duration_ms: ms,
                    files_reparsed: 2,
                    files_scanned: 160,
                }),
            );
        }
        record_stat("p", "skip: nothing changed since the last index", None);
        record_stat("q", "error: codegraph db: disk I/O error", None);

        let s = refresh_stats();
        assert_eq!(s.spawns, 1);
        assert_eq!(s.debounced, 1);
        assert_eq!(s.fires(), 2);
        assert_eq!(s.refreshes, 5);
        assert_eq!(s.files_reparsed, 10);
        assert_eq!(s.projects, 2);
        assert_eq!(s.errors, 1);
        assert!(s
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("disk I/O"));
        assert_eq!(
            s.skipped.first().map(|(r, n)| (r.as_str(), *n)),
            Some(("nothing changed since the last index", 1))
        );

        // The tail is the point: a median that stays fast while the worst case
        // grows is exactly the failure a soak is meant to surface.
        assert_eq!(s.median_ms(), Some(300));
        assert_eq!(s.percentile_ms(95), Some(5_000));
        // 5 of 6 children found work.
        assert_eq!(s.work_rate(), Some(83));
        // 1 error out of 7 children that ran (5 refreshes + 1 skip + 1
        // error) — well under the kill threshold, so this soak is healthy
        // despite having a non-zero error count.
        assert_eq!(s.error_rate(), Some(14));
    }

    #[test]
    fn error_rate_is_a_rate_so_one_old_failure_cannot_latch() {
        // The defect this replaced: the doctor check failed on
        // `errors > 0`, and these stats never reset, so a single transient
        // `database is locked` pinned the check to FAILED forever — while
        // the soak it was supposed to report on was passing every stated
        // criterion.
        let healthy = RefreshStats {
            refreshes: 450,
            skipped: vec![("nothing changed".into(), 200)],
            errors: 1,
            ..Default::default()
        };
        assert_eq!(healthy.error_rate(), Some(0), "1 in 651 rounds to 0%");
        assert!(
            healthy.error_rate().unwrap() <= ERROR_RATE_KILL_PCT,
            "a single old failure among hundreds of good runs must not fail the check"
        );

        // …but a genuinely broken hook still trips it.
        let broken = RefreshStats {
            refreshes: 40,
            skipped: vec![("nothing changed".into(), 10)],
            errors: 50,
            ..Default::default()
        };
        assert_eq!(broken.error_rate(), Some(50));
        assert!(broken.error_rate().unwrap() > ERROR_RATE_KILL_PCT);
    }

    #[test]
    fn error_rate_ignores_debounced_fires() {
        // Debounced fires never started a child, so they cannot have
        // failed; counting them would dilute the rate and hide a hook that
        // fails every time it actually runs.
        let s = RefreshStats {
            spawns: 2,
            debounced: 998,
            refreshes: 0,
            errors: 2,
            ..Default::default()
        };
        assert_eq!(s.error_rate(), Some(100));
    }

    #[test]
    fn error_rate_is_none_when_nothing_ran() {
        assert_eq!(RefreshStats::default().error_rate(), None);
    }

    /// A soak already running must not be discarded by an upgrade that adds
    /// columns to the file it is writing.
    #[test]
    fn rows_written_before_the_cost_columns_still_count() {
        use std::io::Write;
        let runar = tempfile::tempdir().unwrap();
        let _g = with_env("RUNAR_HOME", runar.path().to_str().unwrap());
        std::fs::create_dir_all(crate::setup::runar_dir()).unwrap();

        let mut f = std::fs::File::create(stats_path()).unwrap();
        writeln!(f, "1785690869495\tp\tspawn").unwrap();
        writeln!(f, "1785690869501\tp\tdebounce").unwrap();
        writeln!(f, "1785690869855\tp\tdone").unwrap();
        drop(f);

        let s = refresh_stats();
        assert_eq!(s.fires(), 2, "old rows still count as fires");
        assert_eq!(s.refreshes, 1);
        assert_eq!(
            s.median_ms(),
            None,
            "a row with no duration must not be read as a zero-millisecond refresh"
        );
    }

    /// Every decision leaves a trace. A background task that quietly does
    /// nothing is indistinguishable from one that is broken.
    #[test]
    fn every_automatic_decision_is_recorded() {
        let home = tempfile::tempdir().unwrap();
        let runar = tempfile::tempdir().unwrap();
        let _g = with_env(
            "RUNAR_CODEGRAPH_PATH",
            home.path().join("cg.db").to_str().unwrap(),
        );
        std::env::set_var("RUNAR_HOME", runar.path());

        run_auto("p");
        let stamp = read_stamp("p").expect("a stamp is written even when nothing happens");
        assert!(stamp.last_outcome.starts_with("skip:"), "{stamp:?}");
        assert!(
            stamp.not_before_ms > stamp.last_attempt_ms,
            "a skip must still back off"
        );

        let stats =
            std::fs::read_to_string(crate::setup::runar_dir().join("graph-refresh-stats.tsv"))
                .expect("a statistic is recorded");
        assert!(stats.contains("skip:"), "{stats}");
        let log = crate::hooks_runtime::tail_hook_log(5).join("\n");
        assert!(log.contains("graph-autorefresh"), "no breadcrumb in: {log}");
        std::env::remove_var("RUNAR_HOME");
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
