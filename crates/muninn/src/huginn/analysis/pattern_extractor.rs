use std::collections::HashMap;

use super::{AnalysisBody, CrossFilePattern, FileAnalysisResult};

const MIN_CONFIDENCE: f64 = 0.6;

pub fn extract(analyses: &[FileAnalysisResult]) -> Vec<CrossFilePattern> {
    let index = build_index(analyses);
    let mut out = Vec::new();
    for detector in DETECTORS {
        let matching = find_matching(analyses, detector.signals);
        if matching.len() < 2 {
            continue;
        }
        let pattern = (detector.run)(&matching, &index);
        if pattern.confidence >= MIN_CONFIDENCE {
            out.push(pattern);
        }
    }
    // Language-specific detectors that don't fit the signal/matching shape.
    out.extend(detect_rust_trait_polymorphism(analyses));
    out
}

fn build_index(analyses: &[FileAnalysisResult]) -> HashMap<String, &FileAnalysisResult> {
    analyses.iter().map(|a| (a.file_path.clone(), a)).collect()
}

fn find_matching(analyses: &[FileAnalysisResult], signals: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for a in analyses {
        if matches!(a.body, AnalysisBody::Skip) {
            continue;
        }
        let path_lower = a.file_path.to_lowercase();
        let purpose_lower = match &a.body {
            AnalysisBody::Deep(d) => d.purpose.to_lowercase(),
            AnalysisBody::Medium(m) => m.purpose.to_lowercase(),
            AnalysisBody::Light(l) => l.purpose.to_lowercase(),
            AnalysisBody::Skip => String::new(),
        };
        if signals
            .iter()
            .any(|s| path_lower.contains(s) || purpose_lower.contains(s))
        {
            out.push(a.file_path.clone());
        }
    }
    out
}

fn filter_files<'a>(files: &'a [String], needle: &str) -> Vec<&'a String> {
    files
        .iter()
        .filter(|f| f.to_lowercase().contains(needle))
        .collect()
}

type DetectFn =
    fn(files: &[String], index: &HashMap<String, &FileAnalysisResult>) -> CrossFilePattern;

struct Detector {
    /// Label shown in log output — the detect fn emits its own CrossFilePattern.name.
    #[allow(dead_code)]
    name: &'static str,
    /// Human-readable doc — the detect fn emits its own CrossFilePattern.description.
    #[allow(dead_code)]
    description: &'static str,
    signals: &'static [&'static str],
    run: DetectFn,
}

const DETECTORS: &[Detector] = &[
    Detector {
        name: "authentication-flow",
        description: "How authentication works end-to-end",
        signals: &[
            "guard", "strategy", "jwt", "passport", "session", "auth", "bearer", "token",
        ],
        run: detect_auth,
    },
    Detector {
        name: "data-access-pattern",
        description: "How data is accessed (repository, ORM, query builder)",
        signals: &[
            "repository",
            "typeorm",
            "prisma",
            "sequelize",
            "entity",
            "model",
            ".schema.",
        ],
        run: detect_data_access,
    },
    Detector {
        name: "error-handling",
        description: "How errors are handled and propagated",
        signals: &[
            "exception",
            ".filter.",
            "error-handler",
            "error.handler",
            "error-middleware",
        ],
        run: detect_error_handling,
    },
    Detector {
        name: "dependency-injection",
        description: "How dependencies are wired through the app",
        signals: &[
            "module",
            "provider",
            "injectable",
            "inject",
            "container",
            "di.",
        ],
        run: detect_di,
    },
    Detector {
        name: "middleware-chain",
        description: "HTTP/RPC middleware pipeline",
        signals: &["middleware", "interceptor", "pipe.", ".use("],
        run: detect_middleware,
    },
    Detector {
        name: "validation-layer",
        description: "Input validation and schema enforcement",
        signals: &["validator", "schema", "zod", "joi", "yup", "dto"],
        run: detect_validation,
    },
    Detector {
        name: "factory-pattern",
        description: "Object creation via factories",
        signals: &["factory", ".create", "newinstance"],
        run: detect_factory,
    },
    Detector {
        name: "logger-surface",
        description: "Logging infrastructure",
        signals: &["logger", "logging", "pino", "winston", "bunyan", "tracing"],
        run: detect_logger,
    },
    Detector {
        name: "cache-layer",
        description: "Cache infrastructure",
        signals: &["cache", "redis", "memcached", "lru"],
        run: detect_cache,
    },
    Detector {
        name: "queue-worker",
        description: "Background job / message queue layer",
        signals: &["queue", "worker", "job", "bullmq", "sqs", "kafka"],
        run: detect_queue,
    },
    Detector {
        name: "event-bus",
        description: "Event-driven communication between modules",
        signals: &["eventbus", "event-bus", "emitter", "pubsub", "pub-sub"],
        run: detect_event_bus,
    },
    Detector {
        name: "config-surface",
        description: "Application configuration",
        signals: &["config", ".env", "settings"],
        run: detect_config,
    },
];

fn detect_auth(files: &[String], _idx: &HashMap<String, &FileAnalysisResult>) -> CrossFilePattern {
    let guards = filter_files(files, "guard");
    let strategies = filter_files(files, "strategy");
    let services = files
        .iter()
        .filter(|f| f.to_lowercase().contains("auth") && f.to_lowercase().contains("service"))
        .collect::<Vec<_>>();
    let controllers = files
        .iter()
        .filter(|f| f.to_lowercase().contains("auth") && f.to_lowercase().contains("controller"))
        .collect::<Vec<_>>();
    let mut chain = Vec::new();
    for f in &controllers {
        chain.push(format!("controller: {f}"));
    }
    for f in &guards {
        chain.push(format!("guard: {f}"));
    }
    for f in &strategies {
        chain.push(format!("strategy: {f}"));
    }
    for f in &services {
        chain.push(format!("service: {f}"));
    }
    CrossFilePattern {
        name: "authentication-flow".into(),
        description: "How authentication works end-to-end".into(),
        involved_files: files.to_vec(),
        summary: format!(
            "Auth flows through: {}\nTotal auth-related files: {}",
            chain.join(" → "),
            files.len()
        ),
        confidence: (files.len() as f64 / 4.0).min(1.0),
    }
}

fn detect_data_access(
    files: &[String],
    idx: &HashMap<String, &FileAnalysisResult>,
) -> CrossFilePattern {
    let entities = files
        .iter()
        .filter(|f| {
            let l = f.to_lowercase();
            l.contains("entity") || l.contains("model") || l.contains("schema")
        })
        .cloned()
        .collect::<Vec<_>>();
    let repos = filter_files(files, "repository")
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();

    let mut orm = "unknown".to_string();
    for path in files {
        if let Some(a) = idx.get(path) {
            let imports: &[String] = match &a.body {
                AnalysisBody::Deep(d) => &d.key_imports,
                AnalysisBody::Medium(m) => &m.import_sources,
                _ => &[],
            };
            for imp in imports {
                let il = imp.to_lowercase();
                if il.contains("typeorm") {
                    orm = "TypeORM".into();
                    break;
                }
                if il.contains("prisma") {
                    orm = "Prisma".into();
                    break;
                }
                if il.contains("sequelize") {
                    orm = "Sequelize".into();
                    break;
                }
                if il.contains("drizzle") {
                    orm = "Drizzle".into();
                    break;
                }
                if il.contains("mongoose") {
                    orm = "Mongoose".into();
                    break;
                }
                if il.contains("sqlx") {
                    orm = "sqlx".into();
                    break;
                }
                if il.contains("diesel") {
                    orm = "Diesel".into();
                    break;
                }
                if il.contains("rusqlite") {
                    orm = "rusqlite".into();
                    break;
                }
                if il.contains("tokio-postgres") {
                    orm = "tokio-postgres".into();
                    break;
                }
            }
        }
    }
    CrossFilePattern {
        name: "data-access-pattern".into(),
        description: "How data is accessed (repository, ORM, query builder)".into(),
        involved_files: files.to_vec(),
        summary: format!(
            "Data access via {}. Entities: {}. Repositories: {}.",
            orm,
            if entities.is_empty() {
                "none".into()
            } else {
                entities.join(", ")
            },
            if repos.is_empty() {
                "none".into()
            } else {
                repos.join(", ")
            },
        ),
        confidence: ((entities.len() + repos.len()) as f64 / 4.0).min(1.0),
    }
}

fn detect_error_handling(
    files: &[String],
    _idx: &HashMap<String, &FileAnalysisResult>,
) -> CrossFilePattern {
    let filters: Vec<&String> = filter_files(files, "filter");
    let handlers: Vec<&String> = files
        .iter()
        .filter(|f| {
            let l = f.to_lowercase();
            l.contains("handler") || l.contains("middleware")
        })
        .collect();
    CrossFilePattern {
        name: "error-handling".into(),
        description: "How errors are handled and propagated".into(),
        involved_files: files.to_vec(),
        summary: format!(
            "Filters: {}. Handlers/middleware: {}.",
            filters.len(),
            handlers.len()
        ),
        confidence: (files.len() as f64 / 3.0).min(1.0),
    }
}

fn detect_di(files: &[String], _idx: &HashMap<String, &FileAnalysisResult>) -> CrossFilePattern {
    let modules = filter_files(files, "module").len();
    let providers = filter_files(files, "provider").len();
    CrossFilePattern {
        name: "dependency-injection".into(),
        description: "How dependencies are wired through the app".into(),
        involved_files: files.to_vec(),
        summary: format!(
            "Modules: {modules}. Providers: {providers}. Total DI files: {}.",
            files.len()
        ),
        confidence: (files.len() as f64 / 3.0).min(1.0),
    }
}

fn detect_middleware(
    files: &[String],
    _idx: &HashMap<String, &FileAnalysisResult>,
) -> CrossFilePattern {
    CrossFilePattern {
        name: "middleware-chain".into(),
        description: "HTTP/RPC middleware pipeline".into(),
        involved_files: files.to_vec(),
        summary: format!("{} middleware/interceptor file(s) detected.", files.len()),
        confidence: (files.len() as f64 / 3.0).min(1.0),
    }
}

fn detect_validation(
    files: &[String],
    _idx: &HashMap<String, &FileAnalysisResult>,
) -> CrossFilePattern {
    CrossFilePattern {
        name: "validation-layer".into(),
        description: "Input validation and schema enforcement".into(),
        involved_files: files.to_vec(),
        summary: format!("{} validator/DTO file(s) found.", files.len()),
        confidence: (files.len() as f64 / 3.0).min(1.0),
    }
}

fn detect_factory(
    files: &[String],
    _idx: &HashMap<String, &FileAnalysisResult>,
) -> CrossFilePattern {
    CrossFilePattern {
        name: "factory-pattern".into(),
        description: "Object creation via factories".into(),
        involved_files: files.to_vec(),
        summary: format!("{} factory file(s) detected.", files.len()),
        confidence: (files.len() as f64 / 3.0).min(1.0),
    }
}

fn detect_logger(
    files: &[String],
    _idx: &HashMap<String, &FileAnalysisResult>,
) -> CrossFilePattern {
    CrossFilePattern {
        name: "logger-surface".into(),
        description: "Logging infrastructure".into(),
        involved_files: files.to_vec(),
        summary: format!("{} logger/tracing file(s).", files.len()),
        confidence: (files.len() as f64 / 2.0).min(1.0),
    }
}

fn detect_cache(files: &[String], _idx: &HashMap<String, &FileAnalysisResult>) -> CrossFilePattern {
    CrossFilePattern {
        name: "cache-layer".into(),
        description: "Cache infrastructure".into(),
        involved_files: files.to_vec(),
        summary: format!("{} cache-related file(s).", files.len()),
        confidence: (files.len() as f64 / 2.0).min(1.0),
    }
}

fn detect_queue(files: &[String], _idx: &HashMap<String, &FileAnalysisResult>) -> CrossFilePattern {
    CrossFilePattern {
        name: "queue-worker".into(),
        description: "Background job / message queue layer".into(),
        involved_files: files.to_vec(),
        summary: format!("{} queue/worker file(s).", files.len()),
        confidence: (files.len() as f64 / 2.0).min(1.0),
    }
}

fn detect_event_bus(
    files: &[String],
    _idx: &HashMap<String, &FileAnalysisResult>,
) -> CrossFilePattern {
    CrossFilePattern {
        name: "event-bus".into(),
        description: "Event-driven communication between modules".into(),
        involved_files: files.to_vec(),
        summary: format!("{} event-bus/emitter file(s).", files.len()),
        confidence: (files.len() as f64 / 2.0).min(1.0),
    }
}

fn detect_config(
    files: &[String],
    _idx: &HashMap<String, &FileAnalysisResult>,
) -> CrossFilePattern {
    CrossFilePattern {
        name: "config-surface".into(),
        description: "Application configuration".into(),
        involved_files: files.to_vec(),
        summary: format!("{} config file(s).", files.len()),
        confidence: (files.len() as f64 / 3.0).min(1.0),
    }
}

/// Scan Rust trait-impl pairs recorded on each `DeepAnalysis` and surface a
/// cross-file pattern whenever a single trait has ≥2 implementations across
/// different files. Common names (`storage`, `adapter`, `provider`, etc.)
/// upgrade the pattern label from generic `trait-polymorphism` to the more
/// specific `storage-adapter` idiom, matching Phase 4.8 item 4.8.7.
fn detect_rust_trait_polymorphism(analyses: &[FileAnalysisResult]) -> Vec<CrossFilePattern> {
    use std::collections::BTreeMap;

    // trait_name -> Vec<(file_path, type_name)>
    let mut by_trait: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();

    for a in analyses {
        for imp in &a.rust_trait_impls {
            by_trait
                .entry(imp.trait_name.clone())
                .or_default()
                .push((a.file_path.clone(), imp.type_name.clone()));
        }
    }

    let mut out = Vec::new();
    for (trait_name, entries) in by_trait {
        // Unique files (same file can contain multiple impls; we still want
        // ≥2 *distinct* files before calling this a cross-file pattern).
        let mut files: Vec<String> = entries.iter().map(|(f, _)| f.clone()).collect();
        files.sort();
        files.dedup();
        if files.len() < 2 {
            continue;
        }

        let types: Vec<String> = {
            let mut t: Vec<String> = entries.iter().map(|(_, ty)| ty.clone()).collect();
            t.sort();
            t.dedup();
            t
        };

        let lower = trait_name.to_lowercase();
        let label_is_storage = ["storage", "adapter", "repository", "store", "backend"]
            .iter()
            .any(|kw| lower.contains(kw));

        let (name, description) = if label_is_storage {
            (
                "storage-adapter".to_string(),
                format!(
                    "Trait-based adapter pattern: `{}` is implemented by {} storage backends.",
                    trait_name,
                    types.len()
                ),
            )
        } else {
            (
                "trait-polymorphism".to_string(),
                format!(
                    "Trait `{}` has {} implementations across {} files — classic strategy/adapter pattern.",
                    trait_name,
                    types.len(),
                    files.len()
                ),
            )
        };

        // Confidence scales with impl count; saturates at 1.0 for ≥4 impls.
        let confidence = ((types.len() as f64) / 4.0).clamp(MIN_CONFIDENCE, 1.0);

        let summary = format!(
            "trait `{}` implemented by: {}",
            trait_name,
            types.join(", ")
        );

        out.push(CrossFilePattern {
            name,
            description,
            involved_files: files,
            summary,
            confidence,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huginn::analysis::{DeepAnalysis, FileAnalysisResult};
    use crate::huginn::analyzer::AnalysisDepth;

    fn mk(path: &str) -> FileAnalysisResult {
        FileAnalysisResult {
            file_path: path.into(),
            extension: "ts".into(),
            line_count: 50,
            depth: AnalysisDepth::Deep,
            body: AnalysisBody::Deep(DeepAnalysis {
                purpose: "module".into(),
                ..Default::default()
            }),
            rust_trait_impls: Vec::new(),
            sql_analysis: None,
        }
    }

    #[test]
    fn detects_auth_with_three_files() {
        let analyses = vec![
            mk("src/auth/auth.controller.ts"),
            mk("src/auth/jwt.strategy.ts"),
            mk("src/auth/auth.guard.ts"),
        ];
        let patterns = extract(&analyses);
        assert!(patterns.iter().any(|p| p.name == "authentication-flow"));
    }

    #[test]
    fn no_pattern_with_single_signal() {
        let analyses = vec![mk("src/cache.ts")];
        let patterns = extract(&analyses);
        assert!(patterns.is_empty());
    }

    fn mk_rust_with_impls(path: &str, impls: Vec<(&str, &str)>) -> FileAnalysisResult {
        FileAnalysisResult {
            file_path: path.into(),
            extension: "rs".into(),
            line_count: 100,
            depth: AnalysisDepth::Medium,
            body: AnalysisBody::Medium(super::super::MediumAnalysis {
                purpose: "module".into(),
                ..Default::default()
            }),
            rust_trait_impls: impls
                .into_iter()
                .map(|(t, ty)| crate::huginn::analysis::TraitImpl {
                    trait_name: t.into(),
                    type_name: ty.into(),
                })
                .collect(),
            sql_analysis: None,
        }
    }

    #[test]
    fn detects_storage_adapter_pattern() {
        let analyses = vec![
            mk_rust_with_impls(
                "src/storage/sqlite.rs",
                vec![("MemoryStorage", "SqliteAdapter")],
            ),
            mk_rust_with_impls(
                "src/storage/postgres.rs",
                vec![("MemoryStorage", "PostgresAdapter")],
            ),
        ];
        let patterns = extract(&analyses);
        let p = patterns
            .iter()
            .find(|p| p.name == "storage-adapter")
            .expect("storage-adapter pattern");
        assert_eq!(p.involved_files.len(), 2);
        assert!(p.summary.contains("SqliteAdapter"));
        assert!(p.summary.contains("PostgresAdapter"));
    }

    #[test]
    fn detects_generic_trait_polymorphism() {
        let analyses = vec![
            mk_rust_with_impls("src/a.rs", vec![("Formatter", "JsonFormatter")]),
            mk_rust_with_impls("src/b.rs", vec![("Formatter", "YamlFormatter")]),
            mk_rust_with_impls("src/c.rs", vec![("Formatter", "TomlFormatter")]),
        ];
        let patterns = extract(&analyses);
        assert!(patterns.iter().any(|p| p.name == "trait-polymorphism"));
    }

    #[test]
    fn single_impl_does_not_emit_pattern() {
        let analyses = vec![mk_rust_with_impls(
            "src/only.rs",
            vec![("Lone", "LoneType")],
        )];
        let patterns = extract(&analyses);
        assert!(patterns.iter().all(|p| p.name != "storage-adapter"
            && p.name != "trait-polymorphism"));
    }
}
