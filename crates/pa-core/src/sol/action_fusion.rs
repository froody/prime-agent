//! Action Fusion: ask the model to run the focused check in the same
//! `ipython` cell as the mutation, and count how often it does.
//!
//! Upstream wraps `edit`/`write` with a `then_run` parameter. Prime Agent's
//! mutations already happen inside `ipython` cells, so the port keeps the
//! intent: a system-prompt section ([`FUSION_GUIDANCE`]) plus a lexical
//! classifier ([`classify_cell`]) whose counts measure adoption.

use std::sync::LazyLock;

use fancy_regex::Regex;

/// Appended to the system prompt when Action Fusion is enabled.
pub const FUSION_GUIDANCE: &str = "

## Action fusion
After mutating files in an ipython cell, run the focused check (test/typecheck/lint) in the SAME cell and print only its tail or failures.
Don't spend a separate turn on the check. Put the mutation first so a failed check still leaves the edit applied.";

/// What an `ipython` cell does, lexically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Mutate,
    Check,
    Fused,
    Neither,
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("action-fusion regex compiles")
}

static MUTATE: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        regex(r"(?<![.\w])(?:edit|write)\s*\("),
        regex(r#"(?<![.\w])open\s*\([^)]*,\s*(?:mode\s*=\s*)?[rbf]?["'][^"']*[wa]"#),
        regex(r"\.write_(?:text|bytes)\s*\("),
        regex(r"(?m)^\s*!\s*edit\b"),
    ]
});
static CHECK: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"\b(?:pytest|vitest|jest|tsc|npm (?:run )?(?:test|check)|cargo (?:test|check)|go test|make|ruff|mypy)\b",
    )
});
/// Shell commands in a cell: `bash("...")` arguments and `!` lines.
static COMMANDS: LazyLock<[Regex; 2]> = LazyLock::new(|| {
    [
        regex(r#"(?<![.\w])bash\s*\(\s*[rf]?(["'])((?:(?!\1).)*)\1"#),
        regex(r"(?m)^\s*!(.*)$"),
    ]
});

fn is_match(regex: &Regex, text: &str) -> bool {
    regex.is_match(text).unwrap_or(false)
}

#[must_use]
pub fn classify_cell(code: &str) -> CellKind {
    let mutates = MUTATE.iter().any(|regex| is_match(regex, code));
    let checks = COMMANDS.iter().any(|regex| {
        regex
            .captures_iter(code)
            .filter_map(Result::ok)
            .any(|captures| {
                captures
                    .get(captures.len() - 1)
                    .is_some_and(|command| is_match(&CHECK, command.as_str()))
            })
    });
    match (mutates, checks) {
        (true, true) => CellKind::Fused,
        (true, false) => CellKind::Mutate,
        (false, true) => CellKind::Check,
        (false, false) => CellKind::Neither,
    }
}

/// Per-session fused/split counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FusionCounts {
    /// Cells that mutated and checked together.
    pub fused: u64,
    /// Check cells that directly followed a mutate-only cell.
    pub unfused: u64,
    last: Option<CellKind>,
}

impl FusionCounts {
    /// Record one `ipython` cell and return its classification.
    pub fn record(&mut self, code: &str) -> CellKind {
        let kind = classify_cell(code);
        match kind {
            CellKind::Fused => self.fused += 1,
            CellKind::Check if self.last == Some(CellKind::Mutate) => self.unfused += 1,
            CellKind::Check | CellKind::Mutate | CellKind::Neither => {}
        }
        self.last = Some(kind);
        kind
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDIT: &str = r#"await edit(path="a.py", old_str="x", new_str="y")"#;

    #[test]
    fn classifies_cells() {
        let fused_tsc =
            format!("{EDIT}\nr = await bash(\"npx tsc --noEmit\")\nprint(r.output[-500:])");
        let cases: [(&str, CellKind); 11] = [
            (EDIT, CellKind::Mutate),
            (r#"Path("a").write_text("hi")"#, CellKind::Mutate),
            (
                "with open(\"a.txt\", \"w\") as f:\n    f.write(\"x\")",
                CellKind::Mutate,
            ),
            ("!pytest -q", CellKind::Check),
            (r#"r = await bash("npm test")"#, CellKind::Check),
            (&fused_tsc, CellKind::Fused),
            (
                "Path(\"a\").write_text(\"x\")\n!cargo test",
                CellKind::Fused,
            ),
            (r#"print(open("a.txt").read())"#, CellKind::Neither),
            (r#"r = await bash("ls -la")"#, CellKind::Neither),
            ("x = 1 + 1", CellKind::Neither),
            (r#"print("pytest")"#, CellKind::Neither),
        ];
        for (code, kind) in cases {
            assert_eq!(classify_cell(code), kind, "{code}");
        }
    }

    #[test]
    fn counts_split_mutate_then_check_pairs_and_fused_cells() {
        let mut counts = FusionCounts::default();
        counts.record(EDIT);
        counts.record("!pytest -q");
        counts.record("!pytest -q");
        assert_eq!((counts.fused, counts.unfused), (0, 1));
        assert_eq!(
            counts.record(&format!("{EDIT}\n!pytest -q")),
            CellKind::Fused
        );
        assert_eq!((counts.fused, counts.unfused), (1, 1));
    }
}
