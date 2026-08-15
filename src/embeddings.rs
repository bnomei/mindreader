use crate::config::{EmbeddingProviderKind, SelectedEmbedding};
use crate::{
    embedding_error,
    error::{Context, Error, Result},
};
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f64>>;
    fn provider(&self) -> &'static str;
    fn model(&self) -> &str;
    fn dimensions(&self) -> usize;
}

pub fn build_provider(config: &SelectedEmbedding) -> Result<Box<dyn EmbeddingProvider>> {
    let (provider, endpoint) = match config.provider {
        EmbeddingProviderKind::OpenAi => ("openai", "https://api.openai.com/v1/embeddings"),
        EmbeddingProviderKind::XAi => ("xai", "https://api.x.ai/v1/embeddings"),
    };
    Ok(Box::new(HttpEmbeddingProvider {
        client: Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build embedding HTTP client")?,
        provider,
        endpoint: endpoint.into(),
        api_key: config.api_key.clone(),
        model: config.model.clone(),
        dimensions: config.dimensions,
    }))
}

struct HttpEmbeddingProvider {
    client: Client,
    provider: &'static str,
    endpoint: String,
    api_key: String,
    model: String,
    dimensions: usize,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    embedding: Vec<f64>,
    index: usize,
}

#[async_trait]
impl EmbeddingProvider for HttpEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<Vec<f64>> {
        let text = text.trim();
        if text.is_empty() {
            return Err(embedding_error!("embedding input must not be empty"));
        }
        let body = json!({
            "input": text,
            "model": self.model,
            "dimensions": self.dimensions,
            "encoding_format": "float",
        });
        let mut last_error = None;
        for attempt in 0..3_u64 {
            let response = self
                .client
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => {
                    let mut payload: EmbeddingResponse = response
                        .json()
                        .await
                        .with_context(|| format!("decode {} embedding response", self.provider))?;
                    payload.data.sort_by_key(|item| item.index);
                    if payload.data.len() != 1 {
                        return Err(embedding_error!(
                            "{} embedding response contained {} vectors for one input",
                            self.provider,
                            payload.data.len()
                        ));
                    }
                    return normalize_vector(
                        payload.data.remove(0).embedding,
                        self.dimensions,
                        self.provider,
                    );
                }
                Ok(response) => {
                    let status = response.status();
                    let retry = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                    let body = response.text().await.unwrap_or_default();
                    let body: String = body.chars().take(512).collect();
                    last_error = Some(embedding_error!(
                        "{} embedding request failed with HTTP {}: {}",
                        self.provider,
                        status,
                        body
                    ));
                    if !retry {
                        break;
                    }
                }
                Err(error) => {
                    last_error = Some(
                        Error::from(error)
                            .context(format!("{} embedding request failed", self.provider)),
                    );
                }
            }
            if attempt < 2 {
                sleep(Duration::from_millis(200 * (1 << attempt))).await;
            }
        }
        Err(last_error
            .unwrap_or_else(|| embedding_error!("{} embedding request failed", self.provider)))
    }

    fn provider(&self) -> &'static str {
        self.provider
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

pub fn normalize_vector(
    mut vector: Vec<f64>,
    dimensions: usize,
    provider: &str,
) -> Result<Vec<f64>> {
    if vector.len() != dimensions {
        return Err(embedding_error!(
            "{provider} returned embedding dimension {}, expected {dimensions}",
            vector.len()
        ));
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(embedding_error!(
            "{provider} returned a non-finite embedding value"
        ));
    }
    let norm = vector.iter().map(|value| value * value).sum::<f64>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return Err(embedding_error!(
            "{provider} returned a zero-norm embedding"
        ));
    }
    for value in &mut vector {
        *value /= norm;
    }
    Ok(vector)
}

#[cfg(test)]
mod tests {
    use super::normalize_vector;

    #[test]
    fn normalizes_valid_vectors() {
        let vector = normalize_vector(vec![3.0, 4.0], 2, "test").unwrap();
        assert!((vector[0] - 0.6).abs() < 1e-9);
        assert!((vector[1] - 0.8).abs() < 1e-9);
    }

    #[test]
    fn rejects_invalid_vectors() {
        assert!(normalize_vector(vec![0.0, 0.0], 2, "test").is_err());
        assert!(normalize_vector(vec![1.0], 2, "test").is_err());
        assert!(normalize_vector(vec![f64::NAN, 1.0], 2, "test").is_err());
    }
}
