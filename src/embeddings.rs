//! Remote embedding providers and vector normalization for semantic search.
//!
//! Builds OpenAI or xAI HTTP clients from selected config, retries transient
//! failures with bounded timeouts, and normalizes vectors to unit length so
//! cosine similarity matches Neo4j vector queries. API keys stay in request
//! headers and are never logged.

use crate::config::{EmbeddingProviderKind, SelectedEmbedding};
use crate::{
    embedding_error,
    error::{Context, Error, Result},
};
use async_trait::async_trait;
use reqwest::header::HeaderMap;
use reqwest::{Client, Response, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, SystemTime};
use tokio::time::{sleep, timeout, Instant};
use uuid::Uuid;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 512;

#[derive(Debug, Clone, Copy)]
struct RetryPolicy {
    max_attempts: u32,
    operation_timeout: Duration,
    request_timeout: Duration,
    connect_timeout: Duration,
    base_delay: Duration,
    max_jitter: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            operation_timeout: Duration::from_secs(20),
            request_timeout: Duration::from_secs(6),
            connect_timeout: Duration::from_secs(3),
            base_delay: Duration::from_millis(250),
            max_jitter: Duration::from_millis(250),
        }
    }
}

/// Async embedding backend used by semantic search and smoke fixtures.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Send query text to the remote provider; never log the API key.
    async fn embed(&self, text: &str) -> Result<Vec<f64>>;
    fn provider(&self) -> &'static str;
    fn model(&self) -> &str;
    fn dimensions(&self) -> usize;
}

/// Construct the HTTP embedding provider for the selected OpenAI or xAI config.
pub fn build_provider(config: &SelectedEmbedding) -> Result<Box<dyn EmbeddingProvider>> {
    let (provider, endpoint) = match config.provider {
        EmbeddingProviderKind::OpenAi => ("openai", "https://api.openai.com/v1/embeddings"),
        EmbeddingProviderKind::XAi => ("xai", "https://api.x.ai/v1/embeddings"),
    };
    Ok(Box::new(HttpEmbeddingProvider::new(
        provider,
        endpoint,
        config.api_key.clone(),
        config.model.clone(),
        config.dimensions,
        RetryPolicy::default(),
    )?))
}

struct HttpEmbeddingProvider {
    client: Client,
    provider: &'static str,
    endpoint: String,
    api_key: String,
    model: String,
    dimensions: usize,
    policy: RetryPolicy,
}

impl HttpEmbeddingProvider {
    fn new(
        provider: &'static str,
        endpoint: impl Into<String>,
        api_key: String,
        model: String,
        dimensions: usize,
        policy: RetryPolicy,
    ) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(policy.connect_timeout)
                .timeout(policy.request_timeout)
                .retry(reqwest::retry::never())
                .build()
                .context("build embedding HTTP client")?,
            provider,
            endpoint: endpoint.into(),
            api_key,
            model,
            dimensions,
            policy,
        })
    }

    async fn embed_inner(&self, text: &str) -> Result<Vec<f64>> {
        let body = json!({
            "input": text,
            "model": self.model,
            "dimensions": self.dimensions,
            "encoding_format": "float",
        });
        let deadline = Instant::now() + self.policy.operation_timeout;
        for attempt in 0..self.policy.max_attempts {
            let response = self
                .client
                .post(&self.endpoint)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await;
            let (error, retry, retry_after) = match response {
                Ok(response) if response.status().is_success() => {
                    match read_limited(response, MAX_RESPONSE_BYTES).await {
                        Ok((bytes, truncated)) => {
                            if truncated {
                                return Err(embedding_error!(
                                    "{} embedding response exceeded {MAX_RESPONSE_BYTES} bytes",
                                    self.provider
                                ));
                            }
                            let mut payload: EmbeddingResponse = serde_json::from_slice(&bytes)
                                .with_context(|| {
                                    format!("decode {} embedding response", self.provider)
                                })?;
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
                        Err(error) => (
                            Error::from(error).context(format!(
                                "{} embedding response body read failed",
                                self.provider
                            )),
                            true,
                            None,
                        ),
                    }
                }
                Ok(response) => {
                    let status = response.status();
                    let headers = response.headers().clone();
                    let request_id = header_text(&headers, "x-request-id");
                    let retry = should_retry_response(self.provider, status, &headers);
                    let retry_after = retry_after(&headers, SystemTime::now());
                    let body = match read_limited(response, MAX_ERROR_BYTES).await {
                        Ok((body, _)) => String::from_utf8_lossy(&body).into_owned(),
                        Err(error) => format!("response body read failed: {error}"),
                    };
                    (
                        Error::EmbeddingHttp {
                            provider: self.provider,
                            status: status.as_u16(),
                            request_id,
                            body,
                        },
                        retry,
                        retry_after,
                    )
                }
                Err(error) => {
                    let retry = retryable_reqwest_error(&error);
                    (
                        Error::from(error)
                            .context(format!("{} embedding request failed", self.provider)),
                        retry,
                        None,
                    )
                }
            };
            let completed_attempts = attempt + 1;
            if !retry {
                return Err(error);
            }
            if completed_attempts >= self.policy.max_attempts {
                return Err(error.context(format!(
                    "{} embedding retry budget exhausted after {completed_attempts} attempts",
                    self.provider
                )));
            }
            let delay = retry_delay(
                self.policy,
                attempt,
                retry_after,
                sampled_jitter(self.policy),
            );
            if delay >= deadline.saturating_duration_since(Instant::now()) {
                return Err(error.context(format!(
                    "{} embedding Retry-After exceeds the remaining operation budget",
                    self.provider
                )));
            }
            sleep(delay).await;
        }
        unreachable!("positive bounded retry loop always returns")
    }
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
        timeout(self.policy.operation_timeout, self.embed_inner(text))
            .await
            .with_context(|| {
                format!(
                    "{} embedding operation exceeded {} seconds",
                    self.provider,
                    self.policy.operation_timeout.as_secs()
                )
            })?
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

fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn should_retry_response(provider: &str, status: StatusCode, headers: &HeaderMap) -> bool {
    if provider == "openai" {
        match header_text(headers, "x-should-retry").as_deref() {
            Some("true") => return true,
            Some("false") => return false,
            _ => {}
        }
    }
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT | StatusCode::CONFLICT | StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
}

fn retryable_reqwest_error(error: &reqwest::Error) -> bool {
    !error.is_builder()
        && !error.is_redirect()
        && (error.is_timeout() || error.is_connect() || error.is_request() || error.is_body())
}

fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    if let Some(milliseconds) = header_text(headers, "retry-after-ms")
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0 && *value <= u64::MAX as f64)
    {
        return Some(Duration::from_millis(milliseconds.ceil() as u64));
    }
    let value = header_text(headers, "retry-after")?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(&value)
        .ok()
        .map(|date| date.duration_since(now).unwrap_or_default())
}

fn sampled_jitter(policy: RetryPolicy) -> Duration {
    let maximum = policy.max_jitter.as_millis() as u64;
    if maximum == 0 {
        return Duration::ZERO;
    }
    let bytes = Uuid::new_v4();
    let sample = u16::from_be_bytes([bytes.as_bytes()[0], bytes.as_bytes()[1]]) as u64;
    Duration::from_millis(sample % (maximum + 1))
}

fn retry_delay(
    policy: RetryPolicy,
    attempt: u32,
    retry_after: Option<Duration>,
    jitter: Duration,
) -> Duration {
    retry_after
        .unwrap_or_else(|| policy.base_delay.saturating_mul(1_u32 << attempt))
        .saturating_add(jitter)
}

async fn read_limited(
    mut response: Response,
    limit: usize,
) -> std::result::Result<(Vec<u8>, bool), reqwest::Error> {
    let mut body = Vec::with_capacity(limit.min(8 * 1024));
    while let Some(chunk) = response.chunk().await? {
        let remaining = limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
        if body.len() == limit {
            return Ok((body, response.chunk().await?.is_some()));
        }
    }
    Ok((body, false))
}

/// Validate dimension and finiteness, then L2-normalize for cosine similarity.
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
    let squared_norm = vector.iter().try_fold(0.0, |sum, value| {
        value.is_finite().then_some(sum + value * value)
    });
    let Some(squared_norm) = squared_norm else {
        return Err(embedding_error!(
            "{provider} returned a non-finite embedding value"
        ));
    };
    let norm = squared_norm.sqrt();
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
    use super::*;
    use reqwest::header::HeaderValue;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

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

    #[test]
    fn retry_headers_and_classification_are_bounded() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("2"));
        assert_eq!(
            retry_after(&headers, SystemTime::UNIX_EPOCH),
            Some(Duration::from_secs(2))
        );
        headers.insert("retry-after-ms", HeaderValue::from_static("125.1"));
        assert_eq!(
            retry_after(&headers, SystemTime::UNIX_EPOCH),
            Some(Duration::from_millis(126))
        );
        headers.insert("x-should-retry", HeaderValue::from_static("false"));
        assert!(!should_retry_response(
            "openai",
            StatusCode::SERVICE_UNAVAILABLE,
            &headers
        ));
        assert!(should_retry_response(
            "xai",
            StatusCode::SERVICE_UNAVAILABLE,
            &headers
        ));
        assert!(!should_retry_response(
            "xai",
            StatusCode::BAD_REQUEST,
            &HeaderMap::new()
        ));
    }

    #[test]
    fn fallback_and_server_delays_include_bounded_jitter() {
        let policy = RetryPolicy::default();
        assert_eq!(
            retry_delay(policy, 0, None, Duration::from_millis(10)),
            Duration::from_millis(260)
        );
        assert_eq!(
            retry_delay(
                policy,
                1,
                Some(Duration::from_secs(2)),
                Duration::from_millis(10)
            ),
            Duration::from_millis(2_010)
        );
    }

    fn response(status: &str, headers: &[(&str, &str)], body: &str) -> String {
        partial_response(status, headers, body.len(), body)
    }

    fn partial_response(
        status: &str,
        headers: &[(&str, &str)],
        content_length: usize,
        body: &str,
    ) -> String {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {content_length}\r\nConnection: close\r\n"
        );
        for (name, value) in headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(body);
        response
    }

    struct ScriptedResponse {
        payload: String,
        stall_after_write: Duration,
    }

    fn immediate(payload: String) -> ScriptedResponse {
        ScriptedResponse {
            payload,
            stall_after_write: Duration::ZERO,
        }
    }

    fn scripted_server(
        responses: Vec<String>,
    ) -> (
        String,
        Arc<AtomicUsize>,
        thread::JoinHandle<std::io::Result<()>>,
    ) {
        scripted_server_steps(responses.into_iter().map(immediate).collect())
    }

    fn scripted_server_steps(
        responses: Vec<ScriptedResponse>,
    ) -> (
        String,
        Arc<AtomicUsize>,
        thread::JoinHandle<std::io::Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1/embeddings", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let handle = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            for response in responses {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= deadline {
                                return Ok(());
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => return Err(error),
                    }
                };
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(Duration::from_secs(1)))?;
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = stream.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(header_end) = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .map(|index| index + 4)
                    {
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= header_end + content_length {
                            break;
                        }
                    }
                }
                observed.fetch_add(1, Ordering::SeqCst);
                stream.write_all(response.payload.as_bytes())?;
                if !response.stall_after_write.is_zero() {
                    thread::sleep(response.stall_after_write);
                }
            }
            Ok(())
        });
        (endpoint, requests, handle)
    }

    fn test_provider(endpoint: String) -> HttpEmbeddingProvider {
        HttpEmbeddingProvider::new(
            "openai",
            endpoint,
            "test-key".into(),
            "test-model".into(),
            2,
            RetryPolicy {
                max_attempts: 3,
                operation_timeout: Duration::from_millis(500),
                request_timeout: Duration::from_millis(200),
                connect_timeout: Duration::from_millis(100),
                base_delay: Duration::from_millis(1),
                max_jitter: Duration::ZERO,
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn retries_rate_limits_but_not_bad_requests() {
        let success = response(
            "200 OK",
            &[("Content-Type", "application/json")],
            r#"{"data":[{"embedding":[3.0,4.0],"index":0}]}"#,
        );
        let (endpoint, requests, handle) = scripted_server(vec![
            response("429 Too Many Requests", &[("Retry-After", "0")], "busy"),
            success,
        ]);
        let vector = test_provider(endpoint).embed("spaceship").await;
        handle.join().unwrap().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2, "result={vector:?}");
        let vector = vector.unwrap();
        assert!((vector[0] - 0.6).abs() < 1e-9);

        let (endpoint, requests, handle) =
            scripted_server(vec![response("400 Bad Request", &[], "invalid")]);
        let error = test_provider(endpoint)
            .embed("spaceship")
            .await
            .unwrap_err();
        handle.join().unwrap().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(matches!(error, Error::EmbeddingHttp { status: 400, .. }));
    }

    #[tokio::test]
    async fn rejects_retry_after_beyond_the_operation_budget() {
        let (endpoint, requests, handle) = scripted_server(vec![response(
            "429 Too Many Requests",
            &[("Retry-After", "120")],
            "later",
        )]);
        let started = std::time::Instant::now();
        let error = test_provider(endpoint)
            .embed("spaceship")
            .await
            .unwrap_err();
        handle.join().unwrap().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(error.to_string().contains("remaining operation budget"));
    }

    #[tokio::test]
    async fn retries_truncated_success_response_bodies() {
        let success = response(
            "200 OK",
            &[("Content-Type", "application/json")],
            r#"{"data":[{"embedding":[3.0,4.0],"index":0}]}"#,
        );
        let truncated = partial_response(
            "200 OK",
            &[("Content-Type", "application/json")],
            128,
            r#"{"data":["#,
        );
        let (endpoint, requests, handle) = scripted_server(vec![truncated, success]);

        let vector = test_provider(endpoint).embed("spaceship").await;

        handle.join().unwrap().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2, "result={vector:?}");
        assert!(vector.is_ok());
    }

    #[tokio::test]
    async fn retries_stalled_retryable_http_response_bodies() {
        let success = response(
            "200 OK",
            &[("Content-Type", "application/json")],
            r#"{"data":[{"embedding":[3.0,4.0],"index":0}]}"#,
        );
        let stalled = ScriptedResponse {
            payload: partial_response(
                "503 Service Unavailable",
                &[("X-Request-Id", "request-stalled")],
                4,
                "",
            ),
            stall_after_write: Duration::from_millis(250),
        };
        let (endpoint, requests, handle) = scripted_server_steps(vec![stalled, immediate(success)]);

        let vector = test_provider(endpoint).embed("spaceship").await;

        handle.join().unwrap().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2, "result={vector:?}");
        assert!(vector.is_ok());
    }

    #[tokio::test]
    async fn truncated_nonretryable_http_response_retains_typed_status() {
        let truncated = partial_response(
            "400 Bad Request",
            &[("X-Request-Id", "request-invalid")],
            128,
            "invalid",
        );
        let (endpoint, requests, handle) = scripted_server(vec![truncated]);

        let error = test_provider(endpoint)
            .embed("spaceship")
            .await
            .unwrap_err();

        handle.join().unwrap().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(matches!(
            error,
            Error::EmbeddingHttp {
                status: 400,
                request_id: Some(ref request_id),
                ref body,
                ..
            } if request_id == "request-invalid" && body.contains("body read failed")
        ));
    }
}
