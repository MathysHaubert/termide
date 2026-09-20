//! The visible conversation: items mirrored from agent events and their
//! rendered lines, cached per item so a streaming token re-renders one
//! message rather than the whole history.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use termide_agent_core::{ToolCall, ToolResultMessage};
use termide_core::ThemeColors;
use termide_panel_markdown::render_markdown;
use termide_richtext::Builder;

/// Lines of tool output shown when a call is expanded.
const EXPANDED_OUTPUT_LINES: usize = 60;
/// Tail lines of a collapsed tool's output shown under its command line.
const TOOL_PREVIEW_LINES: usize = 5;
/// Lines of a collapsed user message shown before it is cut off.
const USER_PREVIEW_LINES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    User {
        text: String,
    },
    Assistant {
        text: String,
        /// The reasoning text, shown in full only when the item is expanded.
        thinking: String,
        streaming: bool,
        error: Option<String>,
    },
    Tool {
        call: ToolCall,
        result: Option<ToolResultMessage>,
        /// Accumulated output while the tool is still running.
        live: Option<String>,
    },
    Notice {
        text: String,
        kind: NoticeKind,
    },
}

struct Cached {
    width: u16,
    is_light: bool,
    lines: Vec<Line<'static>>,
}

pub struct Transcript {
    items: Vec<Item>,
    /// Whether new blocks fold to a preview by default. Off shows everything
    /// expanded, the pre-fold behaviour, for users who want it.
    autofold: bool,
    /// Whether each item hides its detail (thinking / full output / the rest
    /// of a long message). Parallel to `items`. The assistant's answer always
    /// shows; collapsing only folds its thinking away.
    collapsed: Vec<bool>,
    cache: Vec<Option<Cached>>,
    /// Flattened lines of every item, rebuilt when any cache entry changed.
    flat: Vec<Line<'static>>,
    /// Item index per flattened line, for click-to-expand.
    line_item: Vec<usize>,
    flat_dirty: bool,
    /// A live footer appended after the last item while the agent works (the
    /// animated spinner + ticking phase); `None` when idle.
    live_footer: Option<Line<'static>>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            autofold: true,
            collapsed: Vec::new(),
            cache: Vec::new(),
            flat: Vec::new(),
            line_item: Vec::new(),
            flat_dirty: false,
            live_footer: None,
        }
    }
}

impl Transcript {
    /// Set whether new blocks fold by default (before any is pushed).
    pub fn set_autofold(&mut self, autofold: bool) {
        self.autofold = autofold;
    }

    #[must_use]
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn push(&mut self, item: Item) {
        // Everything folds by default except a notice (already one line);
        // an item's primary content still shows, only its detail is hidden.
        // With autofold off nothing folds.
        let collapsed = self.autofold && !matches!(item, Item::Notice { .. });
        self.items.push(item);
        self.collapsed.push(collapsed);
        self.cache.push(None);
        self.flat_dirty = true;
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.collapsed.clear();
        self.cache.clear();
        self.flat.clear();
        self.line_item.clear();
        self.flat_dirty = true;
    }

    fn invalidate(&mut self, index: usize) {
        if let Some(slot) = self.cache.get_mut(index) {
            *slot = None;
        }
        self.flat_dirty = true;
    }

    /// Set the live footer shown after the last block while the agent works
    /// (the animated spinner and the ticking phase). `None` removes it.
    pub fn set_live_footer(&mut self, footer: Option<Line<'static>>) {
        // The footer ticks every frame, so a rebuild of the flat line list is
        // needed whenever it is present or is being cleared.
        if footer.is_some() || self.live_footer.is_some() {
            self.flat_dirty = true;
        }
        self.live_footer = footer;
    }

    /// The streaming assistant message, if the last item is one.
    pub fn with_streaming_assistant(&mut self, f: impl FnOnce(&mut String, &mut String)) -> bool {
        let index = self.items.len().checked_sub(1);
        let Some(index) = index else {
            return false;
        };
        if let Item::Assistant {
            text,
            thinking,
            streaming: true,
            ..
        } = &mut self.items[index]
        {
            f(text, thinking);
            self.invalidate(index);
            return true;
        }
        false
    }

    /// Finish the streaming assistant item with the authoritative message.
    /// Complete the assistant message being streamed; a message that
    /// arrives whole, without a `MessageStart` (an external agent's failure,
    /// for one), is appended as it is.
    pub fn finish_assistant(&mut self, text: String, error: Option<String>) {
        if let Some(index) = self.items.len().checked_sub(1) {
            if let Item::Assistant {
                text: current,
                streaming: streaming @ true,
                error: current_error,
                ..
            } = &mut self.items[index]
            {
                *current = text;
                *streaming = false;
                *current_error = error;
                self.invalidate(index);
                return;
            }
        }
        self.push(Item::Assistant {
            text,
            thinking: String::new(),
            streaming: false,
            error,
        });
    }

    pub fn with_tool(&mut self, call_id: &str, f: impl FnOnce(&mut Item)) -> bool {
        let found = self
            .items
            .iter()
            .rposition(|item| matches!(item, Item::Tool { call, .. } if call.id == call_id));
        let Some(index) = found else {
            return false;
        };
        f(&mut self.items[index]);
        self.invalidate(index);
        true
    }

    /// Flip whether item `index` shows its detail. Notices have none.
    pub fn toggle_expanded(&mut self, index: usize) -> bool {
        if matches!(self.items.get(index), None | Some(Item::Notice { .. })) {
            return false;
        }
        let Some(slot) = self.collapsed.get_mut(index) else {
            return false;
        };
        *slot = !*slot;
        self.invalidate(index);
        true
    }

    /// Expand (`value` true) or collapse every foldable item at once.
    pub fn set_all_expanded(&mut self, value: bool) {
        for index in 0..self.items.len() {
            if matches!(self.items[index], Item::Notice { .. }) {
                continue;
            }
            if self.collapsed[index] == value {
                self.collapsed[index] = !value;
                self.invalidate(index);
            }
        }
    }

    /// Whether any foldable item is currently expanded.
    #[must_use]
    pub fn any_expanded(&self) -> bool {
        self.items
            .iter()
            .enumerate()
            .any(|(index, item)| !matches!(item, Item::Notice { .. }) && !self.collapsed[index])
    }

    /// Item index shown on flattened line `line`.
    #[must_use]
    pub fn item_at_line(&self, line: usize) -> Option<usize> {
        self.line_item.get(line).copied()
    }

    /// The first flattened line of item `index`, for scrolling it into view.
    #[must_use]
    pub fn first_line_of(&self, index: usize) -> Option<usize> {
        self.line_item.iter().position(|&i| i == index)
    }

    /// Lay out every item at `width` (re-rendering only what changed) and
    /// return the flattened lines.
    pub fn lines(&mut self, width: u16, colors: &ThemeColors, is_light: bool) -> &[Line<'static>] {
        let width = width.max(1);
        for index in 0..self.items.len() {
            let stale = match &self.cache[index] {
                Some(cached) => cached.width != width || cached.is_light != is_light,
                None => true,
            };
            if stale {
                let lines = render_item(
                    &self.items[index],
                    self.collapsed[index],
                    width,
                    colors,
                    is_light,
                );
                self.cache[index] = Some(Cached {
                    width,
                    is_light,
                    lines,
                });
                self.flat_dirty = true;
            }
        }
        if self.flat_dirty {
            self.flat.clear();
            self.line_item.clear();
            for (index, cached) in self.cache.iter().enumerate() {
                if let Some(cached) = cached {
                    self.flat.extend(cached.lines.iter().cloned());
                    self.line_item
                        .extend(std::iter::repeat_n(index, cached.lines.len()));
                }
            }
            if let Some(footer) = &self.live_footer {
                self.flat.push(footer.clone());
                self.line_item.push(self.items.len().saturating_sub(1));
            }
            self.flat_dirty = false;
        }
        &self.flat
    }

    #[must_use]
    pub fn line_count(&self) -> usize {
        self.flat.len()
    }
}

fn render_item(
    item: &Item,
    collapsed: bool,
    width: u16,
    colors: &ThemeColors,
    is_light: bool,
) -> Vec<Line<'static>> {
    let t = termide_i18n::t();
    let dim = Style::default().fg(colors.disabled);
    match item {
        Item::User { text } => {
            let trimmed = text.trim();
            let total = trimmed.lines().count();
            // A long paste folds to its first lines; the model still gets the
            // whole thing, this is only the transcript.
            let shown: String = if collapsed && total > USER_PREVIEW_LINES {
                trimmed
                    .lines()
                    .take(USER_PREVIEW_LINES)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                trimmed.to_string()
            };
            let mut builder = Builder::new(width, colors, is_light);
            builder.styled(
                "› ",
                Style::default()
                    .fg(colors.info)
                    .add_modifier(Modifier::BOLD),
            );
            builder.push_style(Style::default().add_modifier(Modifier::BOLD));
            builder.text(&shown);
            builder.pop_style();
            builder.end_paragraph();
            let mut lines = builder.finish().lines;
            if collapsed && total > USER_PREVIEW_LINES {
                lines.push(Line::styled(
                    format!("  {}", t.agent_more_lines(total - USER_PREVIEW_LINES)),
                    dim,
                ));
            }
            lines.push(Line::default());
            lines
        }
        Item::Assistant {
            text,
            thinking,
            streaming,
            error,
        } => {
            let mut lines = Vec::new();
            let think_chars = thinking.chars().count();
            if think_chars > 0 {
                if collapsed {
                    lines.push(Line::styled(
                        format!("▸ {}", t.agent_thought_chars(think_chars)),
                        dim,
                    ));
                } else {
                    lines.push(Line::styled(format!("▾ {}", t.agent_thinking()), dim));
                    for line in thinking.lines() {
                        lines.push(Line::styled(format!("  {line}"), dim));
                    }
                }
            }
            // The answer itself always shows in full.
            if text.trim().is_empty() {
                if *streaming {
                    lines.push(Line::styled("…", dim));
                }
            } else {
                lines.extend(render_markdown(text, width, colors, is_light).lines);
            }
            if let Some(error) = error {
                lines.push(Line::styled(
                    format!("✗ {error}"),
                    Style::default().fg(colors.error),
                ));
            }
            if !lines.is_empty() && !*streaming {
                lines.push(Line::default());
            }
            lines
        }
        Item::Tool { call, result, live } => {
            let mut lines = Vec::new();
            let glyph = if collapsed { "▸ " } else { "▾ " };
            let (status, status_style) = match (result, live) {
                (Some(result), _) if result.is_error => ("✗", Style::default().fg(colors.error)),
                (Some(_), _) => ("✓", Style::default().fg(colors.success)),
                (None, _) => ("…", Style::default().fg(colors.info)),
            };
            let summary = summarize_call(call, width.saturating_sub(12) as usize);
            lines.push(Line::from(vec![
                Span::styled(glyph, Style::default().fg(colors.disabled)),
                Span::styled(
                    call.name.clone(),
                    Style::default()
                        .fg(colors.info)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(summary, Style::default().fg(colors.fg)),
                Span::raw(" "),
                Span::styled(status, status_style),
            ]));
            let body = match (result, live) {
                (Some(result), _) => result.plain_text(),
                (None, Some(live)) => live.clone(),
                (None, None) => String::new(),
            };
            let all: Vec<&str> = body.lines().collect();
            if collapsed {
                // The command line plus the tail: the end is where the result
                // and errors are.
                let start = all.len().saturating_sub(TOOL_PREVIEW_LINES);
                if start > 0 {
                    lines.push(Line::styled(
                        format!("  {}", t.agent_more_lines_above(start)),
                        dim,
                    ));
                }
                for line in &all[start..] {
                    lines.push(Line::styled(format!("  {line}"), dim));
                }
            } else {
                for line in all.iter().take(EXPANDED_OUTPUT_LINES) {
                    lines.push(Line::styled(format!("  {line}"), dim));
                }
                if all.len() > EXPANDED_OUTPUT_LINES {
                    lines.push(Line::styled(
                        format!(
                            "  {}",
                            t.agent_more_lines(all.len() - EXPANDED_OUTPUT_LINES)
                        ),
                        dim,
                    ));
                }
            }
            lines
        }
        Item::Notice { text, kind } => {
            let color = match kind {
                NoticeKind::Info => colors.disabled,
                NoticeKind::Warn => colors.warning,
                NoticeKind::Error => colors.error,
            };
            vec![Line::styled(
                format!("· {text}"),
                Style::default().fg(color).add_modifier(Modifier::ITALIC),
            )]
        }
    }
}

/// One-line description of a call's arguments: the command for `bash`, the
/// path for file tools, compact JSON otherwise.
#[must_use]
pub fn summarize_call(call: &ToolCall, max_chars: usize) -> String {
    let text = match call.name.as_str() {
        "bash" => call.arguments["command"].as_str().unwrap_or("").to_string(),
        "read" | "edit" | "write" => call.arguments["path"].as_str().unwrap_or("").to_string(),
        _ => match &call.arguments {
            Value::Object(map) if map.is_empty() => String::new(),
            other => other.to_string(),
        },
    };
    let text = text.replace('\n', " ");
    let max_chars = max_chars.max(8);
    if text.chars().count() > max_chars {
        let cut: String = text.chars().take(max_chars - 1).collect();
        format!("{cut}…")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args,
        }
    }

    fn text_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_live_footer_appears_after_the_last_line_and_clears() {
        let colors = ThemeColors::default();
        let mut transcript = Transcript::default();
        transcript.push(Item::User { text: "hi".into() });
        let base = transcript.lines(40, &colors, false).len();

        transcript.set_live_footer(Some(Line::from("⠙ generating · 1.2s")));
        let with = transcript.lines(40, &colors, false);
        assert_eq!(with.len(), base + 1);
        assert_eq!(
            text_of(with).last().map(String::as_str),
            Some("⠙ generating · 1.2s")
        );

        transcript.set_live_footer(None);
        assert_eq!(transcript.lines(40, &colors, false).len(), base);
    }

    #[test]
    fn items_render_and_cache_per_width() {
        let colors = ThemeColors::default();
        let mut transcript = Transcript::default();
        transcript.push(Item::User {
            text: "Fix the bug".into(),
        });
        transcript.push(Item::Assistant {
            text: String::new(),
            thinking: String::new(),
            streaming: true,
            error: None,
        });
        assert!(transcript.with_streaming_assistant(|text, thinking| {
            text.push_str("Looking at `main.rs` now.");
            thinking.push_str("mulling this");
        }));
        transcript.push(Item::Tool {
            call: call("read", json!({ "path": "main.rs" })),
            result: None,
            live: None,
        });

        let lines = text_of(transcript.lines(40, &colors, false));
        assert_eq!(lines[0], "› Fix the bug");
        // Thinking folds to a one-line summary by default; the answer shows.
        assert!(lines
            .iter()
            .any(|l| l.contains("thought for 12 characters")));
        assert!(lines.iter().any(|l| l.contains("Looking at")));
        assert!(lines.iter().any(|l| l.starts_with("▸ read main.rs …")));
        assert_eq!(transcript.line_count(), lines.len());
        assert_eq!(transcript.item_at_line(0), Some(0));
        assert_eq!(transcript.item_at_line(lines.len() - 1), Some(2));

        // A multi-line result: collapsed shows the command line plus the tail,
        // expanded shows all of it.
        let body = (1..=8)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(transcript.with_tool("c1", |item| {
            if let Item::Tool { result, .. } = item {
                *result = Some(ToolResultMessage::text(&call("read", json!({})), &body));
            }
        }));
        let lines = text_of(transcript.lines(40, &colors, false));
        assert!(lines.iter().any(|l| l.starts_with("▸ read main.rs ✓")));
        assert!(lines.iter().any(|l| l.contains("… 3 more lines above")));
        assert!(lines.iter().any(|l| l.contains("line 8")));
        assert!(!lines
            .iter()
            .any(|l| l.contains("line 1") && !l.contains("line 1 ")));
        assert!(transcript.toggle_expanded(2));
        let lines = text_of(transcript.lines(40, &colors, false));
        assert!(lines.iter().any(|l| l.contains("line 1")));
        assert!(lines.iter().any(|l| l.contains("line 8")));
        assert!(transcript.any_expanded());
        // Expanding the assistant shows the full thinking text.
        assert!(transcript.toggle_expanded(1));
        let lines = text_of(transcript.lines(40, &colors, false));
        assert!(lines.iter().any(|l| l.contains("mulling this")));

        // A different width re-wraps the prose without losing items. Tool
        // summary lines are one-liners clipped at draw time, so only the
        // markdown-rendered ones are checked against the width.
        let narrow = text_of(transcript.lines(14, &colors, false));
        assert!(narrow
            .iter()
            .filter(|l| l.contains("Looking"))
            .all(|l| l.chars().count() <= 14));
        assert!(narrow.iter().any(|l| l.contains("Looking at")));
        assert_eq!(transcript.items().len(), 3);
    }

    #[test]
    fn autofold_off_leaves_every_block_expanded() {
        let mut transcript = Transcript::default();
        transcript.set_autofold(false);
        transcript.push(Item::User { text: "hi".into() });
        transcript.push(Item::Tool {
            call: call("bash", json!({ "command": "ls" })),
            result: Some(ToolResultMessage::text(&call("bash", json!({})), "a\nb")),
            live: None,
        });
        assert!(transcript.any_expanded());
    }

    #[test]
    fn errors_and_notices_are_visible() {
        let colors = ThemeColors::default();
        let mut transcript = Transcript::default();
        transcript.push(Item::Assistant {
            text: String::new(),
            thinking: String::new(),
            streaming: true,
            error: None,
        });
        transcript.finish_assistant(String::new(), Some("HTTP 500".into()));
        transcript.push(Item::Notice {
            text: "compacted 1200 tokens".into(),
            kind: NoticeKind::Info,
        });
        transcript.push(Item::Tool {
            call: call("bash", json!({ "command": "exit 1" })),
            result: Some(ToolResultMessage::error(
                &call("bash", json!({})),
                "[exit code 1]",
            )),
            live: None,
        });
        let lines = text_of(transcript.lines(60, &colors, false));
        assert!(lines.iter().any(|l| l.contains("✗ HTTP 500")));
        assert!(lines.iter().any(|l| l.contains("· compacted 1200 tokens")));
        assert!(lines.iter().any(|l| l.contains("▸ bash exit 1 ✗")));
    }

    #[test]
    fn call_summaries_truncate_and_flatten() {
        let long = "x".repeat(50);
        let summary = summarize_call(
            &call("bash", json!({ "command": format!("echo {long}\nls") })),
            20,
        );
        assert_eq!(summary.chars().count(), 20);
        assert!(summary.ends_with('…'));
        assert!(!summary.contains('\n'));
        assert_eq!(
            summarize_call(&call("edit", json!({ "path": "a.rs" })), 40),
            "a.rs"
        );
        assert_eq!(summarize_call(&call("mcp", json!({})), 40), "");
        assert_eq!(
            summarize_call(&call("mcp", json!({ "q": 1 })), 40),
            "{\"q\":1}"
        );
    }
}
