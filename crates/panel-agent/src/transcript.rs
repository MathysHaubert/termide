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
/// A block whose foldable content is this many lines or fewer is shown in full,
/// without a fold marker or fold logic — there is nothing to save by hiding it.
const FOLD_THRESHOLD: usize = 5;
/// A collapsed content block shows this many lines from the top, then the
/// "… N more lines" marker, then [`FOLD_TAIL_LINES`] from the bottom — the
/// ellipsis sits between the first line and the last few, as a tool call shows.
const FOLD_HEAD_LINES: usize = 1;
/// Lines shown from the bottom of a collapsed content block; with
/// [`FOLD_HEAD_LINES`] they sum to [`FOLD_THRESHOLD`], so a foldable block
/// (more than that many lines) always hides at least one.
const FOLD_TAIL_LINES: usize = FOLD_THRESHOLD - FOLD_HEAD_LINES;
/// Tail lines of a collapsed tool's output shown under its command line.
const TOOL_PREVIEW_LINES: usize = FOLD_THRESHOLD;
/// Lines of a collapsed user message shown before it is cut off.
const USER_PREVIEW_LINES: usize = FOLD_THRESHOLD;

/// The collapsed head/tail split every foldable content block shares: the
/// index where the shown tail begins, and how many lines are hidden between
/// the head and that tail. Only meaningful when the block is foldable
/// (`total > FOLD_THRESHOLD`), where `hidden >= 1`.
fn fold_split(total: usize) -> (usize, usize) {
    let tail_start = total.saturating_sub(FOLD_TAIL_LINES);
    let hidden = tail_start.saturating_sub(FOLD_HEAD_LINES);
    (hidden, tail_start)
}

/// The body text a fold would hide for `item` (tool output, thinking, or a long
/// user paste). The answer of an assistant turn is always shown, so it does not
/// count here.
fn foldable_lines(item: &Item) -> usize {
    match item {
        Item::User { text, .. } => text.trim().lines().count(),
        Item::System { text } => text.trim().lines().count(),
        Item::Thinking { text, .. } => text.trim().lines().count(),
        // The answer is always shown in full, so it never folds.
        Item::Assistant { .. } => 0,
        Item::Tool { result, live, .. } => match (result, live) {
            (Some(result), _) => result.plain_text().lines().count(),
            (None, Some(live)) => live.lines().count(),
            (None, None) => 0,
        },
        Item::Notice { .. } => 0,
    }
}

/// Whether `item` has enough content to be worth folding. Small blocks show in
/// full and ignore the collapse flag, so they carry no fold marker.
fn is_foldable(item: &Item) -> bool {
    foldable_lines(item) > FOLD_THRESHOLD
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warn,
    Error,
}

/// The prefill/generation cost of a finished model turn, split into the two
/// phases for the `⏫` (prefill) and `✍️` (generation) meta lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub prefill_ms: u32,
    pub gen_ms: u32,
    /// Input tokens (the prompt processed during prefill).
    pub input: u64,
    /// Output tokens the model generated.
    pub output: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    User {
        text: String,
        /// Local wall-clock time the message was sent, e.g. `21:03:14`.
        at: String,
    },
    /// The system prompt in effect, shown (folded by default) at the start of a
    /// session and whenever it changes before a new message. Marked with `#`.
    System {
        text: String,
    },
    /// The model's reasoning, its own block above the answer, marked with `@`.
    Thinking {
        text: String,
        streaming: bool,
        /// Local wall-clock time the reasoning finished (its meta's "when").
        at: String,
        /// The turn's cost; the reasoning block shows the prefill part (`⏫`).
        cost: Option<Cost>,
    },
    Assistant {
        text: String,
        streaming: bool,
        error: Option<String>,
        /// Local wall-clock time the turn finished (the meta line's "when").
        at: String,
        /// The turn's prefill/generation cost, once it has finished.
        cost: Option<Cost>,
    },
    Tool {
        call: ToolCall,
        result: Option<ToolResultMessage>,
        /// Accumulated output while the tool is still running.
        live: Option<String>,
        /// Local wall-clock time the call finished.
        at: String,
        /// How long the call took, in ms, once it has finished (`🕒`).
        duration_ms: Option<u32>,
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
    /// Live footer lines appended after the last item while the agent works
    /// (the ticking generation and clock meta with the spinner); empty when
    /// idle.
    live_footer: Vec<Line<'static>>,
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
            live_footer: Vec::new(),
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
    /// (the generation and clock meta with the spinner). An empty vector
    /// removes it.
    pub fn set_live_footer(&mut self, footer: Vec<Line<'static>>) {
        // The footer ticks every frame, so a rebuild of the flat line list is
        // needed whenever it is present or is being cleared.
        if !footer.is_empty() || !self.live_footer.is_empty() {
            self.flat_dirty = true;
        }
        self.live_footer = footer;
    }

    /// Append reasoning to the streaming thinking block, starting one if the
    /// last item is not already a streaming thinking block.
    pub fn stream_thinking(&mut self, delta: &str) {
        if let Some(index) = self.items.len().checked_sub(1) {
            if let Item::Thinking {
                text,
                streaming: true,
                ..
            } = &mut self.items[index]
            {
                text.push_str(delta);
                self.invalidate(index);
                return;
            }
        }
        self.push(Item::Thinking {
            text: delta.to_string(),
            streaming: true,
            at: String::new(),
            cost: None,
        });
    }

    /// Append answer text to the streaming assistant block, starting one if the
    /// last item is not already a streaming assistant block.
    pub fn stream_answer(&mut self, delta: &str) {
        if let Some(index) = self.items.len().checked_sub(1) {
            if let Item::Assistant {
                text,
                streaming: true,
                ..
            } = &mut self.items[index]
            {
                text.push_str(delta);
                self.invalidate(index);
                return;
            }
        }
        self.push(Item::Assistant {
            text: delta.to_string(),
            streaming: true,
            error: None,
            at: String::new(),
            cost: None,
        });
    }

    /// Whether the last block is still streaming, so the live footer is that
    /// block's own meta and should sit under it without a dividing rule. When
    /// it is not (a finished tool, say), the footer is a fresh in-progress
    /// section and wants a rule above it.
    #[must_use]
    pub fn tail_is_streaming(&self) -> bool {
        matches!(
            self.items.last(),
            Some(
                Item::Thinking {
                    streaming: true,
                    ..
                } | Item::Assistant {
                    streaming: true,
                    ..
                }
            )
        )
    }

    fn last_streaming(&self, is_thinking: bool) -> Option<usize> {
        self.items.iter().rposition(|item| match item {
            Item::Thinking { streaming, .. } => is_thinking && *streaming,
            Item::Assistant { streaming, .. } => !is_thinking && *streaming,
            _ => false,
        })
    }

    /// Close the streaming reasoning block, if one is open, giving it its
    /// completion time and cost. Returns whether a reasoning block was closed,
    /// so the caller can decide where the prefill indicator belongs.
    pub fn finish_thinking(&mut self, at: &str, cost: Option<Cost>) -> bool {
        let Some(index) = self.last_streaming(true) else {
            return false;
        };
        if let Item::Thinking {
            streaming,
            at: current_at,
            cost: current_cost,
            ..
        } = &mut self.items[index]
        {
            *streaming = false;
            *current_at = at.to_string();
            *current_cost = cost;
        }
        self.invalidate(index);
        true
    }

    /// Finish the turn: complete the streaming answer with the authoritative
    /// message. When `drop_if_empty` is set (a reasoning block above already
    /// carries the turn's meta) and there is no answer text and no error, no
    /// answer block is created — the reasoning block stands for the turn. A whole
    /// message that arrives without a `MessageStart` (an external agent's
    /// failure, for one) is appended as it is. Close the reasoning block first
    /// with [`Transcript::finish_thinking`].
    pub fn finish_assistant(
        &mut self,
        text: String,
        error: Option<String>,
        cost: Option<Cost>,
        at: String,
        drop_if_empty: bool,
    ) {
        if let Some(index) = self.last_streaming(false) {
            // A streaming answer that turns out empty (only whitespace deltas,
            // as before a tool call) leaves no block behind.
            if text.trim().is_empty() && error.is_none() {
                self.items.remove(index);
                self.collapsed.remove(index);
                self.cache.remove(index);
                self.flat_dirty = true;
                return;
            }
            if let Item::Assistant {
                text: current,
                streaming,
                error: current_error,
                cost: current_cost,
                at: current_at,
                ..
            } = &mut self.items[index]
            {
                *current = text;
                *streaming = false;
                *current_error = error;
                *current_cost = cost;
                *current_at = at;
                self.invalidate(index);
                return;
            }
        }
        if drop_if_empty && text.trim().is_empty() && error.is_none() {
            return;
        }
        self.push(Item::Assistant {
            text,
            streaming: false,
            error,
            at,
            cost,
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

    /// Flip whether item `index` shows its detail. A small block has no detail
    /// to hide, so it does not fold.
    pub fn toggle_expanded(&mut self, index: usize) -> bool {
        if !self.items.get(index).is_some_and(is_foldable) {
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
            .any(|(index, item)| is_foldable(item) && !self.collapsed[index])
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
            for line in &self.live_footer {
                self.flat.push(line.clone());
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

/// Display width of `s`, honouring wide and emoji cells.
fn width_of(s: &str) -> usize {
    termide_ui::str_display_width(s)
}

/// A right-aligned meta line: `spans` pushed to the right edge (one column
/// short of the scrollbar gutter), the rest padded with spaces.
pub(crate) fn right_meta(width: u16, spans: Vec<Span<'static>>) -> Line<'static> {
    let content: usize = spans.iter().map(|s| width_of(&s.content)).sum();
    let pad = (width as usize).saturating_sub(content + 1);
    let mut out = Vec::with_capacity(spans.len() + 1);
    out.push(Span::raw(" ".repeat(pad)));
    out.extend(spans);
    Line::from(out)
}

/// A dim dashed rule drawn above a block to set it apart from the one before.
pub(crate) fn separator(width: u16, colors: &ThemeColors) -> Line<'static> {
    Line::styled(
        "╌".repeat(width.saturating_sub(1) as usize),
        Style::default().fg(colors.disabled),
    )
}

/// The prefill (`⏫`) and generation (`✍️`) indicator lines: each phase's
/// duration, tokens and average speed. Shown on whichever block owns the turn's
/// cost — the reasoning block, or the answer when a turn does not reason.
fn cost_lines(width: u16, cost: &Cost, colors: &ThemeColors) -> Vec<Line<'static>> {
    let dim = Style::default().fg(colors.disabled);
    vec![
        right_meta(
            width,
            vec![Span::styled(
                format!(
                    "⏫ {} (↑{}, {})",
                    fmt_dur(cost.prefill_ms),
                    crate::format_tokens(cost.input),
                    fmt_speed(cost.input, cost.prefill_ms)
                ),
                dim,
            )],
        ),
        right_meta(
            width,
            vec![Span::styled(
                format!(
                    "✍\u{fe0f} {} (↓{}, {})",
                    fmt_dur(cost.gen_ms),
                    crate::format_tokens(cost.output),
                    fmt_speed(cost.output, cost.gen_ms)
                ),
                dim,
            )],
        ),
    ]
}

/// The wall-clock time + status meta line, shown on the user's message and the
/// final answer (the two `›` message blocks).
fn time_meta(
    width: u16,
    at: &str,
    ok: bool,
    color: ratatui::style::Color,
    colors: &ThemeColors,
) -> Line<'static> {
    right_meta(
        width,
        vec![
            Span::styled(format!("{at} "), Style::default().fg(color)),
            status_span(ok, colors),
        ],
    )
}

/// A phase duration in whole seconds with localized units: `2s` under a
/// minute, `1m13s` above. Tenths add no useful information here.
pub(crate) fn fmt_dur(ms: u32) -> String {
    let t = termide_i18n::t();
    let total = (ms as f32 / 1000.0).round() as u32;
    if total < 60 {
        format!("{total}{}", t.agent_unit_secs())
    } else {
        format!(
            "{}{}{}{}",
            total / 60,
            t.agent_unit_mins(),
            total % 60,
            t.agent_unit_secs()
        )
    }
}

/// Average token throughput for a phase, localized (e.g. `88 tok/s`).
pub(crate) fn fmt_speed(tokens: u64, ms: u32) -> String {
    let t = termide_i18n::t();
    let secs = (ms as f32 / 1000.0).max(0.001);
    let rate = (tokens as f32 / secs).round() as u64;
    format!(
        "{} {}",
        crate::format_tokens(rate),
        t.agent_unit_tok_per_sec()
    )
}

/// Wrap plain text to `width` with a single style, using the rich-text builder
/// so long lines fold instead of being clipped at draw time.
fn wrap_plain(
    text: &str,
    width: u16,
    style: Style,
    colors: &ThemeColors,
    is_light: bool,
) -> Vec<Line<'static>> {
    let mut builder = Builder::new(width, colors, is_light);
    builder.push_style(style);
    builder.text(text);
    builder.pop_style();
    builder.end_paragraph();
    builder.finish().lines
}

/// The `✓`/`✗` status glyph for a finished block.
fn status_span(ok: bool, colors: &ThemeColors) -> Span<'static> {
    if ok {
        Span::styled("✓", Style::default().fg(colors.success))
    } else {
        Span::styled("✗", Style::default().fg(colors.error))
    }
}

/// Fill each line's full width with `bg` (used for the user message's faint
/// background), so the tint covers the whole row, not just the text.
fn fill_bg(lines: &mut [Line<'static>], width: u16, bg: ratatui::style::Color) {
    for line in lines.iter_mut() {
        let w: usize = line.spans.iter().map(|s| width_of(&s.content)).sum();
        for span in &mut line.spans {
            span.style = span.style.bg(bg);
        }
        let pad = (width as usize).saturating_sub(w);
        if pad > 0 {
            line.spans
                .push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
        }
    }
}

/// The first line of a tool call: `$ <command>` for a shell, a localized
/// action plus its path for read/write/edit, else the tool name and a summary.
fn tool_headline(call: &ToolCall, width: u16, colors: &ThemeColors) -> Vec<Span<'static>> {
    let t = termide_i18n::t();
    let dim = Style::default().fg(colors.disabled);
    let fg = Style::default().fg(colors.fg);
    let arg = |key: &str| {
        call.arguments
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let accent = Style::default().fg(colors.info);
    match call.name.as_str() {
        "bash" | "shell" => vec![
            Span::styled("$ ", accent),
            Span::styled(arg("command"), dim),
        ],
        "read" => vec![
            Span::styled(format!("{} ", t.agent_tool_read()), fg),
            Span::styled(arg("path"), fg),
        ],
        "write" => vec![
            Span::styled(format!("{} ", t.agent_tool_write()), fg),
            Span::styled(arg("path"), fg),
        ],
        "edit" => vec![
            Span::styled(format!("{} ", t.agent_tool_edit()), fg),
            Span::styled(arg("path"), fg),
        ],
        _ => vec![
            Span::styled(
                call.name.clone(),
                Style::default()
                    .fg(colors.info)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(summarize_call(call, width.saturating_sub(4) as usize), fg),
        ],
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
        Item::User { text, at } => {
            let trimmed = text.trim();
            let all: Vec<&str> = trimmed.lines().collect();
            let mark = Style::default()
                .fg(colors.info)
                .add_modifier(Modifier::BOLD);
            let bold = Style::default().add_modifier(Modifier::BOLD);
            // The plate is a blank line, the text, the time, and a blank line —
            // all on the faint background, with an even margin around them.
            let mut plate = vec![Line::default()];
            if collapsed && all.len() > USER_PREVIEW_LINES {
                // A long paste folds to its first line, a "… N more lines"
                // marker, then the last few — the ellipsis between first and
                // last, like every other block. The model still gets the whole
                // message; this is only the transcript.
                let (hidden, tail_start) = fold_split(all.len());
                let mut head = Builder::new(width, colors, is_light);
                head.styled("› ", mark);
                head.push_style(bold);
                head.text(&all[..FOLD_HEAD_LINES].join("\n"));
                head.pop_style();
                head.end_paragraph();
                plate.extend(head.finish().lines);
                plate.push(Line::styled(
                    format!("  {}", t.agent_more_lines(hidden)),
                    Style::default().fg(colors.fg),
                ));
                let mut tail = Builder::new(width, colors, is_light);
                tail.styled("  ", bold);
                tail.push_style(bold);
                tail.text(&all[tail_start..].join("\n"));
                tail.pop_style();
                tail.end_paragraph();
                plate.extend(tail.finish().lines);
            } else {
                let mut builder = Builder::new(width, colors, is_light);
                builder.styled("› ", mark);
                builder.push_style(bold);
                builder.text(trimmed);
                builder.pop_style();
                builder.end_paragraph();
                plate.extend(builder.finish().lines);
            }
            if !at.is_empty() {
                plate.push(time_meta(width, at, true, colors.fg, colors));
            }
            plate.push(Line::default());
            fill_bg(&mut plate, width, colors.disabled);
            // A plain gap above the plate keeps it off the block before it (the
            // last answer's time), since the user block has no leading rule.
            let mut content = vec![Line::default()];
            content.append(&mut plate);
            content
        }
        Item::System { text } => {
            // The system prompt, marked with an accent `#`, dim. It folds like a
            // tool: collapsed shows the first few lines and a skipped-line note,
            // behind a `▸`; expanded shows a `▾` and wraps the whole thing.
            let accent = Style::default().fg(colors.info);
            let prompt = text.trim();
            let all: Vec<&str> = prompt.lines().collect();
            let foldable = all.len() > FOLD_THRESHOLD;
            // The first line carries the fold marker (when foldable) and the `#`;
            // continuation lines indent to match.
            let head = |first: bool| -> Vec<Span<'static>> {
                if !first {
                    return vec![Span::raw("  ")];
                }
                let mut spans = Vec::new();
                if foldable {
                    spans.push(Span::styled(if collapsed { "▸ " } else { "▾ " }, dim));
                }
                spans.push(Span::styled("# ", accent));
                spans
            };
            let mut lines: Vec<Line<'static>> = Vec::new();
            if foldable && collapsed {
                let (hidden, tail_start) = fold_split(all.len());
                for (i, line) in all[..FOLD_HEAD_LINES].iter().enumerate() {
                    let mut spans = head(i == 0);
                    spans.push(Span::styled((*line).to_string(), dim));
                    lines.push(Line::from(spans));
                }
                lines.push(Line::styled(
                    format!("  {}", t.agent_more_lines(hidden)),
                    dim,
                ));
                for line in &all[tail_start..] {
                    let mut spans = head(false);
                    spans.push(Span::styled((*line).to_string(), dim));
                    lines.push(Line::from(spans));
                }
            } else {
                let mut body = wrap_plain(prompt, width.saturating_sub(2), dim, colors, is_light);
                for (i, line) in body.iter_mut().enumerate() {
                    for span in head(i == 0).into_iter().rev() {
                        line.spans.insert(0, span);
                    }
                }
                lines.append(&mut body);
            }
            let mut framed = vec![separator(width, colors)];
            framed.append(&mut lines);
            framed
        }
        Item::Thinking { text, cost, .. } => {
            // Reasoning is its own dim block, marked with an accent `@`. It folds
            // like the system block: the fold marker sits before the `@`, a
            // collapsed block shows its first lines and a skipped-line note, an
            // expanded one wraps the whole thing.
            let accent = Style::default().fg(colors.info);
            let reasoning = text.trim();
            if reasoning.is_empty() {
                return Vec::new();
            }
            let all: Vec<&str> = reasoning.lines().collect();
            let foldable = all.len() > FOLD_THRESHOLD;
            // The first line carries the fold marker (when foldable) and the `@`;
            // continuation lines indent to match.
            let head = |first: bool| -> Vec<Span<'static>> {
                if !first {
                    return vec![Span::raw("  ")];
                }
                let mut spans = Vec::new();
                if foldable {
                    spans.push(Span::styled(if collapsed { "▸ " } else { "▾ " }, dim));
                }
                spans.push(Span::styled("@ ", accent));
                spans
            };
            let mut lines: Vec<Line<'static>> = Vec::new();
            if foldable && collapsed {
                let (hidden, tail_start) = fold_split(all.len());
                for (i, line) in all[..FOLD_HEAD_LINES].iter().enumerate() {
                    let mut spans = head(i == 0);
                    spans.push(Span::styled((*line).to_string(), dim));
                    lines.push(Line::from(spans));
                }
                lines.push(Line::styled(
                    format!("  {}", t.agent_more_lines(hidden)),
                    dim,
                ));
                for line in &all[tail_start..] {
                    let mut spans = head(false);
                    spans.push(Span::styled((*line).to_string(), dim));
                    lines.push(Line::from(spans));
                }
            } else {
                // Expanded, or short enough to never fold: the reasoning wraps
                // under the marker rather than being clipped.
                let mut body =
                    wrap_plain(reasoning, width.saturating_sub(2), dim, colors, is_light);
                for (i, line) in body.iter_mut().enumerate() {
                    for span in head(i == 0).into_iter().rev() {
                        line.spans.insert(0, span);
                    }
                }
                lines.append(&mut body);
            }
            // The reasoning block shows only the prefill/generation indicators
            // (no wall-clock time — that belongs to the answer).
            if let Some(cost) = cost {
                lines.extend(cost_lines(width, cost, colors));
            }
            // A dashed rule sets the block apart from the one before.
            let mut framed = vec![separator(width, colors)];
            framed.append(&mut lines);
            framed
        }
        Item::Assistant {
            text,
            error,
            at,
            cost,
            ..
        } => {
            // The answer, marked like the user's message with an accent `›`,
            // shown in full and trimmed of the stray blank lines models lead
            // with. While it is still empty the live spinner stands in for it.
            let mark = Style::default()
                .fg(colors.info)
                .add_modifier(Modifier::BOLD);
            let mut lines: Vec<Line<'static>> = Vec::new();
            let answer = text.trim();
            if !answer.is_empty() {
                // Reserve the marker's width and indent wrapped lines under it,
                // so the accent `›` never pushes the first line over the edge.
                let body = render_markdown(answer, width.saturating_sub(2), colors, is_light).lines;
                for (i, mut line) in body.into_iter().enumerate() {
                    line.spans.insert(
                        0,
                        if i == 0 {
                            Span::styled("› ", mark)
                        } else {
                            Span::raw("  ")
                        },
                    );
                    lines.push(line);
                }
            }
            if let Some(error) = error {
                // Wrap the error under a `✗` marker, indenting continuation
                // lines, so a long message reflows instead of being clipped.
                let style = Style::default().fg(colors.error);
                let mut body = wrap_plain(error, width.saturating_sub(2), style, colors, is_light);
                for (i, line) in body.iter_mut().enumerate() {
                    line.spans.insert(
                        0,
                        if i == 0 {
                            Span::styled("✗ ", style)
                        } else {
                            Span::raw("  ")
                        },
                    );
                }
                lines.append(&mut body);
            }
            // An empty answer draws nothing at all — a bare turn (reasoning or
            // tools only) has no final message, and while streaming the live
            // spinner stands in for it.
            if lines.is_empty() {
                return lines;
            }
            // The answer carries the wall-clock time and status, like the user's
            // message; the prefill/generation indicators appear here only when a
            // turn does not reason (otherwise the reasoning block holds them).
            if !at.is_empty() {
                lines.push(time_meta(
                    width,
                    at,
                    error.is_none(),
                    colors.disabled,
                    colors,
                ));
                if let Some(cost) = cost {
                    lines.extend(cost_lines(width, cost, colors));
                }
            }
            // A dashed rule sets the block apart from the one before.
            let mut framed = vec![separator(width, colors)];
            framed.append(&mut lines);
            framed
        }
        Item::Tool {
            call,
            result,
            live,
            at,
            duration_ms,
        } => {
            // A dashed rule sets the block apart from the one before.
            let mut lines = vec![separator(width, colors)];
            let body = match (result, live) {
                (Some(result), _) => result.plain_text(),
                (None, Some(live)) => live.clone(),
                (None, None) => String::new(),
            };
            // Trim the stray blank lines a command's output ends with, so the
            // padding stays even.
            let all: Vec<&str> = body.trim().lines().collect();
            // Only a long output is worth folding; a short one shows in full
            // with no marker.
            let foldable = all.len() > FOLD_THRESHOLD;
            let mut head = Vec::new();
            if foldable {
                head.push(Span::styled(if collapsed { "▸ " } else { "▾ " }, dim));
            }
            head.extend(tool_headline(call, width, colors));
            lines.push(Line::from(head));
            if foldable && collapsed {
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
            // Right-aligned meta once the call has finished: how long it took
            // (`🕒`, when known) and the status — no wall-clock time, which
            // belongs to the message blocks.
            if !at.is_empty() {
                let ok = result.as_ref().is_none_or(|r| !r.is_error);
                let mut spans = Vec::new();
                if let Some(ms) = duration_ms {
                    spans.push(Span::styled(format!("🕒 {} ", fmt_dur(*ms)), dim));
                }
                spans.push(status_span(ok, colors));
                lines.push(right_meta(width, spans));
            }
            lines
        }
        Item::Notice { text, kind } => {
            let color = match kind {
                NoticeKind::Info => colors.disabled,
                NoticeKind::Warn => colors.warning,
                NoticeKind::Error => colors.error,
            };
            vec![
                separator(width, colors),
                Line::styled(
                    format!("· {text}"),
                    Style::default().fg(color).add_modifier(Modifier::ITALIC),
                ),
            ]
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
        transcript.push(Item::User {
            text: "hi".into(),
            at: String::new(),
        });
        let base = transcript.lines(40, &colors, false).len();

        transcript.set_live_footer(vec![
            Line::from("✍️ 3m39s (↓3215, 15 tok/s)"),
            Line::from("🕒 3m41s ⠙"),
        ]);
        let with = transcript.lines(40, &colors, false);
        assert_eq!(with.len(), base + 2);
        assert_eq!(text_of(with).last().map(String::as_str), Some("🕒 3m41s ⠙"));

        transcript.set_live_footer(Vec::new());
        assert_eq!(transcript.lines(40, &colors, false).len(), base);
    }

    #[test]
    fn a_finished_block_shows_its_byline_and_cost() {
        let colors = ThemeColors::default();
        let mut transcript = Transcript::default();
        transcript.push(Item::Assistant {
            text: "Done.".into(),
            streaming: false,
            error: None,
            at: "21:03:16".into(),
            cost: Some(Cost {
                prefill_ms: 600,
                gen_ms: 3600,
                input: 512,
                output: 40000,
            }),
        });
        let lines = text_of(transcript.lines(60, &colors, false));
        // The answer (no reasoning here) carries the wall-clock time + status,
        // then the prefill / generation indicators.
        assert!(
            lines
                .iter()
                .any(|l| l.contains("21:03:16") && l.contains('✓')),
            "answer meta: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("⏫") && l.contains("↑512")),
            "prefill line: {lines:?}"
        );
        // Large token counts are abbreviated (40000 -> 40k).
        assert!(
            lines.iter().any(|l| l.contains("✍") && l.contains("↓40k")),
            "generation line: {lines:?}"
        );

        // A finished tool shows how long it took (`🕒`) and its status, no time.
        transcript.push(Item::Tool {
            call: call("bash", json!({ "command": "cargo test" })),
            result: Some(ToolResultMessage::text(&call("bash", json!({})), "ok")),
            live: None,
            at: "21:03:20".into(),
            duration_ms: Some(1200),
        });
        let lines = text_of(transcript.lines(60, &colors, false));
        assert!(
            lines.iter().any(|l| l.contains("🕒") && l.contains('✓')),
            "tool meta: {lines:?}"
        );
        // The shell headline uses the `$` prefix and the command.
        assert!(
            lines.iter().any(|l| l.contains("$ cargo test")),
            "shell headline: {lines:?}"
        );
    }

    #[test]
    fn items_render_and_cache_per_width() {
        let colors = ThemeColors::default();
        let mut transcript = Transcript::default();
        transcript.push(Item::User {
            text: "Fix the bug".into(),
            at: String::new(),
        });
        // Reasoning and the answer stream into their own blocks.
        transcript.stream_thinking("mulling this\nl2\nl3\nl4\nl5\nl6");
        transcript.stream_answer("Looking at `main.rs` now.");
        transcript.push(Item::Tool {
            call: call("read", json!({ "path": "main.rs" })),
            result: None,
            live: None,
            at: String::new(),
            duration_ms: None,
        });
        // user(0), thinking(1), assistant(2), tool(3)
        assert_eq!(transcript.items().len(), 4);

        let lines = text_of(transcript.lines(40, &colors, false));
        // A plain gap, then the plate's top padding row, then the message: the
        // text is on the third line.
        assert_eq!(lines[0].trim_end(), "");
        assert_eq!(lines[1].trim_end(), "");
        assert_eq!(lines[2].trim_end(), "› Fix the bug");
        // Long thinking folds like the system block: the first lines behind the
        // `▸ @` marker and a skipped-line note; the answer shows behind its own
        // accent mark.
        assert!(lines
            .iter()
            .any(|l| l.contains("@ ") && l.contains("mulling this")));
        assert!(lines.iter().any(|l| l.contains('▸')));
        assert!(lines.iter().any(|l| l.contains("more lines")));
        assert!(lines.iter().all(|l| !l.contains("thought for")));
        assert!(lines.iter().any(|l| l.contains("› Looking at")));
        assert!(lines.iter().any(|l| l.contains("Read main.rs")));
        assert_eq!(transcript.line_count(), lines.len());
        assert_eq!(transcript.item_at_line(0), Some(0));
        assert_eq!(transcript.item_at_line(lines.len() - 1), Some(3));

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
        assert!(lines.iter().any(|l| l.contains("Read main.rs")));
        assert!(lines.iter().any(|l| l.contains("… 3 more lines above")));
        assert!(lines.iter().any(|l| l.contains("line 8")));
        assert!(!lines
            .iter()
            .any(|l| l.contains("line 1") && !l.contains("line 1 ")));
        assert!(transcript.toggle_expanded(3));
        let lines = text_of(transcript.lines(40, &colors, false));
        assert!(lines.iter().any(|l| l.contains("line 1")));
        assert!(lines.iter().any(|l| l.contains("line 8")));
        assert!(transcript.any_expanded());
        // Expanding the thinking block shows the full reasoning text.
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
        assert_eq!(transcript.items().len(), 4);
    }

    #[test]
    fn autofold_off_leaves_every_block_expanded() {
        let mut transcript = Transcript::default();
        transcript.set_autofold(false);
        transcript.push(Item::User {
            text: "hi".into(),
            at: String::new(),
        });
        // A long output is foldable, so autofold-off keeps it open.
        let body = (1..=8).map(|n| format!("line {n}")).collect::<Vec<_>>();
        transcript.push(Item::Tool {
            call: call("bash", json!({ "command": "ls" })),
            result: Some(ToolResultMessage::text(
                &call("bash", json!({})),
                body.join("\n"),
            )),
            live: None,
            at: String::new(),
            duration_ms: None,
        });
        assert!(transcript.any_expanded());
    }

    #[test]
    fn short_blocks_are_not_foldable() {
        let colors = ThemeColors::default();
        let mut transcript = Transcript::default();
        // A short tool output shows in full with no fold marker.
        transcript.push(Item::Tool {
            call: call("bash", json!({ "command": "echo hi" })),
            result: Some(ToolResultMessage::text(&call("bash", json!({})), "a\nb\nc")),
            live: None,
            at: "12:00:00".into(),
            duration_ms: Some(300),
        });
        // A short reasoning shows in full behind `@`, no summary line.
        transcript.push(Item::Thinking {
            text: "a quick thought".into(),
            streaming: false,
            at: "12:00:01".into(),
            cost: None,
        });
        transcript.push(Item::Assistant {
            text: "the answer".into(),
            streaming: false,
            error: None,
            at: "12:00:01".into(),
            cost: None,
        });
        let lines = text_of(transcript.lines(60, &colors, false));
        assert!(lines.iter().all(|l| !l.contains('▸') && !l.contains('▾')));
        assert!(lines.iter().any(|l| l.contains("$ echo hi")));
        assert!(lines.iter().any(|l| l.contains('c')));
        assert!(lines.iter().any(|l| l.contains("@ a quick thought")));
        assert!(lines.iter().any(|l| l.contains("› the answer")));
        assert!(lines.iter().all(|l| !l.contains("thought for")));
        // No small block folds on request (tool 0, thinking 1, answer 2).
        assert!(!transcript.toggle_expanded(0));
        assert!(!transcript.toggle_expanded(1));
        assert!(!transcript.toggle_expanded(2));
        assert!(!transcript.any_expanded());
    }

    #[test]
    fn errors_and_notices_are_visible() {
        let colors = ThemeColors::default();
        let mut transcript = Transcript::default();
        transcript.finish_assistant(
            String::new(),
            Some("HTTP 500".into()),
            None,
            "12:00:00".into(),
            false,
        );
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
            at: "12:00:05".into(),
            duration_ms: Some(500),
        });
        let lines = text_of(transcript.lines(60, &colors, false));
        assert!(lines.iter().any(|l| l.contains("✗ HTTP 500")));
        assert!(lines.iter().any(|l| l.contains("· compacted 1200 tokens")));
        // Shell headline uses `$`; the failed status sits in the tool's meta
        // beside how long it took (`🕒`).
        assert!(lines.iter().any(|l| l.contains("$ exit 1")));
        assert!(lines.iter().any(|l| l.contains("🕒") && l.contains('✗')));
    }

    #[test]
    fn a_turn_error_after_reasoning_shows_on_the_answer() {
        let colors = ThemeColors::default();
        let mut transcript = Transcript::default();
        // Reasoning streams, then the turn fails with no answer text.
        transcript.stream_thinking("weighing it\nl2\nl3\nl4\nl5\nl6");
        transcript.finish_thinking("12:00:00", None);
        transcript.finish_assistant(
            String::new(),
            Some("HTTP 500".into()),
            None,
            "12:00:01".into(),
            true,
        );
        // The error and its failed status land on an answer block, even though
        // the answer text is empty; the reasoning block carries no status.
        let lines = text_of(transcript.lines(60, &colors, false));
        assert!(lines.iter().any(|l| l.contains("✗ HTTP 500")));
        assert!(lines
            .iter()
            .any(|l| l.contains("12:00:01") && l.contains('✗')));
    }

    #[test]
    fn a_long_error_wraps_to_the_width() {
        let colors = ThemeColors::default();
        let mut transcript = Transcript::default();
        let error = "the request failed because the endpoint is unreachable and \
             the retries were exhausted after several attempts"
            .to_string();
        transcript.finish_assistant(String::new(), Some(error), None, "12:00:01".into(), true);
        let width = 30;
        let lines = text_of(transcript.lines(width, &colors, false));
        // The error is marked with `✗` and reflows instead of spilling past the
        // width; every wrapped row stays within it.
        assert!(lines.iter().any(|l| l.contains('✗')));
        let wrapped = lines
            .iter()
            .filter(|l| l.contains("request") || l.contains("retries"))
            .count();
        assert!(wrapped >= 2, "a long error should wrap to several rows");
        assert!(lines.iter().all(|l| l.chars().count() <= width as usize));
    }

    #[test]
    fn the_system_prompt_folds_with_a_marker() {
        let colors = ThemeColors::default();
        let mut transcript = Transcript::default();
        let prompt = (1..=8)
            .map(|n| format!("rule {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.push(Item::System { text: prompt });
        // Collapsed: a `▸` marker before the `#`, the first line, a note, then
        // the last few — the ellipsis sits between the first and the last few.
        let lines = text_of(transcript.lines(60, &colors, false));
        assert!(lines.iter().any(|l| l.contains("▸ # rule 1")));
        assert!(lines.iter().any(|l| l.contains("… 3 more lines")));
        assert!(lines.iter().any(|l| l.contains("rule 5")));
        assert!(lines.iter().any(|l| l.contains("rule 8")));
        // The middle lines are the hidden ones.
        assert!(!lines.iter().any(|l| l.contains("rule 2")));
        assert!(!lines.iter().any(|l| l.contains("rule 4")));
        // Expanded: a `▾` marker and the whole prompt.
        assert!(transcript.toggle_expanded(0));
        let lines = text_of(transcript.lines(60, &colors, false));
        assert!(lines.iter().any(|l| l.contains("▾ # rule 1")));
        assert!(lines.iter().any(|l| l.contains("rule 4")));
        assert!(lines.iter().any(|l| l.contains("rule 8")));
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
