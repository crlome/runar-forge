use std::time::SystemTime;

use crate::huginn::analyzer::AnalysisDepth;

use super::graph_builder::fan_in;
use super::{DependencyGraph, FileNode, ImportanceScore, ScoreComponents};

pub struct ImportanceScorer;

impl ImportanceScorer {
    pub fn score_all(
        graph: &DependencyGraph,
    ) -> std::collections::HashMap<String, ImportanceScore> {
        let mut out = std::collections::HashMap::new();
        for (path, node) in &graph.nodes {
            out.insert(path.clone(), Self::score_file(node, graph));
        }
        out
    }

    pub fn score_file(node: &FileNode, graph: &DependencyGraph) -> ImportanceScore {
        let entry_proximity = calc_entry_proximity(node);
        let import_frequency = calc_import_frequency(&node.path, graph);
        let file_type_signal = file_type_score(&node.path);
        let size_signal = calc_size_signal(node.line_count);
        let recency_signal = calc_recency_signal(node.last_modified);

        let total =
            (entry_proximity + import_frequency + file_type_signal + size_signal + recency_signal)
                .clamp(0, 100);

        let analysis_depth = if total >= 65 {
            AnalysisDepth::Deep
        } else if total >= 40 {
            AnalysisDepth::Medium
        } else if total >= 20 {
            AnalysisDepth::Light
        } else {
            AnalysisDepth::Skip
        };

        ImportanceScore {
            total,
            components: ScoreComponents {
                entry_proximity,
                import_frequency,
                file_type_signal,
                size_signal,
                recency_signal,
            },
            analysis_depth,
        }
    }
}

fn calc_entry_proximity(node: &FileNode) -> i32 {
    if node.is_entry_point {
        30
    } else {
        (30 - node.depth * 5).max(0)
    }
}

fn calc_import_frequency(path: &str, graph: &DependencyGraph) -> i32 {
    let n = fan_in(graph, path);
    if n >= 10 {
        25
    } else if n >= 5 {
        20
    } else if n >= 3 {
        15
    } else if n >= 1 {
        10
    } else {
        0
    }
}

fn calc_size_signal(lines: usize) -> i32 {
    if lines > 500 {
        10
    } else if lines > 200 {
        7
    } else if lines > 50 {
        4
    } else if lines < 20 {
        -5
    } else {
        0
    }
}

fn calc_recency_signal(last_modified: Option<SystemTime>) -> i32 {
    let lm = match last_modified {
        Some(t) => t,
        None => return 0,
    };
    let age = match SystemTime::now().duration_since(lm) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => return 10, // future timestamp = treat as very recent
    };
    let days = age / 86_400;
    if days < 7 {
        10
    } else if days < 30 {
        7
    } else if days < 90 {
        4
    } else if days < 180 {
        2
    } else {
        0
    }
}

/// File-type score — range roughly -8..40. Ports the TS
/// `getFileTypeScore` function using string ops.
pub fn file_type_score(file_path: &str) -> i32 {
    let filename = file_path.rsplit('/').next().unwrap_or(file_path);
    let fname_lower = filename.to_lowercase();
    let path_lower = file_path.to_lowercase();
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| format!(".{}", e.to_lowercase()))
        .unwrap_or_default();

    // Universal signals
    if fname_lower == "claude.md" {
        return 40;
    }
    if fname_lower == "readme.md" {
        return 30;
    }
    if fname_lower == "agents.md" {
        return 30;
    }
    if path_lower.contains("/phases/") && path_lower.ends_with(".md") {
        return 25;
    }
    if ext == ".md" || ext == ".mdx" {
        return 15;
    }

    if fname_lower.starts_with("docker-compose") && (ext == ".yaml" || ext == ".yml") {
        return 18;
    }
    if fname_lower == "dockerfile" {
        return 16;
    }
    if fname_lower == "makefile" {
        return 14;
    }
    if fname_lower == ".env.example" {
        return 14;
    }

    if (path_lower.contains("migration") || path_lower.contains("migrations"))
        && matches!(
            ext.as_str(),
            ".sql" | ".xml" | ".json" | ".rb" | ".py" | ".java" | ".cs"
        )
    {
        return 28;
    }
    if ext == ".sql" {
        return 28;
    }

    // Entry points by name (universal)
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s.to_lowercase())
        .unwrap_or(fname_lower.clone());
    if matches!(
        stem.as_str(),
        "main" | "app" | "server" | "bootstrap" | "startup"
    ) && !ext.is_empty()
    {
        return 25;
    }
    if matches!(stem.as_str(), "program" | "application") && !ext.is_empty() {
        return 22;
    }

    // Test files
    if path_lower.contains(".spec.") || path_lower.contains(".test.") {
        return 5;
    }
    if path_lower.contains("__tests__")
        || path_lower.contains("__test__")
        || path_lower.contains("__specs__")
        || path_lower.contains("__spec__")
    {
        return 3;
    }
    if (path_lower.contains("/tests/") || path_lower.contains("/test/"))
        && !path_lower.contains("/util")
    {
        return 3;
    }

    // TS/JS
    if matches!(
        ext.as_str(),
        ".ts" | ".tsx" | ".js" | ".jsx" | ".mjs" | ".cjs"
    ) {
        if path_lower.ends_with("tsconfig.base.json") {
            return 20;
        }
        if fname_lower.starts_with("tsconfig") && ext == ".json" {
            return 15;
        }
        if fname_lower == "package.json" && !path_lower.contains("node_modules") {
            return 18;
        }

        if path_lower.ends_with(".module.ts") {
            return 22;
        }
        if path_lower.ends_with(".service.ts") {
            return 18;
        }
        if path_lower.ends_with(".controller.ts") {
            return 18;
        }
        if path_lower.ends_with(".entity.ts") {
            return 22;
        }
        if path_lower.ends_with(".guard.ts") {
            return 16;
        }
        if path_lower.ends_with(".middleware.ts") {
            return 16;
        }
        if path_lower.ends_with(".decorator.ts") {
            return 14;
        }

        if path_lower.contains("/migrations/") || path_lower.contains("/migration/") {
            return 25;
        }
        if path_lower.ends_with(".dto.ts") {
            return 12;
        }
        if path_lower.ends_with(".config.ts") {
            return 12;
        }
        if path_lower.ends_with(".schema.ts") || path_lower.ends_with(".model.ts") {
            return 20;
        }
        if path_lower.ends_with(".repository.ts") {
            return 16;
        }
        if path_lower.ends_with(".factory.ts") {
            return 10;
        }

        if path_lower.contains("/storage/")
            || path_lower.contains("/adapters/")
            || path_lower.contains("/adapter/")
        {
            return 20;
        }
        if path_lower.contains("/mcp/") {
            return 20;
        }
        if path_lower.contains("/crawl/")
            || path_lower.contains("/graph/")
            || path_lower.contains("/analysis/")
            || path_lower.contains("/scoring/")
        {
            return 18;
        }
        if path_lower.contains("/scanner/")
            || path_lower.contains("/discovery/")
            || path_lower.contains("/bridge/")
        {
            return 16;
        }
        if fname_lower.contains("interface")
            || fname_lower.contains("adapter")
            || fname_lower.contains("bridge")
        {
            return 18;
        }

        if (path_lower.contains("/pages/") || path_lower.contains("/app/"))
            && (ext == ".tsx" || ext == ".jsx")
        {
            return 18;
        }
        if path_lower.contains("/components/") {
            return 10;
        }
        if path_lower.contains("/hooks/") {
            return 12;
        }
        if path_lower.contains("/context/") {
            return 14;
        }
        if path_lower.contains("/store/") {
            return 14;
        }

        if path_lower.contains("/packages/") && path_lower.ends_with("/src/index.ts") {
            return 14;
        }
        if path_lower.ends_with("/index.ts")
            || path_lower.ends_with("/index.js")
            || path_lower.ends_with("/index.tsx")
            || path_lower.ends_with("/index.jsx")
            || fname_lower == "index.ts"
            || fname_lower == "index.js"
            || fname_lower == "index.tsx"
            || fname_lower == "index.jsx"
        {
            return -8;
        }
        if path_lower.contains("/constants/") || path_lower.contains("/constant/") {
            return 6;
        }
        if path_lower.contains("/utils/") || path_lower.contains("/util/") {
            return 6;
        }
        if path_lower.contains("/types/") || path_lower.contains("/type/") {
            return 8;
        }

        return 5;
    }

    // JSON config files (outside TS/JS block)
    if path_lower.ends_with("tsconfig.base.json") {
        return 20;
    }
    if fname_lower.starts_with("tsconfig") && ext == ".json" {
        return 15;
    }
    if fname_lower == "package.json" && !path_lower.contains("node_modules") {
        return 18;
    }

    // Python
    if ext == ".py" {
        if fname_lower == "__init__.py" {
            return -5;
        }
        if matches!(fname_lower.as_str(), "model.py" | "models.py" | "schema.py") {
            return 22;
        }
        if matches!(
            fname_lower.as_str(),
            "view.py" | "views.py" | "route.py" | "routes.py" | "api.py"
        ) {
            return 18;
        }
        if matches!(
            fname_lower.as_str(),
            "middleware.py" | "decorator.py" | "decorators.py"
        ) {
            return 16;
        }
        if matches!(
            fname_lower.as_str(),
            "setting.py" | "settings.py" | "config.py"
        ) {
            return 18;
        }
        if matches!(
            fname_lower.as_str(),
            "serializer.py" | "serializers.py" | "validator.py" | "validators.py"
        ) {
            return 14;
        }
        if matches!(fname_lower.as_str(), "task.py" | "tasks.py" | "celery.py") {
            return 14;
        }
        if matches!(fname_lower.as_str(), "manage.py" | "wsgi.py" | "asgi.py") {
            return 20;
        }
        return 5;
    }

    // Go
    if ext == ".go" {
        if fname_lower == "main.go" {
            return 25;
        }
        if path_lower.ends_with("_test.go") {
            return 3;
        }
        if fname_lower.contains("handler") {
            return 18;
        }
        if fname_lower.contains("router") {
            return 20;
        }
        if fname_lower.contains("service") {
            return 18;
        }
        if fname_lower.contains("repository") || fname_lower.contains("store") {
            return 20;
        }
        if fname_lower.contains("middleware") {
            return 16;
        }
        return 5;
    }

    // Rust
    if ext == ".rs" {
        if fname_lower == "main.rs" {
            return 25;
        }
        if fname_lower == "lib.rs" {
            return 22;
        }
        if fname_lower == "mod.rs" {
            return 15;
        }
        if fname_lower.contains("handler") {
            return 18;
        }
        if fname_lower.contains("router") {
            return 20;
        }
        return 5;
    }

    // YAML
    if ext == ".yaml" || ext == ".yml" {
        return 10;
    }

    3
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_node(path: &str, deps: i32, lines: usize, entry: bool) -> FileNode {
        FileNode {
            path: path.to_string(),
            absolute_path: std::path::PathBuf::from(path),
            extension: path.rsplit('.').next().unwrap_or("").to_string(),
            imports: vec![],
            resolved_imports: vec![],
            export_count: 0,
            line_count: lines,
            last_modified: Some(SystemTime::now() - Duration::from_secs(3 * 86400)),
            is_entry_point: entry,
            depth: deps,
        }
    }

    #[test]
    fn file_type_scoring_matches_ts() {
        assert_eq!(file_type_score("CLAUDE.md"), 40);
        assert_eq!(file_type_score("README.md"), 30);
        assert_eq!(file_type_score("src/app.module.ts"), 22);
        assert_eq!(file_type_score("src/foo.service.ts"), 18);
        assert_eq!(file_type_score("src/index.ts"), -8);
        assert_eq!(file_type_score("main.rs"), 25);
        assert_eq!(file_type_score("lib.rs"), 22);
        assert_eq!(file_type_score("main.go"), 25);
        assert_eq!(file_type_score("__init__.py"), -5);
        assert_eq!(file_type_score("db/001_init.sql"), 28);
    }

    #[test]
    fn entry_point_gets_30_proximity() {
        let node = make_node("main.rs", 0, 100, true);
        assert_eq!(calc_entry_proximity(&node), 30);
    }

    #[test]
    fn score_buckets_into_depths() {
        let graph = DependencyGraph::default();
        let node = make_node("src/app.module.ts", 0, 300, true);
        let score = ImportanceScorer::score_file(&node, &graph);
        // 30 (entry) + 0 (fan_in=0) + 22 (module.ts) + 7 (size 300) + 10 (recent) = 69
        assert!(score.total >= 65);
        assert_eq!(score.analysis_depth, AnalysisDepth::Deep);
    }

    #[test]
    fn barrel_files_skip() {
        let graph = DependencyGraph::default();
        let node = make_node("src/index.ts", 2, 10, false);
        let score = ImportanceScorer::score_file(&node, &graph);
        // 20 + 0 + (-8) + (-5) + 10 = 17 → skip
        assert_eq!(score.analysis_depth, AnalysisDepth::Skip);
    }
}
