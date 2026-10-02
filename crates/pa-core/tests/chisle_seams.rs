// The fleet-uniform integration-test disposition (see src/lib.rs): engine
// futures are large and the end-to-end scenario reads best as one test.
#![allow(clippy::large_futures, clippy::too_many_lines)]

//! End-to-end check of the Chisle seams through `create_session`: a
//! `chisle.json` turns the mode on, the first request carries the hidden
//! ruleset row, a large `bash` result is elided with a recovery copy, and
//! `/chisle off` withdraws the rules on the next turn and persists.

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
use pa_core::session_engine::session_commands::{execute_session_command, SessionCommandParams};
use pa_core::session_engine::{PromptOptions, PromptOutcome};
use pa_types::ai::{Message, StopReason, UserContent, UserContentBlock};
use serde_json::json;

fn build_log() -> String {
    (0..600)
        .map(|i| format!("   Compiling crate_{i} v0.1.0"))
        .chain(["error[E0308]: mismatched types".to_string()])
        .chain((0..100).map(|i| format!("  checking {i}")))
        .collect::<Vec<_>>()
        .join("\n")
}

struct ScriptedBash {
    parameters: serde_json::Value,
}

impl AgentTool for ScriptedBash {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "scripted bash"
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
        Box::pin(async { Ok(AgentToolResult::text(build_log())) })
    }
}

fn user_texts(context: &pa_types::ai::Context) -> Vec<String> {
    let block_text = |blocks: &[UserContentBlock]| {
        blocks
            .iter()
            .filter_map(|block| match block {
                UserContentBlock::Text(text) => Some(text.text.clone()),
                UserContentBlock::Image(_) | UserContentBlock::Raw(_) => None,
            })
            .collect::<String>()
    };
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(match &user.content {
                UserContent::Text(text) => text.clone(),
                UserContent::Blocks(blocks) => block_text(blocks),
            }),
            Message::ToolResult(result) => Some(block_text(&result.content)),
            Message::Assistant(_) => None,
        })
        .collect()
}

#[tokio::test]
async fn chisle_injects_rules_compresses_output_and_toggles_off() {
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("chisle.json"), "{}").unwrap();

    let main = register_faux_provider(RegisterFauxProviderOptions {
        api: Some("chisle-e2e".to_string()),
        provider: Some("chisle-e2e".to_string()),
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
            vec![
                faux_text("building"),
                faux_tool_call(
                    "bash",
                    &json!({ "command": "cargo build" }),
                    Some("call-build"),
                ),
            ],
            FauxAssistantMessageOptions {
                stop_reason: Some(StopReason::ToolUse),
                ..Default::default()
            },
        ),
        faux_assistant_text_message("Type error.", FauxAssistantMessageOptions::default()),
        faux_assistant_text_message("ok", FauxAssistantMessageOptions::default()),
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
    let session_dir = dir.path().join("sessions").join("project");
    let manager = SessionManager::persisted(dir.path(), &session_dir);
    let session_file = manager.get_session_file().unwrap().to_path_buf();
    let engine = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir: agent_dir.clone(),
        model: Some(json_round_trip(&model).unwrap()),
        stream_fn: Some(real_stream_fn(None, model.clone())),
        tools: vec![Arc::new(ScriptedBash {
            parameters: json!({ "type": "object", "properties": { "command": { "type": "string" } }, "required": ["command"] }),
        })],
        session_manager: Some(manager),
        ..Default::default()
    })
    .await
    .unwrap();
    engine
        .session
        .prompt("build it", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;

    let PromptOutcome::SessionCommand(command) = engine
        .session
        .prompt("/chisle off", PromptOptions::default())
        .await
        .unwrap()
    else {
        panic!("/chisle is a session command");
    };
    let mut autonomous = pa_core::autonomous::create_autonomous_runtime_state(None, None);
    let execution = execute_session_command(
        &engine,
        &mut SessionCommandParams {
            model: &json_round_trip(&model).unwrap(),
            api_key: None,
            autonomous: &mut autonomous,
            global_harness_dir: dir.path().join("harness"),
        },
        &command,
    )
    .await;
    assert_eq!(execution.error, None);
    engine
        .session
        .prompt("thanks", PromptOptions::default())
        .await
        .unwrap();
    engine.session.agent().wait_for_idle().await;

    let contexts = contexts.lock().unwrap().clone();
    main.unregister();
    assert_eq!(contexts.len(), 3);

    // Turn 1: the hidden rules row rides ahead of the prompt.
    let first = user_texts(&contexts[0]);
    let n = first.len();
    assert!(
        first[n - 2].starts_with("CHISLE ACTIVE\n\n# Chisle"),
        "{first:?}"
    );
    assert_eq!(first[n - 1], "build it");

    // The bash result went out elided, error line salvaged, recovery copy on disk.
    let result = user_texts(&contexts[1]).pop().unwrap();
    assert!(result.contains("... [chisle: elided 601 lines, kept first 60, last 40, and 1 error-like line(s) below."), "{result}");
    assert!(result.contains("\nerror[E0308]: mismatched types\n"));
    let spill = result
        .split("Full output: ")
        .nth(1)
        .and_then(|rest| rest.split(" (grep it").next())
        .unwrap();
    assert_eq!(std::fs::read_to_string(spill).unwrap(), build_log());

    // Turn 2 (after /chisle off): the withdrawal row, no second ruleset.
    let last = user_texts(&contexts[2]);
    let rules = last
        .iter()
        .filter(|text| text.starts_with("CHISLE ACTIVE"))
        .count();
    assert_eq!(rules, 1);
    assert_eq!(
        last[last.len() - 2..],
        [
            "CHISLE DISABLED. Ignore only Chisle style and efficiency-ladder instructions."
                .to_string(),
            "thanks".to_string(),
        ]
    );

    // The mode persists: a session rebuilt on the same file starts off.
    let rebuilt = create_session(SessionEngineConfig {
        cwd: dir.path().to_path_buf(),
        agent_dir,
        model: Some(json_round_trip(&model).unwrap()),
        stream_fn: Some(real_stream_fn(None, model.clone())),
        session_manager: Some(SessionManager::open(
            dir.path(),
            &session_dir,
            &session_file,
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(!rebuilt.session.chisle().unwrap().is_active());
}
