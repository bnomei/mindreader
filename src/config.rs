//! Native configuration and colocated secret loading.
//!
//! Non-secret settings live in `config.toml` under the OS config directory.
//! Secrets (`NEO4J_PASSWORD`, embedding API keys) load from a colocated `.env`
//! or the process environment; process values win when non-empty. Passwords
//! are never logged and remain required for Neo4j connections.

use crate::{
    config_error,
    error::{Context, Result},
};
#[cfg(not(unix))]
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{collections::HashMap, env};

const CONFIG_FILE: &str = "config.toml";
const SECRETS_FILE: &str = ".env";

/// Bolt connection endpoints and auth user for Neo4j (password is secret-only).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Neo4jConfig {
    /// Bolt URI as supplied by the operator (not rewritten).
    pub uri: String,
    /// Neo4j auth user; password stays in secrets.
    pub user: String,
}

impl Default for Neo4jConfig {
    fn default() -> Self {
        Self {
            uri: "bolt://127.0.0.1:7687".into(),
            user: "neo4j".into(),
        }
    }
}

/// Per-provider embedding model id and output dimensionality.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    /// Remote model id; required once that provider's API key is set.
    pub model: String,
    /// Output width that must match the Neo4j activation index (1..=4096).
    pub dimensions: usize,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            dimensions: 1536,
        }
    }
}

/// Embedding provider sections in `config.toml` (keys stay in secrets).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingsConfig {
    /// Used when `OPENAI_API_KEY` is present (takes precedence over xAI).
    pub openai: ProviderConfig,
    /// Used when only `XAI_API_KEY` is present.
    pub xai: ProviderConfig,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            openai: ProviderConfig {
                model: "text-embedding-3-small".into(),
                dimensions: 1536,
            },
            xai: ProviderConfig::default(),
        }
    }
}

/// Tunables for semantic activation TTL, neighbor recall, and RRF fusion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticConfig {
    /// Activation lease length; APOC TTL refreshes this on contributing recalls.
    pub ttl_days: u64,
    /// Maximum live activation neighbors pulled from the vector index.
    pub neighbor_limit: usize,
    /// Minimum cosine score for an activation to enter rank fusion.
    pub recall_similarity_threshold: f64,
    /// Cosine floor before two activations may converge into one node.
    pub convergence_similarity_threshold: f64,
    /// Jaccard floor on result-ref overlap required for convergence.
    pub convergence_result_overlap_threshold: f64,
    /// Reciprocal-rank fusion `k`; larger values flatten rank gaps.
    pub rrf_k: f64,
    /// RRF weight for direct facts with exact phrase or property evidence.
    pub direct_weight: f64,
    /// RRF weight for direct facts found only through fallback keywords.
    pub keyword_weight: f64,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            ttl_days: 30,
            neighbor_limit: 10,
            recall_similarity_threshold: 0.65,
            convergence_similarity_threshold: 0.90,
            convergence_result_overlap_threshold: 0.60,
            rrf_k: 15.0,
            direct_weight: 2.0,
            keyword_weight: 0.5,
        }
    }
}

impl SemanticConfig {
    /// Reject non-positive TTL, out-of-range neighbor/RRF knobs, or non-finite thresholds.
    fn validate(&self) -> Result<()> {
        if self.ttl_days == 0 {
            return Err(config_error!("semantic.ttl_days must be greater than zero"));
        }
        if self
            .ttl_days
            .checked_mul(86_400_000)
            .is_none_or(|milliseconds| milliseconds > i64::MAX as u64)
        {
            return Err(config_error!("semantic.ttl_days is too large"));
        }
        if !(1..=100).contains(&self.neighbor_limit) {
            return Err(config_error!(
                "semantic.neighbor_limit must be between 1 and 100"
            ));
        }
        for (name, value) in [
            (
                "semantic.convergence_similarity_threshold",
                self.convergence_similarity_threshold,
            ),
            (
                "semantic.convergence_result_overlap_threshold",
                self.convergence_result_overlap_threshold,
            ),
        ] {
            if !(0.0..=1.0).contains(&value) || !value.is_finite() {
                return Err(config_error!(
                    "{name} must be a finite value between 0 and 1"
                ));
            }
        }
        if !(0.0..1.0).contains(&self.recall_similarity_threshold)
            || !self.recall_similarity_threshold.is_finite()
        {
            return Err(config_error!(
                "semantic.recall_similarity_threshold must be a finite value between zero (inclusive) and one (exclusive)"
            ));
        }
        if !self.rrf_k.is_finite() || self.rrf_k <= 0.0 {
            return Err(config_error!("semantic.rrf_k must be greater than zero"));
        }
        if !self.direct_weight.is_finite() || self.direct_weight <= 0.0 {
            return Err(config_error!(
                "semantic.direct_weight must be greater than zero"
            ));
        }
        if !self.keyword_weight.is_finite() || self.keyword_weight <= 0.0 {
            return Err(config_error!(
                "semantic.keyword_weight must be greater than zero"
            ));
        }
        if self.direct_weight <= self.keyword_weight {
            return Err(config_error!(
                "semantic.direct_weight must be greater than semantic.keyword_weight"
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    neo4j: Neo4jConfig,
    embeddings: EmbeddingsConfig,
    semantic: SemanticConfig,
}

/// Which remote embedding API is selected at runtime.
#[derive(Clone, PartialEq, Eq)]
pub enum EmbeddingProviderKind {
    /// OpenAI embeddings when `OPENAI_API_KEY` is present.
    OpenAi,
    /// xAI embeddings when selected after OpenAI is unset.
    XAi,
}

impl EmbeddingProviderKind {
    /// Stable provider id stored on the Neo4j embedding-space marker.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::XAi => "xai",
        }
    }
}

/// Fully resolved embedding credentials and model settings ready for HTTP use.
#[derive(Clone)]
pub struct SelectedEmbedding {
    /// Which embedding provider won at load (OpenAI if `OPENAI_API_KEY` is set).
    pub provider: EmbeddingProviderKind,
    /// Remote model id from that provider's `config.toml` section.
    pub model: String,
    /// Output width that must match the Neo4j activation index.
    pub dimensions: usize,
    /// Bearer token from secrets or the process environment; never logged.
    pub api_key: String,
}

impl SelectedEmbedding {
    /// Provider/model/dimension identity used to accept or rebuild the vector index.
    pub fn space(&self) -> EmbeddingSpace {
        EmbeddingSpace {
            provider: self.provider.as_str().into(),
            model: self.model.clone(),
            dimensions: self.dimensions,
        }
    }
}

/// Provider/model/dimension triple stored on the Neo4j semantic index marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSpace {
    /// Provider id stored on the marker (`openai`, `xai`, or a fixture such as `smoke`).
    pub provider: String,
    /// Model id stored on the Neo4j embedding-space marker.
    pub model: String,
    /// Index width on the marker; must match the live embedding provider.
    pub dimensions: usize,
}

/// Runtime configuration assembled from `config.toml`, secrets, and the process environment.
#[derive(Clone)]
pub struct Config {
    /// Bolt URI from `config.toml` (not rewritten).
    pub uri: String,
    /// Neo4j auth user from `config.toml`; password stays secret-only.
    pub user: String,
    password: Option<String>,
    /// Semantic activation TTL, neighbor recall, and RRF fusion tunables.
    pub semantic: SemanticConfig,
    /// Present only when an embedding API key was resolved.
    pub embedding: Option<SelectedEmbedding>,
    /// OS or explicit directory that holds `config.toml` and `.env`.
    pub config_dir: PathBuf,
}

/// Resolve the XDG config directory on Unix (`…/mindreader`), creating nothing yet.
#[cfg(unix)]
pub fn config_dir() -> Result<PathBuf> {
    if let Some(root) = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return Ok(root.join("mindreader"));
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| config_error!("cannot resolve HOME for the Mindreader config directory"))?;
    Ok(home.join(".config/mindreader"))
}

/// Resolve the native non-Unix config directory (`…/mindreader`).
#[cfg(not(unix))]
pub fn config_dir() -> Result<PathBuf> {
    let base = BaseDirs::new()
        .ok_or_else(|| config_error!("cannot resolve the Mindreader config directory"))?;
    Ok(base.config_dir().join("mindreader"))
}

/// Create-only write; existing files are left untouched. Secret files use mode 0600 on Unix.
fn write_new_file(path: &Path, contents: &str, secret: bool) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if secret {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
    };
    file.write_all(contents.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Create the config directory and seed `config.toml` plus a colocated `.env` without overwriting.
fn initialize_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("create Mindreader config directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure Mindreader config directory {}", path.display()))?;
    }
    let default_toml = toml::to_string_pretty(&FileConfig::default())?;
    write_new_file(&path.join(CONFIG_FILE), &default_toml, false)?;
    write_new_file(
        &path.join(SECRETS_FILE),
        "# Mindreader secrets. Existing process environment values take precedence.\nNEO4J_PASSWORD=\nOPENAI_API_KEY=\nXAI_API_KEY=\n",
        true,
    )?;
    Ok(())
}

/// Treat whitespace-only values as unset so empty process env can fall back to `.env`.
fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Process environment wins only when non-empty; otherwise use the colocated secret.
fn prefer_nonempty(process: Option<String>, file: Option<String>) -> Option<String> {
    nonempty(process).or_else(|| nonempty(file))
}

/// Prefer a non-empty process environment value over the colocated `.env` entry.
fn resolve_secret(name: &str, file: &HashMap<String, String>) -> Option<String> {
    prefer_nonempty(env::var(name).ok(), file.get(name).cloned())
}

impl Config {
    /// Load configuration from the default OS config directory, initializing files if missing.
    pub fn from_env() -> Result<Self> {
        Self::from_dir(config_dir()?)
    }

    /// Load a complete native configuration from an explicit directory.
    ///
    /// Intended for diagnostics and isolated integration tests that must not
    /// reuse the operator's normal configuration directory.
    #[cfg(feature = "developer-tools")]
    pub fn from_directory(config_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::from_dir(config_dir.into())
    }

    /// Seed missing files, then load `config.toml` plus colocated secrets from `config_dir`.
    fn from_dir(config_dir: PathBuf) -> Result<Self> {
        initialize_directory(&config_dir)?;
        let secrets_path = config_dir.join(SECRETS_FILE);
        let file_secrets = dotenvy::from_path_iter(&secrets_path)
            .with_context(|| format!("load Mindreader secrets from {}", secrets_path.display()))?
            .collect::<std::result::Result<HashMap<_, _>, _>>()
            .with_context(|| format!("parse Mindreader secrets from {}", secrets_path.display()))?;
        let config_path = config_dir.join(CONFIG_FILE);
        let contents = fs::read_to_string(&config_path)
            .with_context(|| format!("read Mindreader config from {}", config_path.display()))?;
        let file: FileConfig = toml::from_str(&contents)
            .with_context(|| format!("parse Mindreader config from {}", config_path.display()))?;
        file.semantic.validate()?;
        let password = resolve_secret("NEO4J_PASSWORD", &file_secrets);

        let embedding = if let Some(api_key) = resolve_secret("OPENAI_API_KEY", &file_secrets) {
            Some(selected_embedding(
                EmbeddingProviderKind::OpenAi,
                &file.embeddings.openai,
                api_key,
                "embeddings.openai",
            )?)
        } else if let Some(api_key) = resolve_secret("XAI_API_KEY", &file_secrets) {
            Some(selected_embedding(
                EmbeddingProviderKind::XAi,
                &file.embeddings.xai,
                api_key,
                "embeddings.xai",
            )?)
        } else {
            None
        };

        Ok(Self {
            uri: file.neo4j.uri,
            user: file.neo4j.user,
            password,
            semantic: file.semantic,
            embedding,
            config_dir,
        })
    }

    /// Path to the colocated `.env` secrets file under this config directory.
    pub fn secrets_path(&self) -> PathBuf {
        self.config_dir.join(SECRETS_FILE)
    }

    #[cfg(test)]
    pub(crate) fn stub() -> Self {
        Self {
            uri: "bolt://127.0.0.1:7687".into(),
            user: "neo4j".into(),
            password: Some("test".into()),
            semantic: SemanticConfig::default(),
            embedding: None,
            config_dir: PathBuf::from("/tmp/mindreader-test"),
        }
    }

    /// Require `NEO4J_PASSWORD` from secrets or the process environment.
    pub fn neo4j_password(&self) -> Result<&str> {
        self.password.as_deref().ok_or_else(|| {
            config_error!(
                "NEO4J_PASSWORD is not set; add it to {} or the process environment",
                self.secrets_path().display()
            )
        })
    }
}

/// Require a model id and 1..=4096 dimensions once an API key is present.
fn selected_embedding(
    provider: EmbeddingProviderKind,
    config: &ProviderConfig,
    api_key: String,
    section: &str,
) -> Result<SelectedEmbedding> {
    let model = config.model.trim();
    if model.is_empty() {
        return Err(config_error!(
            "{section}.model must be configured when its API key is set"
        ));
    }
    if !(1..=4096).contains(&config.dimensions) {
        return Err(config_error!(
            "{section}.dimensions must be between 1 and 4096"
        ));
    }
    Ok(SelectedEmbedding {
        provider,
        model: model.into(),
        dimensions: config.dimensions,
        api_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_round_trip() {
        let encoded = toml::to_string_pretty(&FileConfig::default()).unwrap();
        let decoded: FileConfig = toml::from_str(&encoded).unwrap();
        decoded.semantic.validate().unwrap();
        assert_eq!(decoded.embeddings.openai.model, "text-embedding-3-small");
        assert_eq!(decoded.semantic.ttl_days, 30);
    }

    #[test]
    fn initializes_both_platform_files_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        initialize_directory(dir.path()).unwrap();
        let config_path = dir.path().join(CONFIG_FILE);
        let secrets_path = dir.path().join(SECRETS_FILE);
        assert!(config_path.exists());
        assert!(secrets_path.exists());
        fs::write(&config_path, "custom = true\n").unwrap();
        initialize_directory(dir.path()).unwrap();
        assert_eq!(fs::read_to_string(config_path).unwrap(), "custom = true\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(secrets_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn validates_semantic_ranges() {
        let mut config = SemanticConfig {
            convergence_similarity_threshold: 1.1,
            ..SemanticConfig::default()
        };
        assert!(config.validate().is_err());
        config.convergence_similarity_threshold = 0.9;
        config.neighbor_limit = 0;
        assert!(config.validate().is_err());
        config.neighbor_limit = 10;
        config.recall_similarity_threshold = 1.0;
        assert!(config.validate().is_err());
        config.recall_similarity_threshold = 0.65;
        config.direct_weight = config.keyword_weight;
        assert!(config.validate().is_err());
    }

    #[test]
    fn empty_process_style_value_falls_back_to_nonempty_file_value() {
        assert_eq!(
            prefer_nonempty(Some("  ".into()), Some(" file-secret ".into())),
            Some("file-secret".into())
        );
        assert_eq!(
            prefer_nonempty(Some(" process-secret ".into()), Some("file-secret".into())),
            Some("process-secret".into())
        );
    }
}
