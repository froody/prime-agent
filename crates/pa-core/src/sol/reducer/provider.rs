//! The production reducer call: resolve `provider/model` and its auth through
//! the model registry, then run one `complete_simple` under a timeout.

use std::path::PathBuf;
use std::sync::Arc;

use pa_types::ai::{AssistantContentBlock, StopReason};

use super::{
    ReducerCallError, ReducerCallFn, ReducerRequest, ReducerResponse, ReducerUsage, DEFAULT_TIMEOUT,
};
use crate::sol::config::ReducerModelRef;

fn stop_reason_name(reason: StopReason) -> &'static str {
    match reason {
        StopReason::Stop => "stop",
        StopReason::Length => "length",
        StopReason::ToolUse => "toolUse",
        StopReason::Error => "error",
        StopReason::Aborted => "aborted",
    }
}

async fn call_registry_model(
    agent_dir: PathBuf,
    target: ReducerModelRef,
    request: ReducerRequest,
) -> Result<ReducerResponse, ReducerCallError> {
    let auth = crate::auth::AuthStorage::create(&agent_dir);
    let mut registry = crate::models::ModelRegistry::create(auth, agent_dir.join("models.json"));
    let model = registry
        .get_all()
        .iter()
        .find(|model| model.provider == target.provider && model.id == target.model)
        .cloned()
        .ok_or_else(|| {
            ReducerCallError::ModelUnavailable(format!(
                "Reducer model is unavailable: {}/{}",
                target.provider, target.model
            ))
        })?;
    let resolved = registry.get_api_key_and_headers(&model, model.headers.as_ref());
    if !resolved.ok {
        return Err(ReducerCallError::ModelUnavailable(
            resolved.error.unwrap_or_default(),
        ));
    }
    let context = pa_types::ai::Context {
        system_prompt: Some(request.system_prompt),
        messages: vec![pa_types::ai::Message::User(pa_types::ai::UserMessage {
            content: pa_types::ai::UserContent::Text(request.input),
            timestamp: 0,
            rest: serde_json::Map::default(),
        })],
        tools: None,
    };
    let options = pa_ai::types::SimpleStreamOptions::from_base(pa_ai::types::StreamOptions {
        api_key: resolved.api_key,
        headers: resolved
            .headers
            .map(|headers| headers.into_iter().collect()),
        max_tokens: Some(request.max_output_tokens.min(model.max_tokens.max(1))),
        cache_retention: Some(pa_types::ai::CacheRetention::None),
        session_id: Some(request.session_id),
        ..Default::default()
    });
    let response = tokio::time::timeout(
        DEFAULT_TIMEOUT,
        pa_ai::complete_simple(&model, &context, Some(options)),
    )
    .await
    .map_err(|_| ReducerCallError::Timeout)?
    .map_err(|error| ReducerCallError::Failed(error.into()))?;
    let output_text = response
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContentBlock::Text(text) => Some(text.text.as_str()),
            AssistantContentBlock::Thinking(_) | AssistantContentBlock::ToolCall(_) => None,
        })
        .collect();
    Ok(ReducerResponse {
        provider: response.provider.clone(),
        model: response.model,
        ok: matches!(response.stop_reason, StopReason::Stop | StopReason::Length),
        stop_reason: stop_reason_name(response.stop_reason).to_string(),
        error_message: response.error_message,
        output_text,
        usage: ReducerUsage {
            input: response.usage.input,
            output: response.usage.output,
            total_tokens: response.usage.total_tokens,
        },
    })
}

/// The reducer call backed by `agent_dir`'s model registry and auth store.
#[must_use]
pub fn registry_reducer_call(agent_dir: PathBuf, target: ReducerModelRef) -> ReducerCallFn {
    Arc::new(move |request| {
        Box::pin(call_registry_model(
            agent_dir.clone(),
            target.clone(),
            request,
        ))
    })
}
