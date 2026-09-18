//! A question asked inside a panel: a title and numbered options in a small
//! bordered card, answered with the arrows and `Enter`, a digit, or `Esc`.
//! Two optional extra rows: one that takes an answer typed by the user
//! (`with_custom`), and one that cancels the whole thing (`with_cancel`),
//! which is also what `Esc` does.
//!
//! For questions a panel raises on its own — an agent asking whether it may
//! run a command — this beats an app-wide modal: with several panels open a
//! modal does not say who is asking, a card sits in the panel that is.
//! Modals stay for choices the user starts.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use termide_core::ThemeColors;

use crate::TextInput;

/// What a key did to the form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoiceAction {
    /// The selection moved, or typing went on.
    Handled,
    /// A fixed option was chosen, by `Enter` or its digit.
    Chosen(usize),
    /// The user typed an answer of their own and confirmed it.
    Custom(String),
    /// `Esc`, or the cancel row: the question is declined and whatever asked
    /// it should stop.
    Cancelled,
    /// Not a form key.
    NotHandled,
}

/// One row of the card.
enum Row {
    Option(usize),
    Custom,
    Cancel,
}

pub struct ChoiceForm {
    title: String,
    options: Vec<String>,
    /// Label of the row that takes a typed answer, when offered.
    custom: Option<String>,
    /// Label of the row that cancels, when offered.
    cancel: Option<String>,
    selected: usize,
    /// The answer being typed, once the custom row was chosen.
    typing: Option<TextInput>,
    drawn: Option<Rect>,
}

impl ChoiceForm {
    #[must_use]
    pub fn new(title: impl Into<String>, options: Vec<String>) -> Self {
        Self {
            title: title.into(),
            options,
            custom: None,
            cancel: None,
            selected: 0,
            typing: None,
            drawn: None,
        }
    }

    /// Offer a row where the user types an answer; `label` names it.
    #[must_use]
    pub fn with_custom(mut self, label: impl Into<String>) -> Self {
        self.custom = Some(label.into());
        self
    }

    /// Offer a row that cancels (the same as `Esc`); `label` names it.
    #[must_use]
    pub fn with_cancel(mut self, label: impl Into<String>) -> Self {
        self.cancel = Some(label.into());
        self
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn options(&self) -> &[String] {
        &self.options
    }

    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The answer being typed, while the custom row is active.
    #[must_use]
    pub fn typed(&self) -> Option<&str> {
        self.typing.as_ref().map(TextInput::text)
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = (0..self.options.len()).map(Row::Option).collect();
        if self.custom.is_some() {
            rows.push(Row::Custom);
        }
        if self.cancel.is_some() {
            rows.push(Row::Cancel);
        }
        rows
    }

    fn row_count(&self) -> usize {
        self.options.len() + usize::from(self.custom.is_some()) + usize::from(self.cancel.is_some())
    }

    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn select_down(&mut self) {
        self.selected = (self.selected + 1).min(self.row_count().saturating_sub(1));
    }

    /// Act on the row at `index`: a fixed option is chosen, the custom row
    /// starts typing, the cancel row cancels.
    fn activate(&mut self, index: usize) -> ChoiceAction {
        let rows = self.rows();
        let Some(row) = rows.get(index) else {
            return ChoiceAction::Handled;
        };
        self.selected = index;
        match row {
            Row::Option(option) => ChoiceAction::Chosen(*option),
            Row::Custom => {
                self.typing = Some(TextInput::new());
                ChoiceAction::Handled
            }
            Row::Cancel => ChoiceAction::Cancelled,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ChoiceAction {
        if let Some(input) = &mut self.typing {
            // Typing the custom answer: Enter confirms, Esc goes back to the
            // rows, the rest edits the line.
            return match key.code {
                KeyCode::Enter => {
                    let text = input.text().trim().to_string();
                    if text.is_empty() {
                        ChoiceAction::Handled
                    } else {
                        self.typing = None;
                        ChoiceAction::Custom(text)
                    }
                }
                KeyCode::Esc => {
                    self.typing = None;
                    ChoiceAction::Handled
                }
                KeyCode::Backspace => {
                    input.backspace();
                    ChoiceAction::Handled
                }
                KeyCode::Delete => {
                    input.delete();
                    ChoiceAction::Handled
                }
                KeyCode::Left => {
                    input.move_left();
                    ChoiceAction::Handled
                }
                KeyCode::Right => {
                    input.move_right();
                    ChoiceAction::Handled
                }
                KeyCode::Home => {
                    input.move_home();
                    ChoiceAction::Handled
                }
                KeyCode::End => {
                    input.move_end();
                    ChoiceAction::Handled
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    input.insert(c);
                    ChoiceAction::Handled
                }
                _ => ChoiceAction::NotHandled,
            };
        }
        match key.code {
            KeyCode::Up => {
                self.select_up();
                ChoiceAction::Handled
            }
            KeyCode::Down => {
                self.select_down();
                ChoiceAction::Handled
            }
            KeyCode::Enter if self.row_count() > 0 => self.activate(self.selected),
            KeyCode::Char(digit @ '1'..='9') => {
                let index = digit as usize - '1' as usize;
                if index < self.row_count() {
                    self.activate(index)
                } else {
                    ChoiceAction::Handled
                }
            }
            KeyCode::Esc => ChoiceAction::Cancelled,
            _ => ChoiceAction::NotHandled,
        }
    }

    /// Rows the card takes: a border above and below around one row per
    /// entry, the title sitting in the top border.
    #[must_use]
    pub fn height(&self) -> u16 {
        self.row_count() as u16 + 2
    }

    /// Draw the card filling `area` (use [`ChoiceForm::height`] rows). The
    /// selected row is highlighted in the selection colours while the panel
    /// is focused, in bold otherwise, so an unfocused panel still shows what
    /// it is asking. While an answer is being typed, its row shows the text
    /// and a cursor.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, colors: &ThemeColors, focused: bool) {
        if area.width < 4 || area.height < 3 {
            self.drawn = None;
            return;
        }
        let border = Style::default().fg(if focused {
            colors.border_focused
        } else {
            colors.border
        });
        let text = Style::default().fg(colors.fg).bg(colors.bg);
        let width = area.width as usize;
        let right = area.x + area.width - 1;
        let bottom = area.y + area.height - 1;
        buf.set_string(area.x, area.y, " ".repeat(width), text);
        buf.set_string(area.x, area.y, "┌", border);
        for x in area.x + 1..right {
            buf[(x, area.y)].set_symbol("─").set_style(border);
        }
        buf.set_string(right, area.y, "┐", border);
        let title = format!(" {} ", self.title);
        buf.set_stringn(
            area.x + 1,
            area.y,
            &title,
            width.saturating_sub(2),
            Style::default().fg(colors.fg).add_modifier(Modifier::BOLD),
        );
        buf.set_string(area.x, bottom, " ".repeat(width), text);
        buf.set_string(area.x, bottom, "└", border);
        for x in area.x + 1..right {
            buf[(x, bottom)].set_symbol("─").set_style(border);
        }
        buf.set_string(right, bottom, "┘", border);

        let visible = (area.height - 2) as usize;
        for (index, row) in self.rows().iter().take(visible).enumerate() {
            let y = area.y + 1 + index as u16;
            let selected = index == self.selected;
            let style = match (selected, focused) {
                (true, true) => Style::default()
                    .fg(colors.selection_fg)
                    .bg(colors.selection_bg),
                (true, false) => text.add_modifier(Modifier::BOLD),
                (false, _) => text,
            };
            buf.set_string(area.x, y, " ".repeat(width), text);
            buf.set_string(area.x, y, "│", border);
            buf.set_string(right, y, "│", border);
            let label = match row {
                Row::Option(option) => self.options[*option].as_str(),
                Row::Custom => self.custom.as_deref().unwrap_or(""),
                Row::Cancel => self.cancel.as_deref().unwrap_or(""),
            };
            let line = match (row, &self.typing) {
                (Row::Custom, Some(input)) => format!(" {}. {label}: {}▏", index + 1, input.text()),
                _ => format!(" {}. {label}", index + 1),
            };
            buf.set_stringn(area.x + 1, y, line, width.saturating_sub(2), style);
        }
        self.drawn = Some(area);
    }

    /// Act on a click at `(x, y)`, from the last render: the same as choosing
    /// that row with the keyboard.
    pub fn click(&mut self, x: u16, y: u16) -> ChoiceAction {
        match self.hit(x, y) {
            Some(index) => self.activate(index),
            None => ChoiceAction::NotHandled,
        }
    }

    /// The row under a click at `(x, y)`, from the last render.
    #[must_use]
    pub fn hit(&self, x: u16, y: u16) -> Option<usize> {
        let rect = self.drawn?;
        let inside =
            x >= rect.x && x < rect.x + rect.width && y > rect.y && y < rect.y + rect.height - 1;
        if !inside {
            return None;
        }
        let index = (y - rect.y - 1) as usize;
        (index < self.row_count()).then_some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> ChoiceForm {
        ChoiceForm::new(
            "Agent wants to run bash: git push",
            vec!["Allow once".into(), "Allow always".into(), "Deny".into()],
        )
    }

    #[test]
    fn arrows_digits_enter_and_esc_answer() {
        let mut form = form();
        assert_eq!(
            form.handle_key(KeyEvent::from(KeyCode::Down)),
            ChoiceAction::Handled
        );
        assert_eq!(
            form.handle_key(KeyEvent::from(KeyCode::Enter)),
            ChoiceAction::Chosen(1)
        );
        assert_eq!(
            form.handle_key(KeyEvent::from(KeyCode::Char('3'))),
            ChoiceAction::Chosen(2)
        );
        assert_eq!(
            form.handle_key(KeyEvent::from(KeyCode::Char('9'))),
            ChoiceAction::Handled
        );
        assert_eq!(
            form.handle_key(KeyEvent::from(KeyCode::Esc)),
            ChoiceAction::Cancelled
        );
        assert_eq!(
            form.handle_key(KeyEvent::from(KeyCode::Char('x'))),
            ChoiceAction::NotHandled
        );
        assert_eq!(form.height(), 5);
    }

    #[test]
    fn renders_a_card_and_maps_clicks_to_options() {
        let mut form = form();
        let area = Rect::new(2, 3, 50, 5);
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 10));
        form.render(area, &mut buf, &ThemeColors::default(), true);
        let row =
            |y: u16| -> String { (0..60).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert!(
            row(3).contains("Agent wants to run bash: git push"),
            "{}",
            row(3)
        );
        assert!(row(4).contains("1. Allow once"), "{}", row(4));
        assert!(row(6).contains("3. Deny"), "{}", row(6));
        assert!(row(7).starts_with("  └"), "{}", row(7));
        assert_eq!(form.hit(10, 5), Some(1));
        assert_eq!(form.hit(10, 3), None, "the border is not an option");
        assert_eq!(form.hit(60, 5), None);
    }
}
