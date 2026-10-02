//! Pure text logic of Chisle's tool-output compressor (upstream
//! `hooks/chisle-compress-output.js`, MIT, Jay Pokale): a lossless scrub tier,
//! head/tail elision with error-line salvage, and the duplicate-output marker.
//!
//! Upstream measures lengths in UTF-16 units; this port counts chars and only
//! ever cuts on char boundaries.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use fancy_regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) max_chars: usize,
    pub(crate) head_lines: usize,
    pub(crate) tail_lines: usize,
}

impl Limits {
    pub(crate) const DEFAULT: Limits = Limits {
        max_chars: 8000,
        head_lines: 60,
        tail_lines: 40,
    };
}

/// Outputs shorter than this are never replaced by a duplicate marker.
pub(crate) const DEDUP_MIN: usize = 2048;
/// Below this, scrubbing is not worth it.
const SCRUB_MIN: usize = 1024;
/// Identical consecutive lines before collapsing.
const REPEAT_MIN: usize = 4;
/// Emit transformed output only if it saves at least this many chars.
const MIN_WIN: usize = 64;
/// Error lines rescued from the elided middle.
const MAX_SALVAGED: usize = 12;
/// Per-line char cap on salvaged and preview lines.
const MAX_SALVAGE_LINE: usize = 300;

static ANSI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
        .expect("ansi regex compiles")
});

// JS `[ \t]+$` under the `m` flag: `$` also matches before `\r`, U+2028 and U+2029.
static TRAILING_WS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[ \t]+(?=[\n\r\u{2028}\u{2029}]|\z)").expect("trailing-whitespace regex compiles")
});

static BLANK_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("blank-run regex compiles"));

/// JS `\b` without the `u` flag: a boundary between ASCII word and non-word chars.
const ASCII_BOUNDARY: &str =
    r"(?:(?<=[A-Za-z0-9_])(?![A-Za-z0-9_])|(?<![A-Za-z0-9_])(?=[A-Za-z0-9_]))";

static SALVAGE: LazyLock<Regex> = LazyLock::new(|| {
    let b = ASCII_BOUNDARY;
    Regex::new(&format!(
        r"(?i){b}(error|err!|fail(ed|ure|ing)?|exception|traceback|panic|fatal|denied|refused|timed?[ _-]?out|assert(ion)?|segfault|npe|undefined reference|cannot find|not found|warning){b}"
    ))
    .expect("salvage regex compiles")
});

static ERROR_LIKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)error-like").expect("error-like regex compiles"));

/// Lossless tier: strip ANSI/OSC escapes and trailing whitespace, squeeze blank-line
/// runs to one blank line, and collapse runs of identical non-empty lines.
pub(crate) fn scrub(text: &str) -> String {
    let t = ANSI.replace_all(text, "");
    let t = TRAILING_WS.replace_all(&t, "");
    let t = BLANK_RUN.replace_all(&t, "\n\n");
    let lines: Vec<&str> = t.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let n = lines[i..].iter().take_while(|l| **l == line).count();
        if n >= REPEAT_MIN && !line.trim().is_empty() {
            out.push(line.to_owned());
            out.push(format!("... [chisle: line repeated {n}×] ..."));
        } else {
            out.extend(std::iter::repeat_n(line.to_owned(), n));
        }
        i += n;
    }
    out.join("\n")
}

/// upstream `compress()`: `None` when it would not shrink.
pub(crate) fn elide(text: &str, limits: Limits, spill_path: Option<&Path>) -> Option<String> {
    let Limits {
        max_chars,
        head_lines,
        tail_lines,
    } = limits;
    let lines: Vec<&str> = text.split('\n').collect();
    let recover = recovery_note(spill_path);
    let text_chars = text.chars().count();

    if lines.len() <= head_lines + tail_lines {
        // Big in chars, few lines (one giant line): hard char cut.
        let keep = max_chars / 2;
        let elided = text_chars.checked_sub(2 * keep).filter(|n| *n > 0)?;
        let head = char_slice(text, 0, keep);
        let tail = char_slice(text, text_chars - keep, text_chars);
        let elided = en_us(elided);
        return Some(format!(
            "{head}\n... [chisle: elided {elided} chars from the middle.{recover}] ...\n{tail}"
        ));
    }

    let head = &lines[..head_lines];
    let tail = &lines[lines.len() - tail_lines..];
    let middle = &lines[head_lines..lines.len() - tail_lines];
    let salvaged: Vec<String> = middle
        .iter()
        .filter_map(|line| {
            // The elision marker itself says "error-like"; re-eliding must not salvage it.
            // The replacement keeps the char count, so probe char offsets index `line`.
            let probe = ERROR_LIKE.replace_all(line, "diagnostic");
            let found = SALVAGE.find(&probe).ok().flatten()?;
            let len = line.chars().count();
            if len <= MAX_SALVAGE_LINE {
                return Some((*line).to_owned());
            }
            // Window the long line around the match.
            let content_limit = MAX_SALVAGE_LINE - 2;
            let index = probe[..found.start()].chars().count();
            let lead = content_limit.saturating_sub(found.as_str().chars().count()) / 2;
            let start = index.saturating_sub(lead).min(len - content_limit);
            let end = start + content_limit;
            let open = if start > 0 { "…" } else { "" };
            let close = if end < len { "…" } else { "" };
            Some(format!("{open}{}{close}", char_slice(line, start, end)))
        })
        .take(MAX_SALVAGED)
        .collect();
    let salvage_note = match salvaged.len() {
        0 => String::new(),
        n => format!(", and {n} error-like line(s) below"),
    };
    let elided = en_us(middle.len());
    let marker = format!(
        "... [chisle: elided {elided} lines, kept first {head_lines}, last {tail_lines}{salvage_note}.{recover}] ..."
    );
    let out = head
        .iter()
        .copied()
        .chain(std::iter::once(marker.as_str()))
        .chain(salvaged.iter().map(String::as_str))
        .chain(tail.iter().copied())
        .collect::<Vec<_>>()
        .join("\n");
    (out.chars().count() < text_chars).then_some(out)
}

/// upstream `transform()`: scrub (when `scrub_enabled`) then elide when over `max_chars`; `spill` is called
/// with the scrubbed text only when elision is needed and returns the recovery path. None unless the
/// result saves >= `MIN_WIN` chars.
pub(crate) fn transform(
    text: &str,
    limits: Limits,
    scrub_enabled: bool,
    spill: impl FnOnce(&str) -> Option<PathBuf>,
) -> Option<String> {
    let text_chars = text.chars().count();
    if text_chars <= SCRUB_MIN {
        return None;
    }
    let mut t = if scrub_enabled {
        scrub(text)
    } else {
        text.to_owned()
    };
    if t.chars().count() > limits.max_chars {
        let spill_path = spill(&t);
        if let Some(elided) = elide(&t, limits, spill_path.as_deref()) {
            t = elided;
        }
    }
    (t.chars().count() + MIN_WIN <= text_chars).then_some(t)
}

/// upstream `duplicateMarker(toolName, text, 'earlier')`, plus, when `spill_path` is `Some`, the recovery
/// note inside the bracket: "...unchanged. Full output: P (grep it, do not re-run). First lines:]".
/// Without a path, byte-identical to upstream.
pub(crate) fn duplicate_marker(tool_name: &str, text: &str, spill_path: Option<&Path>) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let preview = lines
        .iter()
        .take(5)
        .map(|line| {
            if line.chars().count() > MAX_SALVAGE_LINE {
                format!("{}…", char_slice(line, 0, MAX_SALVAGE_LINE - 1))
            } else {
                (*line).to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let chars = en_us(text.chars().count());
    let line_count = lines.len();
    let recover = recovery_note(spill_path);
    let recover_end = if recover.is_empty() { "" } else { "." };
    format!(
        "[chisle: output byte-identical to an earlier {tool_name} result — {chars} chars / {line_count} lines, unchanged.{recover}{recover_end} First lines:]\n{preview}"
    )
}

fn recovery_note(spill_path: Option<&Path>) -> String {
    match spill_path {
        Some(path) => format!(" Full output: {} (grep it, do not re-run)", path.display()),
        None => String::new(),
    }
}

/// Chars `[start, end)` of `text`, clamped to its length.
fn char_slice(text: &str, start: usize, end: usize) -> &str {
    let byte_at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    let start = byte_at(start);
    let end = byte_at(end).max(start);
    &text[start..end]
}

/// `Number.prototype.toLocaleString('en-US')` for integers: thousands commas.
fn en_us(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests;
