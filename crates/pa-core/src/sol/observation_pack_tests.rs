use super::*;
use pa_agent::types::{ImageContent, TextContent};

fn tool_result(body: &str) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: "call-1".to_string(),
        tool_name: "ipython".to_string(),
        content: vec![ToolResultContent::text(body)],
        details: None,
        is_error: false,
        timestamp: 1,
    }
}

fn message(result: ToolResultMessage) -> AgentMessage {
    AgentMessage::Standard(Message::ToolResult(result))
}

fn assistant() -> AgentMessage {
    serde_json::from_value(serde_json::json!({
        "role": "assistant", "content": [], "stopReason": "stop", "timestamp": 2,
        "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                   "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } }
    }))
    .unwrap()
}

fn text_of(message: &AgentMessage) -> String {
    let AgentMessage::Standard(Message::ToolResult(result)) = message else {
        return String::new();
    };
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text(TextContent { text, .. }) => Some(text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn large(line: &str) -> String {
    line.repeat((THRESHOLD_BYTES + 1).div_ceil(line.len()))
}

fn field<'a>(placeholder: &'a str, name: &str) -> &'a str {
    placeholder
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
        .unwrap()
}

#[test]
fn sends_in_full_twice_then_a_stable_placeholder_whose_path_holds_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let pack = ObservationPack::new(dir.path().to_path_buf());
    let body = format!("head line\n{}tail line\n", large("middle \u{263e} line\n"));
    let sends: Vec<String> = (0..4)
        .map(|_| text_of(&pack.project(vec![message(tool_result(&body))])[0]))
        .collect();
    assert_eq!(sends[0], body);
    assert_eq!(sends[1], body);
    assert!(sends[2].starts_with("[large tool result replaced"));
    let head = sends[2].find("head line").unwrap();
    assert!(sends[2][head..].contains("tail line"));
    assert_eq!(sends[3], sends[2]);
    let path = field(&sends[2], "path");
    assert_eq!(std::fs::read_to_string(path).unwrap(), body);
    let ledger = std::fs::read_to_string(dir.path().join("observation-pack/ledger.jsonl")).unwrap();
    let events: Vec<String> = ledger
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["event"].to_string())
        .collect();
    assert_eq!(
        events,
        ["\"full\"", "\"full\"", "\"placeholder\"", "\"placeholder\""]
    );
}

#[test]
fn counts_sends_from_the_assistant_turns_that_follow_a_result_on_resume() {
    let dir = tempfile::tempdir().unwrap();
    let pack = ObservationPack::new(dir.path().to_path_buf());
    let projected = pack.project(vec![
        message(tool_result(&large("resumed\n"))),
        assistant(),
        assistant(),
    ]);
    assert!(text_of(&projected[0]).starts_with("[large tool result replaced"));
}

#[test]
fn passes_through_errors_mixed_content_and_small_results() {
    let mut error = tool_result(&large("x"));
    error.is_error = true;
    let mut mixed = tool_result(&large("x"));
    mixed.content.push(ToolResultContent::Image(ImageContent {
        data: "AA==".to_string(),
        mime_type: "image/png".to_string(),
    }));
    for result in [error, mixed, tool_result("small")] {
        let dir = tempfile::tempdir().unwrap();
        let pack = ObservationPack::new(dir.path().to_path_buf());
        let original = message(result);
        for _ in 0..3 {
            assert_eq!(pack.project(vec![original.clone()]), vec![original.clone()]);
        }
    }
}

#[cfg(unix)]
#[test]
fn fails_open_when_the_archive_directory_is_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("observation-pack")).unwrap();
    std::os::unix::fs::symlink(target.path(), dir.path().join("observation-pack/objects")).unwrap();
    let pack = ObservationPack::new(dir.path().to_path_buf());
    let body = large("guarded\n");
    for _ in 0..3 {
        assert_eq!(
            text_of(&pack.project(vec![message(tool_result(&body))])[0]),
            body
        );
    }
    assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
}

#[test]
fn a_tampered_archive_object_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let body = large("tamper\n");
    let observation = create_observation(&tool_result(&body), body.clone(), dir.path()).unwrap();
    ensure_stored(&observation).unwrap();
    ensure_stored(&observation).unwrap();
    std::fs::write(&observation.file_path, body.replace("tamper", "tamPer")).unwrap();
    assert!(ensure_stored(&observation).is_err());
}

#[test]
fn placeholder_excerpts_keep_whole_lines_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!("{}\n{}", "a".repeat(600), large("line\n"));
    let observation = create_observation(&tool_result(&body), body.clone(), dir.path()).unwrap();
    let placeholder = placeholder_for(&observation);
    let head_marker = "[first complete lines, up to 512 bytes]\n";
    let head_start = placeholder.find(head_marker).unwrap() + head_marker.len();
    assert!(
        placeholder[head_start..].starts_with("\n[middle omitted"),
        "a first line over the head budget yields an empty head"
    );
    assert!(placeholder.ends_with(&format!("line\n\n[{} original bytes omitted]", body.len())));
    assert_eq!(count_lines("a\nb\n"), 2);
    assert_eq!(count_lines("a\nb"), 2);
    assert_eq!(
        estimate_tokens("\u{263e}\u{263e}\u{263e}\u{263e}\u{263e}"),
        2
    );
}
