//! Observation Pack: large tool results are sent in full for their first
//! [`FULL_SENDS`] provider requests, then projected as a placeholder that
//! points at the archived original.
//!
//! Only the projection handed to the provider changes (the loop's
//! `transform_context` seam); session history stays intact. The model reads
//! exact bytes back from the archive path in `ipython`. Every failure is
//! fail-open: the observation goes out unchanged.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pa_agent::types::{AgentMessage, Message, ToolResultContent, ToolResultMessage};

use super::{sha256_hex, utf16_len};

/// Only tool results larger than this participate.
pub const THRESHOLD_BYTES: usize = 10 * 1024;
/// Provider requests that still carry the full payload before the placeholder takes over.
pub const FULL_SENDS: usize = 2;
/// Placeholder excerpt budget, split evenly between head and tail, whole lines only.
pub const PLACEHOLDER_EXCERPT_BYTES: usize = 1024;
const CHARS_PER_TOKEN: usize = 4;

/// One large, text-only tool result, content-addressed inside the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub id: String,
    pub content_hash: String,
    pub file_path: PathBuf,
    pub tool_name: String,
    pub text: String,
    pub bytes: usize,
    pub lines: usize,
    pub tokens: usize,
}

/// Token estimate matching the TS reference (UTF-16 length / 4, rounded up).
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    utf16_len(text).div_ceil(CHARS_PER_TOKEN)
}

/// Line count where a trailing newline does not open another line.
#[must_use]
pub fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.matches('\n').count();
    if text.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    }
}

fn pure_text_result(message: &AgentMessage) -> Option<(&ToolResultMessage, String)> {
    let AgentMessage::Standard(Message::ToolResult(result)) = message else {
        return None;
    };
    if result.is_error || result.content.is_empty() {
        return None;
    }
    let mut texts = Vec::with_capacity(result.content.len());
    for block in &result.content {
        match block {
            ToolResultContent::Text(text) => texts.push(text.text.as_str()),
            ToolResultContent::Image(_) => return None,
        }
    }
    Some((result, texts.join("\n")))
}

/// The archive path for an observation id.
#[must_use]
pub fn observation_path(runtime_root: &Path, id: &str) -> PathBuf {
    runtime_root
        .join("observation-pack")
        .join("objects")
        .join(format!("{id}.txt"))
}

/// Build the observation for a text-only result over [`THRESHOLD_BYTES`].
#[must_use]
pub fn create_observation(
    result: &ToolResultMessage,
    text: String,
    runtime_root: &Path,
) -> Option<Observation> {
    let bytes = text.len();
    if bytes <= THRESHOLD_BYTES {
        return None;
    }
    let content_hash = sha256_hex(text.as_bytes());
    let key = format!(
        "{}\0{}\0{content_hash}",
        result.tool_name, result.tool_call_id
    );
    let id = format!("obs_{}", &sha256_hex(key.as_bytes())[..24]);
    Some(Observation {
        file_path: observation_path(runtime_root, &id),
        id,
        content_hash,
        tool_name: result.tool_name.clone(),
        lines: count_lines(&text),
        tokens: estimate_tokens(&text),
        bytes,
        text,
    })
}

/// Write the payload to its content-addressed path. Symlinked directories
/// and objects are refused; an existing object is reused only when it
/// matches byte for byte.
///
/// # Errors
///
/// Returns an error on I/O failure, on a symlinked or non-regular archive
/// entry, or when an existing object differs from the payload.
pub fn ensure_stored(observation: &Observation) -> anyhow::Result<()> {
    let directory = observation
        .file_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("observation path has no parent"))?;
    std::fs::create_dir_all(directory)?;
    let directory_meta = std::fs::symlink_metadata(directory)?;
    anyhow::ensure!(
        directory_meta.is_dir(),
        "Observation directory is not a regular directory for {}",
        observation.id
    );
    // `create_new` is O_CREAT|O_EXCL: it never follows a planted symlink.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&observation.file_path)
    {
        Ok(mut file) => {
            file.write_all(observation.text.as_bytes())?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(&observation.file_path)?;
            anyhow::ensure!(
                meta.is_file(),
                "Content-addressed observation is not a regular file for {}",
                observation.id
            );
            let existing = std::fs::read(&observation.file_path)?;
            anyhow::ensure!(
                existing.len() == observation.bytes
                    && sha256_hex(&existing) == observation.content_hash,
                "Content-addressed observation mismatch for {}",
                observation.id
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// The complete lines from one end of `text` that fit within `budget` bytes.
fn complete_line_excerpt(text: &str, budget: usize, from_end: bool) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut used = 0;
    let mut taken = 0;
    let ordered: Box<dyn Iterator<Item = &&str>> = if from_end {
        Box::new(lines.iter().rev())
    } else {
        Box::new(lines.iter())
    };
    for line in ordered {
        if used + line.len() > budget {
            break;
        }
        used += line.len();
        taken += 1;
    }
    if from_end {
        lines[lines.len() - taken..].concat()
    } else {
        lines[..taken].concat()
    }
}

/// The projection that replaces an observation after its full sends.
#[must_use]
pub fn placeholder_for(observation: &Observation) -> String {
    let head_budget = PLACEHOLDER_EXCERPT_BYTES / 2;
    let tail_budget = PLACEHOLDER_EXCERPT_BYTES - head_budget;
    let head = complete_line_excerpt(&observation.text, head_budget, /*from_end*/ false);
    let tail = complete_line_excerpt(&observation.text, tail_budget, /*from_end*/ true);
    [
        format!("[large tool result replaced after its first {FULL_SENDS} provider requests]"),
        format!("id: {}", observation.id),
        format!("tool: {}", observation.tool_name),
        format!("original_bytes: {}", observation.bytes),
        format!("original_lines: {}", observation.lines),
        format!("estimated_tokens: {}", observation.tokens),
        format!("path: {}", observation.file_path.display()),
        "retrieve: read the exact original from path in ipython, e.g. open(path).read()[a:b] or grep; do not re-run the producing command".to_string(),
        format!("[first complete lines, up to {head_budget} bytes]"),
        head,
        format!("[middle omitted; last complete lines, up to {tail_budget} bytes]"),
        tail,
        format!("[{} original bytes omitted]", observation.bytes),
    ]
    .join("\n")
}

/// Per-session projection state: how often each observation has been sent.
#[derive(Debug)]
pub struct ObservationPack {
    runtime_root: PathBuf,
    sent_counts: Mutex<HashMap<String, usize>>,
}

impl ObservationPack {
    /// A pack archiving under `<runtime_root>/observation-pack/`.
    #[must_use]
    pub fn new(runtime_root: PathBuf) -> Self {
        ObservationPack {
            runtime_root,
            sent_counts: Mutex::new(HashMap::new()),
        }
    }

    /// Project one provider request: large results past their full sends
    /// become placeholders. A per-message failure keeps that message whole.
    ///
    /// # Panics
    ///
    /// Panics if the send-count mutex is poisoned.
    #[must_use]
    pub fn project(&self, messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
        // Requests each message already rode in: the assistant turns after it.
        let mut prior_assistant_counts = vec![0; messages.len()];
        let mut assistant_count = 0;
        for (index, message) in messages.iter().enumerate().rev() {
            prior_assistant_counts[index] = assistant_count;
            if message.role() == "assistant" {
                assistant_count += 1;
            }
        }
        let request = assistant_count + 1;
        let mut sent_counts = self.sent_counts.lock().unwrap();
        messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| {
                let Some((result, text)) = pure_text_result(&message) else {
                    return message;
                };
                let Some(observation) = create_observation(result, text, &self.runtime_root) else {
                    return message;
                };
                if let Err(error) = ensure_stored(&observation) {
                    tracing::warn!(%error, "observation-pack fail-open");
                    return message;
                }
                let previous = *sent_counts
                    .entry(observation.id.clone())
                    .or_insert(prior_assistant_counts[index]);
                sent_counts.insert(observation.id.clone(), previous + 1);
                if previous < FULL_SENDS {
                    self.ledger(&serde_json::json!({
                        "event": "full", "id": observation.id, "request": request,
                        "tool": observation.tool_name, "originalTokens": observation.tokens,
                    }));
                    return message;
                }
                let placeholder = placeholder_for(&observation);
                let removed = observation
                    .tokens
                    .saturating_sub(estimate_tokens(&placeholder));
                self.ledger(&serde_json::json!({
                    "event": "placeholder", "id": observation.id, "request": request,
                    "tool": observation.tool_name, "removedTokens": removed,
                }));
                let mut projected = result.clone();
                projected.content = vec![ToolResultContent::text(placeholder)];
                AgentMessage::Standard(Message::ToolResult(projected))
            })
            .collect()
    }

    /// Append one JSONL row to the pack's ledger (best effort).
    fn ledger(&self, entry: &serde_json::Value) {
        let mut row = serde_json::Map::new();
        row.insert(
            "timestamp".to_string(),
            crate::session::manager::format_iso_now().into(),
        );
        if let Some(fields) = entry.as_object() {
            row.extend(fields.clone());
        }
        let path = self
            .runtime_root
            .join("observation-pack")
            .join("ledger.jsonl");
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| writeln!(file, "{}", serde_json::Value::Object(row)));
        if let Err(error) = written {
            tracing::warn!(%error, path = %path.display(), "observation-pack ledger write failed");
        }
    }
}

#[cfg(test)]
#[path = "observation_pack_tests.rs"]
mod tests;
