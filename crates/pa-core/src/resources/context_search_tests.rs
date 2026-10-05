use std::fs;
use std::path::Path;

use pa_agent::types::{AgentMessage, Message, ToolResultContent, ToolResultMessage};

use super::*;

/// The polez layout: root rules (one unscoped, one scoped to Dart) reached
/// through both rule directories, and a subtree AGENTS.md with a CLAUDE.md
/// symlink beside it.
fn repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join(".agents/rules")).unwrap();
    fs::create_dir_all(root.join(".claude")).unwrap();
    fs::create_dir_all(root.join("client/lib/gen")).unwrap();
    fs::write(root.join(".agents/rules/general.md"), "general rule").unwrap();
    fs::write(
        root.join(".agents/rules/dart.md"),
        "---\nglobs:\n- \"**/*.dart\"\n- \"!**/gen/**\"\n---\ndart rule",
    )
    .unwrap();
    std::os::unix::fs::symlink("../.agents/rules", root.join(".claude/rules")).unwrap();
    fs::write(root.join("client/AGENTS.md"), "client rules").unwrap();
    std::os::unix::fs::symlink("AGENTS.md", root.join("client/CLAUDE.md")).unwrap();
    for file in ["client/lib/a.dart", "client/lib/gen/b.dart", "README.md"] {
        fs::write(root.join(file), "").unwrap();
    }
    tmp
}

fn block_texts(blocks: &[ToolResultContent]) -> Vec<String> {
    blocks
        .iter()
        .map(|block| match block {
            ToolResultContent::Text(text) => text.text.clone(),
            ToolResultContent::Image(_) => unreachable!(),
        })
        .collect()
}

#[test]
fn unscoped_rules_load_once_and_scoped_rules_wait() {
    let tmp = repo();
    let (unscoped, search) = discover(tmp.path());
    let root = tmp.path().canonicalize().unwrap();
    assert_eq!(
        unscoped,
        vec![ContextFile {
            path: root.join(".agents/rules/general.md"),
            content: "general rule".to_string(),
        }]
    );
    assert_eq!(search.scoped_rules.len(), 1);
}

#[test]
fn touched_files_attach_subtree_context_and_matching_rules_once() {
    let tmp = repo();
    let (_, search) = discover(tmp.path());
    let root = tmp.path().canonicalize().unwrap();
    let client = root.join("client/AGENTS.md").display().to_string();
    let dart = root.join(".agents/rules/dart.md").display().to_string();

    let blocks = search.blocks_for(&[Path::new("client/lib/a.dart").to_path_buf()], &[]);
    assert_eq!(
        block_texts(&blocks),
        vec![
            format!("[project context: {client}]\n\nclient rules"),
            format!("[project context: {dart}]\n\ndart rule"),
        ]
    );

    let excluded = search.blocks_for(&[root.join("client/lib/gen/b.dart")], &[]);
    assert_eq!(
        block_texts(&excluded),
        vec![format!("[project context: {client}]\n\nclient rules")]
    );
    assert!(search.blocks_for(&[root.join("README.md")], &[]).is_empty());

    let in_context = vec![AgentMessage::Standard(Message::ToolResult(
        ToolResultMessage {
            tool_call_id: "call".to_string(),
            tool_name: "edit".to_string(),
            content: blocks,
            details: None,
            is_error: false,
            timestamp: 0,
        },
    ))];
    assert!(search
        .blocks_for(&[root.join("client/lib/a.dart")], &in_context)
        .is_empty());
}
