use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn name(&self) -> &str;
    fn dimensions(&self) -> usize;
    fn is_available(&self) -> bool;
    async fn embed(&self, text: &str) -> Option<Vec<f32>>;
    async fn health_check(&self) -> bool;
}

// ── Disabled (no embeddings) ───────────────────────────────────────

pub struct DisabledEmbeddingProvider;

#[async_trait]
impl EmbeddingProvider for DisabledEmbeddingProvider {
    fn name(&self) -> &str {
        "disabled"
    }
    fn dimensions(&self) -> usize {
        0
    }
    fn is_available(&self) -> bool {
        false
    }
    async fn embed(&self, _text: &str) -> Option<Vec<f32>> {
        None
    }
    async fn health_check(&self) -> bool {
        false
    }
}

// ── OpenAI Embeddings API ──────────────────────────────────────────

pub struct OpenAIEmbeddingProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    dims: usize,
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a str,
    dimensions: usize,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

impl OpenAIEmbeddingProvider {
    pub fn new(api_key: String, model: Option<String>) -> Self {
        let model = model.unwrap_or_else(|| "text-embedding-3-small".into());
        let dims = if model == "text-embedding-3-large" {
            3072
        } else {
            1536
        };

        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            dims,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAIEmbeddingProvider {
    fn name(&self) -> &str {
        "openai"
    }
    fn dimensions(&self) -> usize {
        self.dims
    }
    fn is_available(&self) -> bool {
        true
    }

    async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let req = EmbeddingRequest {
            model: &self.model,
            input: text,
            dimensions: self.dims,
        };

        let response = self
            .client
            .post("https://api.openai.com/v1/embeddings")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&req)
            .send()
            .await
            .ok()?;

        if !response.status().is_success() {
            tracing::warn!(
                status = %response.status(),
                "OpenAI embedding API returned non-200"
            );
            return None;
        }

        let body: EmbeddingResponse = response.json().await.ok()?;
        body.data.into_iter().next().map(|d| d.embedding)
    }

    async fn health_check(&self) -> bool {
        match self.embed("health check").await {
            Some(v) => v.len() == self.dims,
            None => false,
        }
    }
}

// ── Local ONNX (feature-gated) ─────────────────────────────────────

#[cfg(feature = "local-embeddings")]
pub mod local;

// ── Factory ────────────────────────────────────────────────────────

/// Select the embedding provider from `RUNAR_EMBEDDINGS` (`local | openai
/// | disabled`). When unset, auto-pick: OpenAI if `OPENAI_API_KEY` is set,
/// else disabled. Local mode requires the `local-embeddings` Cargo feature.
pub fn create_embedding_provider() -> Box<dyn EmbeddingProvider> {
    let mode = std::env::var("RUNAR_EMBEDDINGS")
        .ok()
        .map(|s| s.to_lowercase());

    match mode.as_deref() {
        Some("disabled") => {
            tracing::info!("RUNAR_EMBEDDINGS=disabled — using FTS only");
            return Box::new(DisabledEmbeddingProvider);
        }
        Some("openai") => {
            if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
                if !api_key.is_empty() {
                    let model = std::env::var("RUNAR_EMBEDDING_MODEL").ok();
                    tracing::info!(
                        model = model.as_deref().unwrap_or("text-embedding-3-small"),
                        "RUNAR_EMBEDDINGS=openai — using OpenAI embeddings"
                    );
                    return Box::new(OpenAIEmbeddingProvider::new(api_key, model));
                }
            }
            tracing::warn!(
                "RUNAR_EMBEDDINGS=openai but no OPENAI_API_KEY set — falling back to disabled"
            );
            return Box::new(DisabledEmbeddingProvider);
        }
        Some("local") => {
            return create_local_provider();
        }
        Some(other) => {
            tracing::warn!(
                mode = %other,
                "unknown RUNAR_EMBEDDINGS value; expected local|openai|disabled"
            );
        }
        None => {}
    }

    // Auto-detect when RUNAR_EMBEDDINGS is unset.
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        if !api_key.is_empty() {
            let model = std::env::var("RUNAR_EMBEDDING_MODEL").ok();
            tracing::info!(
                model = model.as_deref().unwrap_or("text-embedding-3-small"),
                "using OpenAI embeddings"
            );
            return Box::new(OpenAIEmbeddingProvider::new(api_key, model));
        }
    }

    tracing::info!("no OPENAI_API_KEY set — embeddings disabled, using FTS only");
    Box::new(DisabledEmbeddingProvider)
}

#[cfg(feature = "local-embeddings")]
fn create_local_provider() -> Box<dyn EmbeddingProvider> {
    match local::LocalOnnxEmbeddingProvider::try_new() {
        Ok(p) => {
            tracing::info!(
                model = p.name(),
                dims = p.dimensions(),
                "RUNAR_EMBEDDINGS=local — using local ONNX embeddings"
            );
            Box::new(p)
        }
        Err(e) => {
            tracing::warn!(error = %e, "local ONNX init failed — falling back to disabled");
            Box::new(DisabledEmbeddingProvider)
        }
    }
}

#[cfg(not(feature = "local-embeddings"))]
fn create_local_provider() -> Box<dyn EmbeddingProvider> {
    tracing::warn!(
        "RUNAR_EMBEDDINGS=local requested but binary was built without the \
         `local-embeddings` Cargo feature — falling back to disabled"
    );
    Box::new(DisabledEmbeddingProvider)
}
