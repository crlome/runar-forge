use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use serde::Serialize;

use crate::curator::{CuratorAnswer, CuratorOracle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Architecture,
    Implementation,
    Discovery,
    Identity,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Architecture => "architecture",
            Category::Implementation => "implementation",
            Category::Discovery => "discovery",
            Category::Identity => "identity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Grade {
    A,
    B,
    C,
    D,
    F,
}

impl Grade {
    pub fn from_score(s: i32) -> Self {
        if s >= 85 {
            Grade::A
        } else if s >= 70 {
            Grade::B
        } else if s >= 55 {
            Grade::C
        } else if s >= 40 {
            Grade::D
        } else {
            Grade::F
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Grade::A => "A",
            Grade::B => "B",
            Grade::C => "C",
            Grade::D => "D",
            Grade::F => "F",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Question {
    pub id: &'static str,
    pub question: &'static str,
    pub category: Category,
    pub expected_keywords: &'static [&'static str],
    pub expected_citations: usize,
    pub quick_set: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionResult {
    pub question_id: String,
    pub category: Category,
    pub question: String,
    pub answer: String,
    pub has_answer: bool,
    pub confidence: f64,
    pub citation_count: usize,
    pub expected_citations: usize,
    pub keyword_hits: Vec<String>,
    pub keyword_misses: Vec<String>,
    pub score: i32,
    pub grade: Grade,
    pub response_time_ms: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CategoryScore {
    pub average: f64,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkSummary {
    pub total_questions: usize,
    pub answered: usize,
    pub unanswered: usize,
    pub average_confidence: f64,
    pub average_score: f64,
    pub overall_grade: Grade,
    pub category_scores: HashMap<String, CategoryScore>,
    pub average_response_time_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkResult {
    pub project_id: String,
    pub mode: String,
    pub timestamp: String,
    pub duration_ms: u128,
    pub question_results: Vec<QuestionResult>,
    pub summary: BenchmarkSummary,
}

pub const QUESTIONS: &[Question] = &[
    // Architecture (10)
    Question {
        id: "arch-01",
        question: "What is the overall architecture pattern of this project?",
        category: Category::Architecture,
        expected_keywords: &["monorepo", "module", "layered", "package"],
        expected_citations: 1,
        quick_set: true,
    },
    Question {
        id: "arch-02",
        question: "What are the main modules or packages?",
        category: Category::Architecture,
        expected_keywords: &["package", "module", "src"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "arch-03",
        question: "How is the project organized?",
        category: Category::Architecture,
        expected_keywords: &["src", "directory", "package"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "arch-04",
        question: "What are the entry points to this application?",
        category: Category::Architecture,
        expected_keywords: &["index", "main", "entry"],
        expected_citations: 1,
        quick_set: true,
    },
    Question {
        id: "arch-05",
        question: "How is error handling structured in this project?",
        category: Category::Architecture,
        expected_keywords: &["error", "exception", "catch"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "arch-06",
        question: "What design patterns are used in this codebase?",
        category: Category::Architecture,
        expected_keywords: &["pattern", "factory", "strategy"],
        expected_citations: 2,
        quick_set: false,
    },
    Question {
        id: "arch-07",
        question: "How is configuration managed?",
        category: Category::Architecture,
        expected_keywords: &["config", "env", "environment"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "arch-08",
        question: "What are the external dependencies and infrastructure?",
        category: Category::Architecture,
        expected_keywords: &["dependency", "package", "docker"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "arch-09",
        question: "How is the build process structured?",
        category: Category::Architecture,
        expected_keywords: &["build", "compile", "bundle"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "arch-10",
        question: "What CI/CD pipeline is used?",
        category: Category::Architecture,
        expected_keywords: &["ci", "pipeline", "workflow"],
        expected_citations: 1,
        quick_set: false,
    },
    // Implementation (10)
    Question {
        id: "impl-01",
        question: "How does authentication work in this project?",
        category: Category::Implementation,
        expected_keywords: &["auth", "login", "token"],
        expected_citations: 2,
        quick_set: false,
    },
    Question {
        id: "impl-02",
        question: "How is data stored and retrieved?",
        category: Category::Implementation,
        expected_keywords: &["database", "storage", "query"],
        expected_citations: 2,
        quick_set: true,
    },
    Question {
        id: "impl-03",
        question: "How are API endpoints structured?",
        category: Category::Implementation,
        expected_keywords: &["endpoint", "route", "api"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "impl-04",
        question: "How is testing organized in this project?",
        category: Category::Implementation,
        expected_keywords: &["test", "spec", "framework"],
        expected_citations: 1,
        quick_set: true,
    },
    Question {
        id: "impl-05",
        question: "What coding conventions does this project follow?",
        category: Category::Implementation,
        expected_keywords: &["lint", "format", "style"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "impl-06",
        question: "How are environment variables managed?",
        category: Category::Implementation,
        expected_keywords: &["env", "environment", "variable"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "impl-07",
        question: "How is logging handled?",
        category: Category::Implementation,
        expected_keywords: &["log", "logger", "logging"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "impl-08",
        question: "How are database migrations managed?",
        category: Category::Implementation,
        expected_keywords: &["migration", "schema", "database"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "impl-09",
        question: "What language configuration or compiler settings are used?",
        category: Category::Implementation,
        expected_keywords: &["config", "compiler", "strict"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "impl-10",
        question: "How are packages or modules structured internally?",
        category: Category::Implementation,
        expected_keywords: &["package", "module", "export"],
        expected_citations: 1,
        quick_set: false,
    },
    // Discovery (5)
    Question {
        id: "disc-01",
        question: "What tech debt exists in this project?",
        category: Category::Discovery,
        expected_keywords: &["todo", "fixme", "debt"],
        expected_citations: 1,
        quick_set: true,
    },
    Question {
        id: "disc-02",
        question: "What are the known bugs or issues?",
        category: Category::Discovery,
        expected_keywords: &["bug", "issue", "fix"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "disc-03",
        question: "What has been worked on recently?",
        category: Category::Discovery,
        expected_keywords: &["session", "recent", "commit"],
        expected_citations: 1,
        quick_set: true,
    },
    Question {
        id: "disc-04",
        question: "What architectural decisions were made and why?",
        category: Category::Discovery,
        expected_keywords: &["decision", "because", "chose"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "disc-05",
        question: "What are the gotchas a new developer should know?",
        category: Category::Discovery,
        expected_keywords: &["gotcha", "note", "careful"],
        expected_citations: 1,
        quick_set: false,
    },
    // Identity (5)
    Question {
        id: "id-01",
        question: "What does the core or main package do?",
        category: Category::Identity,
        expected_keywords: &["core", "package", "provides"],
        expected_citations: 1,
        quick_set: true,
    },
    Question {
        id: "id-02",
        question: "What is this project for and who uses it?",
        category: Category::Identity,
        expected_keywords: &["project", "provides", "tool"],
        expected_citations: 1,
        quick_set: true,
    },
    Question {
        id: "id-03",
        question: "What problem does this project solve?",
        category: Category::Identity,
        expected_keywords: &["problem", "solve", "challenge"],
        expected_citations: 1,
        quick_set: false,
    },
    Question {
        id: "id-04",
        question: "What makes this project different from similar tools?",
        category: Category::Identity,
        expected_keywords: &["different", "unique", "versus"],
        expected_citations: 1,
        quick_set: true,
    },
    Question {
        id: "id-05",
        question: "What is the technology stack used?",
        category: Category::Identity,
        expected_keywords: &["language", "runtime", "stack"],
        expected_citations: 1,
        quick_set: false,
    },
];

/// Score one Curator answer against its expected question (deterministic).
pub fn grade(
    question: &Question,
    answer: &CuratorAnswer,
    response_time_ms: u128,
) -> QuestionResult {
    let has_answer_score: i32 = if answer.has_answer { 30 } else { 0 };
    let confidence_score: i32 = (answer.confidence * 25.0).round() as i32;

    let citation_count = answer.citations.len();
    let citation_ratio = if question.expected_citations > 0 {
        ((citation_count as f64) / (question.expected_citations as f64)).min(1.0)
    } else {
        1.0
    };
    let citation_score = (citation_ratio * 25.0).round() as i32;

    let answer_lower = answer.answer.to_lowercase();
    let mut hits = Vec::new();
    let mut misses = Vec::new();
    for kw in question.expected_keywords {
        if answer_lower.contains(&kw.to_lowercase()) {
            hits.push(kw.to_string());
        } else {
            misses.push(kw.to_string());
        }
    }
    let keyword_rate = hits.len() as f64 / question.expected_keywords.len() as f64;
    let keyword_score = (keyword_rate * 10.0).round() as i32;

    let answer_len = answer.answer.trim().len();
    let quality_score = if answer_len >= 100 {
        10
    } else if answer_len >= 20 {
        5
    } else {
        0
    };

    let mut raw =
        has_answer_score + confidence_score + citation_score + keyword_score + quality_score;
    if hits.is_empty() {
        raw = raw.min(54);
    } else if keyword_rate < 0.5 {
        raw = raw.min(69);
    }
    let score = raw.clamp(0, 100);

    QuestionResult {
        question_id: question.id.into(),
        category: question.category,
        question: question.question.into(),
        answer: answer.answer.clone(),
        has_answer: answer.has_answer,
        confidence: answer.confidence,
        citation_count,
        expected_citations: question.expected_citations,
        keyword_hits: hits,
        keyword_misses: misses,
        score,
        grade: Grade::from_score(score),
        response_time_ms,
    }
}

/// Run the benchmark by asking the Curator each question and grading each answer.
pub async fn run(
    curator: &Arc<CuratorOracle>,
    project_id: &str,
    quick: bool,
) -> anyhow::Result<BenchmarkResult> {
    let start = Instant::now();
    let questions: Vec<&Question> = QUESTIONS.iter().filter(|q| !quick || q.quick_set).collect();
    let total = questions.len();

    let mut results: Vec<QuestionResult> = Vec::with_capacity(total);
    for q in &questions {
        let q_start = Instant::now();
        let answer = curator.ask(q.question, Some(project_id)).await?;
        let elapsed = q_start.elapsed().as_millis();
        results.push(grade(q, &answer, elapsed));
    }

    let summary = summarize(&results);

    Ok(BenchmarkResult {
        project_id: project_id.into(),
        mode: if quick { "quick".into() } else { "full".into() },
        timestamp: Utc::now().to_rfc3339(),
        duration_ms: start.elapsed().as_millis(),
        question_results: results,
        summary,
    })
}

fn summarize(results: &[QuestionResult]) -> BenchmarkSummary {
    let total = results.len();
    let answered = results.iter().filter(|r| r.has_answer).count();
    let unanswered = total - answered;
    let avg_conf = if total == 0 {
        0.0
    } else {
        results.iter().map(|r| r.confidence).sum::<f64>() / total as f64
    };
    let avg_score = if total == 0 {
        0.0
    } else {
        results.iter().map(|r| r.score as f64).sum::<f64>() / total as f64
    };
    let avg_rt = if total == 0 {
        0.0
    } else {
        results
            .iter()
            .map(|r| r.response_time_ms as f64)
            .sum::<f64>()
            / total as f64
    };

    let mut by_cat: HashMap<String, (f64, usize)> = HashMap::new();
    for r in results {
        let entry = by_cat
            .entry(r.category.as_str().to_string())
            .or_insert((0.0, 0));
        entry.0 += r.score as f64;
        entry.1 += 1;
    }
    let category_scores = by_cat
        .into_iter()
        .map(|(k, (sum, n))| {
            (
                k,
                CategoryScore {
                    average: if n > 0 { sum / n as f64 } else { 0.0 },
                    count: n,
                },
            )
        })
        .collect();

    BenchmarkSummary {
        total_questions: total,
        answered,
        unanswered,
        average_confidence: avg_conf,
        average_score: avg_score,
        overall_grade: Grade::from_score(avg_score.round() as i32),
        category_scores,
        average_response_time_ms: avg_rt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(s: &str, conf: f64, cites: usize, has: bool) -> CuratorAnswer {
        CuratorAnswer {
            question: "q".into(),
            answer: s.into(),
            citations: (0..cites)
                .map(|i| crate::curator::Citation {
                    entry_id: format!("e{i}"),
                    title: format!("t{i}"),
                    entry_type: "context".into(),
                    relevance_score: 0.5,
                    excerpt: "x".into(),
                })
                .collect(),
            confidence: conf,
            has_answer: has,
            insufficient_context_reason: None,
            suggested_action: None,
        }
    }

    fn q(kws: &'static [&'static str], expected_cites: usize) -> Question {
        Question {
            id: "test",
            question: "q",
            category: Category::Architecture,
            expected_keywords: kws,
            expected_citations: expected_cites,
            quick_set: true,
        }
    }

    #[test]
    fn perfect_answer_grades_a() {
        let question = q(&["module", "package"], 1);
        let ans = answer("This codebase organizes modules and packages cleanly across many files. It uses a layered approach.", 1.0, 1, true);
        let r = grade(&question, &ans, 50);
        assert_eq!(
            r.grade,
            Grade::A,
            "got {} score={}",
            r.grade.as_str(),
            r.score
        );
    }

    #[test]
    fn off_topic_capped_below_b() {
        // High confidence + cited but ZERO keywords → max 54 → D or F
        let question = q(&["zebra", "elephant"], 1);
        let ans = answer(
            "totally unrelated answer with great length and lots of words but nothing matches",
            1.0,
            5,
            true,
        );
        let r = grade(&question, &ans, 50);
        assert!(r.score <= 54, "expected cap at 54, got {}", r.score);
    }

    #[test]
    fn unanswered_zero_has_answer_component() {
        let question = q(&["module"], 0);
        let ans = answer("", 0.0, 0, false);
        let r = grade(&question, &ans, 50);
        assert!(r.score < 40);
    }

    #[test]
    fn questions_count_correct() {
        let total = QUESTIONS.len();
        let quick = QUESTIONS.iter().filter(|q| q.quick_set).count();
        assert_eq!(total, 30);
        // TS source has 9 quick-set questions (despite "10/30" naming)
        assert_eq!(quick, 9);
    }
}
