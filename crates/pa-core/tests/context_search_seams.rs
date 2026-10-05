// The fleet-uniform integration-test disposition (see src/lib.rs): engine
// futures are large.
#![allow(clippy::large_futures)]

//! End-to-end check of `contextSearchPolicy: "claude"` through
//! `create_session`: unscoped `.agents/rules` files join the system prompt,
//! and an `edit` below cwd attaches the subtree AGENTS.md and the matching
//! scoped rule to that tool result.

use std::sync::{Arc, Mutex};

use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_ai::faux::{
    faux_assistant_message, faux_assistant_text_message, faux_tool_call, register_faux_provider,
    FauxAssistantMessageOptions, FauxModelDefinition, FauxResponseStep,
    RegisterFauxProviderOptions,
};
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::provider_adapter::{json_round_trip, real_stream_fn};
use pa_core::session_engine::PromptOptions;
use pa_types::ai::{Message, StopReason, UserContentBlock};
use serde_json::json;

struct ScriptedEdit {
    parameters: serde_json::Value,
}

impl AgentTool for ScriptedEdit {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "scripted edit"
    }
    fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }
    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        _params: serde_json::Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> pa_agent::BoxFut<'static, anyhow::Result<AgentToolResult>> {
        Box::pin(async { Ok(AgentToolResult::text("edited")) })
    }
}

#[tokio::test]
async fn claude_policy_loads_rules_and_attaches_subtree_context_on_edit() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let repo = dir.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({ "contextSearchPolicy": "claude" }).to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(repo.join(".agents/rules")).unwrap();
    std::fs::create_dir_all(repo.join("client/lib")).unwrap();
    std::fs::write(repo.join(".agents/rules/general.md"), "general rule").unwrap();
    std::fs::write(
        repo.join(".agents/rules/dart.md"),
        "---\npaths: \"**/*.dart\"\n---\ndart rule",
    )
    .unwrap();
    std::fs::write(repo.join("client/AGENTS.md"), "client rules").unwrap();
    std::fs::write(repo.join("client/lib/a.dart"), "").unwrap();

    let main = register_faux_provider(RegisterFauxProviderOptions {
        api: Some("context-search-e2e".to_string()),
        provider: Some("context-search-e2e".to_string()),
        models: Some(vec![FauxModelDefinition {
            id: "main-1".to_string(),
            name: Some("main-1".to_string()),
            reasoning: Some(false),
            input: Some(vec![pa_types::ai::ModelInput::Text]),
            cost: None,
            context_window: Some(200_000),
            max_tokens: Some(4_096),
        }]),
        ..Default::default()
    });
    let contexts: Arc<Mutex<Vec<pa_types::ai::Context>>> = Arc::default();
    let replies = [
        faux_assistant_message(
            vec![faux_tool_call(
                "edit",
                &json!({ "path": "client/lib/a.dart" }),
                Some("call-edit"),
            )],
            FauxAssistantMessageOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        ),
        faux_assistant_text_message("done", FauxAssistantMessageOptions::default()),
    ];
    main.set_responses(
        replies
            .into_iter()
            .map(|reply| {
                let contexts = Arc::clone(&contexts);
                let step: pa_ai::faux::FauxResponseFactory = Arc::new(move |context, _, _, _| {
                    contexts.lock().unwrap().push(context.clone());
                    Ok(reply.clone())
                });
                FauxResponseStep::Factory(step)
            })
            .collect(),
    );

    let model = main.get_model();
    let engine = create_session(SessionEngineConfig {
        cwd: repo.clone(),
        agent_dir,
        model: Some(json_round_trip(&model).unwrap()),
        stream_fn: Some(real_stream_fn(None, model.clone())),
        tools: vec![Arc::new(ScriptedEdit {
            parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] }),
        })],
        ..Default::default()
    })
    .await
    .unwrap();
    engine
        .session
        .prompt("fix it", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;

    let contexts = contexts.lock().unwrap().clone();
    main.unregister();
    assert_eq!(contexts.len(), 2);
    let system_prompt = contexts[0].system_prompt.clone().unwrap_or_default();
    assert!(system_prompt.contains("general rule"));
    assert!(!system_prompt.contains("dart rule"));

    let Some(Message::ToolResult(result)) = contexts[1].messages.last() else {
        panic!("the edit result is the last message");
    };
    let texts: Vec<&str> = result
        .content
        .iter()
        .filter_map(|block| match block {
            UserContentBlock::Text(text) => Some(text.text.as_str()),
            UserContentBlock::Image(_) | UserContentBlock::Raw(_) => None,
        })
        .collect();
    assert_eq!(
        texts,
        [
            "edited".to_string(),
            format!(
                "[project context: {}]\n\nclient rules",
                repo.join("client/AGENTS.md").display()
            ),
            format!(
                "[project context: {}]\n\ndart rule",
                repo.join(".agents/rules/dart.md").display()
            ),
        ]
    );
}
