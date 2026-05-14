use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::librarian::MemoryLibrarian;
use crate::storage::StorageResult;
use crate::types::*;

pub mod onboarding;

// ── Question Types ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuestionType {
    ArchitectureOverview,
    ModuleExplanation,
    FileExplanation,
    DependencyQuestion,
    EntryPointsQuestion,
    DifferentiationQuestion,
    FlowQuestion,
    PatternQuestion,
    AuthQuestion,
    DataQuestion,
    Implementation,
    DecisionQuestion,
    RuleQuestion,
    BugHistoryQuestion,
    ConventionQuestion,
    OnboardingRequest,
    GettingStartedQuestion,
    StatusQuestion,
    TechDebtQuestion,
    General,
}

#[derive(Debug, Clone)]
pub struct ClassifiedQuestion {
    pub original: String,
    pub question_type: QuestionType,
    pub project_id: Option<String>,
    pub subject: Option<String>,
    pub confidence: f64,
    pub search_terms: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub question: ClassifiedQuestion,
    pub primary_entries: Vec<MemoryEntry>,
    pub supporting_entries: Vec<MemoryEntry>,
    pub assembly_time_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CuratorAnswer {
    pub question: String,
    pub answer: String,
    pub citations: Vec<Citation>,
    pub confidence: f64,
    pub has_answer: bool,
    pub insufficient_context_reason: Option<String>,
    pub suggested_action: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Citation {
    pub entry_id: String,
    pub title: String,
    pub entry_type: String,
    pub relevance_score: f64,
    pub excerpt: String,
}

// ── Classifier ─────────────────────────────────────────────────────

fn classify(question: &str) -> ClassifiedQuestion {
    let lower = question.to_lowercase();

    let (question_type, confidence) = detect_type(&lower);
    let subject = extract_subject(&lower);
    let search_terms = extract_search_terms(question);

    ClassifiedQuestion {
        original: question.to_string(),
        question_type,
        project_id: None,
        subject,
        confidence,
        search_terms,
    }
}

fn detect_type(q: &str) -> (QuestionType, f64) {
    // Architecture
    if q.contains("architecture") || q.contains("structure") || q.contains("organized") {
        return (QuestionType::ArchitectureOverview, 0.90);
    }

    // Entry points
    if q.contains("entry point") || q.contains("main entry") || q.contains("starts from") {
        return (QuestionType::EntryPointsQuestion, 0.92);
    }

    // Differentiation
    if q.contains("different from") || q.contains("what makes") && q.contains("different") {
        return (QuestionType::DifferentiationQuestion, 0.88);
    }

    // Bug history (check before auth — "bugs in auth module" should be bug, not auth)
    if q.contains("bug")
        || (q.contains("issue") && q.contains("fix"))
        || (q.contains("problem") && q.contains("fix"))
    {
        return (QuestionType::BugHistoryQuestion, 0.80);
    }

    // Decision
    if q.contains("why did")
        || q.contains("decision")
        || q.contains("chose")
        || q.contains("rationale")
    {
        return (QuestionType::DecisionQuestion, 0.85);
    }

    // Auth
    if q.contains("auth")
        || q.contains("login")
        || q.contains("jwt")
        || (q.contains("session") && q.contains("token"))
    {
        return (QuestionType::AuthQuestion, 0.88);
    }

    // Flow
    if q.contains("how does") || q.contains("how do") || q.contains("flow") || q.contains("process")
    {
        return (QuestionType::FlowQuestion, 0.82);
    }

    // Pattern
    if q.contains("pattern") || q.contains("convention") || q.contains("approach") {
        return (QuestionType::PatternQuestion, 0.80);
    }

    // Tech debt
    if q.contains("tech debt")
        || q.contains("todo")
        || q.contains("technical debt")
        || q.contains("refactor")
    {
        return (QuestionType::TechDebtQuestion, 0.85);
    }

    // Dependencies
    if q.contains("depend")
        || q.contains("package")
        || q.contains("library")
        || q.contains("framework")
    {
        return (QuestionType::DependencyQuestion, 0.78);
    }

    // Data
    if q.contains("database") || q.contains("schema") || q.contains("table") || q.contains("model")
    {
        return (QuestionType::DataQuestion, 0.80);
    }

    // Module explanation
    if q.contains("module") || q.contains("component") || q.contains("service") {
        return (QuestionType::ModuleExplanation, 0.75);
    }

    // File explanation
    if q.contains("file")
        || q.contains(".ts")
        || q.contains(".rs")
        || q.contains(".py")
        || q.contains(".go")
    {
        return (QuestionType::FileExplanation, 0.72);
    }

    // Implementation
    if q.contains("implement") || q.contains("how is") || q.contains("where is") {
        return (QuestionType::Implementation, 0.70);
    }

    // Onboarding
    if q.contains("onboard")
        || q.contains("new developer")
        || q.contains("getting started")
        || q.contains("overview")
    {
        return (QuestionType::OnboardingRequest, 0.85);
    }

    // Status
    if q.contains("status") || q.contains("progress") || q.contains("recent") {
        return (QuestionType::StatusQuestion, 0.75);
    }

    // Rules
    if q.contains("rule") || q.contains("policy") || q.contains("constraint") {
        return (QuestionType::RuleQuestion, 0.78);
    }

    (QuestionType::General, 0.50)
}

fn extract_subject(q: &str) -> Option<String> {
    // Try to extract what the question is about
    let patterns = ["about ", "regarding ", "for the ", "of the ", "in the "];
    for pat in &patterns {
        if let Some(idx) = q.find(pat) {
            let rest = &q[idx + pat.len()..];
            let subject: String = rest
                .split(['?', '.', ',', '\n'])
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !subject.is_empty() && subject.len() < 100 {
                return Some(subject);
            }
        }
    }
    None
}

const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "could", "should", "may", "might", "can", "shall", "to",
    "of", "in", "for", "on", "with", "at", "by", "from", "as", "into", "through", "during",
    "before", "after", "above", "below", "between", "and", "but", "or", "not", "no", "nor", "so",
    "yet", "both", "either", "neither", "this", "that", "these", "those", "what", "which", "who",
    "whom", "how", "where", "when", "why", "it", "its", "my", "your", "our", "their", "we", "you",
    "they", "i", "me", "him", "her", "us", "them",
];

fn extract_search_terms(question: &str) -> Vec<String> {
    question
        .split_whitespace()
        .map(|w| {
            w.to_lowercase()
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_string()
        })
        .filter(|w| w.len() > 2 && !STOP_WORDS.contains(&w.as_str()))
        .collect()
}

// ── Context Assembler ──────────────────────────────────────────────

async fn assemble_context(
    classified: &ClassifiedQuestion,
    project_id: Option<&str>,
    librarian: &MemoryLibrarian,
) -> StorageResult<AssembledContext> {
    let start = std::time::Instant::now();

    let search_query = if classified.search_terms.is_empty() {
        classified.original.clone()
    } else {
        classified.search_terms.join(" ")
    };

    let entry_type_filter = match classified.question_type {
        QuestionType::DecisionQuestion => Some(EntryType::Decision),
        QuestionType::BugHistoryQuestion => Some(EntryType::Bug),
        QuestionType::PatternQuestion => Some(EntryType::Pattern),
        QuestionType::TechDebtQuestion => Some(EntryType::TechDebt),
        QuestionType::RuleQuestion => Some(EntryType::Rule),
        _ => None,
    };

    let mut primary = librarian
        .search(&search_query, 10, None, project_id, entry_type_filter, None)
        .await
        .unwrap_or_default();

    // Fallback 1: if the type-filtered search was empty (small categories like
    // bug/decision often have <10 rows total), retry without the type filter.
    if primary.is_empty() && entry_type_filter.is_some() {
        primary = librarian
            .search(&search_query, 10, None, project_id, None, None)
            .await
            .unwrap_or_default();
    }

    // Fallback 2: multi-term queries hit FTS AND-mode and return 0 even when
    // each term individually has many hits. Retry with each term separately
    // and merge by relevance.
    if primary.is_empty() && classified.search_terms.len() > 1 {
        let mut seen: HashSet<uuid::Uuid> = HashSet::new();
        for term in &classified.search_terms {
            if let Ok(hits) = librarian
                .search(term, 5, None, project_id, None, None)
                .await
            {
                for entry in hits {
                    if seen.insert(entry.id) {
                        primary.push(entry);
                    }
                    if primary.len() >= 10 {
                        break;
                    }
                }
            }
            if primary.len() >= 10 {
                break;
            }
        }
    }

    // Supporting context: architecture entries for structural questions
    let supporting = match classified.question_type {
        QuestionType::ArchitectureOverview
        | QuestionType::ModuleExplanation
        | QuestionType::EntryPointsQuestion
        | QuestionType::OnboardingRequest => librarian
            .search(
                "architecture summary",
                5,
                None,
                project_id,
                Some(EntryType::Architecture),
                None,
            )
            .await
            .unwrap_or_default(),
        _ => vec![],
    };

    // Deduplicate
    let primary_ids: HashSet<_> = primary.iter().map(|e| e.id).collect();
    let supporting: Vec<MemoryEntry> = supporting
        .into_iter()
        .filter(|e| !primary_ids.contains(&e.id))
        .collect();

    Ok(AssembledContext {
        question: classified.clone(),
        primary_entries: primary,
        supporting_entries: supporting,
        assembly_time_ms: start.elapsed().as_millis() as u64,
    })
}

// ── Answer Synthesizer ─────────────────────────────────────────────

fn synthesize(context: &AssembledContext, project_id: Option<&str>) -> CuratorAnswer {
    let all_entries: Vec<&MemoryEntry> = context
        .primary_entries
        .iter()
        .chain(context.supporting_entries.iter())
        .collect();

    if all_entries.is_empty() {
        return CuratorAnswer {
            question: context.question.original.clone(),
            answer: String::new(),
            citations: vec![],
            confidence: 0.40,
            has_answer: false,
            insufficient_context_reason: Some(
                "No relevant memory entries found. The project may not have been crawled yet."
                    .into(),
            ),
            suggested_action: Some(format!(
                "Run `runar crawl . --project {}` to populate memory, then ask again.",
                project_id.unwrap_or("your-project")
            )),
        };
    }

    let citations: Vec<Citation> = all_entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let excerpt = if e.content.len() > 200 {
                format!("{}...", &e.content[..200])
            } else {
                e.content.clone()
            };
            Citation {
                entry_id: e.id.to_string(),
                title: e.title.clone(),
                entry_type: e.entry_type.as_str().to_string(),
                relevance_score: 1.0 - (i as f64 * 0.05),
                excerpt,
            }
        })
        .collect();

    let answer = build_answer(&context.question, &all_entries);

    let confidence = confidence_from_context(context, &all_entries, &answer);

    CuratorAnswer {
        question: context.question.original.clone(),
        answer,
        citations,
        confidence,
        has_answer: true,
        insufficient_context_reason: None,
        suggested_action: None,
    }
}

fn build_answer(question: &ClassifiedQuestion, entries: &[&MemoryEntry]) -> String {
    let mut parts = Vec::new();

    match question.question_type {
        QuestionType::OnboardingRequest => {
            return synthesize_onboarding(question, entries);
        }
        QuestionType::TechDebtQuestion => {
            return synthesize_techdebt(question, entries);
        }
        QuestionType::ArchitectureOverview => {
            parts.push(format!("## {}\n", question.original));
            for entry in entries {
                if entry.entry_type == EntryType::Architecture {
                    parts.push(entry.content.clone());
                    parts.push(String::new());
                }
            }
            if parts.len() <= 1 {
                for entry in entries.iter().take(5) {
                    parts.push(format!(
                        "- **{}** [{}]: {}",
                        entry.title,
                        entry.entry_type.as_str(),
                        truncate(&entry.content, 200)
                    ));
                }
            }
        }
        QuestionType::DecisionQuestion => {
            parts.push("## Relevant Decisions\n".into());
            for entry in entries {
                parts.push(format!("### {}\n{}\n", entry.title, entry.content));
            }
        }
        QuestionType::BugHistoryQuestion => {
            parts.push("## Bug History\n".into());
            for entry in entries {
                parts.push(format!(
                    "- **{}** ({}): {}",
                    entry.title,
                    entry.created_at.format("%Y-%m-%d"),
                    truncate(&entry.content, 300)
                ));
            }
        }
        _ => {
            parts.push(format!("## {}\n", question.original));
            for entry in entries.iter().take(8) {
                parts.push(format!(
                    "- **{}** [{}]: {}",
                    entry.title,
                    entry.entry_type.as_str(),
                    truncate(&entry.content, 250)
                ));
            }
        }
    }

    parts.join("\n")
}

/// Expected entry type for a given question type. Used by confidence scoring
/// to reward retrievals whose type matches the question's intent.
fn expected_entry_type(q: QuestionType) -> Option<EntryType> {
    match q {
        QuestionType::DecisionQuestion => Some(EntryType::Decision),
        QuestionType::BugHistoryQuestion => Some(EntryType::Bug),
        QuestionType::PatternQuestion | QuestionType::ConventionQuestion => {
            Some(EntryType::Pattern)
        }
        QuestionType::TechDebtQuestion => Some(EntryType::TechDebt),
        QuestionType::RuleQuestion => Some(EntryType::Rule),
        QuestionType::ArchitectureOverview
        | QuestionType::OnboardingRequest
        | QuestionType::ModuleExplanation
        | QuestionType::EntryPointsQuestion => Some(EntryType::Architecture),
        _ => None,
    }
}

/// Real confidence score combining:
/// - citation rank quality (more top-ranked primary hits → higher)
/// - keyword overlap (expected search terms appearing in returned content)
/// - entry-type alignment (retrieved types match question intent)
/// - recency (newer source entries weighted up, decay past ~180d)
///
/// Range [0.40, 0.97]. Replaces prior static bucket scorer.
fn confidence_from_context(
    context: &AssembledContext,
    all_entries: &[&MemoryEntry],
    answer: &str,
) -> f64 {
    if all_entries.is_empty() {
        return 0.40;
    }

    // 1. Rank quality: primary list is ordered by fused RRF score. Approximate
    //    "citation relevance" by weighting top ranks more. Saturates at ~5 hits.
    let primary_n = context.primary_entries.len() as f64;
    let rank_quality = 1.0_f64 - (-primary_n / 3.0).exp(); // 0..~1

    // 2. Keyword overlap: fraction of non-stopword search terms appearing in
    //    primary content (title+content lowercased). High overlap = the
    //    retrieval actually contains the asked-about terms.
    let terms = &context.question.search_terms;
    let overlap = if terms.is_empty() {
        0.5
    } else {
        let corpus: String = context
            .primary_entries
            .iter()
            .map(|e| format!("{} {}", e.title.to_lowercase(), e.content.to_lowercase()))
            .collect::<Vec<_>>()
            .join(" ");
        let hits = terms.iter().filter(|t| corpus.contains(t.as_str())).count();
        hits as f64 / terms.len() as f64
    };

    // 3. Type alignment: share of primary entries whose type matches the
    //    question's expected type. 1.0 when no specific type is expected.
    let alignment = match expected_entry_type(context.question.question_type) {
        None => 1.0,
        Some(expected) => {
            if primary_n == 0.0 {
                0.0
            } else {
                let matches = context
                    .primary_entries
                    .iter()
                    .filter(|e| e.entry_type == expected)
                    .count() as f64;
                // Partial credit: 0.5 floor even if no type match (related
                // content can still answer the question).
                0.5 + 0.5 * (matches / primary_n)
            }
        }
    };

    // 4. Recency: mean decay over primary entries. Half-life ~180d.
    let now = chrono::Utc::now();
    let recency = if primary_n == 0.0 {
        0.5
    } else {
        let sum: f64 = context
            .primary_entries
            .iter()
            .map(|e| {
                let days = (now - e.created_at).num_seconds().max(0) as f64 / 86_400.0;
                (-days / 180.0).exp()
            })
            .sum();
        sum / primary_n
    };

    // 5. Answer non-emptiness guard: synthesizer produced some body.
    let answer_signal = if answer.trim().len() < 20 { 0.6 } else { 1.0 };

    // Weighted blend. Weights chosen so strong hits on the two most meaningful
    // signals (overlap + alignment) can drive the score above 0.90 while a
    // single lucky hit with weak overlap stays below 0.75.
    let raw = 0.30 * rank_quality
        + 0.30 * overlap
        + 0.20 * alignment
        + 0.15 * recency
        + 0.05 * answer_signal;

    // Map [0,1] raw into [0.40, 0.97] so "we have nothing" still reads 0.40
    // and a perfect retrieval caps short of absolute certainty.
    let scaled = 0.40 + raw * 0.57;
    scaled.clamp(0.40, 0.97)
}

/// Tech-debt synthesizer: groups entries by source file (when present),
/// surfaces counts per type, and orders by oldest-first so the longest-
/// standing debt appears at the top.
fn synthesize_techdebt(question: &ClassifiedQuestion, entries: &[&MemoryEntry]) -> String {
    use std::collections::BTreeMap;
    let mut parts: Vec<String> = vec![format!("## Technical Debt — {}\n", question.original)];

    // Group by first associated file (or "(unattributed)").
    let mut by_file: BTreeMap<String, Vec<&&MemoryEntry>> = BTreeMap::new();
    for entry in entries {
        // Group key: first tag that looks like a file path, else topic_key,
        // else "(unattributed)".
        let file = entry
            .tags
            .iter()
            .find(|t| t.contains('/') || t.contains('.'))
            .cloned()
            .or_else(|| entry.topic_key.clone())
            .unwrap_or_else(|| "(unattributed)".to_string());
        by_file.entry(file).or_default().push(entry);
    }

    // Per-type tally for the top summary line.
    let mut type_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for e in entries {
        *type_counts.entry(e.entry_type.as_str()).or_insert(0) += 1;
    }
    if !type_counts.is_empty() {
        let summary: Vec<String> = type_counts
            .iter()
            .map(|(t, n)| format!("{} {}", n, t))
            .collect();
        parts.push(format!("**Totals:** {}\n", summary.join(", ")));
    }

    for (file, mut items) in by_file {
        // Oldest first.
        items.sort_by_key(|e| e.created_at);
        parts.push(format!("### {}\n", file));
        for entry in items {
            parts.push(format!(
                "- **{}** ({}): {}",
                entry.title,
                entry.created_at.format("%Y-%m-%d"),
                truncate(&entry.content, 280)
            ));
        }
        parts.push(String::new());
    }

    parts.join("\n")
}

/// Onboarding synthesizer: narrative flow across architecture, decisions,
/// patterns, and tech debt. Degrades gracefully when sections are empty.
fn synthesize_onboarding(question: &ClassifiedQuestion, entries: &[&MemoryEntry]) -> String {
    let mut parts: Vec<String> = vec![format!("# Onboarding — {}\n", question.original)];

    let arch: Vec<&&MemoryEntry> = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Architecture)
        .collect();
    let decisions: Vec<&&MemoryEntry> = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Decision)
        .collect();
    let patterns: Vec<&&MemoryEntry> = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Pattern)
        .collect();
    let debt: Vec<&&MemoryEntry> = entries
        .iter()
        .filter(|e| e.entry_type == EntryType::TechDebt)
        .collect();

    parts.push("## 1. What this project is\n".into());
    if arch.is_empty() {
        parts.push(
            "_No architecture entries yet. Run `runar crawl .` to populate._\n".into(),
        );
    } else {
        for e in arch.iter().take(3) {
            parts.push(format!("- **{}**: {}\n", e.title, truncate(&e.content, 300)));
        }
    }

    parts.push("## 2. Key decisions to know\n".into());
    if decisions.is_empty() {
        parts.push("_No recorded decisions._\n".into());
    } else {
        for e in decisions.iter().take(6) {
            parts.push(format!(
                "- **{}** ({}): {}",
                e.title,
                e.created_at.format("%Y-%m-%d"),
                truncate(&e.content, 240)
            ));
        }
        parts.push(String::new());
    }

    parts.push("## 3. Patterns & conventions\n".into());
    if patterns.is_empty() {
        parts.push("_No patterns captured yet._\n".into());
    } else {
        for e in patterns.iter().take(6) {
            parts.push(format!("- **{}**: {}", e.title, truncate(&e.content, 240)));
        }
        parts.push(String::new());
    }

    parts.push("## 4. Tech debt to be aware of\n".into());
    if debt.is_empty() {
        parts.push("_No tech-debt markers recorded._\n".into());
    } else {
        for e in debt.iter().take(5) {
            parts.push(format!("- **{}**: {}", e.title, truncate(&e.content, 220)));
        }
        parts.push(String::new());
    }

    parts.push("## 5. Next steps\n".into());
    parts.push(
        "- Skim the files referenced above\n\
         - Ask the Curator follow-up questions (`runar ask ...`)\n\
         - Check `runar architecture` for the structural map\n"
            .into(),
    );

    parts.join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

// ── Oracle (main pipeline) ─────────────────────────────────────────

pub struct CuratorOracle {
    librarian: Arc<MemoryLibrarian>,
}

impl CuratorOracle {
    pub fn new(librarian: Arc<MemoryLibrarian>) -> Self {
        Self { librarian }
    }

    pub async fn ask(
        &self,
        question: &str,
        project_id: Option<&str>,
    ) -> StorageResult<CuratorAnswer> {
        let classified = classify(question);
        let resolved_project_id = project_id.or(classified.project_id.as_deref());
        let context = assemble_context(&classified, resolved_project_id, &self.librarian).await?;
        Ok(synthesize(&context, resolved_project_id))
    }

    pub async fn get_topics(&self, project_id: Option<&str>) -> StorageResult<Vec<TopicSummary>> {
        let entries = self
            .librarian
            .list(ListFilters {
                project_id: project_id.map(|s| s.to_string()),
                limit: Some(100),
                ..Default::default()
            })
            .await?;

        let mut type_groups: std::collections::HashMap<String, Vec<&MemoryEntry>> =
            std::collections::HashMap::new();
        for entry in &entries {
            type_groups
                .entry(entry.entry_type.as_str().to_string())
                .or_default()
                .push(entry);
        }

        let mut topics: Vec<TopicSummary> = type_groups
            .into_iter()
            .map(|(type_name, entries)| TopicSummary {
                topic: type_name,
                count: entries.len(),
                titles: entries.iter().take(5).map(|e| e.title.clone()).collect(),
            })
            .collect();

        topics.sort_by(|a, b| b.count.cmp(&a.count));
        Ok(topics)
    }

    pub async fn get_decisions(&self, project_id: Option<&str>) -> StorageResult<Vec<MemoryEntry>> {
        self.librarian
            .search(
                "decision",
                20,
                None,
                project_id,
                Some(EntryType::Decision),
                None,
            )
            .await
    }

    /// Generate a multi-section onboarding report for a project: architecture,
    /// decisions, patterns, rules, tech debt, and next-step pointers.
    pub async fn onboard(
        &self,
        project_id: Option<&str>,
    ) -> StorageResult<onboarding::OnboardingReport> {
        onboarding::generate(&self.librarian, project_id).await
    }

    pub async fn get_tech_debt(&self, project_id: Option<&str>) -> StorageResult<Vec<MemoryEntry>> {
        self.librarian
            .search(
                "tech debt TODO FIXME",
                20,
                None,
                project_id,
                Some(EntryType::TechDebt),
                None,
            )
            .await
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicSummary {
    pub topic: String,
    pub count: usize,
    pub titles: Vec<String>,
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_architecture() {
        let q = classify("What is the architecture of this project?");
        assert_eq!(q.question_type, QuestionType::ArchitectureOverview);
        assert!(q.confidence >= 0.85);
    }

    #[test]
    fn test_classify_decision() {
        let q = classify("Why did we choose PostgreSQL over MySQL?");
        assert_eq!(q.question_type, QuestionType::DecisionQuestion);
    }

    #[test]
    fn test_classify_bug() {
        let q = classify("What bugs have we fixed in the auth module?");
        assert_eq!(q.question_type, QuestionType::BugHistoryQuestion);
    }

    #[test]
    fn test_classify_tech_debt() {
        let q = classify("What technical debt do we have?");
        assert_eq!(q.question_type, QuestionType::TechDebtQuestion);
    }

    #[test]
    fn test_classify_flow() {
        let q = classify("How does the payment processing flow work?");
        assert_eq!(q.question_type, QuestionType::FlowQuestion);
    }

    #[test]
    fn test_classify_general() {
        let q = classify("Tell me something interesting");
        assert_eq!(q.question_type, QuestionType::General);
        assert!(q.confidence <= 0.55);
    }

    #[test]
    fn test_extract_search_terms() {
        let terms = extract_search_terms("How does the authentication flow work?");
        assert!(terms.contains(&"authentication".to_string()));
        assert!(terms.contains(&"flow".to_string()));
        assert!(terms.contains(&"work".to_string()));
        assert!(!terms.contains(&"how".to_string()));
        assert!(!terms.contains(&"does".to_string()));
        assert!(!terms.contains(&"the".to_string()));
    }

    #[test]
    fn test_synthesize_empty_context() {
        let classified = classify("What is the architecture?");
        let context = AssembledContext {
            question: classified,
            primary_entries: vec![],
            supporting_entries: vec![],
            assembly_time_ms: 0,
        };
        let answer = synthesize(&context, None);
        assert!(!answer.has_answer);
        assert!(answer.confidence < 0.50);
        assert!(answer.insufficient_context_reason.is_some());
    }

    #[test]
    fn test_confidence_scoring() {
        let classified = classify("What is the architecture?");

        // 0 entries → 0.40
        let ctx0 = AssembledContext {
            question: classified.clone(),
            primary_entries: vec![],
            supporting_entries: vec![],
            assembly_time_ms: 0,
        };
        assert!((synthesize(&ctx0, None).confidence - 0.40).abs() < 0.01);
    }

    fn mk_entry(title: &str, content: &str, etype: EntryType) -> MemoryEntry {
        MemoryEntry {
            id: uuid::Uuid::new_v4(),
            title: title.into(),
            content: content.into(),
            entry_type: etype,
            source: MemorySource::Agent,
            tags: vec![],
            namespace: "default".into(),
            project_id: None,
            topic_key: None,
            layer: MemoryLayer::SEMANTIC,
            importance: 0.5,
            decay_score: 1.0,
            access_count: 0,
            confidence: crate::types::DEFAULT_CONFIDENCE,
            embedding: None,
            verified: false,
            verified_at: None,
            author: None,
            verified_by: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_accessed_at: None,
            deleted_at: None,
        }
    }

    #[test]
    fn confidence_rewards_keyword_overlap_and_type_alignment() {
        // Decision question, retrieved entries are Decision type + mention
        // the exact search terms → confidence should land high (>0.80).
        let classified = classify("Why did we choose PostgreSQL over MySQL?");
        let e1 = mk_entry(
            "Chose PostgreSQL",
            "We chose PostgreSQL over MySQL because pgvector gives semantic search.",
            EntryType::Decision,
        );
        let e2 = mk_entry(
            "PostgreSQL migration",
            "Migration from MySQL to PostgreSQL completed; reason: pgvector.",
            EntryType::Decision,
        );
        let ctx_good = AssembledContext {
            question: classified.clone(),
            primary_entries: vec![e1, e2],
            supporting_entries: vec![],
            assembly_time_ms: 0,
        };
        let good = synthesize(&ctx_good, None).confidence;
        assert!(good > 0.80, "expected high confidence, got {}", good);

        // Same question, but entries are wrong type AND don't mention the
        // search terms → confidence should be noticeably lower.
        let e3 = mk_entry(
            "Random pattern",
            "Some unrelated description of a repository pattern.",
            EntryType::Pattern,
        );
        let ctx_weak = AssembledContext {
            question: classified,
            primary_entries: vec![e3],
            supporting_entries: vec![],
            assembly_time_ms: 0,
        };
        let weak = synthesize(&ctx_weak, None).confidence;
        assert!(weak < good - 0.10, "weak={} good={}", weak, good);
        assert!(weak >= 0.40);
    }

    #[test]
    fn techdebt_synth_groups_and_summarizes() {
        let classified = classify("What technical debt do we have?");
        let mut a = mk_entry("TODO refactor auth", "auth.rs line 42", EntryType::TechDebt);
        a.tags = vec!["src/auth.rs".into()];
        let mut b = mk_entry("FIXME token bug", "token.rs", EntryType::TechDebt);
        b.tags = vec!["src/auth.rs".into()];
        let mut c = mk_entry("TODO cleanup", "misc", EntryType::TechDebt);
        c.tags = vec!["src/util.rs".into()];
        let ctx = AssembledContext {
            question: classified,
            primary_entries: vec![a, b, c],
            supporting_entries: vec![],
            assembly_time_ms: 0,
        };
        let ans = synthesize(&ctx, None).answer;
        assert!(ans.contains("Totals:"));
        assert!(ans.contains("src/auth.rs"));
        assert!(ans.contains("src/util.rs"));
    }

    #[test]
    fn onboarding_synth_has_sections() {
        let classified = classify("onboard me to this project");
        assert_eq!(classified.question_type, QuestionType::OnboardingRequest);
        let arch = mk_entry("Architecture summary", "Rust monorepo.", EntryType::Architecture);
        let dec = mk_entry("Chose Postgres", "Because pgvector.", EntryType::Decision);
        let ctx = AssembledContext {
            question: classified,
            primary_entries: vec![arch, dec],
            supporting_entries: vec![],
            assembly_time_ms: 0,
        };
        let ans = synthesize(&ctx, None).answer;
        assert!(ans.contains("## 1. What this project is"));
        assert!(ans.contains("## 2. Key decisions to know"));
        assert!(ans.contains("## 5. Next steps"));
    }
}
