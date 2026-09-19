//! A reusable bottom input bar: labeled text fields with a cursor, an
//! optional row of buttons and toggles, an optional right-aligned status, and
//! an optional titled top border with a left and a right slot.
//!
//! It is the shared engine behind the panels' bottom bars — the agent's
//! prompt input and the find/replace bar — so they share one focus model, one
//! field renderer and one set of key and mouse semantics. Like
//! [`crate::CompletionList`] and [`crate::ChoiceForm`], the widget owns its
//! layout, focus and keys and reports what happened through
//! [`InputBarAction`]; the host wires the meaning (run a search, send a
//! prompt) and reads the field values back.
//!
//! Focus runs as a ring: every field first, then every control. `Tab` and the
//! arrows move within it, `Enter` on a field submits, `Enter`/`Space` on a
//! control activates it, `Esc` closes.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use termide_core::ThemeColors;

use crate::grapheme_utils::str_display_width;
use crate::TextInput;

/// A control on the bar's bottom row: a push button or a toggle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// A button that reports [`InputBarAction::Activated`] when pressed.
    Button { label: String },
    /// A checkbox-style toggle. Activation flips `on` and still reports
    /// [`InputBarAction::Activated`], so the host can react to the new state.
    Toggle { label: String, on: bool },
}

/// The left and right text embedded in the bar's top border.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BorderSlots {
    pub left: String,
    pub right: String,
}

/// What a key or click did, for the host to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBarAction {
    /// Field `index`'s text changed.
    Edited(usize),
    /// `Enter` on field `index`.
    Submit(usize),
    /// A control was activated (a toggle has already flipped its own state).
    Activated(usize),
    /// `Esc`.
    Close,
}

/// A focusable control: a field, or a bottom-row control by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Field(usize),
    Control(usize),
}

/// A bottom input bar. Build it with [`InputBar::new`] and the `with_*`
/// methods, then drive it with [`InputBar::render`], [`InputBar::handle_key`]
/// and [`InputBar::handle_mouse`].
pub struct InputBar {
    labels: Vec<String>,
    inputs: Vec<TextInput>,
    controls: Vec<Control>,
    /// Index into the focus ring (fields then controls).
    focus: usize,
    /// Right-aligned status on the controls row (a match counter, say).
    status: Option<String>,
    /// The top border and its slots, or `None` for a borderless bar.
    border: Option<BorderSlots>,
    /// Rendered field rows, parallel to `labels`, for mouse hit-testing.
    field_areas: Vec<Rect>,
    /// Rendered control areas: (area, index into `controls`).
    control_areas: Vec<(Rect, usize)>,
}

impl InputBar {
    /// A bar with one labeled field per entry in `labels` and no controls.
    /// Include any trailing space in a label, e.g. `"Find: "`.
    #[must_use]
    pub fn new(labels: Vec<String>) -> Self {
        let inputs = labels.iter().map(|_| TextInput::new()).collect();
        Self {
            labels,
            inputs,
            controls: Vec::new(),
            focus: 0,
            status: None,
            border: None,
            field_areas: Vec::new(),
            control_areas: Vec::new(),
        }
    }

    /// Append a control to the bottom row.
    #[must_use]
    pub fn with_control(mut self, control: Control) -> Self {
        self.controls.push(control);
        self
    }

    /// Give the bar a top border with a left and a right slot.
    #[must_use]
    pub fn with_border(mut self, left: impl Into<String>, right: impl Into<String>) -> Self {
        self.border = Some(BorderSlots {
            left: left.into(),
            right: right.into(),
        });
        self
    }

    // === Structure ===

    #[must_use]
    pub fn field_count(&self) -> usize {
        self.labels.len()
    }

    /// The focus ring: field indices first, then control indices.
    fn ring(&self) -> Vec<Focus> {
        let mut ring: Vec<Focus> = (0..self.labels.len()).map(Focus::Field).collect();
        ring.extend((0..self.controls.len()).map(Focus::Control));
        ring
    }

    /// The control the focus is on.
    #[must_use]
    pub fn focus(&self) -> Focus {
        let ring = self.ring();
        ring[self.focus.min(ring.len().saturating_sub(1))]
    }

    /// The focused field's index, or `None` when a control is focused.
    #[must_use]
    pub fn focused_field(&self) -> Option<usize> {
        match self.focus() {
            Focus::Field(i) => Some(i),
            Focus::Control(_) => None,
        }
    }

    /// Rows the bar occupies: the border, one row per field, and the controls
    /// row when there are controls.
    #[must_use]
    pub fn height(&self) -> u16 {
        u16::from(self.border.is_some())
            + self.labels.len() as u16
            + u16::from(!self.controls.is_empty())
    }

    // === Values ===

    #[must_use]
    pub fn field_text(&self, index: usize) -> &str {
        self.inputs.get(index).map_or("", TextInput::text)
    }

    pub fn set_field_text(&mut self, index: usize, text: impl Into<String>) {
        if let Some(input) = self.inputs.get_mut(index) {
            *input = TextInput::with_default(text.into());
        }
    }

    /// Whether the control at `index` is a toggle that is on.
    #[must_use]
    pub fn control_on(&self, index: usize) -> bool {
        matches!(
            self.controls.get(index),
            Some(Control::Toggle { on: true, .. })
        )
    }

    pub fn set_control_on(&mut self, index: usize, value: bool) {
        if let Some(Control::Toggle { on, .. }) = self.controls.get_mut(index) {
            *on = value;
        }
    }

    pub fn set_control_label(&mut self, index: usize, label: impl Into<String>) {
        match self.controls.get_mut(index) {
            Some(Control::Button { label: l } | Control::Toggle { label: l, .. }) => {
                *l = label.into();
            }
            None => {}
        }
    }

    pub fn set_label(&mut self, index: usize, label: impl Into<String>) {
        if let Some(l) = self.labels.get_mut(index) {
            *l = label.into();
        }
    }

    pub fn set_status(&mut self, status: Option<String>) {
        self.status = status;
    }

    pub fn set_border_slots(&mut self, left: impl Into<String>, right: impl Into<String>) {
        self.border = Some(BorderSlots {
            left: left.into(),
            right: right.into(),
        });
    }

    // === Focus ===

    pub fn focus_first(&mut self) {
        self.focus = 0;
    }

    pub fn focus_field(&mut self, index: usize) {
        if index < self.labels.len() {
            self.focus = index;
        }
    }

    fn focus_next(&mut self) {
        let len = self.ring().len();
        if len > 0 {
            self.focus = (self.focus + 1) % len;
        }
    }

    fn focus_prev(&mut self) {
        let len = self.ring().len();
        if len > 0 {
            self.focus = (self.focus + len - 1) % len;
        }
    }

    // === Input ===

    /// Handle a key while the bar holds focus. `Esc` closes; the rest depends
    /// on whether a field or a control is focused.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<InputBarAction> {
        if key.code == KeyCode::Esc {
            return Some(InputBarAction::Close);
        }
        match self.focus() {
            Focus::Field(i) => self.handle_field_key(i, key),
            Focus::Control(i) => self.handle_control_key(i, key),
        }
    }

    fn handle_field_key(&mut self, index: usize, key: KeyEvent) -> Option<InputBarAction> {
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.focus_next();
                None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.focus_prev();
                None
            }
            KeyCode::Enter => Some(InputBarAction::Submit(index)),
            _ => {
                let input = &mut self.inputs[index];
                let before = input.text().to_string();
                if edit_text_input(input, key) && input.text() != before {
                    Some(InputBarAction::Edited(index))
                } else {
                    None
                }
            }
        }
    }

    fn handle_control_key(&mut self, index: usize, key: KeyEvent) -> Option<InputBarAction> {
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                self.focus_prev();
                None
            }
            KeyCode::Right | KeyCode::Tab => {
                self.focus_next();
                None
            }
            KeyCode::Up => {
                // Jump back to the last field, if there is one.
                if !self.labels.is_empty() {
                    self.focus = self.labels.len() - 1;
                }
                None
            }
            KeyCode::Enter | KeyCode::Char(' ') => Some(self.activate(index)),
            _ => None,
        }
    }

    fn activate(&mut self, index: usize) -> InputBarAction {
        if let Some(Control::Toggle { on, .. }) = self.controls.get_mut(index) {
            *on = !*on;
        }
        InputBarAction::Activated(index)
    }

    /// Handle a left click: on a field it focuses it and places the cursor, on
    /// a control it activates it.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<InputBarAction> {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }
        let (col, row) = (mouse.column, mouse.row);
        for (i, area) in self.field_areas.clone().into_iter().enumerate() {
            if hit(area, col, row) {
                self.focus = i;
                let label_w = str_display_width(&self.labels[i]) as u16;
                let start_x = area.x + label_w;
                if col >= start_x {
                    let pos = screen_x_to_char_pos(self.inputs[i].text(), (col - start_x) as usize);
                    self.inputs[i].set_cursor_with_selection_start(pos);
                }
                return None;
            }
        }
        let clicked = self
            .control_areas
            .iter()
            .find_map(|(area, idx)| hit(*area, col, row).then_some(*idx));
        if let Some(idx) = clicked {
            self.focus = self.labels.len() + idx;
            return Some(self.activate(idx));
        }
        None
    }

    /// Whether a click at `(col, row)` lands on one of the bar's controls
    /// (after [`InputBar::render`] recorded their areas).
    #[must_use]
    pub fn click_hits(&self, col: u16, row: u16) -> bool {
        self.field_areas.iter().any(|a| hit(*a, col, row))
            || self.control_areas.iter().any(|(a, _)| hit(*a, col, row))
    }

    // === Rendering ===

    /// Render the bar into `area`. `active` is whether the bar (not the panel
    /// body) holds focus — it controls the cursor and focus highlight.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, colors: &ThemeColors, active: bool) {
        self.field_areas.clear();
        self.control_areas.clear();
        if area.width == 0 || area.height == 0 {
            return;
        }
        let focus = self.focus();
        let mut y = area.y;
        if let Some(border) = &self.border {
            render_border(area, y, buf, colors, &border.left, &border.right);
            y += 1;
        }

        for i in 0..self.labels.len() {
            let row = Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            };
            self.field_areas.push(row);
            render_labeled_input(
                buf,
                row,
                &LabeledInput {
                    label: &self.labels[i],
                    text: self.inputs[i].text(),
                    cursor: self.inputs[i].cursor_pos(),
                    selection: self.inputs[i].selection_range(),
                    focused: active && focus == Focus::Field(i),
                },
                colors,
            );
            y += 1;
        }

        if !self.controls.is_empty() {
            let row = Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            };
            self.render_controls(row, buf, colors, active, focus);
        } else if let Some(last) = self.field_areas.last().copied() {
            // With no controls row, the status rides on the last field row so
            // a bar of fields alone (a name search) still shows its counter.
            self.render_status(last, buf, colors);
        }
    }

    /// Right-aligned status text; returns the x where it starts so controls
    /// can stop short of it.
    fn render_status(&self, area: Rect, buf: &mut Buffer, colors: &ThemeColors) -> u16 {
        let status = self.status.as_deref().unwrap_or("");
        let status_w = str_display_width(status) as u16;
        let left = area.x + area.width.saturating_sub(status_w);
        if !status.is_empty() {
            buf.set_string(left, area.y, status, Style::default().fg(colors.disabled));
        }
        left
    }

    fn render_controls(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        colors: &ThemeColors,
        active: bool,
        focus: Focus,
    ) {
        // Right-aligned status first, so controls stop short of it.
        let status_left = self.render_status(area, buf, colors);

        let mut x = area.x;
        for idx in 0..self.controls.len() {
            let focused = active && focus == Focus::Control(idx);
            let (text, style) = self.control_render(idx, focused, colors);
            let w = str_display_width(&text) as u16;
            if x + w >= status_left {
                break;
            }
            self.control_areas.push((
                Rect {
                    x,
                    y: area.y,
                    width: w,
                    height: 1,
                },
                idx,
            ));
            buf.set_string(x, area.y, &text, style);
            x += w + 1;
        }
    }

    fn control_render(&self, index: usize, focused: bool, colors: &ThemeColors) -> (String, Style) {
        match &self.controls[index] {
            Control::Button { label } => {
                let text = if focused {
                    format!("[ {label} ]")
                } else {
                    format!("  {label}  ")
                };
                let mut style = Style::default().fg(colors.fg);
                if focused {
                    style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
                }
                (text, style)
            }
            Control::Toggle { label, on } => {
                let mark = if *on { "x" } else { " " };
                let text = format!("[{mark}] {label}");
                let mut style = if *on {
                    Style::default()
                        .fg(colors.info)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(colors.disabled)
                };
                if focused {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                (text, style)
            }
        }
    }
}

/// Draw a top border `───` with the left text just after the corner and the
/// right text just before it.
fn render_border(
    area: Rect,
    y: u16,
    buf: &mut Buffer,
    colors: &ThemeColors,
    left: &str,
    right: &str,
) {
    let style = Style::default().fg(colors.border);
    for dx in 0..area.width {
        buf[(area.x + dx, y)].set_symbol("─").set_style(style);
    }
    let label_style = Style::default().fg(colors.disabled);
    if !left.is_empty() && area.width > 2 {
        let text: String = left
            .chars()
            .take(area.width.saturating_sub(2) as usize)
            .collect();
        buf.set_string(area.x + 1, y, &text, label_style);
    }
    if !right.is_empty() {
        let w = str_display_width(right) as u16;
        if w + 1 < area.width {
            buf.set_string(area.x + area.width - 1 - w, y, right, label_style);
        }
    }
}

/// One labeled single-line input to render.
struct LabeledInput<'a> {
    label: &'a str,
    text: &'a str,
    cursor: usize,
    selection: Option<(usize, usize)>,
    focused: bool,
}

/// Render `label` then `text` as a single-line input, scrolled to keep the
/// cursor visible, with a selection highlight and (when focused) the cursor.
fn render_labeled_input(buf: &mut Buffer, area: Rect, field: &LabeledInput, colors: &ThemeColors) {
    let LabeledInput {
        label,
        text,
        cursor,
        selection,
        focused,
    } = *field;
    let label_w = str_display_width(label) as u16;
    buf.set_string(area.x, area.y, label, Style::default().fg(colors.fg));
    let x0 = area.x + label_w;
    let width = area.width.saturating_sub(label_w);
    if width == 0 {
        return;
    }

    let chars: Vec<char> = text.chars().collect();
    let widths: Vec<usize> = chars.iter().map(|c| char_width(*c)).collect();
    // Scroll so the cursor's column stays within `width`.
    let cursor_col: usize = widths.iter().take(cursor).sum();
    let mut start = 0usize;
    let mut lead: usize = 0;
    if cursor_col >= width as usize {
        // Drop leading chars until the cursor fits.
        let mut used = 0usize;
        start = chars.len();
        for i in (0..chars.len()).rev() {
            let w = widths[i];
            if used + w > width as usize - 1 {
                break;
            }
            used += w;
            start = i;
        }
        lead = widths[..start].iter().sum();
    }
    let _ = lead;

    let base = if focused {
        Style::default().fg(colors.fg).bg(colors.bg)
    } else {
        Style::default().fg(colors.fg)
    };
    let invert = Style::default().fg(colors.bg).bg(colors.fg);
    let mut col = 0u16;
    for i in start..chars.len() {
        let cw = widths[i] as u16;
        if col + cw > width {
            break;
        }
        let selected = selection.is_some_and(|(s, e)| i >= s && i < e);
        let at_cursor = focused && i == cursor;
        let style = if at_cursor || selected { invert } else { base };
        buf.set_string(x0 + col, area.y, chars[i].to_string(), style);
        col += cw;
    }
    // Cursor at end of text.
    if focused && cursor >= chars.len() && col < width {
        buf[(x0 + col, area.y)].set_style(invert);
    }
}

fn char_width(c: char) -> usize {
    str_display_width(&c.to_string()).max(1)
}

/// The char position in `text` under a screen x-offset inside the input,
/// accounting for wide characters; past the end returns the length.
fn screen_x_to_char_pos(text: &str, screen_x: usize) -> usize {
    let mut width = 0;
    for (i, c) in text.chars().enumerate() {
        let cw = char_width(c);
        if width + cw > screen_x {
            return i;
        }
        width += cw;
    }
    text.chars().count()
}

/// Apply an editing key to a single-line input. Returns whether it was
/// handled (text may or may not have changed).
fn edit_text_input(input: &mut TextInput, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(c) => {
            input.insert(c);
            true
        }
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.move_left(),
        KeyCode::Right => input.move_right(),
        KeyCode::Home => {
            input.move_home();
            true
        }
        KeyCode::End => {
            input.move_end();
            true
        }
        _ => false,
    }
}

fn hit(area: Rect, col: u16, row: u16) -> bool {
    col >= area.x && col < area.x + area.width && row == area.y
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn bar() -> InputBar {
        InputBar::new(vec!["Find: ".into(), "Repl: ".into()])
            .with_control(Control::Toggle {
                label: "Case".into(),
                on: false,
            })
            .with_control(Control::Button {
                label: "Next".into(),
            })
    }

    #[test]
    fn height_counts_border_fields_and_controls() {
        assert_eq!(bar().height(), 3); // 2 fields + control row
        assert_eq!(InputBar::new(vec!["x".into()]).height(), 1);
        assert_eq!(bar().with_border("l", "r").height(), 4);
    }

    #[test]
    fn typing_edits_the_focused_field_and_reports_it() {
        let mut b = bar();
        assert_eq!(b.focused_field(), Some(0));
        assert_eq!(
            b.handle_key(key(KeyCode::Char('a'))),
            Some(InputBarAction::Edited(0))
        );
        assert_eq!(b.field_text(0), "a");
        assert_eq!(b.field_text(1), "");
    }

    #[test]
    fn tab_walks_fields_then_controls_and_wraps() {
        let mut b = bar();
        b.handle_key(key(KeyCode::Tab)); // Find -> Repl
        assert_eq!(b.focused_field(), Some(1));
        b.handle_key(key(KeyCode::Tab)); // Repl -> control 0
        assert_eq!(b.focus(), Focus::Control(0));
        b.handle_key(key(KeyCode::Tab)); // -> control 1
        assert_eq!(b.focus(), Focus::Control(1));
        b.handle_key(key(KeyCode::Tab)); // wraps -> field 0
        assert_eq!(b.focused_field(), Some(0));
    }

    #[test]
    fn enter_on_a_field_submits_it() {
        let mut b = bar();
        b.handle_key(key(KeyCode::Tab));
        assert_eq!(
            b.handle_key(key(KeyCode::Enter)),
            Some(InputBarAction::Submit(1))
        );
    }

    #[test]
    fn activating_a_toggle_flips_it_and_reports() {
        let mut b = bar();
        b.focus_field(0);
        for _ in 0..2 {
            b.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(b.focus(), Focus::Control(0));
        assert!(!b.control_on(0));
        assert_eq!(
            b.handle_key(key(KeyCode::Char(' '))),
            Some(InputBarAction::Activated(0))
        );
        assert!(b.control_on(0));
        // A plain button activates without a state.
        b.handle_key(key(KeyCode::Right));
        assert_eq!(
            b.handle_key(key(KeyCode::Enter)),
            Some(InputBarAction::Activated(1))
        );
    }

    #[test]
    fn left_right_cycle_controls_and_up_returns_to_last_field() {
        let mut b = bar();
        for _ in 0..2 {
            b.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(b.focus(), Focus::Control(0));
        b.handle_key(key(KeyCode::Right));
        assert_eq!(b.focus(), Focus::Control(1));
        b.handle_key(key(KeyCode::Left));
        assert_eq!(b.focus(), Focus::Control(0));
        b.handle_key(key(KeyCode::Up));
        assert_eq!(b.focused_field(), Some(1));
    }

    #[test]
    fn esc_closes_from_anywhere() {
        let mut b = bar();
        assert_eq!(b.handle_key(key(KeyCode::Esc)), Some(InputBarAction::Close));
        b.handle_key(key(KeyCode::Tab));
        assert_eq!(b.handle_key(key(KeyCode::Esc)), Some(InputBarAction::Close));
    }

    #[test]
    fn seed_and_read_back_field_text() {
        let mut b = bar();
        b.set_field_text(0, "needle");
        b.set_field_text(1, "thread");
        assert_eq!(b.field_text(0), "needle");
        assert_eq!(b.field_text(1), "thread");
    }

    #[test]
    fn a_click_focuses_a_field_and_places_the_cursor() {
        use ratatui::style::Style;
        let mut b = bar();
        b.set_field_text(0, "hello");
        let area = Rect::new(0, 0, 40, b.height());
        let mut buf = Buffer::filled(area, ratatui::buffer::Cell::default());
        let _ = Style::default();
        b.render(area, &mut buf, &ThemeColors::default(), true);
        // Click inside the second field's text (row 1, after the "Repl: " label).
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 8,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(b.handle_mouse(click), None);
        assert_eq!(b.focused_field(), Some(1));
        // Clicking a control activates it.
        let (carea, _) = b.control_areas[0];
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: carea.x,
            row: carea.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(b.handle_mouse(click), Some(InputBarAction::Activated(0)));
    }
}
