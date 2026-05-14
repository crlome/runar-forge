pub mod arch_summarizer;
pub mod file_analyzer;
pub mod markdown_analyzer;
pub mod memory_entry_generator;
pub mod pattern_extractor;
pub mod sql_analyzer;
pub mod techdebt;

use crate::huginn::analyzer::AnalysisDepth;
use crate::huginn::scanner::FileEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TechDebtType {
    Todo,
    Fixme,
    Hack,
    Xxx,
    Optimize,
    Bug,
    Note,
}

impl TechDebtType {
    pub fn as_str(self) -> &'static str {
        match self {
            TechDebtType::Todo => "TODO",
            TechDebtType::Fixme => "FIXME",
            TechDebtType::Hack => "HACK",
            TechDebtType::Xxx => "XXX",
            TechDebtType::Optimize => "OPTIMIZE",
            TechDebtType::Bug => "BUG",
            TechDebtType::Note => "NOTE",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TechDebtMarker {
    pub marker_type: TechDebtType,
    pub line: usize,
    pub comment: String,
    pub file_path: String,
}

#[derive(Debug, Clone, Default)]
pub struct DeepAnalysis {
    pub purpose: String,
    pub responsibilities: Vec<String>,
    pub key_exports: Vec<String>,
    pub key_imports: Vec<String>,
    pub patterns: Vec<String>,
    pub business_rules: Vec<String>,
    pub gotchas: Vec<String>,
    pub security_notes: Vec<String>,
    pub relationships: Vec<String>,
    pub tech_debt_markers: Vec<TechDebtMarker>,
}

#[derive(Debug, Clone)]
pub struct TraitImpl {
    pub trait_name: String,
    pub type_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct MediumAnalysis {
    pub purpose: String,
    pub key_exports: Vec<String>,
    pub import_sources: Vec<String>,
    pub relationships: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LightAnalysis {
    pub purpose: String,
    pub relationships: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum AnalysisBody {
    Deep(DeepAnalysis),
    Medium(MediumAnalysis),
    Light(LightAnalysis),
    Skip,
}

#[derive(Debug, Clone)]
pub struct FileAnalysisResult {
    pub file_path: String,
    pub extension: String,
    pub line_count: usize,
    pub depth: AnalysisDepth,
    pub body: AnalysisBody,
    /// Rust `impl Trait for Type` pairs found in this file. Populated
    /// regardless of analysis depth so cross-file trait-polymorphism
    /// detection works for Medium/Light-classified adapter files too.
    pub rust_trait_impls: Vec<TraitImpl>,
    /// Structured SQL schema info for `.sql` files (tables, alters,
    /// indexes, migration metadata). `None` for non-SQL or when depth
    /// is Skip.
    pub sql_analysis: Option<sql_analyzer::SqlAnalysis>,
}

impl FileAnalysisResult {
    pub fn from_file(file: &FileEntry, depth: AnalysisDepth, body: AnalysisBody) -> Self {
        Self {
            file_path: file.relative_path.clone(),
            extension: file.extension.clone(),
            line_count: file.line_count,
            depth,
            body,
            rust_trait_impls: Vec::new(),
            sql_analysis: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CrossFilePattern {
    pub name: String,
    pub description: String,
    pub involved_files: Vec<String>,
    pub summary: String,
    pub confidence: f64,
}
