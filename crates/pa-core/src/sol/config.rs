//! `SoL` configuration: which mechanisms run for a session.
//!
//! Two file shapes are accepted, both strictly validated (unknown keys and
//! bad types are errors):
//!
//! - `sol-prime.json` (the prime-agent adaptation): `observationPack`,
//!   `evidencePreservingReducer`, `reducerModel` (`provider/model`),
//!   `onlineContextCompact`, `cacheWriteReadRatio`, `actionFusion`.
//! - `sol-pi.json` (the upstream SoL-Pi schema): `version: 1` plus the
//!   feature flags and `evidencePreservingReducerModel`/`Provider`.
//!
//! `SOL_PRIME_CONFIG` names an explicit `sol-prime.json`-shaped file.
//! Otherwise `<agent_dir>/sol-prime.json` wins over `<agent_dir>/sol-pi.json`.
//! With no file every mechanism stays off: the file is the opt-in.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The environment override naming an explicit `sol-prime.json` file.
pub const SOL_PRIME_CONFIG_ENV: &str = "SOL_PRIME_CONFIG";
pub const DEFAULT_CACHE_WRITE_READ_RATIO: f64 = 12.5;
const UPSTREAM_REDUCER_PROVIDER: &str = "openai-codex";
const UPSTREAM_REDUCER_MODEL: &str = "gpt-5.6-luna";

/// The reducer model, resolved through the model registry at call time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReducerModelRef {
    pub provider: String,
    pub model: String,
}

/// The resolved `SoL` configuration for one session.
#[derive(Debug, Clone, PartialEq)]
pub struct SolConfig {
    pub observation_pack: bool,
    /// `Some` when the evidence-preserving reducer is enabled.
    pub evidence_preserving_reducer: Option<ReducerModelRef>,
    pub online_context_compact: bool,
    /// Cache-write price divided by cache-read price for the main model.
    pub cache_write_read_ratio: f64,
    pub action_fusion: bool,
}

impl SolConfig {
    /// Every mechanism off (no config file).
    #[must_use]
    pub fn disabled() -> Self {
        SolConfig {
            observation_pack: false,
            evidence_preserving_reducer: None,
            online_context_compact: false,
            cache_write_read_ratio: DEFAULT_CACHE_WRITE_READ_RATIO,
            action_fusion: false,
        }
    }

    /// Load the session's config from `SOL_PRIME_CONFIG` or `agent_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error when a config file exists but cannot be read, is not
    /// valid JSON, or fails validation.
    pub fn load(agent_dir: &Path) -> anyhow::Result<Self> {
        let explicit = std::env::var_os(SOL_PRIME_CONFIG_ENV).map(PathBuf::from);
        match config_source(agent_dir, explicit) {
            Some(ConfigSource::SolPrime(path)) => load_sol_prime(&path),
            Some(ConfigSource::SolPi(path)) => load_sol_pi(&path),
            None => Ok(Self::disabled()),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ConfigSource {
    SolPrime(PathBuf),
    SolPi(PathBuf),
}

fn config_source(agent_dir: &Path, explicit: Option<PathBuf>) -> Option<ConfigSource> {
    if let Some(path) = explicit {
        return path.exists().then_some(ConfigSource::SolPrime(path));
    }
    let sol_prime = agent_dir.join("sol-prime.json");
    if sol_prime.exists() {
        return Some(ConfigSource::SolPrime(sol_prime));
    }
    let sol_pi = agent_dir.join("sol-pi.json");
    sol_pi.exists().then_some(ConfigSource::SolPi(sol_pi))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SolPrimeFile {
    observation_pack: Option<bool>,
    evidence_preserving_reducer: Option<bool>,
    reducer_model: Option<String>,
    online_context_compact: Option<bool>,
    cache_write_read_ratio: Option<f64>,
    action_fusion: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SolPiFile {
    version: u32,
    action_fusion: Option<bool>,
    observation_pack: Option<bool>,
    evidence_preserving_reducer: Option<bool>,
    evidence_preserving_reducer_model: Option<String>,
    evidence_preserving_reducer_provider: Option<String>,
    online_context_compact: Option<bool>,
    cache_write_read_ratio: Option<f64>,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        anyhow::anyhow!("Unable to read SoL config {}: {error}", path.display())
    })?;
    let invalid = |error: serde_json::Error| {
        anyhow::anyhow!("Invalid SoL config {}: {error}", path.display())
    };
    // Serde also accepts a JSON array as a struct; only an object is a config.
    let value: serde_json::Value = serde_json::from_str(&text).map_err(invalid)?;
    anyhow::ensure!(
        value.is_object(),
        "SoL config must be a JSON object: {}",
        path.display()
    );
    serde_json::from_value(value).map_err(invalid)
}

fn validated_ratio(ratio: Option<f64>, path: &Path) -> anyhow::Result<f64> {
    let ratio = ratio.unwrap_or(DEFAULT_CACHE_WRITE_READ_RATIO);
    anyhow::ensure!(
        ratio.is_finite() && ratio >= 0.0,
        "SoL config cacheWriteReadRatio must be a finite non-negative number: {}",
        path.display()
    );
    Ok(ratio)
}

fn load_sol_prime(path: &Path) -> anyhow::Result<SolConfig> {
    let file: SolPrimeFile = read_json(path)?;
    let reducer_model = match file.reducer_model {
        Some(selector) => Some(parse_selector(&selector).ok_or_else(|| {
            anyhow::anyhow!(
                "SoL config reducerModel must be \"provider/model\": {}",
                path.display()
            )
        })?),
        None => None,
    };
    let evidence_preserving_reducer = if file.evidence_preserving_reducer.unwrap_or(false) {
        Some(reducer_model.ok_or_else(|| {
            anyhow::anyhow!(
                "SoL config evidencePreservingReducer requires reducerModel: {}",
                path.display()
            )
        })?)
    } else {
        None
    };
    Ok(SolConfig {
        // sol-prime's default: the pack is on once the file exists.
        observation_pack: file.observation_pack.unwrap_or(true),
        evidence_preserving_reducer,
        online_context_compact: file.online_context_compact.unwrap_or(false),
        cache_write_read_ratio: validated_ratio(file.cache_write_read_ratio, path)?,
        action_fusion: file.action_fusion.unwrap_or(false),
    })
}

/// `provider/model` with a non-empty, whitespace-free provider and model.
fn parse_selector(selector: &str) -> Option<ReducerModelRef> {
    let (provider, model) = selector.split_once('/')?;
    let valid =
        !provider.is_empty() && !model.is_empty() && !selector.chars().any(char::is_whitespace);
    valid.then(|| ReducerModelRef {
        provider: provider.to_string(),
        model: model.to_string(),
    })
}

fn non_empty(
    value: Option<String>,
    default: &str,
    key: &str,
    path: &Path,
) -> anyhow::Result<String> {
    let value = value.unwrap_or_else(|| default.to_string());
    let trimmed = value.trim();
    anyhow::ensure!(
        !trimmed.is_empty(),
        "SoL config {key} must be a non-empty string: {}",
        path.display()
    );
    Ok(trimmed.to_string())
}

fn load_sol_pi(path: &Path) -> anyhow::Result<SolConfig> {
    let file: SolPiFile = read_json(path)?;
    anyhow::ensure!(
        file.version == 1,
        "SoL config version must be 1: {}",
        path.display()
    );
    let model = non_empty(
        file.evidence_preserving_reducer_model,
        UPSTREAM_REDUCER_MODEL,
        "evidencePreservingReducerModel",
        path,
    )?;
    let provider = non_empty(
        file.evidence_preserving_reducer_provider,
        UPSTREAM_REDUCER_PROVIDER,
        "evidencePreservingReducerProvider",
        path,
    )?;
    Ok(SolConfig {
        observation_pack: file.observation_pack.unwrap_or(false),
        evidence_preserving_reducer: file
            .evidence_preserving_reducer
            .unwrap_or(false)
            .then_some(ReducerModelRef { provider, model }),
        online_context_compact: file.online_context_compact.unwrap_or(false),
        cache_write_read_ratio: validated_ratio(file.cache_write_read_ratio, path)?,
        action_fusion: file.action_fusion.unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(name: &str, body: &serde_json::Value) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(name), body.to_string()).unwrap();
        dir
    }

    fn load_dir(dir: &Path) -> anyhow::Result<SolConfig> {
        match config_source(dir, None) {
            Some(ConfigSource::SolPrime(path)) => load_sol_prime(&path),
            Some(ConfigSource::SolPi(path)) => load_sol_pi(&path),
            None => Ok(SolConfig::disabled()),
        }
    }

    #[test]
    fn no_file_disables_every_mechanism() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_dir(dir.path()).unwrap(), SolConfig::disabled());
    }

    #[test]
    fn a_sol_prime_file_merges_over_its_defaults() {
        let dir = dir_with(
            "sol-prime.json",
            &serde_json::json!({
                "onlineContextCompact": true,
                "cacheWriteReadRatio": 4,
                "evidencePreservingReducer": true,
                "reducerModel": "anthropic/claude-haiku-4-5"
            }),
        );
        assert_eq!(
            load_dir(dir.path()).unwrap(),
            SolConfig {
                observation_pack: true,
                evidence_preserving_reducer: Some(ReducerModelRef {
                    provider: "anthropic".to_string(),
                    model: "claude-haiku-4-5".to_string(),
                }),
                online_context_compact: true,
                cache_write_read_ratio: 4.0,
                action_fusion: false,
            }
        );
    }

    #[test]
    fn sol_prime_wins_over_sol_pi_and_the_env_path_wins_over_both() {
        let dir = dir_with("sol-pi.json", &serde_json::json!({ "version": 1 }));
        std::fs::write(dir.path().join("sol-prime.json"), "{}").unwrap();
        let explicit = dir.path().join("explicit.json");
        assert_eq!(
            config_source(dir.path(), None),
            Some(ConfigSource::SolPrime(dir.path().join("sol-prime.json")))
        );
        assert_eq!(config_source(dir.path(), Some(explicit)), None);
        std::fs::remove_file(dir.path().join("sol-prime.json")).unwrap();
        assert_eq!(
            config_source(dir.path(), None),
            Some(ConfigSource::SolPi(dir.path().join("sol-pi.json")))
        );
    }

    #[test]
    fn an_upstream_sol_pi_file_uses_the_upstream_reducer_defaults() {
        let dir = dir_with(
            "sol-pi.json",
            &serde_json::json!({ "version": 1, "evidencePreservingReducer": true, "actionFusion": true }),
        );
        assert_eq!(
            load_dir(dir.path()).unwrap(),
            SolConfig {
                observation_pack: false,
                evidence_preserving_reducer: Some(ReducerModelRef {
                    provider: UPSTREAM_REDUCER_PROVIDER.to_string(),
                    model: UPSTREAM_REDUCER_MODEL.to_string(),
                }),
                online_context_compact: false,
                cache_write_read_ratio: DEFAULT_CACHE_WRITE_READ_RATIO,
                action_fusion: true,
            }
        );
    }

    #[test]
    fn invalid_files_are_rejected_with_the_offending_key() {
        let cases = [
            (
                "sol-prime.json",
                serde_json::json!({ "typo": true }),
                "typo",
            ),
            (
                "sol-prime.json",
                serde_json::json!({ "actionFusion": "yes" }),
                "invalid type",
            ),
            (
                "sol-prime.json",
                serde_json::json!({ "cacheWriteReadRatio": -1 }),
                "cacheWriteReadRatio",
            ),
            (
                "sol-prime.json",
                serde_json::json!({ "reducerModel": "no-slash" }),
                "provider/model",
            ),
            (
                "sol-prime.json",
                serde_json::json!({ "evidencePreservingReducer": true }),
                "requires reducerModel",
            ),
            (
                "sol-prime.json",
                serde_json::json!([]),
                "must be a JSON object",
            ),
            (
                "sol-pi.json",
                serde_json::json!({ "version": 2 }),
                "version must be 1",
            ),
            ("sol-pi.json", serde_json::json!({}), "version"),
            (
                "sol-pi.json",
                serde_json::json!({ "version": 1, "evidencePreservingReducerModel": " " }),
                "evidencePreservingReducerModel",
            ),
        ];
        for (name, body, expected) in cases {
            let dir = dir_with(name, &body);
            let error = load_dir(dir.path()).unwrap_err().to_string();
            assert!(error.contains(expected), "{body}: {error}");
        }
    }
}
