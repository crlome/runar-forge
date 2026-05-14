//! Shared enum for analysis depth. Historical home of the legacy
//! regex-based analyzer — superseded by `huginn::graph` (import parser,
//! importance scorer) and `huginn::analysis` (file analyzer, pattern
//! extractor, arch summarizer). Only the depth enum remains.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisDepth {
    Deep,
    Medium,
    Light,
    Skip,
}

impl PartialOrd for AnalysisDepth {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AnalysisDepth {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let to_num = |d: &AnalysisDepth| match d {
            AnalysisDepth::Skip => 0,
            AnalysisDepth::Light => 1,
            AnalysisDepth::Medium => 2,
            AnalysisDepth::Deep => 3,
        };
        to_num(self).cmp(&to_num(other))
    }
}
