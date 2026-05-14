pub mod graph_builder;
pub mod import_parser;
pub mod importance_scorer;
pub mod parsers;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;

use crate::huginn::analyzer::AnalysisDepth;

#[derive(Debug, Clone)]
pub struct ParsedImport {
    pub raw: String,
    pub source: String,
    pub is_relative: bool,
    pub is_external: bool,
    pub resolved_path: Option<String>,
    pub imported_names: Vec<String>,
    pub is_default: bool,
    pub is_side_effect: bool,
    pub is_type_only: bool,
}

#[derive(Debug, Clone)]
pub struct ParsedExport {
    pub name: String,
    pub is_default: bool,
    pub is_type_only: bool,
}

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub file_path: String,
    pub imports: Vec<ParsedImport>,
    pub exports: Vec<ParsedExport>,
    pub parse_errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FileNode {
    pub path: String,
    pub absolute_path: PathBuf,
    pub extension: String,
    pub imports: Vec<String>,
    pub resolved_imports: Vec<String>,
    pub export_count: usize,
    pub line_count: usize,
    pub last_modified: Option<SystemTime>,
    pub is_entry_point: bool,
    pub depth: i32, // -1 means unreachable (was Infinity in TS)
}

#[derive(Debug, Default)]
pub struct DependencyGraph {
    pub nodes: HashMap<String, FileNode>,
    pub edges: HashMap<String, HashSet<String>>,
    pub reverse_edges: HashMap<String, HashSet<String>>,
}

#[derive(Debug, Clone, Copy)]
pub struct ScoreComponents {
    pub entry_proximity: i32,
    pub import_frequency: i32,
    pub file_type_signal: i32,
    pub size_signal: i32,
    pub recency_signal: i32,
}

#[derive(Debug, Clone, Copy)]
pub struct ImportanceScore {
    pub total: i32,
    pub components: ScoreComponents,
    pub analysis_depth: AnalysisDepth,
}
