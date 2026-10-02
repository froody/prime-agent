//! Chisle mode vocabulary: the natural-language toggle parser (upstream
//! `hooks/chisle-mode.js`) and the rules text the session injects, with the
//! upstream `sections` filter (upstream `filterSections`).

use std::sync::LazyLock;

use fancy_regex::Regex;

/// The rules body (upstream `skills/chisle/SKILL.md`, frontmatter stripped).
const RULES: &str = include_str!("rules.md");

/// Headings `sections.prose: false` drops.
const PROSE_HEADINGS: &[&str] = &["Prose: Maximum Signal Per Token", "Output Format"];
/// Headings `sections.code: false` drops.
const CODE_HEADINGS: &[&str] = &[
    "Code: The Efficiency Ladder",
    "Code Rules",
    "Context Diet: Read Less Into the Window",
];

/// Which rule groups the injected ruleset keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sections {
    pub(crate) prose: bool,
    pub(crate) code: bool,
}

/// The mode a prompt asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    On,
    Off,
}

impl Mode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Mode::On => "on",
            Mode::Off => "off",
        }
    }
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("chisle mode regex compiles")
}

static OFF: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        regex(r"\b(turn off|disable|deactivate|stop|kill|exit)\s+chisle\b"),
        regex(r"\bchisle\s+(mode\s+)?(off|stop|disable|deactivate)\b"),
        regex(r"\bnormal mode\b"),
    ]
});

static ON: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        regex(r"^/chisle(\b|:chisle\b)"),
        regex(r"\b(activate|enable|turn on|start|use)\b.*\bchisle\b"),
        regex(r"\bchisle\b.*\b(mode|activate|enable|on)\b"),
        regex(r"\bchislif(y|ier)\b"),
    ]
});

/// upstream `requestedMode`: the mode a prompt asks for, if any. Off wins.
pub(crate) fn requested_mode(prompt: &str) -> Option<Mode> {
    let text = prompt.trim().to_lowercase();
    let matches = |patterns: &[Regex]| {
        patterns
            .iter()
            .any(|pattern| pattern.is_match(&text).unwrap_or(false))
    };
    if matches(OFF.as_slice()) {
        return Some(Mode::Off);
    }
    matches(ON.as_slice()).then_some(Mode::On)
}

/// The injected ruleset: `CHISLE ACTIVE` plus the rules minus the
/// `## ` sections a disabled group owns.
pub(crate) fn rules_text(sections: Sections) -> String {
    let mut out = String::from("CHISLE ACTIVE\n\n");
    let mut keep = true;
    for line in RULES.split_inclusive('\n') {
        if let Some(heading) = line.strip_prefix("## ") {
            let heading = heading.trim();
            let dropped_prose = !sections.prose && PROSE_HEADINGS.contains(&heading);
            let dropped_code = !sections.code && CODE_HEADINGS.contains(&heading);
            keep = !dropped_prose && !dropped_code;
        }
        if keep {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_mode_matches_the_upstream_phrases() {
        let cases = [
            ("/chisle", Some(Mode::On)),
            ("activate chisle please", Some(Mode::On)),
            ("chisle mode", Some(Mode::On)),
            ("chislify this answer", Some(Mode::On)),
            ("stop chisle", Some(Mode::Off)),
            ("Chisle off", Some(Mode::Off)),
            ("back to normal mode", Some(Mode::Off)),
            ("fix the parser bug", None),
            ("what does chisle do?", None),
        ];
        let actual: Vec<_> = cases
            .iter()
            .map(|(text, _)| (*text, requested_mode(text)))
            .collect();
        assert_eq!(actual, cases.to_vec());
    }

    #[test]
    fn rules_text_drops_only_the_disabled_group() {
        let all = rules_text(Sections {
            prose: true,
            code: true,
        });
        assert_eq!(all, format!("CHISLE ACTIVE\n\n{RULES}"));
        let code_only = rules_text(Sections {
            prose: false,
            code: true,
        });
        assert!(!code_only.contains("## Prose: Maximum Signal Per Token"));
        assert!(!code_only.contains("## Output Format"));
        assert!(code_only.contains("## Code: The Efficiency Ladder"));
        assert!(code_only.contains("## When NOT to be lazy"));
    }
}
