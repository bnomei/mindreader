use crate::{
    config_error,
    error::{Context, Result},
};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{collections::HashMap, env};

const CONFIG_FILE: &str = "config.toml";
const SECRETS_FILE: &str = ".env";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Neo4jConfig {
    pub uri: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub model: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingsConfig {
    pub openai: ProviderConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticConfig {
    pub ttl_days: u64,
    pub neighbor_limit: usize,
    pub recall_similarity_threshold: f64,
    pub convergence_similarity_threshold: f64,
    pub convergence_result_overlap_threshold: f64,
    pub rrf_k: f64,
    pub direct_weight: f64,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            ttl_days: 30,
            neighbor_limit: 10,
            recall_similarity_threshold: 0.70,
            convergence_similarity_threshold: 0.90,
            convergence_result_overlap_threshold: 0.60,
            rrf_k: 60.0,
            direct_weight: 2.0,
        }
    }
}

impl SemanticConfig {
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
                "semantic.recall_similarity_threshold",
                self.recall_similarity_threshold,
            ),
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
        if !self.rrf_k.is_finite() || self.rrf_k <= 0.0 {
            return Err(config_error!("semantic.rrf_k must be greater than zero"));
        }
        if !self.direct_weight.is_finite() || self.direct_weight <= 0.0 {
            return Err(config_error!(
                "semantic.direct_weight must be greater than zero"
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

#[derive(Clone, PartialEq, Eq)]
pub enum EmbeddingProviderKind {
    OpenAi,
    XAi,
}

impl EmbeddingProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::XAi => "xai",
        }
    }
}

#[derive(Clone)]
pub struct SelectedEmbedding {
    pub provider: EmbeddingProviderKind,
    pub model: String,
    pub dimensions: usize,
    pub api_key: String,
}

impl SelectedEmbedding {
    pub fn space(&self) -> EmbeddingSpace {
        EmbeddingSpace {
            provider: self.provider.as_str().into(),
            model: self.model.clone(),
            dimensions: self.dimensions,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSpace {
    pub provider: String,
    pub model: String,
    pub dimensions: usize,
}

#[derive(Clone)]
pub struct Config {
    pub uri: String,
    pub user: String,
    password: Option<String>,
    pub semantic: SemanticConfig,
    pub embedding: Option<SelectedEmbedding>,
    pub config_dir: PathBuf,
}

pub fn config_dir() -> Result<PathBuf> {
    let base =
        BaseDirs::new().ok_or_else(|| config_error!("cannot resolve the OS config directory"))?;
    Ok(base.config_dir().join("mindreader"))
}

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

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn prefer_nonempty(process: Option<String>, file: Option<String>) -> Option<String> {
    nonempty(process).or_else(|| nonempty(file))
}

fn resolve_secret(name: &str, file: &HashMap<String, String>) -> Option<String> {
    prefer_nonempty(env::var(name).ok(), file.get(name).cloned())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_dir(config_dir()?)
    }

    /// Load a complete native configuration from an explicit directory.
    ///
    /// This is intended for diagnostics and isolated integration tests that
    /// must not reuse the operator's normal configuration directory.
    pub fn from_directory(config_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::from_dir(config_dir.into())
    }

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

    pub fn secrets_path(&self) -> PathBuf {
        self.config_dir.join(SECRETS_FILE)
    }

    pub fn neo4j_password(&self) -> Result<&str> {
        self.password.as_deref().ok_or_else(|| {
            config_error!(
                "NEO4J_PASSWORD is not set; add it to {} or the process environment",
                self.secrets_path().display()
            )
        })
    }
}

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
