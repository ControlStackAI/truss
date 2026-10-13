//! Provider-backed embedders for memory recall.

use anyhow::{anyhow, Result};
use serde_json::json;
use truss_core::memory::Embedder;

/// OpenAI-compatible `/v1/embeddings` backend.
#[derive(Debug, Clone)]
pub struct OpenAiEmbedder {
    client: reqwest::blocking::Client,
    api_key: String,
    base_url: String,
    model: String,
    dims: usize,
    id: String,
}

impl OpenAiEmbedder {
    pub fn from_env(
        model: impl Into<String>,
        base_url: Option<String>,
        api_key_env: impl AsRef<str>,
        dims: usize,
    ) -> Result<Self> {
        let env_var = api_key_env.as_ref();
        let api_key = std::env::var(env_var).map_err(|_| anyhow!("missing env var {env_var}"))?;
        let model = model.into();
        let dims = dims.max(1);
        let base_url = base_url.unwrap_or_else(|| "https://api.openai.com/v1".into());
        let id = format!("openai:{model}:{dims}");
        Ok(Self {
            client: reqwest::blocking::Client::new(),
            api_key,
            base_url,
            model,
            dims,
            id,
        })
    }
}

impl Embedder for OpenAiEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let body = json!({
            "model": self.model,
            "input": text,
            "dimensions": self.dims,
        });
        let response: serde_json::Value = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()?
            .error_for_status()?
            .json()?;
        let values = response
            .get("data")
            .and_then(|data| data.get(0))
            .and_then(|item| item.get("embedding"))
            .and_then(|embedding| embedding.as_array())
            .ok_or_else(|| anyhow!("embedding response missing data[0].embedding"))?;
        values
            .iter()
            .map(|value| {
                value
                    .as_f64()
                    .map(|number| number as f32)
                    .ok_or_else(|| anyhow!("embedding response contained a non-number"))
            })
            .collect()
    }

    fn dims(&self) -> usize {
        self.dims
    }

    fn id(&self) -> &str {
        &self.id
    }
}
