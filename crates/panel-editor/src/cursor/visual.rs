//! Visual cursor movement operations (word wrap aware).
//!
//! This module provides cursor movement that accounts for word wrapping.

use termide_buffer::{Cursor, TextBuffer};
use unicode_segmentation::UnicodeSegmentation;

use crate::word_wrap::{self, WrapLayout};

/// Wrap points and grapheme count of `line`, computed without a cache, for
/// [`word_wrap::step_up`] / [`word_wrap::step_down`].
fn computed_rows(
    buffer: &TextBuffer,
    layout: WrapLayout,
) -> impl FnMut(usize) -> (Vec<usize>, usize) + '_ {
    move |line| {
        let text = buffer.line(line).unwrap_or_default();
        let text = text.trim_end_matches('\n');
        let (_, wrap_points) = word_wrap::get_line_wrap_points(text, layout);
        (wrap_points, text.graphemes(true).count())
    }
}

/// Move cursor up by one visual line.
///
/// Returns new cursor position if movement occurred, None otherwise.
/// `preferred_column` is the visual offset within a visual row, in screen
/// columns.
pub fn move_up(
    cursor: &Cursor,
    buffer: &TextBuffer,
    preferred_column: Option<usize>,
    layout: WrapLayout,
) -> Option<Cursor> {
    let WrapLayout { tab_size, .. } = layout;
    let mut rows = computed_rows(buffer, layout);
    word_wrap::step_up(
        buffer,
        (cursor.line, cursor.column),
        preferred_column,
        tab_size,
        &mut rows,
    )
    .map(|(line, column)| Cursor::at(line, column))
}

/// Move cursor down by one visual line.
///
/// Returns new cursor position if movement occurred, None otherwise.
/// `preferred_column` is the visual offset within a visual row, in screen
/// columns.
pub fn move_down(
    cursor: &Cursor,
    buffer: &TextBuffer,
    preferred_column: Option<usize>,
    layout: WrapLayout,
) -> Option<Cursor> {
    let WrapLayout { tab_size, .. } = layout;
    let mut rows = computed_rows(buffer, layout);
    word_wrap::step_down(
        buffer,
        (cursor.line, cursor.column),
        preferred_column,
        tab_size,
        &mut rows,
    )
    .map(|(line, column)| Cursor::at(line, column))
}

/// Move cursor to start of current visual line.
///
/// Returns new column position.
pub fn move_to_visual_line_start(
    cursor: &Cursor,
    buffer: &TextBuffer,
    layout: WrapLayout,
) -> usize {
    if let Some(line_text) = buffer.line(cursor.line) {
        let line_text = line_text.trim_end_matches('\n');
        let line_len = line_text.graphemes(true).count();
        let cursor_col = cursor.column.min(line_len);

        let (_visual_rows, wrap_points) = word_wrap::get_line_wrap_points(line_text, layout);

        // Find which visual row the cursor is on
        let current_visual_row = wrap_points.iter().filter(|&&wp| wp <= cursor_col).count();

        // Get start of this visual row
        let (visual_row_start, _) =
            word_wrap::get_visual_row_bounds(current_visual_row, &wrap_points, line_len);
        return visual_row_start;
    }

    0
}

/// Move cursor to end of current visual line.
///
/// Returns new column position.
pub fn move_to_visual_line_end(cursor: &Cursor, buffer: &TextBuffer, layout: WrapLayout) -> usize {
    if let Some(line_text) = buffer.line(cursor.line) {
        let line_text = line_text.trim_end_matches('\n');
        let line_len = line_text.graphemes(true).count();
        let cursor_col = cursor.column.min(line_len);

        let (_visual_rows, wrap_points) = word_wrap::get_line_wrap_points(line_text, layout);

        // Find which visual row the cursor is on
        let current_visual_row = wrap_points.iter().filter(|&&wp| wp <= cursor_col).count();

        // Get end of this visual row
        let (_, visual_row_end) =
            word_wrap::get_visual_row_bounds(current_visual_row, &wrap_points, line_len);

        // For non-last visual rows, visual_row_end is the wrap point (first char of next row),
        // so we need to return the position before it
        let is_last_visual_row = current_visual_row >= wrap_points.len();
        if is_last_visual_row {
            return visual_row_end; // Last visual row — end of physical line
        } else {
            return visual_row_end.saturating_sub(1); // Before wrap point
        }
    }

    0
}
