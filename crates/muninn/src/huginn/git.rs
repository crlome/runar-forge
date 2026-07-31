use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::graph::DependencyGraph;
use super::scanner::FileEntry;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlState {
    pub project_root: String,
    pub project_id: String,
    pub last_crawled_at: DateTime<Utc>,
    pub last_commit_hash: String,
    /// path → content hash
    pub file_hashes: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct ChangedFiles {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub renamed: Vec<(String, String)>,
    pub has_git: bool,
}

impl ChangedFiles {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.modified.is_empty()
            && self.deleted.is_empty()
            && self.renamed.is_empty()
    }
}

/// Detect changed files since last crawl. Unions git history with content
/// hashes: `git diff <since> HEAD` compares two commits and cannot see
/// uncommitted or untracked work, which the hash comparison covers.
pub fn get_changed_files(
    project_root: &Path,
    last_state: &CrawlState,
    current: &[FileEntry],
) -> ChangedFiles {
    let from_hashes = changed_from_hashes(last_state, current);
    match changed_from_git(project_root, &last_state.last_commit_hash) {
        Some(from_git) => merge_changes(from_git, from_hashes),
        None => from_hashes,
    }
}

/// Expand the changed set to include direct dependents (reverse edges).
pub fn expand_to_affected(changed: &ChangedFiles, graph: &DependencyGraph) -> Vec<String> {
    let mut affected: HashSet<String> = HashSet::new();
    for f in &changed.added {
        affected.insert(f.clone());
    }
    for f in &changed.modified {
        affected.insert(f.clone());
    }
    for (_, to) in &changed.renamed {
        affected.insert(to.clone());
    }

    let mut sources: Vec<&String> = changed
        .modified
        .iter()
        .chain(changed.deleted.iter())
        .collect();
    let renamed_from: Vec<String> = changed.renamed.iter().map(|(f, _)| f.clone()).collect();
    sources.extend(renamed_from.iter());

    for s in sources {
        if let Some(deps) = graph.reverse_edges.get(s) {
            for d in deps {
                affected.insert(d.clone());
            }
        }
    }

    affected.into_iter().collect()
}

/// Snapshot the current crawl state for storage.
pub fn build_state(project_root: &Path, project_id: &str, files: &[FileEntry]) -> CrawlState {
    let last_commit_hash = current_commit_hash(project_root).unwrap_or_else(|| "no-git".into());
    let mut file_hashes = HashMap::with_capacity(files.len());
    for f in files {
        if let Ok(bytes) = fs::read(&f.path) {
            file_hashes.insert(f.relative_path.clone(), hash_content(&bytes));
        }
    }
    CrawlState {
        project_root: project_root.to_string_lossy().to_string(),
        project_id: project_id.to_string(),
        last_crawled_at: Utc::now(),
        last_commit_hash,
        file_hashes,
    }
}

pub fn serialize_state(state: &CrawlState) -> String {
    serde_json::to_string(state).unwrap_or_default()
}

pub fn deserialize_state(json: &str) -> Option<CrawlState> {
    serde_json::from_str(json).ok()
}

// ── Internals ──────────────────────────────────────────────────────

fn changed_from_git(project_root: &Path, since_commit: &str) -> Option<ChangedFiles> {
    if since_commit.is_empty() || since_commit == "no-git" {
        return None;
    }

    // Verify git repo
    let out = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    // Verify commit exists
    let out = Command::new("git")
        .args(["cat-file", "-t", since_commit])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    let out = Command::new("git")
        .args(["diff", "--name-status", since_commit, "HEAD"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Some(parse_git_diff(&stdout))
}

fn parse_git_diff(stdout: &str) -> ChangedFiles {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    let mut renamed = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let status = parts[0];
        let path = parts[1];
        match status.chars().next() {
            Some('A') => added.push(path.to_string()),
            Some('M') => modified.push(path.to_string()),
            Some('D') => deleted.push(path.to_string()),
            Some('R') => {
                if let Some(to) = parts.get(2) {
                    renamed.push((path.to_string(), to.to_string()));
                }
            }
            _ => {}
        }
    }

    ChangedFiles {
        added,
        modified,
        deleted,
        renamed,
        has_git: true,
    }
}

fn sorted_unique(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = paths.into_iter().collect();
    out.sort();
    out.dedup();
    out
}

/// Union the two change sources. A rename is authoritative: its target is not
/// an addition and its source is not a deletion, because downstream treats the
/// three categories differently.
fn merge_changes(git: ChangedFiles, hashes: ChangedFiles) -> ChangedFiles {
    let mut renamed = git.renamed;
    renamed.sort();
    renamed.dedup();
    let rename_sources: HashSet<&String> = renamed.iter().map(|(from, _)| from).collect();
    let rename_targets: HashSet<&String> = renamed.iter().map(|(_, to)| to).collect();

    let mut added = sorted_unique(git.added.into_iter().chain(hashes.added));
    added.retain(|p| !rename_targets.contains(p));

    let added_set: HashSet<&String> = added.iter().collect();
    let mut modified = sorted_unique(git.modified.into_iter().chain(hashes.modified));
    modified.retain(|p| !added_set.contains(p));

    let mut deleted = sorted_unique(git.deleted.into_iter().chain(hashes.deleted));
    deleted.retain(|p| !rename_sources.contains(p));

    ChangedFiles {
        added,
        modified,
        deleted,
        renamed,
        has_git: true,
    }
}

fn changed_from_hashes(last_state: &CrawlState, current: &[FileEntry]) -> ChangedFiles {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    let mut current_paths: HashSet<&String> = HashSet::new();

    for file in current {
        current_paths.insert(&file.relative_path);
        match last_state.file_hashes.get(&file.relative_path) {
            None => added.push(file.relative_path.clone()),
            Some(prev_hash) => {
                if let Ok(bytes) = fs::read(&file.path) {
                    if hash_content(&bytes) != *prev_hash {
                        modified.push(file.relative_path.clone());
                    }
                } else {
                    modified.push(file.relative_path.clone());
                }
            }
        }
    }

    for prev in last_state.file_hashes.keys() {
        if !current_paths.contains(prev) {
            deleted.push(prev.clone());
        }
    }

    ChangedFiles {
        added,
        modified,
        deleted,
        renamed: vec![],
        has_git: false,
    }
}

fn current_commit_hash(project_root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Stable, deterministic 64-bit FNV-1a hash. Not cryptographic — just used
/// for cross-run change detection. Hashes raw bytes so that files which are
/// not valid UTF-8 still get recorded; skipping them left them permanently
/// absent from the state and so reported as added on every crawl.
fn hash_content(content: &[u8]) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h = FNV_OFFSET;
    for b in content {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn entry(rel: &str, dir: &Path) -> FileEntry {
        let path = dir.join(rel);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&path, "x").unwrap();
        FileEntry {
            path,
            relative_path: rel.to_string(),
            size: 1,
            line_count: 1,
            last_modified: Some(SystemTime::now()),
            extension: rel.rsplit('.').next().unwrap_or("").to_string(),
        }
    }

    #[test]
    fn hash_content_stable() {
        assert_eq!(hash_content(b"hello"), hash_content(b"hello"));
        assert_ne!(hash_content(b"hello"), hash_content(b"hellp"));
    }

    #[test]
    fn non_utf8_files_are_recorded_and_not_perpetually_added() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = root.join("hero.avif");
        fs::write(&path, [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10]).unwrap();
        let f = FileEntry {
            path,
            relative_path: "hero.avif".into(),
            size: 6,
            line_count: 1,
            last_modified: Some(SystemTime::now()),
            extension: "avif".into(),
        };

        let state = build_state(root, "p", std::slice::from_ref(&f));
        assert!(
            state.file_hashes.contains_key("hero.avif"),
            "a file that is not valid UTF-8 must still be hashed into the state"
        );

        let changed = changed_from_hashes(&state, std::slice::from_ref(&f));
        assert!(
            changed.is_empty(),
            "an untouched binary file must not be reported as changed, got {changed:?}"
        );

        fs::write(&f.path, [0x00, 0x01]).unwrap();
        let changed = changed_from_hashes(&state, std::slice::from_ref(&f));
        assert_eq!(changed.modified, vec!["hero.avif"]);
    }

    #[test]
    fn changed_from_hashes_detects_modifications() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let f = entry("a.ts", root);
        // Initial state: hash of "x"
        let mut state = CrawlState::default();
        state.file_hashes.insert("a.ts".into(), hash_content(b"x"));

        // No change yet
        let changed = changed_from_hashes(&state, std::slice::from_ref(&f));
        assert!(changed.is_empty());

        // Modify
        fs::write(&f.path, "y").unwrap();
        let changed = changed_from_hashes(&state, std::slice::from_ref(&f));
        assert_eq!(changed.modified, vec!["a.ts"]);
    }

    #[test]
    fn changed_from_hashes_detects_added_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let new_file = entry("new.ts", root);

        let mut state = CrawlState::default();
        state.file_hashes.insert("gone.ts".into(), "h".into());

        let changed = changed_from_hashes(&state, &[new_file]);
        assert_eq!(changed.added, vec!["new.ts"]);
        assert_eq!(changed.deleted, vec!["gone.ts"]);
    }

    #[test]
    fn parse_git_diff_categories() {
        let out = "A\tnew.ts\nM\tchanged.ts\nD\tgone.ts\nR100\told.ts\trenamed.ts\n";
        let r = parse_git_diff(out);
        assert_eq!(r.added, vec!["new.ts"]);
        assert_eq!(r.modified, vec!["changed.ts"]);
        assert_eq!(r.deleted, vec!["gone.ts"]);
        assert_eq!(r.renamed, vec![("old.ts".into(), "renamed.ts".into())]);
    }

    #[test]
    fn merge_changes_unions_and_dedupes() {
        let git = ChangedFiles {
            added: vec!["b.ts".into()],
            modified: vec!["a.ts".into(), "c.ts".into()],
            deleted: vec!["d.ts".into()],
            renamed: vec![],
            has_git: true,
        };
        let hashes = ChangedFiles {
            modified: vec!["c.ts".into(), "e.ts".into()],
            deleted: vec!["d.ts".into(), "f.ts".into()],
            ..Default::default()
        };
        let m = merge_changes(git, hashes);
        assert!(m.has_git);
        assert_eq!(m.added, vec!["b.ts"]);
        assert_eq!(m.modified, vec!["a.ts", "c.ts", "e.ts"]);
        assert_eq!(m.deleted, vec!["d.ts", "f.ts"]);
    }

    #[test]
    fn merge_changes_rename_target_not_also_added() {
        let git = ChangedFiles {
            renamed: vec![("old.ts".into(), "new.ts".into())],
            has_git: true,
            ..Default::default()
        };
        let hashes = ChangedFiles {
            added: vec!["new.ts".into(), "other.ts".into()],
            ..Default::default()
        };
        let m = merge_changes(git, hashes);
        assert_eq!(m.added, vec!["other.ts"]);
        assert_eq!(m.renamed, vec![("old.ts".into(), "new.ts".into())]);
    }

    #[test]
    fn merge_changes_rename_source_not_also_deleted() {
        let git = ChangedFiles {
            renamed: vec![("old.ts".into(), "new.ts".into())],
            has_git: true,
            ..Default::default()
        };
        let hashes = ChangedFiles {
            deleted: vec!["old.ts".into(), "gone.ts".into()],
            ..Default::default()
        };
        let m = merge_changes(git, hashes);
        assert_eq!(m.deleted, vec!["gone.ts"]);
    }

    #[test]
    fn merge_changes_added_wins_over_modified() {
        let git = ChangedFiles {
            added: vec!["a.ts".into()],
            has_git: true,
            ..Default::default()
        };
        let hashes = ChangedFiles {
            modified: vec!["a.ts".into(), "b.ts".into()],
            ..Default::default()
        };
        let m = merge_changes(git, hashes);
        assert_eq!(m.added, vec!["a.ts"]);
        assert_eq!(m.modified, vec!["b.ts"]);
    }

    #[test]
    fn get_changed_files_without_git_uses_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let f = entry("a.ts", root);
        let mut state = CrawlState::default();
        state
            .file_hashes
            .insert("a.ts".into(), hash_content(b"old"));

        let changed = get_changed_files(root, &state, std::slice::from_ref(&f));
        assert!(!changed.has_git);
        assert_eq!(changed.modified, vec!["a.ts"]);
    }

    fn run_git(args: &[&str], dir: &Path) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn get_changed_files_sees_uncommitted_edits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        if !run_git(&["init", "--quiet"], root) {
            return;
        }
        let tracked = entry("a.ts", root);
        assert!(run_git(&["add", "."], root));
        assert!(run_git(
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
                "init",
            ],
            root
        ));

        let head = current_commit_hash(root).unwrap();
        let mut state = CrawlState {
            last_commit_hash: head,
            ..Default::default()
        };
        state.file_hashes.insert("a.ts".into(), hash_content(b"x"));

        // HEAD is unchanged since the last crawl; only the working tree moved.
        fs::write(&tracked.path, "y").unwrap();
        let untracked = entry("b.ts", root);

        let changed = get_changed_files(root, &state, &[tracked, untracked]);
        assert!(changed.has_git);
        assert_eq!(changed.modified, vec!["a.ts"]);
        assert_eq!(changed.added, vec!["b.ts"]);
    }

    #[test]
    fn round_trip_state() {
        let mut state = CrawlState {
            project_id: "p".into(),
            last_commit_hash: "abc".into(),
            ..Default::default()
        };
        state.file_hashes.insert("a.ts".into(), "h1".into());
        let json = serialize_state(&state);
        let back = deserialize_state(&json).unwrap();
        assert_eq!(back.project_id, "p");
        assert_eq!(back.last_commit_hash, "abc");
        assert_eq!(back.file_hashes.get("a.ts"), Some(&"h1".to_string()));
    }
}
