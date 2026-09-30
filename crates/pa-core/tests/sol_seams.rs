// The fleet-uniform integration-test disposition (see src/lib.rs): engine
// futures are large and the end-to-end scenario reads best as one test.
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! End-to-end check of the `SoL` seams through `create_session`: a
//! `sol-prime.json` in the agent dir turns on the Observation Pack (the
//! loop's `transform_context`) and the Evidence-Preserving Reducer (the
//! loop's `after_tool_call`, routed through the model registry to a faux
//! custom provider). A scripted `ipython` tool returns a large pytest log.

use std::sync::{Arc, Mutex};

use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_ai::faux::{
    faux_assistant_message, faux_assistant_text_message, faux_text, faux_tool_call,
    register_faux_provider, FauxAssistantMessageOptions, FauxModelDefinition, FauxResponseStep,
    RegisterFauxProviderOptions,
};
use pa_core::session::manager::SessionManager;
use pa_core::session_engine::engine::{create_session, SessionEngineConfig};
use pa_core::session_engine::provider_adapter::{json_round_trip, real_stream_fn};
use pa_core::session_engine::PromptOptions;
use pa_types::ai::{AssistantContentBlock, Message, StopReason, UserContent, UserContentBlock};
use serde_json::json;

const FAILED_LINE: &str = "FAILED test/a.py::test_x - AssertionError: 1 != 2";

fn pytest_log() -> String {
    format!(
        "{}{FAILED_LINE}\n1 failed, 399 passed\n",
        "collecting ... ok\n".repeat(800)
    )
}

/// An `ipython` stand-in returning the same pytest log for every cell.
struct ScriptedIpython {
    parameters: serde_json::Value,
}

impl AgentTool for ScriptedIpython {
    fn name(&self) -> &'static str {
        "ipython"
    }
    fn description(&self) -> &'static str {
        "scripted ipython"
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
        Box::pin(async { Ok(AgentToolResult::text(pytest_log())) })
    }
}

fn model_definition(id: &str) -> FauxModelDefinition {
    FauxModelDefinition {
        id: id.to_string(),
        name: Some(id.to_string()),
        reasoning: Some(false),
        input: Some(vec![pa_types::ai::ModelInput::Text]),
        cost: None,
        context_window: Some(200_000),
        max_tokens: Some(4_096),
    }
}

/// The receipt the reducer model returns: the source hash is read back
/// from the reducer input, and the quote exists in the log byte for byte.
fn reducer_reply(context: &pa_types::ai::Context) -> String {
    let input = match context.messages.first() {
        Some(Message::User(user)) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    UserContentBlock::Text(text) => Some(text.text.clone()),
                    UserContentBlock::Image(_) | UserContentBlock::Raw(_) => None,
                })
                .collect(),
        },
        _ => String::new(),
    };
    let source = input
        .lines()
        .find_map(|line| line.strip_prefix("source_sha256="))
        .unwrap_or_default()
        .to_string();
    json!({
        "schema": "sol-pi-evidence-receipt/1", "source_sha256": source,
        "status": "failure", "uncertain": false,
        "evidence": [{ "kind": "failure", "quote": FAILED_LINE }],
    })
    .to_string()
}

fn tool_result_texts(context: &pa_types::ai::Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(
                result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        UserContentBlock::Text(text) => Some(text.text.clone()),
                        UserContentBlock::Image(_) | UserContentBlock::Raw(_) => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_sol_prime_config_reduces_test_logs_and_packs_large_results() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();

    let main = register_faux_provider(RegisterFauxProviderOptions {
        api: Some("sol-e2e-main".to_string()),
        provider: Some("sol-e2e-main".to_string()),
        models: Some(vec![model_definition("main-1")]),
        ..Default::default()
    });
    let reducer = register_faux_provider(RegisterFauxProviderOptions {
        api: Some("sol-e2e-reducer".to_string()),
        provider: Some("sol-e2e-reducer".to_string()),
        models: Some(vec![model_definition("small-1")]),
        ..Default::default()
    });
    std::fs::write(
        agent_dir.join("models.json"),
        json!({ "providers": { "solreducer": {
            "baseUrl": "http://localhost:0", "apiKey": "reducer-key", "api": "sol-e2e-reducer",
            "models": [{ "id": "small-1", "name": "Small", "contextWindow": 200_000, "maxTokens": 4_096 }],
        } } })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        agent_dir.join("sol-prime.json"),
        json!({ "evidencePreservingReducer": true, "reducerModel": "solreducer/small-1" })
            .to_string(),
    )
    .unwrap();

    // Every main-model request records its context, then answers in order:
    // turn 1 runs pytest (reduced); turn 2 prints a large file (not a
    // diagnostic command, so it rides the Observation Pack); turns 3-4 add
    // requests until the pack's placeholder appears.
    let contexts: Arc<Mutex<Vec<pa_types::ai::Context>>> = Arc::default();
    let tool_turn = |id: &str, code: &str| {
        faux_assistant_message(
            vec![
                faux_text("running"),
                faux_tool_call("ipython", &json!({ "code": code }), Some(id)),
            ],
            FauxAssistantMessageOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        )
    };
    let replies = [
        tool_turn("call-pytest", "!pytest -q"),
        tool_turn("call-cat", "print(open('big.log').read())"),
        tool_turn("call-one", "print(1)"),
        tool_turn("call-two", "print(2)"),
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
    reducer.set_responses(vec![FauxResponseStep::Factory(Arc::new(
        |context, _options, _call, model| {
            let mut message = faux_assistant_text_message(
                &reducer_reply(context),
                FauxAssistantMessageOptions::default(),
            );
            message.provider.clone_from(&model.provider);
            message.model.clone_from(&model.id);
            Ok(message)
        },
    ))]);

    let model = main.get_model();
    let session_dir = dir.path().join("sessions").join("project");
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: agent_dir.clone(),
        model: Some(json_round_trip(&model).unwrap()),
        stream_fn: Some(real_stream_fn(None, model.clone())),
        tools: vec![Arc::new(ScriptedIpython {
            parameters: json!({ "type": "object", "properties": { "code": { "type": "string" } }, "required": ["code"] }),
        })],
        session_manager: Some(SessionManager::persisted(dir.path(), &session_dir)),
        ..Default::default()
    })
    .await
    .unwrap();
    engine
        .session
        .prompt("run the tests", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;

    let contexts = contexts.lock().unwrap().clone();
    main.unregister();
    reducer.unregister();
    assert_eq!(contexts.len(), 5, "one provider request per scripted turn");

    // The reducer receipt replaced the pytest log from its first send on.
    let receipt = &tool_result_texts(&contexts[1])[0];
    assert!(
        receipt.starts_with("sol_pi_evidence_receipt_v1\nstatus=failure"),
        "{receipt}"
    );
    assert!(receipt.contains(&serde_json::to_string(FAILED_LINE).unwrap()));
    let artifact = receipt
        .lines()
        .find_map(|line| line.strip_prefix("source_artifact="))
        .unwrap();
    assert_eq!(std::fs::read_to_string(artifact).unwrap(), pytest_log());

    // The unreduced log went out in full twice, then as a placeholder.
    let packed: Vec<String> = contexts[2..]
        .iter()
        .map(|context| tool_result_texts(context)[1].clone())
        .collect();
    assert_eq!(packed[0], pytest_log());
    assert_eq!(packed[1], pytest_log());
    assert!(
        packed[2].starts_with("[large tool result replaced"),
        "{}",
        packed[2]
    );
    let path = packed[2]
        .lines()
        .find_map(|line| line.strip_prefix("path: "))
        .unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), pytest_log());

    // Session history keeps the unprojected result.
    let entries = engine.session.entries().await;
    let persisted_full = entries.iter().any(|entry| {
        serde_json::to_string(entry)
            .is_ok_and(|text| text.contains("collecting ... ok") && text.contains("call-cat"))
    });
    assert!(persisted_full, "the stored tool result is never rewritten");
    let assistant_texts: Vec<String> = contexts[4]
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => {
                assistant.content.iter().find_map(|block| match block {
                    AssistantContentBlock::Text(text) => Some(text.text.clone()),
                    AssistantContentBlock::Thinking(_) | AssistantContentBlock::ToolCall(_) => None,
                })
            }
            _ => None,
        })
        .collect();
    assert_eq!(assistant_texts.len(), 4);
}
