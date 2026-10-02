//! `/chisle [on|off|status|help|audit [target]|review [target]]`.
//! `audit` and `review` run the upstream one-shot skills as a hidden
//! instruction row (the echo row is the visible prompt), so they need no
//! separately installed skill.

use super::mode::Mode;
use super::session::ChisleSession;

const AUDIT: &str = include_str!("audit.md");
const REVIEW: &str = include_str!("review.md");
/// Hidden row carrying a one-shot audit/review instruction.
pub const TASK_CUSTOM_TYPE: &str = "chisle_task";

const HELP: &str = "Chisle: zero-fluff prose + YAGNI-first code, plus tool-output compression.
/chisle [on]        enable for this session
/chisle off         disable (also: \"stop chisle\", \"normal mode\")
/chisle status      mode and compression savings
/chisle audit [x]   rank code and prose bloat in the diff, a path, or \"repo\" (read-only)
/chisle review [x]  flag over-engineering in the diff or a file (read-only)
Config: ~/.prime/agent/chisle.json (defaultMode, sections, compress); CHISLE_* env overrides.";

/// What one `/chisle` invocation did.
#[derive(Debug, PartialEq)]
pub(crate) struct Reply {
    pub(crate) status: String,
    /// The mode change to record as a session entry.
    pub(crate) mode_entry: Option<(&'static str, serde_json::Value)>,
    /// A one-shot instruction the follow-up turn runs on.
    pub(crate) task: Option<String>,
}

pub(crate) fn run(chisle: &ChisleSession, args: &str) -> Result<Reply, String> {
    let args = args.trim();
    let (sub, target) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let target = target.trim();
    let reply = |status: String| Reply {
        status,
        mode_entry: None,
        task: None,
    };
    let toggle = |mode: Mode, status: &str| Reply {
        status: status.to_string(),
        mode_entry: Some(chisle.set_mode(mode)),
        task: None,
    };
    let task = |skill: &str, label: &str| {
        let scope = if target.is_empty() {
            "the current git diff".to_string()
        } else {
            format!("`{target}`")
        };
        Reply {
            status: format!("Chisle {label}: {scope}"),
            mode_entry: None,
            task: Some(format!("{skill}\n\nTarget: {scope}.")),
        }
    };
    Ok(match sub {
        "" | "on" => toggle(Mode::On, "Chisle enabled."),
        "off" => toggle(Mode::Off, "Chisle disabled."),
        "status" => {
            let tokens = chisle.saved_chars() / 4;
            let mode = if chisle.is_active() { "on" } else { "off" };
            reply(format!(
                "Chisle {mode}. Tool-output compression saved ~{tokens} tokens this session."
            ))
        }
        "help" => reply(HELP.to_string()),
        "audit" => task(AUDIT, "audit"),
        "review" => task(REVIEW, "review"),
        other => {
            return Err(format!(
                "Unknown /chisle argument: {other}. Use on, off, status, help, audit, or review."
            ))
        }
    })
}
