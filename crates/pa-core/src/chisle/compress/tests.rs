use std::path::{Path, PathBuf};

use super::{duplicate_marker, elide, scrub, transform, Limits};

fn big_output(lines: usize) -> String {
    let x = "x".repeat(80);
    (0..lines)
        .map(|i| format!("line {i} {x}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn scrub_strips_escapes_and_trailing_whitespace() {
    assert_eq!(
        scrub("\x1b[31mFAIL\x1b[0m test_foo  \t\n\x1b]0;title\x07plain \r\nend"),
        "FAIL test_foo\nplain\r\nend"
    );
}

#[test]
fn scrub_collapses_repeats_and_blank_runs() {
    let repeated = ["WARN: retrying connection"; 10].join("\n");
    assert_eq!(
        scrub(&format!("{repeated}\n\n\n\n\ny")),
        "WARN: retrying connection\n... [chisle: line repeated 10×] ...\n\ny"
    );
    let short_repeats = "a\na\na\n\nb";
    assert_eq!(scrub(short_repeats), short_repeats);
}

#[test]
fn elide_keeps_head_tail_and_salvages_error_lines() {
    let mut lines: Vec<String> = big_output(500).split('\n').map(str::to_owned).collect();
    lines[250] = "FATAL ERROR: connection refused at db.js:42".to_owned();
    lines[300] = "an earlier marker said error-like, all good".to_owned();
    let limits = Limits::DEFAULT;
    let spill = Path::new("/tmp/spill/bash-1.txt");
    let out = elide(&lines.join("\n"), limits, Some(spill)).expect("shrinks");
    let expected = [
        lines[..60].join("\n"),
        "... [chisle: elided 400 lines, kept first 60, last 40, and 1 error-like line(s) below. \
         Full output: /tmp/spill/bash-1.txt (grep it, do not re-run)] ..."
            .to_owned(),
        lines[250].clone(),
        lines[460..].join("\n"),
    ]
    .join("\n");
    assert_eq!(out, expected);
}

#[test]
fn elide_returns_none_without_a_win() {
    assert_eq!(elide("short", Limits::DEFAULT, None), None);
}

#[test]
fn elide_char_cuts_a_huge_line_with_thousands_commas() {
    let text = "y".repeat(50_000);
    let half = "y".repeat(4000);
    assert_eq!(
        elide(&text, Limits::DEFAULT, None),
        Some(format!(
            "{half}\n... [chisle: elided 42,000 chars from the middle.] ...\n{half}"
        ))
    );
}

#[test]
fn elide_never_splits_multibyte_chars() {
    let text = format!(
        "{}😀{}😀{}",
        "a".repeat(3999),
        "b".repeat(5000),
        "c".repeat(3999)
    );
    let head = format!("{}😀", "a".repeat(3999));
    let tail = format!("😀{}", "c".repeat(3999));
    assert_eq!(
        elide(&text, Limits::DEFAULT, None),
        Some(format!(
            "{head}\n... [chisle: elided 5,000 chars from the middle.] ...\n{tail}"
        ))
    );
}

#[test]
fn elide_windows_long_diagnostics_around_the_match() {
    let mut lines: Vec<String> = big_output(500).split('\n').map(str::to_owned).collect();
    let smiles = "😀".repeat(1000);
    lines[250] = format!("{smiles} FATAL LATE_ERROR {smiles}");
    let out = elide(&lines.join("\n"), Limits::DEFAULT, None).expect("shrinks");
    let salvaged = out.split('\n').nth(61).expect("salvaged line");
    // 298 content chars centred on "FATAL": 146 before it, 147 after its 5 chars.
    let expected = format!(
        "…{} FATAL LATE_ERROR {}…",
        "😀".repeat(145),
        "😀".repeat(147 - " LATE_ERROR ".len())
    );
    assert_eq!(salvaged, expected);
}

#[test]
fn transform_scrubs_medium_output_and_skips_trivial_wins() {
    let noisy = format!(
        "{}\n{}",
        ["\x1b[32m✓\x1b[0m ok"; 50].join("\n"),
        "tail ".repeat(300)
    );
    let spill_never = |_: &str| -> Option<PathBuf> { panic!("no elision needed") };
    assert_eq!(
        transform(&noisy, Limits::DEFAULT, true, spill_never),
        Some(format!(
            "✓ ok\n... [chisle: line repeated 50×] ...\n{}",
            "tail ".repeat(300).trim_end()
        ))
    );
    assert_eq!(
        transform(&"z".repeat(2000), Limits::DEFAULT, true, spill_never),
        None
    );
    assert_eq!(transform(&noisy, Limits::DEFAULT, false, spill_never), None);
}

#[test]
fn transform_elides_oversized_output_with_spill_path() {
    let text = big_output(500);
    let out = transform(&text, Limits::DEFAULT, true, |scrubbed| {
        assert_eq!(scrubbed, text);
        Some(PathBuf::from("/s/out.txt"))
    })
    .expect("elides");
    assert!(out.contains(
        "... [chisle: elided 400 lines, kept first 60, last 40. Full output: /s/out.txt (grep it, do not re-run)] ..."
    ));
}

#[test]
fn duplicate_marker_matches_upstream_text() {
    let long = "é".repeat(400);
    let text = format!("{long}\nb\nc\nd\ne\nf");
    let preview = format!("{}…\nb\nc\nd\ne", "é".repeat(299));
    assert_eq!(
        duplicate_marker("Bash", &text, None),
        format!(
            "[chisle: output byte-identical to an earlier Bash result — 410 chars / 6 lines, unchanged. First lines:]\n{preview}"
        )
    );
    assert_eq!(
        duplicate_marker("Bash", &"q".repeat(1234), Some(Path::new("/s/p.txt"))),
        format!(
            "[chisle: output byte-identical to an earlier Bash result — 1,234 chars / 1 lines, unchanged. \
             Full output: /s/p.txt (grep it, do not re-run). First lines:]\n{}…",
            "q".repeat(299)
        )
    );
}
