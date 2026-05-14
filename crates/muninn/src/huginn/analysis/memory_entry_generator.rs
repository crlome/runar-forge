use crate::types::{EntryType, MemoryEntryInput, MemorySource};

use super::sql_analyzer::{SqlAlter, SqlAnalysis, SqlTable};
use super::{
    AnalysisBody, CrossFilePattern, DeepAnalysis, FileAnalysisResult, LightAnalysis,
    MediumAnalysis, TechDebtMarker,
};

/// Convert a file analysis into one or more Muninn memory entries. Most
/// files yield a single entry, but SQL migrations can fan out into one
/// entry per `CREATE TABLE` plus the per-file structural summary.
pub fn file_entries(result: &FileAnalysisResult, project_id: &str) -> Vec<MemoryEntryInput> {
    let lower = result.file_path.to_lowercase();
    if lower.ends_with(".sql") {
        if let Some(sql_entries) = sql_entries_for_file(result, project_id) {
            return sql_entries;
        }
    }
    file_entry(result, project_id).into_iter().collect()
}

/// Convert a file analysis into a Muninn memory entry input.
pub fn file_entry(result: &FileAnalysisResult, project_id: &str) -> Option<MemoryEntryInput> {
    let is_markdown =
        result.file_path.to_lowercase().ends_with(".md")
            || result.file_path.to_lowercase().ends_with(".mdx");

    let (title, content, entry_type) = match &result.body {
        AnalysisBody::Deep(d) => {
            // Markdown-specific override: classify Architecture vs Context by
            // role (README / CLAUDE / AGENTS / docs / phases vs everything else)
            // rather than by analysis depth.
            let etype = if is_markdown {
                match super::markdown_analyzer::classify_path(&result.file_path) {
                    super::markdown_analyzer::MarkdownRole::Architecture => {
                        EntryType::Architecture
                    }
                    super::markdown_analyzer::MarkdownRole::Contextual => EntryType::Context,
                }
            } else {
                EntryType::Architecture
            };
            (
                format!("{} — {}", result.file_path, short_purpose(&d.purpose)),
                format_deep(&result.file_path, d),
                etype,
            )
        }
        AnalysisBody::Medium(m) => (
            format!("{} — {}", result.file_path, short_purpose(&m.purpose)),
            format_medium(&result.file_path, m),
            EntryType::Context,
        ),
        AnalysisBody::Light(l) => (
            format!("{} — {}", result.file_path, short_purpose(&l.purpose)),
            format_light(&result.file_path, l),
            EntryType::Context,
        ),
        AnalysisBody::Skip => return None,
    };

    let mut tags = vec![project_id.to_string(), "scout".into()];
    if let Some(t) = file_type_tag(&result.file_path) {
        tags.push(t.into());
    }

    Some(MemoryEntryInput {
        title,
        content,
        entry_type,
        source: Some(MemorySource::Scout),
        tags,
        project_id: Some(project_id.to_string()),
        ..Default::default()
    })
}

/// Convert a cross-file pattern into a Pattern memory entry.
pub fn pattern_entry(pattern: &CrossFilePattern, project_id: &str) -> MemoryEntryInput {
    let content = format!(
        "# {}\n\n{}\n\n**Confidence:** {:.0}%\n\n**Files involved ({}):**\n{}\n\n**Summary:**\n{}\n",
        pattern.name,
        pattern.description,
        pattern.confidence * 100.0,
        pattern.involved_files.len(),
        pattern.involved_files.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n"),
        pattern.summary,
    );
    MemoryEntryInput {
        title: format!("Pattern: {}", pattern.name),
        content,
        entry_type: EntryType::Pattern,
        source: Some(MemorySource::Scout),
        tags: vec![
            project_id.into(),
            "scout".into(),
            "pattern".into(),
            pattern.name.clone(),
        ],
        project_id: Some(project_id.into()),
        ..Default::default()
    }
}

/// Convert a tech-debt marker batch (grouped per file) into TechDebt entries.
pub fn techdebt_entries(markers: &[TechDebtMarker], project_id: &str) -> Vec<MemoryEntryInput> {
    use std::collections::HashMap;
    let mut by_file: HashMap<String, Vec<&TechDebtMarker>> = HashMap::new();
    for m in markers {
        by_file.entry(m.file_path.clone()).or_default().push(m);
    }

    let mut out = Vec::new();
    for (file, ms) in by_file {
        let mut content = format!("# Tech debt: {file}\n\n");
        for m in &ms {
            content.push_str(&format!(
                "- **{}** (line {}): {}\n",
                m.marker_type.as_str(),
                m.line,
                m.comment
            ));
        }
        let types: Vec<String> = {
            let mut t: Vec<String> = ms
                .iter()
                .map(|m| m.marker_type.as_str().to_string())
                .collect();
            t.sort();
            t.dedup();
            t
        };
        out.push(MemoryEntryInput {
            title: format!("Tech debt: {} ({} markers)", file, ms.len()),
            content,
            entry_type: EntryType::TechDebt,
            source: Some(MemorySource::Scout),
            tags: {
                let mut tags = vec![
                    project_id.into(),
                    "scout".into(),
                    "techdebt".into(),
                    file.clone(),
                ];
                tags.extend(types.into_iter().map(|t| t.to_lowercase()));
                tags
            },
            project_id: Some(project_id.into()),
            ..Default::default()
        });
    }
    out
}

fn short_purpose(p: &str) -> String {
    let first = p.lines().next().unwrap_or(p);
    let truncated: String = first.chars().take(80).collect();
    truncated
}

fn format_deep(path: &str, a: &DeepAnalysis) -> String {
    let mut s = format!("# {path}\n\n**Purpose:** {}\n", a.purpose);
    if !a.responsibilities.is_empty() {
        s.push_str("\n**Responsibilities:**\n");
        for r in &a.responsibilities {
            s.push_str(&format!("- {r}\n"));
        }
    }
    if !a.key_exports.is_empty() {
        s.push_str(&format!("\n**Exports:** {}\n", a.key_exports.join(", ")));
    }
    if !a.key_imports.is_empty() {
        s.push_str(&format!("\n**Imports:** {}\n", a.key_imports.join(", ")));
    }
    if !a.patterns.is_empty() {
        s.push_str(&format!("\n**Patterns:** {}\n", a.patterns.join(", ")));
    }
    if !a.business_rules.is_empty() {
        s.push_str("\n**Business rules:**\n");
        for r in &a.business_rules {
            s.push_str(&format!("- {r}\n"));
        }
    }
    if !a.gotchas.is_empty() {
        s.push_str("\n**Gotchas:**\n");
        for g in &a.gotchas {
            s.push_str(&format!("- {g}\n"));
        }
    }
    if !a.security_notes.is_empty() {
        s.push_str("\n**Security:**\n");
        for n in &a.security_notes {
            s.push_str(&format!("- {n}\n"));
        }
    }
    if !a.relationships.is_empty() {
        s.push_str("\n**Relationships:**\n");
        for r in &a.relationships {
            s.push_str(&format!("- {r}\n"));
        }
    }
    if !a.tech_debt_markers.is_empty() {
        s.push_str(&format!(
            "\n**Tech-debt markers:** {}\n",
            a.tech_debt_markers.len()
        ));
    }
    s
}

fn format_medium(path: &str, a: &MediumAnalysis) -> String {
    let mut s = format!("# {path}\n\n**Purpose:** {}\n", a.purpose);
    if !a.key_exports.is_empty() {
        s.push_str(&format!("\n**Exports:** {}\n", a.key_exports.join(", ")));
    }
    if !a.import_sources.is_empty() {
        s.push_str(&format!("\n**Imports:** {}\n", a.import_sources.join(", ")));
    }
    if !a.relationships.is_empty() {
        s.push_str("\n**Relationships:**\n");
        for r in &a.relationships {
            s.push_str(&format!("- {r}\n"));
        }
    }
    s
}

fn format_light(path: &str, a: &LightAnalysis) -> String {
    let mut s = format!("# {path}\n\n**Purpose:** {}\n", a.purpose);
    if !a.relationships.is_empty() {
        s.push_str("\n**Relationships:**\n");
        for r in &a.relationships {
            s.push_str(&format!("- {r}\n"));
        }
    }
    s
}

/// Fan out a `.sql` file into (1) an overview entry summarizing the file's
/// schema impact, (2) one entry per `CREATE TABLE`, and (3) one entry per
/// alter action. Returns `None` when the file has no structured SQL
/// content, letting the caller fall back to the generic per-file entry.
fn sql_entries_for_file(
    result: &FileAnalysisResult,
    project_id: &str,
) -> Option<Vec<MemoryEntryInput>> {
    let sql = result.sql_analysis.as_ref()?;
    if sql.tables.is_empty() && sql.alters.is_empty() && sql.indexes.is_empty() {
        return None;
    }

    let mut out = Vec::new();
    let base_tags = || {
        let mut tags = vec![project_id.to_string(), "scout".into(), "sql".into()];
        if let Some(seq) = &sql.migration_sequence {
            tags.push(format!("migration:{seq}"));
        }
        tags
    };

    // ── 1. Overview entry (one per file) ────────────────────────────
    out.push(MemoryEntryInput {
        title: format!(
            "{} — schema summary ({} table(s), {} alter(s))",
            result.file_path,
            sql.tables.len(),
            sql.alters.len()
        ),
        content: format_sql_overview(&result.file_path, sql),
        entry_type: EntryType::Architecture,
        source: Some(MemorySource::Scout),
        tags: base_tags(),
        project_id: Some(project_id.to_string()),
        ..Default::default()
    });

    // ── 2. One entry per CREATE TABLE ───────────────────────────────
    for table in &sql.tables {
        let qualified = match &table.schema {
            Some(s) => format!("{s}.{}", table.name),
            None => table.name.clone(),
        };
        let mut tags = base_tags();
        tags.push(format!("table:{qualified}"));
        out.push(MemoryEntryInput {
            title: format!("Table: {qualified}"),
            content: format_sql_table(table, &result.file_path),
            entry_type: EntryType::Architecture,
            source: Some(MemorySource::Scout),
            tags,
            project_id: Some(project_id.to_string()),
            ..Default::default()
        });
    }

    // ── 3. One entry per ALTER ──────────────────────────────────────
    for alter in &sql.alters {
        let (table, body) = match alter {
            SqlAlter::AddColumn { table, column } => (
                table.clone(),
                format!(
                    "Adds column `{}` `{}` {}",
                    column.name, column.data_type, column.constraints
                ),
            ),
            SqlAlter::DropColumn { table, column } => {
                (table.clone(), format!("Drops column `{column}`"))
            }
            SqlAlter::AddForeignKey(fk) => (
                fk.from_table.clone(),
                format!(
                    "Adds FOREIGN KEY ({}) → {}({})",
                    fk.from_columns, fk.ref_table, fk.ref_columns
                ),
            ),
            SqlAlter::Other { table, raw } => (table.clone(), raw.clone()),
        };
        let mut tags = base_tags();
        tags.push(format!("table:{table}"));
        tags.push("alter".into());
        out.push(MemoryEntryInput {
            title: format!("ALTER {table} — {}", short_alter_label(alter)),
            content: format!(
                "From `{path}`:\n\n{body}\n",
                path = result.file_path,
                body = body
            ),
            entry_type: EntryType::Context,
            source: Some(MemorySource::Scout),
            tags,
            project_id: Some(project_id.to_string()),
            ..Default::default()
        });
    }

    Some(out)
}

fn short_alter_label(a: &SqlAlter) -> &'static str {
    match a {
        SqlAlter::AddColumn { .. } => "ADD COLUMN",
        SqlAlter::DropColumn { .. } => "DROP COLUMN",
        SqlAlter::AddForeignKey(_) => "ADD FOREIGN KEY",
        SqlAlter::Other { .. } => "OTHER",
    }
}

fn format_sql_overview(path: &str, sql: &SqlAnalysis) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Schema summary — `{path}`\n\n"));
    if let Some(seq) = &sql.migration_sequence {
        out.push_str(&format!("**Migration sequence:** `{seq}`\n"));
    }
    if let Some(name) = &sql.migration_name {
        out.push_str(&format!("**Migration name:** `{name}`\n"));
    }
    out.push('\n');

    if !sql.tables.is_empty() {
        out.push_str("## Tables\n");
        for t in &sql.tables {
            let q = match &t.schema {
                Some(s) => format!("{s}.{}", t.name),
                None => t.name.clone(),
            };
            out.push_str(&format!(
                "- `{q}` ({} columns, {} inline FK)\n",
                t.columns.len(),
                t.inline_foreign_keys.len()
            ));
        }
        out.push('\n');
    }
    if !sql.alters.is_empty() {
        out.push_str(&format!("## Alters: {}\n", sql.alters.len()));
    }
    if !sql.indexes.is_empty() {
        out.push_str("## Indexes\n");
        for i in &sql.indexes {
            out.push_str(&format!(
                "- `{}` on `{}` ({}){}\n",
                i.name,
                i.table,
                i.columns,
                if i.unique { " UNIQUE" } else { "" }
            ));
        }
    }
    out
}

fn format_sql_table(t: &SqlTable, path: &str) -> String {
    let qualified = match &t.schema {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    };
    let mut out = format!("# Table `{qualified}`\n\nDefined in `{path}`.\n\n");
    out.push_str("## Columns\n\n");
    for c in &t.columns {
        out.push_str(&format!(
            "- `{}` `{}` {}\n",
            c.name,
            c.data_type,
            c.constraints
        ));
    }
    if !t.inline_foreign_keys.is_empty() {
        out.push_str("\n## Foreign Keys\n\n");
        for fk in &t.inline_foreign_keys {
            out.push_str(&format!(
                "- `({})` → `{}({})`\n",
                fk.from_columns, fk.ref_table, fk.ref_columns
            ));
        }
    }
    out
}

fn file_type_tag(path: &str) -> Option<&'static str> {
    let lower = path.to_lowercase();
    if lower.contains(".test.") || lower.contains(".spec.") {
        Some("test")
    } else if lower.contains("migration") {
        Some("migration")
    } else if lower.ends_with(".md") {
        Some("docs")
    } else if lower.ends_with(".sql") {
        Some("sql")
    } else if lower.contains(".service.") {
        Some("service")
    } else if lower.contains(".controller.") {
        Some("controller")
    } else if lower.contains(".module.") {
        Some("module")
    } else {
        None
    }
}
