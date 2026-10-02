//! One session's Chisle runtime: the on/off mode, the hidden rule rows the
//! next admitted turn carries, and tool-output compression at the loop's
//! `after_tool_call` seam.
//!
//! The rules ride as hidden custom rows appended at admission (upstream
//! Pi's `chisle-rules` message) instead of a system-prompt edit, so a
//! toggle never invalidates the cached prompt prefix. Whether a row is
//! owed is read off the live context, so a compaction that summarized the
//! rules away gets them re-sent on the next turn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pa_agent::agent_loop::AfterToolCallFn;
use pa_agent::types::{AfterToolCallResult, AgentMessage, Message, ToolResultContent};
use pa_types::session::{CustomMessage, FileEntry};

use super::compress::{duplicate_marker, transform, DEDUP_MIN};
use super::config::ChisleConfig;
use super::mode::{rules_text, Mode};

/// Hidden context row carrying the ruleset.
pub const RULES_CUSTOM_TYPE: &str = "chisle_rules";
/// Hidden context row withdrawing the ruleset.
pub const DISABLED_CUSTOM_TYPE: &str = "chisle_disabled";
/// Non-context session entry recording the mode, so a resumed session keeps it.
pub const MODE_ENTRY_TYPE: &str = "chisle_mode";
const DISABLED_TEXT: &str =
    "CHISLE DISABLED. Ignore only Chisle style and efficiency-ladder instructions.";
const SPILL_DIR: &str = "chisle-spill";
const SPILL_KEEP: usize = 40;

/// The last full copy of one tool's output block.
struct Seen {
    hash: String,
    tool_call_id: String,
}

pub struct ChisleSession {
    config: ChisleConfig,
    active: AtomicBool,
    spill_dir: Option<PathBuf>,
    saved_chars: AtomicUsize,
    seen: Mutex<HashMap<String, Seen>>,
}

impl ChisleSession {
    /// `entries` is the session branch: its newest mode entry wins over the
    /// configured default. `artifact_dir` hosts the spill copies; without
    /// one (in-memory sessions) elision keeps no recovery copy.
    #[must_use]
    pub fn new(config: ChisleConfig, entries: &[&FileEntry], artifact_dir: Option<&Path>) -> Self {
        let restored = entries.iter().rev().find_map(|entry| match entry {
            FileEntry::Custom { payload, .. } if payload.custom_type == MODE_ENTRY_TYPE => payload
                .data
                .as_ref()
                .and_then(|data| data.get("mode"))
                .and_then(serde_json::Value::as_str)
                .map(|mode| mode == Mode::On.as_str()),
            _ => None,
        });
        ChisleSession {
            active: AtomicBool::new(restored.unwrap_or(config.default_mode_on)),
            config,
            spill_dir: artifact_dir.map(|dir| dir.join(SPILL_DIR)),
            saved_chars: AtomicUsize::new(0),
            seen: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Tool-output chars compression removed this session.
    #[must_use]
    pub fn saved_chars(&self) -> usize {
        self.saved_chars.load(Ordering::Relaxed)
    }

    /// Set the mode and return the session entry recording it.
    pub(crate) fn set_mode(&self, mode: Mode) -> (&'static str, serde_json::Value) {
        self.active.store(mode == Mode::On, Ordering::Relaxed);
        (
            MODE_ENTRY_TYPE,
            serde_json::json!({ "mode": mode.as_str() }),
        )
    }

    /// The hidden row the next turn owes: the rules when on and the context
    /// does not hold them, the withdrawal when off and it does.
    pub(crate) fn owed_row(&self, messages: &[AgentMessage]) -> Option<CustomMessage> {
        let in_context = messages
            .iter()
            .rev()
            .find_map(|message| match message {
                AgentMessage::Custom(custom) => match custom.payload.get("customType")?.as_str()? {
                    RULES_CUSTOM_TYPE => Some(true),
                    DISABLED_CUSTOM_TYPE => Some(false),
                    _ => None,
                },
                AgentMessage::Standard(_) => None,
            })
            .unwrap_or(false);
        let (custom_type, text) = match (self.is_active(), in_context) {
            (true, false) => (RULES_CUSTOM_TYPE, rules_text(self.config.sections)),
            (false, true) => (DISABLED_CUSTOM_TYPE, DISABLED_TEXT.to_string()),
            (true, true) | (false, false) => return None,
        };
        Some(CustomMessage {
            custom_type: custom_type.to_string(),
            content: pa_types::ai::UserContent::Text(text),
            display: false,
            details: None,
            timestamp: now_millis(),
            rest: serde_json::Map::default(),
        })
    }

    /// Wrap the loop's `after_tool_call` hook: `inner` runs first, and a
    /// result it already rewrote is left alone. `inner` returns unchanged
    /// when compression is disabled.
    #[must_use]
    pub fn wrap_after_tool_call(
        self: &Arc<Self>,
        inner: Option<AfterToolCallFn>,
    ) -> Option<AfterToolCallFn> {
        if !self.config.compress.enabled {
            return inner;
        }
        let session = Arc::clone(self);
        Some(Arc::new(move |context, signal| {
            let session = Arc::clone(&session);
            let inner = inner.clone();
            Box::pin(async move {
                let overrides = match &inner {
                    Some(inner) => inner(context.clone(), signal).await?,
                    None => None,
                };
                if overrides
                    .as_ref()
                    .is_some_and(|result| result.content.is_some())
                    || !session.is_active()
                    || !session
                        .config
                        .compress
                        .tool_allowed(&context.tool_call.name)
                {
                    return Ok(overrides);
                }
                let Some(content) = session.compress(
                    &context.tool_call.name,
                    &context.tool_call.id,
                    &context.result.content,
                    &context.context.messages,
                ) else {
                    return Ok(overrides);
                };
                let mut result = overrides.unwrap_or_else(AfterToolCallResult::default);
                result.content = Some(content);
                Ok(Some(result))
            })
        }))
    }

    /// Compressed content blocks, or `None` when no block got smaller.
    fn compress(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        content: &[ToolResultContent],
        messages: &[AgentMessage],
    ) -> Option<Vec<ToolResultContent>> {
        let compress = &self.config.compress;
        let mut changed = false;
        let blocks = content
            .iter()
            .enumerate()
            .map(|(index, block)| {
                let ToolResultContent::Text(text_block) = block else {
                    return block.clone();
                };
                let text = &text_block.text;
                let updated = self
                    .duplicate(tool_name, tool_call_id, index, text, messages)
                    .or_else(|| {
                        transform(text, compress.limits, compress.scrub, |kept| {
                            self.spill(tool_name, kept)
                        })
                    });
                match updated {
                    Some(updated) if updated.len() < text.len() => {
                        changed = true;
                        self.saved_chars.fetch_add(
                            text.chars().count() - updated.chars().count(),
                            Ordering::Relaxed,
                        );
                        let mut text_block = text_block.clone();
                        text_block.text = updated;
                        ToolResultContent::Text(text_block)
                    }
                    Some(_) | None => block.clone(),
                }
            })
            .collect();
        changed.then_some(blocks)
    }

    /// The duplicate marker when this block is byte-identical to the tool's
    /// last full output and that output is still in the context (a
    /// compaction drops it, so the marker would point at nothing).
    fn duplicate(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        index: usize,
        text: &str,
        messages: &[AgentMessage],
    ) -> Option<String> {
        if !self.config.compress.dedup || text.chars().count() < DEDUP_MIN {
            return None;
        }
        let hash = crate::sol::sha256_hex(text.as_bytes());
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = format!("{tool_name}:{index}");
        let still_in_context = |id: &str| {
            messages.iter().any(|message| {
                matches!(message, AgentMessage::Standard(Message::ToolResult(result)) if result.tool_call_id == id)
            })
        };
        if let Some(previous) = seen.get(&key) {
            if previous.hash == hash && still_in_context(&previous.tool_call_id) {
                return Some(duplicate_marker(
                    tool_name,
                    text,
                    self.spill(tool_name, text).as_deref(),
                ));
            }
        }
        seen.insert(
            key,
            Seen {
                hash,
                tool_call_id: tool_call_id.to_string(),
            },
        );
        None
    }

    /// Write the recovery copy; keeps the newest `SPILL_KEEP` files.
    fn spill(&self, tool_name: &str, text: &str) -> Option<PathBuf> {
        if !self.config.compress.spill {
            return None;
        }
        let dir = self.spill_dir.as_ref()?;
        let write = || -> std::io::Result<PathBuf> {
            std::fs::create_dir_all(dir)?;
            let hash = crate::sol::sha256_hex(text.as_bytes());
            let tool: String = tool_name
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                .collect();
            let tool = if tool.is_empty() {
                "tool".to_string()
            } else {
                tool
            };
            let file = dir.join(format!("{tool}-{}.txt", &hash[..12]));
            if !file.exists() {
                std::fs::write(&file, text)?;
            }
            let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "txt"))
                .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
                .collect();
            files.sort_by_key(|file| std::cmp::Reverse(file.0));
            for (_, stale) in files.into_iter().skip(SPILL_KEEP) {
                let _ = std::fs::remove_file(stale);
            }
            Ok(file)
        };
        match write() {
            Ok(file) => Some(file),
            Err(error) => {
                tracing::debug!(%error, "chisle spill failed; eliding without a recovery copy");
                None
            }
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests;
