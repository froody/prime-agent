//! Transcript search: find, navigate, and highlight matches in the transcript.
//!
//! Activated via `tui.transcript.search` (default `ctrl+f`). Mounts a search
//! prompt in the editor dock, computes case-insensitive substring matches across
//! all transcript lines, highlights matches on screen, and scrolls the viewport
//! to the focused match.

use crate::keybindings::KeybindingsManager;
use crate::search_input::SearchInput;
use crate::selection::row_text;
use crate::theme::{Theme, ThemeColor};
use crate::width::str_width;
use crate::{Line, Span};
use ratatui::style::{Modifier, Style};

/// A match location within the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchLocation {
    /// Absolute line index in the rendered transcript (`0..total_lines`).
    pub line_index: usize,
    /// Visible column start.
    pub col_start: usize,
    /// Visible column end.
    pub col_end: usize,
}

/// Active transcript search state.
#[derive(Debug)]
pub struct TranscriptSearch {
    pub(crate) input: SearchInput,
    pub(crate) initial_scroll_top: usize,
    pub(crate) matches: Vec<MatchLocation>,
    pub(crate) current_match: Option<usize>,
}

impl TranscriptSearch {
    /// Create a new transcript search session recording the initial scroll offset.
    #[must_use]
    pub fn new(initial_scroll_top: usize) -> Self {
        TranscriptSearch {
            input: SearchInput::new(),
            initial_scroll_top,
            matches: Vec::new(),
            current_match: None,
        }
    }

    /// The current search query.
    #[must_use]
    pub fn query(&self) -> &str {
        self.input.value()
    }

    /// The initial scroll offset before search started.
    #[must_use]
    pub fn initial_scroll_top(&self) -> usize {
        self.initial_scroll_top
    }

    /// Dispatch a key to the underlying search input.
    pub fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) {
        self.input.handle_key(key, kb);
    }

    /// Paste text into the search input.
    pub fn paste(&mut self, text: &str) {
        self.input.paste(text);
    }

    /// Recompute matches across the rendered transcript lines.
    pub fn recompute_matches(&mut self, lines: &[Line], current_scroll: usize) {
        self.matches.clear();
        let query = self.input.value().trim();
        if query.is_empty() {
            self.current_match = None;
            return;
        }

        let query_lower = query.to_lowercase();

        for (line_idx, line) in lines.iter().enumerate() {
            let text = row_text(line);
            let text_lower = text.to_lowercase();
            let mut search_from = 0;
            while let Some(byte_rel) = text_lower[search_from..].find(&query_lower) {
                let byte_start = search_from + byte_rel;
                let byte_end = byte_start + query_lower.len();
                let col_start = str_width(&text[..byte_start]);
                let col_end = col_start + str_width(&text[byte_start..byte_end]);
                self.matches.push(MatchLocation {
                    line_index: line_idx,
                    col_start,
                    col_end,
                });
                let next_char = text[byte_start..].chars().next().map_or(1, char::len_utf8);
                search_from = byte_start + next_char;
                if search_from >= text_lower.len() {
                    break;
                }
            }
        }

        if self.matches.is_empty() {
            self.current_match = None;
        } else {
            // Pick match closest to current scroll
            let closest = self
                .matches
                .iter()
                .enumerate()
                .min_by_key(|(_, m)| (m.line_index as isize - current_scroll as isize).abs())
                .map_or(0, |(idx, _)| idx);
            self.current_match = Some(closest);
        }
    }

    /// Advance to the next match (wrapping).
    pub fn next_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.current_match = Some(
            self.current_match
                .map_or(0, |curr| (curr + 1) % self.matches.len()),
        );
    }

    /// Move to the previous match (wrapping).
    pub fn prev_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.current_match = Some(self.current_match.map_or_else(
            || self.matches.len().saturating_sub(1),
            |curr| {
                if curr == 0 {
                    self.matches.len().saturating_sub(1)
                } else {
                    curr - 1
                }
            },
        ));
    }

    /// The column offset of the hardware cursor inside the search row.
    #[must_use]
    pub fn cursor_col(&self) -> usize {
        // " Find: " is 7 visible columns
        7 + str_width(&self.input.value()[..self.input.cursor().min(self.input.value().len())])
    }

    /// Render the 4-line search dock.
    #[must_use]
    pub fn render(&self, theme: &Theme, width: usize, _kb: &KeybindingsManager) -> Vec<Line> {
        let value = self.input.value();
        let cursor = self.input.cursor();

        let status = if value.trim().is_empty() {
            String::new()
        } else if self.matches.is_empty() {
            " [no matches]".to_string()
        } else {
            let curr = self.current_match.map_or(0, |i| i + 1);
            format!(" [{curr}/{}]", self.matches.len())
        };

        let top_rule = crate::menu_panel::rule_row(theme, width);

        let mut field: Line = vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Accent, "Find: ".to_string()),
        ];

        let chars: Vec<char> = value.chars().collect();
        let (before, at_and_after) = chars.split_at(cursor.min(chars.len()));
        if !before.is_empty() {
            field.push(Span::raw(before.iter().collect::<String>()));
        }
        if let Some((at, after)) = at_and_after.split_first() {
            field.push(Span::styled(
                at.to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
            if !after.is_empty() {
                field.push(Span::raw(after.iter().collect::<String>()));
            }
        } else {
            field.push(Span::styled(
                " ".to_string(),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
        }

        if !status.is_empty() {
            let color = if self.matches.is_empty() {
                ThemeColor::Error
            } else {
                ThemeColor::Dim
            };
            field.push(theme.fg_span(color, status));
        }

        let bottom_rule = crate::menu_panel::rule_row(theme, width);
        let hint = crate::menu_panel::hint_row(
            theme,
            width,
            "Enter/Down next \u{00b7} Shift+Enter/Up prev \u{00b7} Esc close",
        );

        vec![top_rule, field, bottom_rule, hint]
    }
}

/// Highlight matching search spans within a rendered line.
#[must_use]
pub(crate) fn highlight_search_match(
    line: &Line,
    from: usize,
    to: usize,
    is_current: bool,
) -> Line {
    let width = crate::width::line_width(line);
    let from = from.min(width);
    let to = to.min(width);
    if to <= from {
        return line.clone();
    }
    let mut out = crate::width::slice_line_by_column(line, 0, from);
    let mut matched = crate::width::slice_line_by_column(line, from, to - from);
    for span in &mut matched {
        if is_current {
            span.style = span.style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        } else {
            span.style = span.style.add_modifier(Modifier::REVERSED);
        }
    }
    out.extend(matched);
    out.extend(crate::width::slice_line_by_column(line, to, width - to));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorMode, Theme};

    fn test_theme() -> Theme {
        Theme::builtin("prime", ColorMode::TrueColor)
    }

    #[test]
    fn search_finds_case_insensitive_matches() {
        let lines: Vec<Line> = vec![
            vec![Span::raw("First line of text")],
            vec![Span::raw("Second Line with TEXT")],
            vec![Span::raw("Nothing here")],
        ];
        let mut search = TranscriptSearch::new(0);
        search.input.prefill("text");
        search.recompute_matches(&lines, 0);

        assert_eq!(search.matches.len(), 2);
        assert_eq!(search.matches[0].line_index, 0);
        assert_eq!(search.matches[0].col_start, 14);
        assert_eq!(search.matches[0].col_end, 18);

        assert_eq!(search.matches[1].line_index, 1);
        assert_eq!(search.matches[1].col_start, 17);
        assert_eq!(search.matches[1].col_end, 21);

        assert_eq!(search.current_match, Some(0));

        search.next_match();
        assert_eq!(search.current_match, Some(1));

        search.next_match();
        assert_eq!(search.current_match, Some(0));

        search.prev_match();
        assert_eq!(search.current_match, Some(1));
    }

    #[test]
    fn highlight_match_applies_reversed() {
        let line: Line = vec![Span::raw("Hello World")];
        let highlighted = highlight_search_match(&line, 6, 11, false);
        assert_eq!(crate::width::line_width(&highlighted), 11);
        let styled = highlighted
            .iter()
            .find(|s| s.style.add_modifier.contains(Modifier::REVERSED));
        assert!(styled.is_some());
        assert_eq!(styled.unwrap().content, "World");
    }

    #[test]
    fn render_search_dock_shows_find_prompt_and_status() {
        let theme = test_theme();
        let kb = KeybindingsManager::new();
        let mut search = TranscriptSearch::new(10);
        search.input.prefill("test");
        search.matches.push(MatchLocation {
            line_index: 5,
            col_start: 0,
            col_end: 4,
        });
        search.current_match = Some(0);

        let rows = search.render(&theme, 80, &kb);
        assert_eq!(rows.len(), 4);
        let field_text = rows[1]
            .iter()
            .map(|s| s.content.as_str())
            .collect::<String>();
        assert!(field_text.contains("Find:"));
        assert!(field_text.contains("test"));
        assert!(field_text.contains("[1/1]"));
    }

    #[test]
    fn agent_view_search_integration() {
        let mut view = crate::view::AgentView::new(test_theme());
        view.chat.push(crate::chat::ChatEntry::User {
            text: "Searching for magic keyword here".to_string(),
        });
        view.chat.push(crate::chat::ChatEntry::Status {
            text: "Another line without it".to_string(),
            kind: crate::chat::StatusKind::Info,
        });
        view.chat.push(crate::chat::ChatEntry::User {
            text: "Second magic keyword occurrence".to_string(),
        });

        view.open_transcript_search();
        assert!(view.transcript_search.is_some());

        if let Some(search) = view.transcript_search.as_mut() {
            search.input.prefill("magic keyword");
        }
        view.update_transcript_search();

        let search = view.transcript_search.as_ref().unwrap();
        assert_eq!(search.matches.len(), 2);

        // Frame rendering with search active renders search dock and highlights
        let frame = view.render_frame(80, 24);
        assert!(!frame.is_empty());

        view.close_transcript_search(false);
        assert!(view.transcript_search.is_none());
    }
}
