//! Chisle configuration. `CHISLE_CONFIG` names an explicit file, otherwise
//! `<agent_dir>/chisle.json`. The file is the opt-in: it makes `defaultMode`
//! (default `on`) apply to new sessions. Without it sessions start off and
//! `/chisle` turns the mode on per session. The upstream `CHISLE_*`
//! environment variables override the file.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::compress::Limits;
use super::mode::{Mode, Sections};

pub const CHISLE_CONFIG_ENV: &str = "CHISLE_CONFIG";

/// Tools whose output may be compressed by default. `edit` is never
/// compressed: its output feeds exact-match edits.
const DEFAULT_TOOLS: &[&str] = &["bash", "ipython"];
const NEVER_COMPRESS: &[&str] = &["edit", "read", "write"];

/// Tool-output compression settings (upstream `CHISLE_COMPRESS_*`).
// Four independent kill switches, one per upstream env var; a state enum
// would encode 16 combinations nobody names.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressConfig {
    pub enabled: bool,
    pub scrub: bool,
    pub dedup: bool,
    pub spill: bool,
    pub(crate) limits: Limits,
    /// `None` keeps the default allowlist (plus `mcp__*` tools).
    pub tools: Option<Vec<String>>,
}

impl CompressConfig {
    pub(crate) fn tool_allowed(&self, name: &str) -> bool {
        if NEVER_COMPRESS.contains(&name.to_lowercase().as_str()) {
            return false;
        }
        match &self.tools {
            Some(tools) => tools.iter().any(|tool| tool == name),
            None => DEFAULT_TOOLS.contains(&name) || name.starts_with("mcp__"),
        }
    }
}

/// The resolved Chisle configuration for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChisleConfig {
    pub default_mode_on: bool,
    pub(crate) sections: Sections,
    pub compress: CompressConfig,
}

impl ChisleConfig {
    /// Load from `CHISLE_CONFIG` or `agent_dir`, then apply the environment.
    ///
    /// # Errors
    ///
    /// Returns an error when a config file exists but cannot be read, is not
    /// valid JSON, or fails validation.
    pub fn load(agent_dir: &Path) -> anyhow::Result<Self> {
        let path = std::env::var_os(CHISLE_CONFIG_ENV)
            .map_or_else(|| agent_dir.join("chisle.json"), PathBuf::from);
        let file = if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|error| {
                anyhow::anyhow!("Unable to read Chisle config {}: {error}", path.display())
            })?;
            Some(serde_json::from_str::<ChisleFile>(&text).map_err(|error| {
                anyhow::anyhow!("Invalid Chisle config {}: {error}", path.display())
            })?)
        } else {
            None
        };
        Ok(resolve(file, |name| std::env::var(name).ok()))
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ChisleFile {
    default_mode: Option<FileMode>,
    sections: Option<FileSections>,
    compress: Option<FileCompress>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum FileMode {
    On,
    Off,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileSections {
    prose: Option<bool>,
    code: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct FileCompress {
    enabled: Option<bool>,
    scrub: Option<bool>,
    dedup: Option<bool>,
    spill: Option<bool>,
    max_chars: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    tools: Option<Vec<String>>,
}

fn resolve(file: Option<ChisleFile>, env: impl Fn(&str) -> Option<String>) -> ChisleConfig {
    let installed = file.is_some();
    let file = file.unwrap_or_default();
    let compress = file.compress.unwrap_or_default();
    let sections = file.sections.unwrap_or_default();
    let env_on = |name: &str, value: Option<bool>| match env(name).as_deref() {
        Some("0") => false,
        Some(_) | None => value.unwrap_or(true),
    };
    let env_count = |name: &str, value: Option<usize>, default: usize| {
        env(name)
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .or(value)
            .filter(|n| *n > 0)
            .unwrap_or(default)
    };
    let env_mode = env("CHISLE_DEFAULT_MODE").and_then(|raw| match raw.to_lowercase().as_str() {
        "on" => Some(Mode::On),
        "off" => Some(Mode::Off),
        _ => None,
    });
    let default_mode_on = match (env_mode, file.default_mode) {
        (Some(Mode::On), _) | (None, Some(FileMode::On)) => true,
        (Some(Mode::Off), _) | (None, Some(FileMode::Off)) => false,
        (None, None) => installed,
    };
    let tools = env("CHISLE_COMPRESS_TOOLS")
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
                .map(str::to_string)
                .collect()
        })
        .or(compress.tools);
    ChisleConfig {
        default_mode_on,
        sections: Sections {
            prose: sections.prose.unwrap_or(true),
            code: sections.code.unwrap_or(true),
        },
        compress: CompressConfig {
            enabled: env_on("CHISLE_COMPRESS", compress.enabled),
            scrub: env_on("CHISLE_COMPRESS_SCRUB", compress.scrub),
            dedup: env_on("CHISLE_COMPRESS_DEDUP", compress.dedup),
            spill: env_on("CHISLE_COMPRESS_SPILL", compress.spill),
            limits: Limits {
                max_chars: env_count(
                    "CHISLE_COMPRESS_MAX_CHARS",
                    compress.max_chars,
                    Limits::DEFAULT.max_chars,
                ),
                head_lines: env_count(
                    "CHISLE_COMPRESS_HEAD_LINES",
                    compress.head_lines,
                    Limits::DEFAULT.head_lines,
                ),
                tail_lines: env_count(
                    "CHISLE_COMPRESS_TAIL_LINES",
                    compress.tail_lines,
                    Limits::DEFAULT.tail_lines,
                ),
            },
            tools,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults(default_mode_on: bool) -> ChisleConfig {
        ChisleConfig {
            default_mode_on,
            sections: Sections {
                prose: true,
                code: true,
            },
            compress: CompressConfig {
                enabled: true,
                scrub: true,
                dedup: true,
                spill: true,
                limits: Limits::DEFAULT,
                tools: None,
            },
        }
    }

    #[test]
    fn the_file_is_the_opt_in() {
        assert_eq!(resolve(None, |_| None), defaults(false));
        assert_eq!(
            resolve(Some(ChisleFile::default()), |_| None),
            defaults(true)
        );
    }

    #[test]
    fn file_values_apply_and_the_environment_overrides_them() {
        let file: ChisleFile = serde_json::from_str(
            r#"{"defaultMode":"off","sections":{"prose":false},
                "compress":{"maxChars":4000,"tools":["bash"],"spill":false}}"#,
        )
        .unwrap();
        let env = |name: &str| match name {
            "CHISLE_DEFAULT_MODE" => Some("ON".to_string()),
            "CHISLE_COMPRESS_DEDUP" => Some("0".to_string()),
            "CHISLE_COMPRESS_HEAD_LINES" => Some("10".to_string()),
            "CHISLE_COMPRESS_TAIL_LINES" => Some("junk".to_string()),
            _ => None,
        };
        let mut expected = defaults(true);
        expected.sections.prose = false;
        expected.compress.dedup = false;
        expected.compress.spill = false;
        expected.compress.limits = Limits {
            max_chars: 4000,
            head_lines: 10,
            tail_lines: Limits::DEFAULT.tail_lines,
        };
        expected.compress.tools = Some(vec!["bash".to_string()]);
        assert_eq!(resolve(Some(file), env), expected);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(serde_json::from_str::<ChisleFile>(r#"{"defaultMode":"ultra"}"#).is_err());
        assert!(serde_json::from_str::<ChisleFile>(r#"{"level":"full"}"#).is_err());
    }

    #[test]
    fn edit_output_is_never_compressed() {
        let mut compress = defaults(true).compress;
        assert!(compress.tool_allowed("bash"));
        assert!(compress.tool_allowed("mcp__slack__search"));
        assert!(!compress.tool_allowed("edit"));
        compress.tools = Some(vec!["edit".to_string(), "websearch".to_string()]);
        assert!(!compress.tool_allowed("edit"));
        assert!(!compress.tool_allowed("bash"));
        assert!(compress.tool_allowed("websearch"));
    }
}
