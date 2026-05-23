//! Local ONNX embedding provider — fastembed-backed, sentence-transformers
//! `all-MiniLM-L6-v2` (384-dim) by default. First-run downloads model files
//! to `$HOME/.runar-forge/models/`; subsequent runs hit the local cache so
//! there's no network dependency at query time.
//!
//! Gated behind the `local-embeddings` Cargo feature so the default binary
//! stays lean (fastembed pulls in ort + tokenizers + ONNX Runtime native
//! libraries, adding ~20 MB). Release CI can enable the feature for the
//! shipped artifact.

use std::sync::Arc;

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use super::EmbeddingProvider;

pub struct LocalOnnxEmbeddingProvider {
    // Arc so `embed` can move a cheap clone into spawn_blocking without
    // borrowing `&self` across the await boundary.
    model: Arc<TextEmbedding>,
    name: &'static str,
    dims: usize,
}

impl LocalOnnxEmbeddingProvider {
    /// Construct the provider, downloading the model on first use. Blocks
    /// until the model is ready; callers that need non-blocking behavior
    /// should wrap in `tokio::task::spawn_blocking`.
    pub fn try_new() -> Result<Self, String> {
        let cache_dir = resolve_cache_dir();
        std::fs::create_dir_all(&cache_dir).map_err(|e| format!("cache dir: {e}"))?;

        let requested = std::env::var("RUNAR_EMBEDDING_MODEL")
            .ok()
            .unwrap_or_else(|| "all-MiniLM-L6-v2".to_string());
        let (model_id, name, dims) = resolve_model(&requested)?;

        let opts = InitOptions::new(model_id)
            .with_show_download_progress(false)
            .with_cache_dir(cache_dir);

        let model =
            TextEmbedding::try_new(opts).map_err(|e| format!("fastembed init failed: {e}"))?;

        Ok(Self {
            model: Arc::new(model),
            name,
            dims,
        })
    }
}

fn resolve_cache_dir() -> std::path::PathBuf {
    if let Ok(custom) = std::env::var("RUNAR_MODELS_DIR") {
        return std::path::PathBuf::from(custom);
    }
    crate::setup::runar_dir().join("models")
}

/// Map a user-facing model name to its fastembed enum + dimension count.
/// Intentionally short: two small, well-tested sentence-transformer models.
/// Additional models can be added once we have a benchmark number to
/// justify the binary/disk cost.
fn resolve_model(name: &str) -> Result<(EmbeddingModel, &'static str, usize), String> {
    let lower = name.to_lowercase();
    Ok(match lower.as_str() {
        "all-minilm-l6-v2" | "minilm" | "minilm-l6-v2" => {
            (EmbeddingModel::AllMiniLML6V2, "all-MiniLM-L6-v2", 384)
        }
        "bge-small-en-v1.5" | "bge-small" => {
            (EmbeddingModel::BGESmallENV15, "bge-small-en-v1.5", 384)
        }
        other => {
            return Err(format!(
                "unknown local embedding model '{other}'. Supported: all-MiniLM-L6-v2, bge-small-en-v1.5"
            ));
        }
    })
}

#[async_trait]
impl EmbeddingProvider for LocalOnnxEmbeddingProvider {
    fn name(&self) -> &str {
        self.name
    }
    fn dimensions(&self) -> usize {
        self.dims
    }
    fn is_available(&self) -> bool {
        true
    }

    async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let model = self.model.clone();
        let input = text.to_string();
        // fastembed's `embed` is synchronous + CPU-bound; hop to the blocking
        // pool so we don't stall tokio.
        tokio::task::spawn_blocking(move || {
            model
                .embed(vec![input], None)
                .ok()
                .and_then(|mut v| v.pop())
        })
        .await
        .ok()
        .flatten()
    }

    async fn health_check(&self) -> bool {
        match self.embed("health check").await {
            Some(v) => v.len() == self.dims,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_accepts_known_ids() {
        assert_eq!(resolve_model("all-MiniLM-L6-v2").unwrap().2, 384);
        assert_eq!(resolve_model("minilm").unwrap().2, 384);
        assert_eq!(resolve_model("BGE-Small").unwrap().2, 384);
        assert!(resolve_model("gpt-5-turbo").is_err());
    }

    #[test]
    fn cache_dir_honors_env_override() {
        let custom = std::env::temp_dir().join("runar-local-embeddings");
        std::env::set_var("RUNAR_MODELS_DIR", &custom);
        let dir = resolve_cache_dir();
        assert_eq!(dir, custom);
        std::env::remove_var("RUNAR_MODELS_DIR");
    }
}
