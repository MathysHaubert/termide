//! Selection modal dialog (single selection).

use anyhow::Result;
use crossterm::event::KeyCode;
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph, Widget},
};

use termide_theme::Theme;

use termide_ui::str_display_width;

use crate::{
    base::render_modal_block, centered_rect_with_size, fit_modal_width, max_item_width,
    max_line_width, Modal, ModalResult,
};

/// Selection modal window (single selection only)
#[derive(Debug)]
pub struct SelectModal {
    title: String,
    prompt: String,
    items: Vec<String>,
    cursor: usize,
    last_modal_area: Option<Rect>,
    last_list_area: Option<Rect>,
}

impl SelectModal {
    /// Create a single selection window from strings
    pub fn single(
        title: impl Into<String>,
        prompt: impl Into<String>,
        labels: Vec<String>,
    ) -> Self {
        Self {
            title: title.into(),
            prompt: prompt.into(),
            items: labels,
            cursor: 0,
            last_modal_area: None,
            last_list_area: None,
        }
    }

    /// Set initial cursor position.
    pub fn set_cursor(&mut self, index: usize) {
        if index < self.items.len() {
            self.cursor = index;
        }
    }

    /// The modal's width from its content: the widest of the title, the
    /// prompt and the items, each item with one column of padding on either
    /// side. The row under the cursor is inverted across that whole width,
    /// so it needs no `▶` marker beside it.
    fn calculate_modal_width(&self, screen_width: u16) -> u16 {
        let title_width = str_display_width(&self.title) as u16 + 2;
        let prompt_width = max_line_width(&self.prompt);
        let items_width = max_item_width(&self.items, 2);
        let inner = title_width.max(prompt_width).max(items_width);
        fit_modal_width(inner, screen_width)
    }
}

impl Modal for SelectModal {
    type Result = Vec<usize>;

    fn render(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        // Calculate dynamic width
        let modal_width = self.calculate_modal_width(area.width);

        // Calculate prompt lines dynamically
        let prompt_lines = self.prompt.lines().count().max(1) as u16;

        // Calculate height:
        // 1 (top border) + N (prompt) + M (list) + 1 (bottom border)
        let list_height = self.items.len().min(20) as u16; // Limit to 20 items
        let modal_height = 1 + prompt_lines + list_height + 1;

        // Create centered area
        let modal_area = centered_rect_with_size(modal_width, modal_height, area);
        self.last_modal_area = Some(modal_area);
        let inner = render_modal_block(modal_area, buf, &self.title, theme);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(prompt_lines), // Prompt
                Constraint::Length(list_height),  // List
            ])
            .split(inner);

        let prompt = Paragraph::new(self.prompt.clone())
            .alignment(Alignment::Left)
            .style(Style::default().fg(theme.fg));
        prompt.render(chunks[0], buf);

        let row_width = chunks[1].width as usize;
        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(idx, label)| {
                let row = format!(" {label}");
                let pad = row_width.saturating_sub(str_display_width(&row));
                let style = if idx == self.cursor {
                    Style::default()
                        .fg(theme.bg)
                        .bg(theme.fg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg)
                };

                ListItem::new(Line::from(vec![
                    Span::styled(row, style),
                    Span::styled(" ".repeat(pad), style),
                ]))
            })
            .collect();

        let list = List::new(items).style(Style::default().bg(theme.bg));

        list.render(chunks[1], buf);

        // Save list area for mouse handling
        self.last_list_area = Some(chunks[1]);
    }

    fn handle_key(
        &mut self,
        chord: termide_core::KeyChord,
    ) -> Result<Option<ModalResult<Self::Result>>> {
        let key = chord.raw;
        match key.code {
            KeyCode::Esc => Ok(Some(ModalResult::Cancelled)),
            KeyCode::Up => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                Ok(None)
            }
            KeyCode::Down => {
                if self.cursor < self.items.len().saturating_sub(1) {
                    self.cursor += 1;
                }
                Ok(None)
            }
            KeyCode::Home => {
                self.cursor = 0;
                Ok(None)
            }
            KeyCode::End => {
                self.cursor = self.items.len().saturating_sub(1);
                Ok(None)
            }
            KeyCode::Enter => Ok(Some(ModalResult::Confirmed(vec![self.cursor]))),
            _ => Ok(None),
        }
    }

    fn handle_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        _modal_area: Rect,
    ) -> Result<Option<ModalResult<Self::Result>>> {
        use crate::{check_mouse_click, MouseClickResult};
        use crossterm::event::MouseEventKind;

        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.cursor = self.cursor.saturating_sub(3);
                return Ok(None);
            }
            MouseEventKind::ScrollDown => {
                let last = self.items.len().saturating_sub(1);
                self.cursor = (self.cursor + 3).min(last);
                return Ok(None);
            }
            _ => {}
        }

        // Only handle left button press
        if mouse.kind != MouseEventKind::Down(crossterm::event::MouseButton::Left) {
            return Ok(None);
        }

        match check_mouse_click(
            mouse.column,
            mouse.row,
            self.last_modal_area,
            self.last_list_area,
            0, // No scroll offset in simple select
        ) {
            // A click beside the modal dismisses it, as Esc does.
            MouseClickResult::OutsideModal => Ok(Some(ModalResult::Cancelled)),
            MouseClickResult::OutsideList => Ok(None),
            MouseClickResult::OnListItem(clicked_index) => {
                if clicked_index < self.items.len() {
                    // Item clicked - select and confirm immediately
                    self.cursor = clicked_index;
                    Ok(Some(ModalResult::Confirmed(vec![self.cursor])))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_row_under_the_cursor_needs_no_marker() {
        let mut modal = SelectModal::single("Mode", "", vec!["● ask".into(), "  auto".into()]);
        modal.set_cursor(1);
        let area = Rect::new(0, 0, 40, 12);
        let mut buf = Buffer::empty(area);
        modal.render(area, &mut buf, &Theme::default());
        let rows: Vec<String> = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        // Inverted, the cursor row reads as the others do: one column of
        // padding, no `▶`, so the current value's `●` stands alone.
        assert!(rows.iter().all(|r| !r.contains('▶')), "{rows:?}");
        let row = rows.iter().find(|r| r.contains("auto")).unwrap();
        assert!(row.contains("│   auto"), "{row:?}");
        assert!(rows.iter().any(|r| r.contains("│ ● ask")), "{rows:?}");
    }

    #[test]
    fn the_width_follows_the_content_and_the_cursor_row_spans_it() {
        let labels = vec![
            "● ask — спрашивать обо всём, без разрешений из настроек".to_string(),
            "  all — разрешать всё".to_string(),
        ];
        let widest = str_display_width(&labels[0]) as u16;
        let mut modal = SelectModal::single("Режим разрешений", "", labels);
        modal.set_cursor(1);
        let area = Rect::new(0, 0, 200, 12);
        let mut buf = Buffer::empty(area);
        modal.render(area, &mut buf, &Theme::default());
        // Columns, not bytes: the widest label, a column of padding on
        // either side, and the two borders.
        let left = (0..area.width)
            .find(|&x| buf[(x, 4)].symbol() == "│")
            .unwrap();
        let right = (left + 1..area.width)
            .find(|&x| buf[(x, 4)].symbol() == "│")
            .unwrap();
        assert_eq!(right - left + 1, widest + 4);
        // The cursor row is inverted from border to border.
        let y = (0..area.height)
            .find(|&y| {
                (left..right)
                    .any(|x| buf[(x, y)].symbol() == "a" && buf[(x + 1, y)].symbol() == "l")
            })
            .unwrap();
        let theme = Theme::default();
        assert!((left + 1..right).all(|x| buf[(x, y)].bg == theme.fg));
    }
}
