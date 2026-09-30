//! Physical cursor movement operations.
//!
//! This module provides basic cursor movement that doesn't account for word wrap.

use termide_buffer::{Cursor, TextBuffer};
use unicode_segmentation::UnicodeSegmentation;

use crate::word_boundary;

/// Move cursor up by one line. The column is the caller's to settle (see
/// `Editor::begin_vertical_move`).
pub(crate) fn move_up(cursor: &mut Cursor) {
    cursor.move_up(1);
}

/// Move cursor down by one line. The column is the caller's to settle (see
/// `Editor::begin_vertical_move`).
pub(crate) fn move_down(cursor: &mut Cursor, buffer: &TextBuffer) {
    let max_line = buffer.line_count().saturating_sub(1);
    cursor.move_down(1, max_line);
}

/// Move cursor left by one character.
///
/// Returns true to reset preferred column (horizontal movement).
pub fn move_left(cursor: &mut Cursor, buffer: &TextBuffer) -> bool {
    cursor.move_left(1);

    // Clamp cursor to line length
    if cursor.line < buffer.line_count() {
        let line_len = buffer.line_len_graphemes(cursor.line);
        cursor.clamp_column(line_len);
    }

    false // Reset preferred column
}

/// Move cursor right by one character.
///
/// Returns true to reset preferred column (horizontal movement).
pub fn move_right(cursor: &mut Cursor, buffer: &TextBuffer) -> bool {
    let line_len = buffer.line_len_graphemes(cursor.line);
    let max_line = buffer.line_count().saturating_sub(1);
    cursor.move_right(1, line_len, max_line);
    false // Reset preferred column
}

/// Move cursor to start of current line.
///
/// Returns true to reset preferred column.
pub fn move_to_line_start(cursor: &mut Cursor) -> bool {
    cursor.column = 0;
    false // Reset preferred column
}

/// Move cursor to end of current line.
///
/// Returns true to reset preferred column.
pub fn move_to_line_end(cursor: &mut Cursor, buffer: &TextBuffer) -> bool {
    let line_len = buffer.line_len_graphemes(cursor.line);
    cursor.column = line_len;
    false // Reset preferred column
}

/// Move cursor to start of document.
///
/// Returns (new_cursor, should_scroll_viewport_to_top).
pub fn move_to_document_start() -> (Cursor, bool) {
    (Cursor::at(0, 0), true)
}

/// Move cursor to end of document.
///
/// Returns (new_cursor, should_scroll_viewport_to_bottom).
pub fn move_to_document_end(buffer: &TextBuffer) -> (Cursor, bool) {
    let max_line = buffer.line_count().saturating_sub(1);
    let line_len = buffer.line_len_graphemes(max_line);
    (Cursor::at(max_line, line_len), true)
}

/// Move cursor forward by one word.
///
/// Jumps to the start of the next word. If no next word exists on the current line,
/// moves to the beginning of the next line.
/// Returns false to reset preferred column (horizontal movement).
pub fn move_word_forward(cursor: &mut Cursor, buffer: &TextBuffer) -> bool {
    if let Some(line) = buffer.line(cursor.line) {
        let line = line.trim_end_matches('\n');
        let graphemes: Vec<&str> = line.graphemes(true).collect();

        if let Some(next_pos) = word_boundary::find_next_word_start(&graphemes, cursor.column) {
            cursor.column = next_pos;
            return false;
        }
    }

    // No next word on current line — move to start of next line
    let max_line = buffer.line_count().saturating_sub(1);
    if cursor.line < max_line {
        cursor.line += 1;
        cursor.column = 0;
    } else {
        // At last line — move to end
        cursor.column = buffer.line_len_graphemes(cursor.line);
    }
    false
}

/// Move cursor backward by one word.
///
/// Jumps to the start of the previous word. If no previous word exists on the current line,
/// moves to the end of the previous line.
/// Returns false to reset preferred column (horizontal movement).
pub fn move_word_backward(cursor: &mut Cursor, buffer: &TextBuffer) -> bool {
    if cursor.column > 0 {
        if let Some(line) = buffer.line(cursor.line) {
            let line = line.trim_end_matches('\n');
            let graphemes: Vec<&str> = line.graphemes(true).collect();

            if let Some(prev_pos) = word_boundary::find_prev_word_start(&graphemes, cursor.column) {
                cursor.column = prev_pos;
                return false;
            }
        }
    }

    // No previous word on current line — move to end of previous line
    if cursor.line > 0 {
        cursor.line -= 1;
        cursor.column = buffer.line_len_graphemes(cursor.line);
    } else {
        cursor.column = 0;
    }
    false
}

/// Clamp cursor position to valid buffer bounds.
pub fn clamp_cursor(cursor: &mut Cursor, buffer: &TextBuffer) {
    let max_line = buffer.line_count().saturating_sub(1);
    if cursor.line > max_line {
        cursor.line = max_line;
    }

    let line_len = buffer.line_len_graphemes(cursor.line);
    cursor.clamp_column(line_len);
}
