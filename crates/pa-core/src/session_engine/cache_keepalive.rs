//! Prompt-cache keepalive: once a session's turn ends and it goes idle, one
//! ping re-sends the last request with long cache retention, so the prompt
//! cache an Anthropic-style provider holds for 5 minutes stays for an hour.
//!
//! The ping costs a cache read of the prompt; a user who returns after the
//! 5-minute window would otherwise pay a full cache write. Requests after the
//! ping keep the default retention: re-marking an already-cached prefix with
//! the long TTL reads it without a new write.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_agent::stream::{
    AssistantMessageEvent, LlmContext, ModelStream, StreamFn, StreamRequestOptions,
};
use pa_agent::types::{AssistantMessage, CacheRetention, Model, StopReason, Usage};

use super::model_usage::ModelUsage;

/// Idle time after an end-of-turn reply before the ping, inside the 5-minute window.
const PING_DELAY: Duration = Duration::from_secs(270);
/// Prompts smaller than this save too little to be worth a ping.
const MIN_CACHED_TOKENS: u64 = 20_000;
/// The ping must cost at most this fraction of the input price per token, so a
/// read is cheap next to the write it saves.
const MAX_READ_TO_INPUT_PRICE: f64 = 0.1;

/// The `model_usage` row source a ping bills under.
pub const PING_USAGE_SOURCE: &str = "cache_keepalive";

/// Where a ping's bill goes; the engine records it as a `model_usage` row.
pub type RecordPing = Arc<dyn Fn(ModelUsage) + Send + Sync>;

/// Read live per ping, so a settings change applies to a running session.
pub type KeepaliveEnabled = Arc<dyn Fn() -> bool + Send + Sync>;

struct LastRequest {
    model: Model,
    context: LlmContext,
    options: StreamRequestOptions,
}

/// One session's ping schedule: every request cancels the pending ping, and a
/// qualifying end-of-turn reply schedules the next one.
struct Schedule {
    generation: u64,
    pending: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Schedule {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.abort();
        }
    }
}

/// Whether a model's cache pricing makes a ping pay: it bills cache writes,
/// and a cache read costs at most [`MAX_READ_TO_INPUT_PRICE`] of input.
fn worth_pinging(model: &Model) -> bool {
    let cost = &model.cost;
    cost.cache_write > 0.0
        && cost.input > 0.0
        && cost.cache_read <= cost.input * MAX_READ_TO_INPUT_PRICE
}

fn cached_tokens(usage: &Usage) -> u64 {
    usage.cache_read + usage.cache_write
}

/// Wrap `inner` so each end-of-turn reply on a model worth pinging schedules
/// one long-retention ping of the same request after [`PING_DELAY`]. The ping
/// runs on `inner` directly, so it never reaches the agent loop or the
/// session history; its usage goes to `record`.
pub(crate) fn wrap_stream_fn(
    enabled: KeepaliveEnabled,
    record: RecordPing,
    inner: StreamFn,
) -> StreamFn {
    let schedule = Arc::new(Mutex::new(Schedule {
        generation: 0,
        pending: None,
    }));
    Arc::new(move |model, context, options| {
        let generation = {
            let mut schedule = schedule
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(pending) = schedule.pending.take() {
                pending.abort();
            }
            schedule.generation += 1;
            schedule.generation
        };
        let last = (worth_pinging(&model) && options.cache_retention != Some(CacheRetention::Long))
            .then(|| LastRequest {
                model: model.clone(),
                context: context.clone(),
                options: options.clone(),
            });
        let stream = inner(model, context, options);
        let schedule = Arc::clone(&schedule);
        let enabled = Arc::clone(&enabled);
        let record = Arc::clone(&record);
        let inner = Arc::clone(&inner);
        Box::pin(async move {
            let stream = stream.await?;
            let Some(last) = last else {
                return Ok(stream);
            };
            let on_reply: OnReply = Box::new(move |reply| {
                if reply.stop_reason != StopReason::Stop
                    || cached_tokens(&reply.usage) < MIN_CACHED_TOKENS
                {
                    return;
                }
                let mut guard = schedule
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if guard.generation != generation {
                    return;
                }
                guard.pending = Some(tokio::spawn(async move {
                    tokio::time::sleep(PING_DELAY).await;
                    if enabled() {
                        ping(&inner, last, &record).await;
                    }
                }));
            });
            Ok(Box::new(ObservedStream {
                inner: stream,
                on_reply: Some(on_reply),
            }) as Box<dyn ModelStream>)
        })
    })
}

/// The long-retention ping: the last request as sent, with a one-token reply.
/// The reasoning setting stays as sent: Anthropic drops the cached messages
/// when thinking changes. The reply is dropped; only the bill is kept.
async fn ping(inner: &StreamFn, last: LastRequest, record: &RecordPing) {
    let LastRequest {
        model,
        context,
        mut options,
    } = last;
    options.cache_retention = Some(CacheRetention::Long);
    options.max_tokens = Some(1);
    options.signal = pa_agent::abort::AbortSignal::never();
    let reply = match inner(model.clone(), context, options).await {
        Ok(mut stream) => stream.result().await,
        Err(error) => Err(error),
    };
    match reply {
        Ok(reply) => {
            tracing::debug!(
                cache_read = reply.usage.cache_read,
                cache_write = reply.usage.cache_write,
                "prompt cache keepalive ping"
            );
            if let Some(usage) = ModelUsage::new(
                PING_USAGE_SOURCE,
                &model.provider,
                &model.id,
                super::provider_adapter::json_round_trip(&reply.usage).unwrap_or_default(),
            ) {
                record(usage);
            }
        }
        Err(error) => tracing::warn!(%error, "prompt cache keepalive ping failed"),
    }
}

type OnReply = Box<dyn FnOnce(&AssistantMessage) + Send>;

/// A provider stream whose final reply decides the next ping.
struct ObservedStream {
    inner: Box<dyn ModelStream>,
    on_reply: Option<OnReply>,
}

impl ModelStream for ObservedStream {
    fn next_event(&mut self) -> pa_agent::BoxFut<'_, Option<AssistantMessageEvent>> {
        self.inner.next_event()
    }

    fn result(&mut self) -> pa_agent::BoxFut<'_, anyhow::Result<AssistantMessage>> {
        Box::pin(async move {
            let reply = self.inner.result().await;
            if let (Ok(reply), Some(on_reply)) = (&reply, self.on_reply.take()) {
                on_reply(reply);
            }
            reply
        })
    }

    fn close(&mut self) {
        self.on_reply = None;
        self.inner.close();
    }
}

#[cfg(test)]
#[path = "cache_keepalive_tests.rs"]
mod tests;
