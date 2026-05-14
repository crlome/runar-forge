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

/// Detect changed files since last crawl. Tries git first, falls back to hashes.
pub fn get_changed_files(
    project_root: &Path,
    last_state: &CrawlState,
    current: &[FileEntry],
) -> ChangedFiles {
    if let Some(git_result) = changed_from_git(project_root, &last_state.last_commit_hash) {
        return git_result;
    }
    changed_from_hashes(last_state, current)
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
        if let Ok(content) = fs::read_to_string(&f.path) {
            file_hashes.insert(f.relative_path.clone(), hash_content(&content));
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
                if let Ok(content) = fs::read_to_string(&file.path) {
                    if hash_content(&content) != *prev_hash {
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
/// for cross-run change detection.
fn hash_content(content: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h = FNV_OFFSET;
    for b in content.as_bytes() {
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
        assert_eq!(hash_content("hello"), hash_content("hello"));
        assert_ne!(hash_content("hello"), hash_content("hellp"));
    }

    #[test]
    fn changed_from_hashes_detects_modifications() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let f = entry("a.ts", root);
        // Initial state: hash of "x"
        let mut state = CrawlState::default();
        state.file_hashes.insert("a.ts".into(), hash_content("x"));

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
