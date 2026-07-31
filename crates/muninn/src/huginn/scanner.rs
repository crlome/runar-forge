use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_FILES: usize = 50_000;

const DEFAULT_IGNORE: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "target",
    ".next",
    ".nuxt",
    ".turbo",
    "__pycache__",
    ".venv",
    "venv",
    ".env",
    ".DS_Store",
    "coverage",
    ".cache",
    "tmp",
    ".tmp",
    "*.min.js",
    "*.min.css",
    "*.map",
    "*.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "bun.lock",
    "yarn.lock",
    "Cargo.lock",
];

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub relative_path: String,
    pub size: u64,
    pub line_count: usize,
    pub last_modified: Option<SystemTime>,
    pub extension: String,
}

#[derive(Debug)]
pub struct ScanResult {
    pub files: Vec<FileEntry>,
    pub root: PathBuf,
    pub total_dirs_scanned: usize,
}

pub fn scan_project(root: &Path) -> ScanResult {
    let mut files = Vec::new();
    let mut dirs_scanned = 0;

    let scout_ignore = load_ignore_file(root, ".scoutignore");
    let git_ignore = load_ignore_file(root, ".gitignore");
    let all_ignore: Vec<&str> = DEFAULT_IGNORE
        .iter()
        .copied()
        .chain(scout_ignore.iter().map(|s| s.as_str()))
        .chain(git_ignore.iter().map(|s| s.as_str()))
        .collect();

    walk_dir(root, root, &all_ignore, &mut files, &mut dirs_scanned);

    ScanResult {
        files,
        root: root.to_path_buf(),
        total_dirs_scanned: dirs_scanned,
    }
}

fn walk_dir(
    dir: &Path,
    root: &Path,
    ignore: &[&str],
    files: &mut Vec<FileEntry>,
    dirs_scanned: &mut usize,
) {
    if files.len() >= MAX_FILES {
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    *dirs_scanned += 1;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if should_ignore(&name, &path, ignore) {
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };

        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            walk_dir(&path, root, ignore, files, dirs_scanned);
        } else if file_type.is_file() {
            if let Some(file_entry) = create_file_entry(&path, root) {
                files.push(file_entry);
            }
        }
    }
}

fn should_ignore(name: &str, _path: &Path, patterns: &[&str]) -> bool {
    if name.starts_with('.') && name != ".env.example" && name != ".scoutignore" {
        return true;
    }

    for pattern in patterns {
        if let Some(suffix) = pattern.strip_prefix('*') {
            if name.ends_with(suffix) {
                return true;
            }
        } else if name == *pattern {
            return true;
        }
    }

    false
}

fn create_file_entry(path: &Path, root: &Path) -> Option<FileEntry> {
    let metadata = fs::metadata(path).ok()?;
    let size = metadata.len();

    // Skip very large files (>1MB)
    if size > 1_048_576 {
        return None;
    }

    let relative = path.strip_prefix(root).unwrap_or(path);
    let extension = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Skip binary files
    if is_binary_extension(&extension) {
        return None;
    }

    let line_count = count_lines(path);

    Some(FileEntry {
        path: path.to_path_buf(),
        relative_path: relative.to_string_lossy().to_string(),
        size,
        line_count,
        last_modified: metadata.modified().ok(),
        extension,
    })
}

fn count_lines(path: &Path) -> usize {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return 0,
    };

    let reader = BufReader::with_capacity(65536, file);
    reader.lines().count()
}

fn is_binary_extension(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "ico"
            | "svg"
            | "webp"
            | "bmp"
            | "tiff"
            | "mp3"
            | "mp4"
            | "wav"
            | "avi"
            | "mov"
            | "mkv"
            | "zip"
            | "tar"
            | "gz"
            | "bz2"
            | "xz"
            | "7z"
            | "rar"
            | "pdf"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "o"
            | "a"
            | "wasm"
            | "node"
            | "woff"
            | "woff2"
            | "ttf"
            | "eot"
            | "otf"
    )
}

/// Load ignore patterns from a `.gitignore`- or `.scoutignore`-style file.
///
/// Pragmatic subset of gitignore syntax — enough to cover 95% of real
/// projects without pulling in the full `ignore` crate:
/// - Blank lines and `#`-prefixed comments skipped
/// - Negation (`!foo`) NOT supported (will keep the pattern with `!` prefix,
///   which won't match anything — so negations are silently no-ops)
/// - Leading `/` stripped (we match by basename, not anchored path)
/// - Trailing `/` stripped (directory-only markers become dir-name matches)
/// - Glob patterns beyond leading `*` NOT supported (e.g. `src/**/foo`)
///
/// Good enough for the real ignore content in typical projects: `target/`,
/// `node_modules`, `dist`, `*.lock`, `.env.local`, etc.
fn load_ignore_file(root: &Path, filename: &str) -> Vec<String> {
    let ignore_path = root.join(filename);
    match fs::read_to_string(ignore_path) {
        Ok(content) => content
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.trim_start_matches('/').trim_end_matches('/').to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_scan_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("main.ts"), "console.log('hello');\n").unwrap();
        fs::write(
            root.join("utils.ts"),
            "export const x = 1;\nexport const y = 2;\n",
        )
        .unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/app.ts"), "import { x } from '../utils';\n").unwrap();

        // Should be ignored
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/pkg.js"), "module.exports = {};").unwrap();

        let result = scan_project(root);
        assert_eq!(result.files.len(), 3);
        assert!(result
            .files
            .iter()
            .all(|f| !f.relative_path.contains("node_modules")));
    }

    #[test]
    fn test_line_counting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.ts");
        fs::write(&path, "line1\nline2\nline3\n").unwrap();

        assert_eq!(count_lines(&path), 3);
    }

    #[test]
    fn test_ignore_patterns() {
        assert!(should_ignore(
            "node_modules",
            Path::new("node_modules"),
            DEFAULT_IGNORE
        ));
        assert!(should_ignore(".git", Path::new(".git"), DEFAULT_IGNORE));
        assert!(should_ignore(
            "bundle.min.js",
            Path::new("bundle.min.js"),
            DEFAULT_IGNORE
        ));
        assert!(!should_ignore(
            "main.ts",
            Path::new("main.ts"),
            DEFAULT_IGNORE
        ));
    }

    #[test]
    fn gitignore_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("keep.ts"), "export const a = 1\n").unwrap();
        fs::write(root.join("secret.env"), "KEY=value\n").unwrap();
        fs::create_dir_all(root.join("custom_build/artifacts")).unwrap();
        fs::write(root.join("custom_build/artifacts/out.txt"), "x").unwrap();
        fs::write(
            root.join(".gitignore"),
            "# comment\n\ncustom_build/\n*.env\n",
        )
        .unwrap();

        let result = scan_project(root);
        let paths: Vec<&str> = result
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert!(paths.contains(&"keep.ts"));
        assert!(!paths.iter().any(|p| p.contains("custom_build")));
        assert!(!paths.iter().any(|p| p.ends_with(".env")));
    }

    #[test]
    fn scoutignore_still_works_alongside_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("keep.ts"), "x\n").unwrap();
        fs::create_dir(root.join("generated")).unwrap();
        fs::write(root.join("generated/out.ts"), "x\n").unwrap();
        fs::create_dir(root.join("vendor")).unwrap();
        fs::write(root.join("vendor/lib.ts"), "x\n").unwrap();
        fs::write(root.join(".scoutignore"), "generated\n").unwrap();
        fs::write(root.join(".gitignore"), "vendor/\n").unwrap();

        let result = scan_project(root);
        let paths: Vec<&str> = result
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert!(paths.contains(&"keep.ts"));
        assert!(!paths.iter().any(|p| p.contains("generated")));
        assert!(!paths.iter().any(|p| p.contains("vendor")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("real.ts"), "export const a = 1\n").unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/app.ts"), "export const b = 2\n").unwrap();

        symlink(root, root.join("src/loop")).unwrap();
        symlink(root.join("src"), root.join("mirror")).unwrap();
        symlink(root.join("real.ts"), root.join("alias.ts")).unwrap();

        let result = scan_project(root);
        let paths: Vec<&str> = result
            .files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();
        assert_eq!(result.files.len(), 2);
        assert!(paths.contains(&"real.ts"));
        assert!(paths.contains(&"src/app.ts"));
        assert!(!paths.contains(&"alias.ts"));
        assert!(!paths.iter().any(|p| p.contains("loop")));
        assert!(!paths.iter().any(|p| p.contains("mirror")));
    }

    #[test]
    fn load_ignore_strips_slashes_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join(".gitignore"),
            "# comment\n\n/absolute\ntrailing/\nboth/slashes/\n  spaced  \n",
        )
        .unwrap();
        let patterns = load_ignore_file(root, ".gitignore");
        assert_eq!(
            patterns,
            vec![
                "absolute".to_string(),
                "trailing".to_string(),
                "both/slashes".to_string(),
                "spaced".to_string(),
            ]
        );
    }
}
