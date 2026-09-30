//! Evidence-Preserving Reducer: delegate the first read of a long build or
//! test log to a cheaper reducer model, then verify what comes back.
//!
//! A candidate is an `ipython` cell whose source runs a diagnostic
//! build/test command and whose single text output is at least
//! [`DEFAULT_MIN_BYTES`]. The raw log is archived by SHA-256; the reducer
//! model returns a JSON receipt of verbatim quotes, and the receipt replaces
//! the log only when every quote is found byte for byte in the archive (see
//! [`receipt::validate_receipt`]). Any failure keeps the raw output. Every
//! decision is journaled to `<store>/journal.jsonl`.
//!
//! The hook is the loop's `after_tool_call` seam.

mod provider;
pub mod receipt;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use pa_agent::types::{AfterToolCallResult, ToolResultContent};
use serde_json::{json, Value};

pub use provider::registry_reducer_call;

use super::{sha256_hex, utf16_len};

pub const REDUCER_EVENT_SCHEMA: &str = "sol-pi-evidence-preserving-reducer/1";
pub const DEFAULT_MIN_BYTES: usize = 4_096;
pub const DEFAULT_MAX_CHARS: usize = 600_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 2_048;
pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

static DIAGNOSTIC_COMMAND: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(concat!(
        r#"(?i)(?:^|[;&|()\s'"`!])(?:lake\s+build|lake\s+env\s+lean|lean|coq|cargo(?:\s+(?:build|test|check))?"#,
        r"|zig\s+build|pytest|python(?:3)?\s+-m\s+(?:pytest|unittest|py_compile)|ctest|cmake\s+--build",
        r"|ninja|make|npm\s+test|pnpm\s+test|yarn\s+test|go\s+test|bazel\s+test|vitest|jest|tsc|mypy|ruff",
        r#"|npm\s+run\s+(?:test|check|build|lint))(?:[\s'"`]|$)"#,
    ))
    .expect("diagnostic-command regex compiles")
});

static LIKELY_SECRET: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    fancy_regex::Regex::new(
        r"(?i)(?:api[_-]?key|authorization|bearer|access[_-]?token|secret)[^\n]{0,32}[=:][^\n]+",
    )
    .expect("likely-secret regex compiles")
});

/// Whether an `ipython` cell source runs a diagnostic build/test command.
#[must_use]
pub fn is_diagnostic_command(code: &str) -> bool {
    DIAGNOSTIC_COMMAND.is_match(code).unwrap_or(false)
}

/// Whether a log likely carries a credential (such logs are never delegated).
#[must_use]
pub fn has_likely_secret(body: &str) -> bool {
    LIKELY_SECRET.is_match(body).unwrap_or(false)
}

/// The raw log archived under its content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    pub hash: String,
    pub bytes: usize,
    pub lines: usize,
    pub path: PathBuf,
}

/// Store `body` at `<root>/objects/<hash[..2]>/<hash>.txt`. An existing
/// object with different bytes is an integrity failure, not a cache hit.
///
/// # Errors
///
/// Returns an error on I/O failure or an integrity mismatch.
pub fn archive_body(root: &Path, body: &str) -> anyhow::Result<Archive> {
    let hash = sha256_hex(body.as_bytes());
    let directory = root.join("objects").join(&hash[..2]);
    let path = directory.join(format!("{hash}.txt"));
    std::fs::create_dir_all(&directory)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => file.write_all(body.as_bytes())?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = std::fs::read(&path)?;
            anyhow::ensure!(
                existing == body.as_bytes(),
                "Reducer archive integrity failure: {}",
                path.display()
            );
        }
        Err(error) => return Err(error.into()),
    }
    Ok(Archive {
        hash,
        bytes: body.len(),
        lines: if body.is_empty() {
            0
        } else {
            body.split('\n').count()
        },
        path,
    })
}

/// One reducer model request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReducerRequest {
    pub system_prompt: String,
    pub input: String,
    pub max_output_tokens: u64,
    pub session_id: String,
}

/// Token usage the reducer call reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReducerUsage {
    pub input: u64,
    pub output: u64,
    pub total_tokens: u64,
}

/// The reducer model's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReducerResponse {
    pub provider: String,
    pub model: String,
    /// The reply ended normally (`stop` or `length`).
    pub ok: bool,
    pub stop_reason: String,
    pub error_message: Option<String>,
    pub output_text: String,
    pub usage: ReducerUsage,
}

/// Why a reducer call produced no response.
#[derive(Debug)]
pub enum ReducerCallError {
    /// The configured model is missing from the registry or lacks auth.
    ModelUnavailable(String),
    Timeout,
    Failed(anyhow::Error),
}

impl ReducerCallError {
    fn reason(&self) -> &'static str {
        match self {
            ReducerCallError::ModelUnavailable(_) => "reducer-model-unavailable",
            ReducerCallError::Timeout => "model-call-timeout",
            ReducerCallError::Failed(_) => "model-call-exception",
        }
    }
}

/// The reducer model seam: production resolves through the model registry
/// ([`registry_reducer_call`]); tests script replies.
pub type ReducerCallFn = Arc<
    dyn Fn(ReducerRequest) -> pa_agent::BoxFut<'static, Result<ReducerResponse, ReducerCallError>>
        + Send
        + Sync,
>;

/// One executed tool call as the reducer sees it.
#[derive(Debug, Clone, Copy)]
pub struct ToolCallOutcome<'a> {
    pub tool_name: &'a str,
    pub args: &'a Value,
    pub content: &'a [ToolResultContent],
    pub details: &'a Value,
    pub is_error: bool,
}

/// The text block of an `ipython` result (exactly one text block; images
/// and other blocks ride along untouched).
fn reducible_body<'a>(outcome: &ToolCallOutcome<'a>) -> Option<(&'a str, usize, &'a str)> {
    if outcome.tool_name != "ipython" {
        return None;
    }
    let code = outcome.args.get("code").and_then(Value::as_str)?;
    let mut texts = outcome
        .content
        .iter()
        .enumerate()
        .filter_map(|(index, block)| match block {
            ToolResultContent::Text(text) => Some((index, text.text.as_str())),
            ToolResultContent::Image(_) => None,
        });
    let (index, body) = texts.next()?;
    (texts.next().is_none() && !code.is_empty()).then_some((code, index, body))
}

/// Per-session reducer state.
pub struct EvidenceReducer {
    store_root: PathBuf,
    run_id: String,
    call: ReducerCallFn,
}

impl EvidenceReducer {
    /// A reducer archiving and journaling under
    /// `<runtime_root>/evidence-preserving-reducer/`.
    #[must_use]
    pub fn new(runtime_root: &Path, call: ReducerCallFn) -> Self {
        EvidenceReducer {
            store_root: runtime_root.join("evidence-preserving-reducer"),
            run_id: sha256_hex(runtime_root.display().to_string().as_bytes())[..16].to_string(),
            call,
        }
    }

    fn journal(&self, kind: &str, data: Value) {
        let mut row = serde_json::Map::new();
        row.insert(
            "timestamp".to_string(),
            crate::session::manager::format_iso_now().into(),
        );
        row.insert("schema".to_string(), REDUCER_EVENT_SCHEMA.into());
        row.insert("runId".to_string(), self.run_id.clone().into());
        row.insert("kind".to_string(), kind.into());
        if let Value::Object(fields) = data {
            row.extend(fields);
        }
        let path = self.store_root.join("journal.jsonl");
        let written = std::fs::create_dir_all(&self.store_root)
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
            })
            .and_then(|mut file| writeln!(file, "{}", Value::Object(row)));
        if let Err(error) = written {
            tracing::warn!(%error, path = %path.display(), "reducer journal write failed");
        }
    }

    /// Reduce one executed tool call. `None` keeps the raw result.
    pub async fn reduce(&self, outcome: ToolCallOutcome<'_>) -> Option<AfterToolCallResult> {
        let (command, index, body) = reducible_body(&outcome)?;
        if !is_diagnostic_command(command) || body.len() < DEFAULT_MIN_BYTES {
            return None;
        }
        let source_chars = utf16_len(body);
        if source_chars > DEFAULT_MAX_CHARS {
            self.journal(
                "fallback",
                json!({ "reason": "source-over-max-chars", "sourceChars": source_chars, "maxChars": DEFAULT_MAX_CHARS }),
            );
            return None;
        }
        if has_likely_secret(body) {
            self.journal("fallback", json!({ "reason": "likely-secret" }));
            return None;
        }
        let archive = match archive_body(&self.store_root, body) {
            Ok(archive) => archive,
            Err(error) => {
                tracing::warn!(%error, "reducer archive failed; keeping the raw output");
                return None;
            }
        };
        let command_sha256 = sha256_hex(command.as_bytes());
        self.journal(
            "candidate",
            json!({
                "commandSha256": command_sha256, "isError": outcome.is_error,
                "sourceSha256": archive.hash, "sourceBytes": archive.bytes,
                "sourceLines": archive.lines, "sourcePath": archive.path.display().to_string(),
            }),
        );
        let request = ReducerRequest {
            system_prompt: receipt::reducer_instructions(),
            input: receipt::reducer_input(command, outcome.is_error, &archive, body),
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            session_id: self.run_id.clone(),
        };
        let fallback = |reason: &str, extra: Value| {
            let mut data = json!({ "sourceSha256": archive.hash, "reason": reason });
            if let (Value::Object(data), Value::Object(extra)) = (&mut data, extra) {
                data.extend(extra);
            }
            self.journal("fallback", data);
        };
        let response = match (self.call)(request).await {
            Ok(response) => response,
            Err(error) => {
                fallback(error.reason(), json!({}));
                return None;
            }
        };
        let usage = json!({ "input": response.usage.input, "output": response.usage.output, "totalTokens": response.usage.total_tokens });
        self.journal(
            "provider_response",
            json!({
                "sourceSha256": archive.hash, "provider": response.provider, "model": response.model,
                "stopReason": response.stop_reason, "errorMessage": response.error_message, "usage": usage,
            }),
        );
        if !response.ok {
            fallback(
                "model-response-error",
                json!({ "stopReason": response.stop_reason }),
            );
            return None;
        }
        let validated = match receipt::validate_receipt(
            &response.output_text,
            &archive,
            body,
            outcome.is_error,
        ) {
            Ok(validated) => validated,
            Err(rejection) => {
                fallback(rejection.reason(), json!({ "usage": usage }));
                return None;
            }
        };
        let receipt = receipt::receipt_text(command, &archive, &validated, &response);
        if receipt.len() >= archive.bytes {
            fallback(
                "receipt-not-smaller",
                json!({ "receiptBytes": receipt.len(), "sourceBytes": archive.bytes }),
            );
            return None;
        }
        let summary = json!({
            "schema": receipt::REDUCER_RECEIPT_SCHEMA,
            "sourceSha256": archive.hash, "sourceBytes": archive.bytes,
            "receiptSha256": sha256_hex(receipt.as_bytes()), "receiptBytes": receipt.len(),
            "evidenceCount": validated.evidence.len(), "uncertain": validated.uncertain,
        });
        self.journal(
            "applied",
            json!({ "commandSha256": command_sha256, "summary": summary, "usage": usage }),
        );
        let mut details = match outcome.details {
            Value::Object(fields) => fields.clone(),
            _ => serde_json::Map::new(),
        };
        details.insert("evidencePreservingReducer".to_string(), summary);
        let mut content = outcome.content.to_vec();
        content[index] = ToolResultContent::text(receipt);
        Some(AfterToolCallResult {
            content: Some(content),
            details: Some(Value::Object(details)),
            is_error: None,
            terminate: None,
        })
    }
}

#[cfg(test)]
mod tests;
