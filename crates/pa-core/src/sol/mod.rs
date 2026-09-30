//! `SoL` mechanisms (a native port of NVlabs/SoL-Pi, MIT) that cut the tokens
//! a session spends without changing what the model can learn:
//!
//! - [`observation_pack`]: large tool results become archive placeholders
//!   after their first two provider requests (`transform_context` seam).
//! - [`reducer`]: long `ipython` build/test logs become verified quote
//!   receipts from a cheaper model (`after_tool_call` seam).
//! - [`online_compact`]: cache economics that price a compaction at each
//!   completed plan step.
//! - [`action_fusion`]: prompt guidance to fuse a mutation with its check,
//!   plus a cell classifier that counts adoption.
//!
//! [`config::SolConfig`] opts in per mechanism. Artifacts live under the
//! session's artifact directory, so deleting a session deletes them. Every
//! mechanism fails open: an internal error leaves the original result.

pub mod action_fusion;
pub mod config;
pub mod observation_pack;
pub mod online_compact;
pub mod reducer;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_agent::agent_loop::{AfterToolCallFn, TransformContextFn};

use config::SolConfig;
use observation_pack::ObservationPack;
use reducer::{EvidenceReducer, ReducerCallFn, ToolCallOutcome};

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// JavaScript `string.length`: the reference measures chars in UTF-16 units.
pub(crate) fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The `SoL` hooks one session installs into its agent loop.
pub struct SolSession {
    observation_pack: Option<ObservationPack>,
    reducer: Option<EvidenceReducer>,
    fusion: Option<Mutex<action_fusion::FusionCounts>>,
}

impl SolSession {
    /// Build the session's hooks. `runtime_root` is the session artifact
    /// directory; without one (in-memory sessions) nothing is archived, so
    /// only Action Fusion can run. `reducer_call` overrides the registry
    /// model call (tests); `None` resolves the configured reducer model
    /// through `agent_dir`'s registry.
    #[must_use]
    pub fn new(
        config: &SolConfig,
        runtime_root: Option<&Path>,
        agent_dir: &Path,
        reducer_call: Option<ReducerCallFn>,
    ) -> Self {
        let observation_pack = runtime_root
            .filter(|_| config.observation_pack)
            .map(|root| ObservationPack::new(root.to_path_buf()));
        let reducer = match (runtime_root, &config.evidence_preserving_reducer) {
            (Some(root), Some(target)) => {
                let call = reducer_call.unwrap_or_else(|| {
                    reducer::registry_reducer_call(PathBuf::from(agent_dir), target.clone())
                });
                Some(EvidenceReducer::new(root, call))
            }
            (None, Some(_) | None) | (Some(_), None) => None,
        };
        SolSession {
            observation_pack,
            reducer,
            fusion: config
                .action_fusion
                .then(|| Mutex::new(action_fusion::FusionCounts::default())),
        }
    }

    /// The system-prompt section Action Fusion appends, when enabled.
    #[must_use]
    pub fn system_prompt_addendum(&self) -> Option<&'static str> {
        self.fusion
            .as_ref()
            .map(|_| action_fusion::FUSION_GUIDANCE.trim_start())
    }

    /// Wrap the loop's context transform with the Observation Pack
    /// projection (applied after `inner`). Without a pack, `inner` returns.
    #[must_use]
    pub fn wrap_transform_context(
        self: &Arc<Self>,
        inner: TransformContextFn,
    ) -> TransformContextFn {
        if self.observation_pack.is_none() {
            return inner;
        }
        let session = Arc::clone(self);
        Arc::new(move |messages, signal| {
            let session = Arc::clone(&session);
            let inner = Arc::clone(&inner);
            Box::pin(async move {
                let messages = inner(messages, signal).await?;
                Ok(match &session.observation_pack {
                    Some(pack) => pack.project(messages),
                    None => messages,
                })
            })
        })
    }

    /// The loop's `after_tool_call` hook: fusion counting, then the reducer.
    /// `None` when neither mechanism is enabled.
    #[must_use]
    pub fn after_tool_call(self: &Arc<Self>) -> Option<AfterToolCallFn> {
        if self.reducer.is_none() && self.fusion.is_none() {
            return None;
        }
        let session = Arc::clone(self);
        Some(Arc::new(move |context, _signal| {
            let session = Arc::clone(&session);
            Box::pin(async move {
                let code = context.args.get("code").and_then(serde_json::Value::as_str);
                if let (Some(fusion), Some(code), "ipython") =
                    (&session.fusion, code, context.tool_call.name.as_str())
                {
                    let mut counts = fusion
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let kind = counts.record(code);
                    tracing::debug!(
                        ?kind,
                        fused = counts.fused,
                        split = counts.unfused,
                        "action fusion"
                    );
                }
                let Some(reducer) = &session.reducer else {
                    return Ok(None);
                };
                Ok(reducer
                    .reduce(ToolCallOutcome {
                        tool_name: &context.tool_call.name,
                        args: &context.args,
                        content: &context.result.content,
                        details: &context.result.details,
                        is_error: context.is_error,
                    })
                    .await)
            })
        }))
    }
}
