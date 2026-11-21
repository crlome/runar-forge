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

// ── HTTP time bounds ───────────────────────────────────────────────

/// Time to set up a connection (DNS, TCP, TLS). Without it an unreachable
/// host waits for the OS connect timeout, about 75 s on macOS.
const HTTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// An HTTP client with connect and whole-request bounds. reqwest's default
/// client has neither, so an endpoint that accepted a connection and never
/// answered held the caller, and the serial MCP loop behind it, for the
/// rest of the session.
pub(crate) fn bounded_http_client(
    request_timeout: std::time::Duration,
) -> reqwest::Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(request_timeout);
    // Tests talk to a listener on 127.0.0.1; a proxy configured on the
    // developer's machine must not sit in between.
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder.build()
}

// ── OpenAI Embeddings API ──────────────────────────────────────────

const OPENAI_EMBEDDINGS_URL: &str = "https://api.openai.com/v1/embeddings";

/// Whole-request bound for one embedding call.
const EMBED_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct OpenAIEmbeddingProvider {
    client: reqwest::Client,
    endpoint: String,
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
    /// Fails only when the HTTP client cannot be built; the factory then
    /// runs without embeddings.
    pub fn new(api_key: String, model: Option<String>) -> reqwest::Result<Self> {
        Self::with_endpoint(api_key, model, OPENAI_EMBEDDINGS_URL, EMBED_REQUEST_TIMEOUT)
    }

    fn with_endpoint(
        api_key: String,
        model: Option<String>,
        endpoint: &str,
        request_timeout: std::time::Duration,
    ) -> reqwest::Result<Self> {
        let model = model.unwrap_or_else(|| "text-embedding-3-small".into());
        let dims = if model == "text-embedding-3-large" {
            3072
        } else {
            1536
        };

        Ok(Self {
            client: bounded_http_client(request_timeout)?,
            endpoint: endpoint.to_string(),
            api_key,
            model,
            dims,
        })
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

        let response = match self
            .client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&req)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    timed_out = e.is_timeout(),
                    "embedding request failed; continuing without a vector"
                );
                return None;
            }
        };

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
                    return openai_provider(api_key, model);
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
            return openai_provider(api_key, model);
        }
    }

    tracing::info!("no OPENAI_API_KEY set — embeddings disabled, using FTS only");
    Box::new(DisabledEmbeddingProvider)
}

fn openai_provider(api_key: String, model: Option<String>) -> Box<dyn EmbeddingProvider> {
    match OpenAIEmbeddingProvider::new(api_key, model) {
        Ok(provider) => Box::new(provider),
        Err(e) => {
            tracing::warn!(error = %e, "could not build the embeddings HTTP client; embeddings disabled");
            Box::new(DisabledEmbeddingProvider)
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// reqwest's default client has no timeout, so against an endpoint that
    /// accepts and never answers, `embed` never returned.
    #[tokio::test]
    async fn openai_embed_gives_up_on_a_server_that_never_answers() {
        let base = crate::test_support::silent_http_listener().await;
        let provider = OpenAIEmbeddingProvider::with_endpoint(
            "test-key".into(),
            None,
            &format!("{base}/v1/embeddings"),
            std::time::Duration::from_millis(300),
        )
        .unwrap();
        let outcome =
            tokio::time::timeout(std::time::Duration::from_secs(5), provider.embed("x")).await;
        assert_eq!(outcome, Ok(None), "embed must give up, not hang");
    }
}
