//! Calendar modal dialog.
//!
//! Displays a monthly calendar with day navigation.
//! Opened by clicking the clock in the menu bar.

use anyhow::Result;
use chrono::{Datelike, Local, NaiveDate};
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};

use crate::base::render_modal_block;
use crate::{centered_rect_with_size, Modal, ModalResult};
use termide_i18n as i18n;
use termide_theme::Theme;

/// Width of the calendar modal (borders + 7 columns × 3 chars + padding).
const CALENDAR_WIDTH: u16 = 24;
/// Height: border(1) + weekday header(1) + 6 week rows + border(1) = 9
const CALENDAR_HEIGHT: u16 = 9;

/// Calendar modal — shows a monthly calendar grid.
///
/// Navigation uses a grid-based cursor (0..41 for 6×7 cells).
/// Left/Right move within the visible grid without switching months.
/// Up/Down switch months only when leaving the visible rows.
#[derive(Debug)]
pub struct CalendarModal {
    year: i32,
    month: u32,
    /// Position in the 6×7 grid (0 = top-left Monday of first week)
    cursor_pos: usize,
    today: NaiveDate,
    /// Cached area for mouse hit-testing
    last_area: Option<Rect>,
    /// Optional anchor position (x, y) instead of centering
    anchor: Option<(u16, u16)>,
}

impl Default for CalendarModal {
    fn default() -> Self {
        Self::new()
    }
}

impl CalendarModal {
    /// Create a new calendar modal showing the current month.
    pub fn new() -> Self {
        let today = Local::now().date_naive();
        let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
        let offset = first.weekday().num_days_from_monday() as usize;
        Self {
            year: today.year(),
            month: today.month(),
            cursor_pos: offset + today.day() as usize - 1,
            today,
            last_area: None,
            anchor: None,
        }
    }

    /// Position calendar at anchor point instead of centering.
    pub fn with_anchor(mut self, x: u16, y: u16) -> Self {
        self.anchor = Some((x, y));
        self
    }

    /// First day of the displayed month.
    fn first_of_month(&self) -> NaiveDate {
        NaiveDate::from_ymd_opt(self.year, self.month, 1).unwrap()
    }

    /// Number of days in the displayed month.
    fn days_in_month(&self) -> u32 {
        let first = self.first_of_month();
        if self.month == 12 {
            NaiveDate::from_ymd_opt(self.year + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(self.year, self.month + 1, 1)
        }
        .unwrap()
        .signed_duration_since(first)
        .num_days() as u32
    }

    /// Grid offset of the 1st day of the month (0 = Monday).
    fn start_offset(&self) -> usize {
        self.first_of_month().weekday().num_days_from_monday() as usize
    }

    /// Index of last row that contains at least one day of current month.
    fn last_row(&self) -> usize {
        (self.start_offset() + self.days_in_month() as usize - 1) / 7
    }

    /// Move to the previous month, keeping cursor column.
    fn prev_month(&mut self) {
        let col = self.cursor_pos % 7;
        if self.month == 1 {
            self.month = 12;
            self.year -= 1;
        } else {
            self.month -= 1;
        }
        // Place cursor on last row, same column
        self.cursor_pos = self.last_row() * 7 + col;
    }

    /// Move to the next month, keeping cursor column.
    fn next_month(&mut self) {
        let col = self.cursor_pos % 7;
        if self.month == 12 {
            self.month = 1;
            self.year += 1;
        } else {
            self.month += 1;
        }
        // Place cursor on first row, same column
        self.cursor_pos = col;
    }

    /// Jump to today.
    fn go_today(&mut self) {
        self.year = self.today.year();
        self.month = self.today.month();
        let offset = self.start_offset();
        self.cursor_pos = offset + self.today.day() as usize - 1;
    }

    /// Day number for a grid cell (1-based for current month, negative/overflow for adjacent).
    fn cell_day(&self, cell_idx: usize) -> i32 {
        cell_idx as i32 - self.start_offset() as i32 + 1
    }

    /// Check if a given day of the current month is today.
    fn is_today(&self, day: u32) -> bool {
        self.today.year() == self.year
            && self.today.month() == self.month
            && self.today.day() == day
    }

    /// Cursor is in leftmost column (Monday).
    pub fn at_left_edge(&self) -> bool {
        self.cursor_pos.is_multiple_of(7)
    }

    /// Cursor is in rightmost column (Sunday).
    pub fn at_right_edge(&self) -> bool {
        self.cursor_pos % 7 == 6
    }
}

impl Modal for CalendarModal {
    type Result = ();

    fn render(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let modal_area = if let Some((ax, ay)) = self.anchor {
            let x = ax.min(area.width.saturating_sub(CALENDAR_WIDTH));
            let y = ay.min(area.height.saturating_sub(CALENDAR_HEIGHT));
            Rect {
                x,
                y,
                width: CALENDAR_WIDTH,
                height: CALENDAR_HEIGHT,
            }
        } else {
            centered_rect_with_size(CALENDAR_WIDTH, CALENDAR_HEIGHT, area)
        };
        self.last_area = Some(modal_area);

        let t = i18n::t();
        let month_names = [
            t.calendar_january(),
            t.calendar_february(),
            t.calendar_march(),
            t.calendar_april(),
            t.calendar_may(),
            t.calendar_june(),
            t.calendar_july(),
            t.calendar_august(),
            t.calendar_september(),
            t.calendar_october(),
            t.calendar_november(),
            t.calendar_december(),
        ];
        let title = format!("{} {}", month_names[(self.month - 1) as usize], self.year);

        let inner = render_modal_block(modal_area, buf, &title, theme);

        if inner.width < 2 || inner.height < 2 {
            return;
        }

        let default_style = Style::default().fg(theme.fg).bg(theme.bg);
        let header_style = Style::default()
            .fg(theme.accented_fg)
            .bg(theme.bg)
            .add_modifier(Modifier::BOLD);
        let today_style = Style::default()
            .fg(theme.accented_fg)
            .bg(theme.bg)
            .add_modifier(Modifier::BOLD);
        let selected_style = Style::default().fg(theme.selected_fg).bg(theme.selected_bg);
        let dim_style = Style::default().fg(theme.disabled).bg(theme.bg);

        // Row 0: Weekday headers (Monday-first)
        let weekday_header = Line::from(vec![Span::styled(
            format!(
                " {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2}",
                t.calendar_mon(),
                t.calendar_tue(),
                t.calendar_wed(),
                t.calendar_thu(),
                t.calendar_fri(),
                t.calendar_sat(),
                t.calendar_sun(),
            ),
            header_style,
        )]);
        let header_area = Rect::new(inner.x, inner.y, inner.width, 1);
        Paragraph::new(weekday_header).render(header_area, buf);

        // Rows 1-6: Day grid
        let total_days = self.days_in_month();
        let prev_month_last_day = self
            .first_of_month()
            .pred_opt()
            .map(|d| d.day())
            .unwrap_or(28);

        for week in 0..6u16 {
            let row_y = inner.y + 1 + week;
            if row_y >= inner.y + inner.height {
                break;
            }

            let mut spans = Vec::with_capacity(8);
            spans.push(Span::styled(" ", default_style));

            for weekday in 0..7usize {
                let cell_idx = week as usize * 7 + weekday;
                let day = self.cell_day(cell_idx);
                let is_cursor = cell_idx == self.cursor_pos;

                let (display_day, base_style) = if day < 1 {
                    // Previous month
                    let d = prev_month_last_day as i32 + day;
                    (d as u32, dim_style)
                } else if day > total_days as i32 {
                    // Next month
                    let d = day - total_days as i32;
                    (d as u32, dim_style)
                } else {
                    // Current month
                    let d = day as u32;
                    let s = if self.is_today(d) {
                        today_style
                    } else {
                        default_style
                    };
                    (d, s)
                };

                let style = if is_cursor {
                    if day >= 1 && day <= total_days as i32 && self.is_today(day as u32) {
                        selected_style.add_modifier(Modifier::BOLD)
                    } else {
                        selected_style
                    }
                } else {
                    base_style
                };

                spans.push(Span::styled(format!("{:>2} ", display_day), style));
            }

            let line = Line::from(spans);
            let row_area = Rect::new(inner.x, row_y, inner.width, 1);
            Paragraph::new(line).render(row_area, buf);
        }
    }

    fn handle_key(
        &mut self,
        chord: termide_core::KeyChord,
    ) -> Result<Option<ModalResult<Self::Result>>> {
        // No text input here: letter keys are shortcuts, matched on the
        // layout-normalized form so they work on a Cyrillic layout too.
        let key = chord.canonical;
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                return Ok(Some(ModalResult::Cancelled));
            }
            KeyCode::Left => {
                if !self.at_left_edge() {
                    self.cursor_pos -= 1;
                }
            }
            KeyCode::Right => {
                if !self.at_right_edge() {
                    self.cursor_pos += 1;
                }
            }
            KeyCode::Up => {
                if self.cursor_pos >= 7 {
                    self.cursor_pos -= 7;
                } else {
                    // Above first row → previous month
                    self.prev_month();
                }
            }
            KeyCode::Down => {
                if self.cursor_pos / 7 < self.last_row() {
                    self.cursor_pos += 7;
                } else {
                    // Below last row → next month
                    self.next_month();
                }
            }
            KeyCode::PageUp | KeyCode::Char('h') => self.prev_month(),
            KeyCode::PageDown | KeyCode::Char('l') => self.next_month(),
            KeyCode::Home | KeyCode::Char('t') => self.go_today(),
            _ => {}
        }
        Ok(None)
    }

    fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        modal_area: Rect,
    ) -> Result<Option<ModalResult<Self::Result>>> {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Ok(None);
        }

        let x = mouse.column;
        let y = mouse.row;

        // Check if click is outside modal
        if x < modal_area.x
            || x >= modal_area.x + modal_area.width
            || y < modal_area.y
            || y >= modal_area.y + modal_area.height
        {
            return Ok(Some(ModalResult::Cancelled));
        }

        // Inner area (1 pixel border)
        let inner_x = modal_area.x + 1;
        let inner_y = modal_area.y + 1;

        // Day grid rows (inner_y + 1 through inner_y + 6)
        if y > inner_y && y < inner_y + 7 {
            let week = (y - inner_y - 1) as usize;
            let col_offset = x.saturating_sub(inner_x + 1);
            let weekday = (col_offset / 3) as usize;
            if weekday < 7 {
                self.cursor_pos = week * 7 + weekday;
            }
        }

        Ok(None)
    }
}
