use std::path::Path;

use pa_agent::types::{AgentMessage, CustomAgentMessage, Message, TextContent, ToolResultContent};
use pa_types::session::{CustomEntry, EntryBase, FileEntry};

use super::*;
use crate::chisle::config::ChisleConfig;

fn config() -> ChisleConfig {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("chisle.json"), "{}").unwrap();
    ChisleConfig::load(dir.path()).unwrap()
}

fn session(artifacts: Option<&Path>) -> ChisleSession {
    ChisleSession::new(config(), &[], artifacts)
}

fn text(text: &str) -> ToolResultContent {
    ToolResultContent::Text(TextContent {
        text: text.to_string(),
        text_signature: None,
    })
}

fn texts(blocks: &[ToolResultContent]) -> Vec<&str> {
    blocks
        .iter()
        .map(|block| match block {
            ToolResultContent::Text(text) => text.text.as_str(),
            ToolResultContent::Image(_) => "<image>",
        })
        .collect()
}

fn tool_result(id: &str) -> AgentMessage {
    AgentMessage::Standard(Message::ToolResult(
        serde_json::from_value(serde_json::json!({
            "toolCallId": id, "toolName": "bash", "content": [], "isError": false,
        }))
        .unwrap(),
    ))
}

fn custom_row(custom_type: &str) -> AgentMessage {
    AgentMessage::Custom(CustomAgentMessage {
        role: "custom".to_string(),
        payload: serde_json::json!({ "customType": custom_type }),
    })
}

fn mode_entry(mode: &str) -> FileEntry {
    FileEntry::Custom {
        payload: CustomEntry {
            custom_type: MODE_ENTRY_TYPE.to_string(),
            data: Some(serde_json::json!({ "mode": mode })),
            rest: serde_json::Map::default(),
        },
        base: EntryBase {
            id: Some("e1".to_string()),
            parent_id: None,
            timestamp: None,
            rest: serde_json::Map::default(),
        },
    }
}

fn build_log(lines: usize) -> String {
    (0..lines)
        .map(|i| format!("compiling crate_{i} v0.1.0 {}", "x".repeat(60)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_newest_mode_entry_wins_over_the_default() {
    assert!(session(None).is_active());
    let off = mode_entry("off");
    assert!(!ChisleSession::new(config(), &[&off], None).is_active());
    let on = mode_entry("on");
    assert!(ChisleSession::new(config(), &[&off, &on], None).is_active());
}

#[test]
fn rules_are_owed_once_per_context_and_withdrawn_when_off() {
    let chisle = session(None);
    let owed = |messages: &[AgentMessage]| {
        chisle
            .owed_row(messages)
            .map(|row| (row.custom_type, row.display))
    };
    assert_eq!(owed(&[]), Some((RULES_CUSTOM_TYPE.to_string(), false)));
    assert_eq!(owed(&[custom_row(RULES_CUSTOM_TYPE)]), None);
    chisle.set_mode(Mode::Off);
    assert_eq!(owed(&[]), None);
    assert_eq!(
        owed(&[custom_row(RULES_CUSTOM_TYPE)]),
        Some((DISABLED_CUSTOM_TYPE.to_string(), false))
    );
    chisle.set_mode(Mode::On);
    assert_eq!(
        owed(&[
            custom_row(RULES_CUSTOM_TYPE),
            custom_row(DISABLED_CUSTOM_TYPE)
        ]),
        Some((RULES_CUSTOM_TYPE.to_string(), false))
    );
}

#[test]
fn large_output_is_elided_with_a_recovery_copy() {
    let artifacts = tempfile::tempdir().unwrap();
    let chisle = session(Some(artifacts.path()));
    let log = build_log(500);
    let compressed = chisle
        .compress("bash", "call-1", &[text(&log), text("ok")], &[])
        .unwrap();
    let spill = artifacts.path().join(SPILL_DIR);
    let spilled: Vec<_> = std::fs::read_dir(&spill).unwrap().collect();
    assert_eq!(spilled.len(), 1);
    let spill_file = spilled[0].as_ref().unwrap().path();
    assert_eq!(std::fs::read_to_string(&spill_file).unwrap(), log);
    let kept: Vec<&str> = log.split('\n').collect();
    let expected = format!(
        "{}\n... [chisle: elided 400 lines, kept first 60, last 40. Full output: {} (grep it, do not re-run)] ...\n{}",
        kept[..60].join("\n"),
        spill_file.display(),
        kept[460..].join("\n"),
    );
    assert_eq!(texts(&compressed), vec![expected.as_str(), "ok"]);
    assert_eq!(
        chisle.saved_chars(),
        log.chars().count() - expected.chars().count()
    );
}

#[test]
fn a_repeat_dedups_only_while_the_first_copy_is_in_context() {
    let chisle = session(None);
    let output = "y".repeat(3000);
    assert_eq!(chisle.duplicate("bash", "call-1", 0, &output, &[]), None);
    assert_eq!(chisle.duplicate("bash", "call-2", 0, &output, &[]), None);
    let marker = chisle.duplicate("bash", "call-3", 0, &output, &[tool_result("call-2")]);
    assert_eq!(marker, Some(duplicate_marker("bash", &output, None)));
}
