//! Onboarding report generator.
//!
//! Pulls architecture summary, decisions, patterns, and tech-debt entries
//! for a project and renders a single Markdown report aimed at someone
//! joining the project for the first time.

use serde::Serialize;

use crate::librarian::MemoryLibrarian;
use crate::storage::StorageResult;
use crate::types::{EntryType, ListFilters, MemoryEntry};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingSection {
    pub heading: String,
    pub entries: Vec<OnboardingItem>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingItem {
    pub title: String,
    pub entry_type: String,
    pub excerpt: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingReport {
    pub project_id: Option<String>,
    pub sections: Vec<OnboardingSection>,
    pub markdown: String,
    pub total_entries: usize,
}

pub async fn generate(
    librarian: &MemoryLibrarian,
    project_id: Option<&str>,
) -> StorageResult<OnboardingReport> {
    let architecture = fetch(librarian, project_id, EntryType::Architecture, 5).await?;
    let decisions = fetch(librarian, project_id, EntryType::Decision, 8).await?;
    let patterns = fetch(librarian, project_id, EntryType::Pattern, 8).await?;
    let tech_debt = fetch(librarian, project_id, EntryType::TechDebt, 6).await?;
    let rules = fetch(librarian, project_id, EntryType::Rule, 5).await?;

    let total =
        architecture.len() + decisions.len() + patterns.len() + tech_debt.len() + rules.len();

    let sections = vec![
        section("Architecture Summary", &architecture),
        section("Key Decisions", &decisions),
        section("Patterns & Conventions", &patterns),
        section("Rules & Constraints", &rules),
        section("Tech Debt to Know", &tech_debt),
    ];

    let markdown = render_markdown(project_id, &sections);

    Ok(OnboardingReport {
        project_id: project_id.map(|s| s.to_string()),
        sections,
        markdown,
        total_entries: total,
    })
}

async fn fetch(
    librarian: &MemoryLibrarian,
    project_id: Option<&str>,
    entry_type: EntryType,
    limit: usize,
) -> StorageResult<Vec<MemoryEntry>> {
    librarian
        .list(ListFilters {
            project_id: project_id.map(|s| s.to_string()),
            entry_type: Some(entry_type),
            limit: Some(limit),
            ..Default::default()
        })
        .await
}

fn section(heading: &str, entries: &[MemoryEntry]) -> OnboardingSection {
    OnboardingSection {
        heading: heading.to_string(),
        entries: entries
            .iter()
            .map(|e| OnboardingItem {
                title: e.title.clone(),
                entry_type: e.entry_type.as_str().to_string(),
                excerpt: truncate(&e.content, 280),
                created_at: e.created_at.format("%Y-%m-%d").to_string(),
            })
            .collect(),
    }
}

fn render_markdown(project_id: Option<&str>, sections: &[OnboardingSection]) -> String {
    let mut out: Vec<String> = Vec::new();
    let title = project_id
        .map(|p| format!("# Onboarding: {p}"))
        .unwrap_or_else(|| "# Onboarding".to_string());
    out.push(title);
    out.push(String::new());
    out.push(
        "_Generated from memory. If a section is empty, run `runar crawl .` to populate._".into(),
    );
    out.push(String::new());

    for (i, sec) in sections.iter().enumerate() {
        out.push(format!("## {}. {}", i + 1, sec.heading));
        if sec.entries.is_empty() {
            out.push("_No entries._".into());
        } else {
            for item in &sec.entries {
                out.push(format!(
                    "- **{}** ({}, {}): {}",
                    item.title, item.entry_type, item.created_at, item.excerpt
                ));
            }
        }
        out.push(String::new());
    }

    out.push("## Next Steps".into());
    out.push(
        "- Skim referenced files and entries above\n\
         - `runar architecture --project <id>` for the structural map\n\
         - `runar ask \"...\" --project <id>` for targeted questions"
            .into(),
    );

    out.join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    crate::text::truncate_ellipsis(s, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, etype: &str) -> OnboardingItem {
        OnboardingItem {
            title: title.into(),
            entry_type: etype.into(),
            excerpt: "excerpt".into(),
            created_at: "2026-04-15".into(),
        }
    }

    #[test]
    fn render_markdown_includes_all_sections_and_project_title() {
        let sections = vec![
            OnboardingSection {
                heading: "Architecture Summary".into(),
                entries: vec![item("Rust workspace", "architecture")],
            },
            OnboardingSection {
                heading: "Key Decisions".into(),
                entries: vec![],
            },
        ];
        let md = render_markdown(Some("runar-forge"), &sections);
        assert!(md.contains("# Onboarding: runar-forge"));
        assert!(md.contains("## 1. Architecture Summary"));
        assert!(md.contains("Rust workspace"));
        assert!(md.contains("## 2. Key Decisions"));
        assert!(md.contains("_No entries._"));
        assert!(md.contains("## Next Steps"));
    }

    #[test]
    fn render_markdown_handles_no_project_id() {
        let md = render_markdown(None, &[]);
        assert!(md.starts_with("# Onboarding\n"));
    }
}
