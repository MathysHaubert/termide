//! Visual cursor movement operations (word wrap aware).
//!
//! This module provides cursor movement that accounts for word wrapping.

use termide_buffer::{Cursor, TextBuffer};
use unicode_segmentation::UnicodeSegmentation;

use crate::word_wrap;

/// Helper to calculate visual row boundaries from wrap points.
fn get_visual_row_bounds(
    visual_row: usize,
    wrap_points: &[usize],
    line_len: usize,
) -> (usize, usize) {
    let start = if visual_row == 0 {
        0
    } else if visual_row - 1 < wrap_points.len() {
        wrap_points[visual_row - 1]
    } else {
        line_len
    };

    let end = if visual_row < wrap_points.len() {
        wrap_points[visual_row]
    } else {
        line_len
    };

    (start, end)
}

/// Calculate column position within visual row bounds.
///
/// `preferred_col` is the visual offset in screen columns from the start of
/// a visual row. `line_len` is the total grapheme count of the physical line.
/// Returns the grapheme drawn at that offset on the row of `line_text`,
/// clamped to row bounds.
/// On the last visual row (where visual_row_end == line_len), cursor can be at line_len.
/// On intermediate rows, cursor stops before the wrap point.
fn column_in_visual_row(
    line_text: &str,
    preferred_col: usize,
    visual_row_start: usize,
    visual_row_end: usize,
    line_len: usize,
    tab_size: usize,
) -> usize {
    let max_col = if visual_row_end == line_len {
        visual_row_end
    } else {
        visual_row_end.saturating_sub(1)
    };
    word_wrap::column_at_row_offset(
        line_text,
        visual_row_start,
        max_col.max(visual_row_start),
        preferred_col,
        tab_size,
    )
}

/// Move cursor up by one visual line.
///
/// Returns new cursor position if movement occurred, None otherwise.
/// `preferred_column` is the visual offset within a visual row (0-based).
pub fn move_up(
    cursor: &Cursor,
    buffer: &TextBuffer,
    preferred_column: Option<usize>,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> Option<Cursor> {
    // Calculate visual offset from current position if not provided
    let visual_offset = preferred_column.unwrap_or_else(|| {
        if let Some(line_text) = buffer.line(cursor.line) {
            let line_text = line_text.trim_end_matches('\n');
            let line_len = line_text.graphemes(true).count();
            let cursor_col = cursor.column.min(line_len);
            let (_visual_rows, wrap_points) =
                word_wrap::get_line_wrap_points(line_text, content_width, use_smart_wrap, tab_size);
            let current_visual_row = wrap_points.iter().filter(|&&wp| wp <= cursor_col).count();
            let (visual_row_start, _) =
                get_visual_row_bounds(current_visual_row, &wrap_points, line_len);
            word_wrap::row_offset_columns(line_text, visual_row_start, cursor_col, tab_size)
        } else {
            cursor.column
        }
    });

    // Try to move within current line first
    if let Some(line_text) = buffer.line(cursor.line) {
        let line_text = line_text.trim_end_matches('\n');
        let line_len = line_text.graphemes(true).count();
        let cursor_col = cursor.column.min(line_len);

        let (_visual_rows, wrap_points) =
            word_wrap::get_line_wrap_points(line_text, content_width, use_smart_wrap, tab_size);

        let current_visual_row = wrap_points.iter().filter(|&&wp| wp <= cursor_col).count();

        if current_visual_row > 0 {
            // Move up within same physical line
            let target_visual_row = current_visual_row - 1;
            let (visual_row_start, visual_row_end) =
                get_visual_row_bounds(target_visual_row, &wrap_points, line_len);
            let new_col = column_in_visual_row(
                line_text,
                visual_offset,
                visual_row_start,
                visual_row_end,
                line_len,
                tab_size,
            );
            return Some(Cursor::at(cursor.line, new_col));
        }
    }

    // Move to previous physical line
    if cursor.line > 0 {
        let new_line = cursor.line - 1;

        if let Some(line_text) = buffer.line(new_line) {
            let line_text = line_text.trim_end_matches('\n');
            let line_len = line_text.graphemes(true).count();

            if line_len == 0 {
                return Some(Cursor::at(new_line, 0));
            }

            let (visual_rows, wrap_points) =
                word_wrap::get_line_wrap_points(line_text, content_width, use_smart_wrap, tab_size);
            let last_visual_row = visual_rows - 1;

            let (visual_row_start, visual_row_end) =
                get_visual_row_bounds(last_visual_row, &wrap_points, line_len);
            let new_col = column_in_visual_row(
                line_text,
                visual_offset,
                visual_row_start,
                visual_row_end,
                line_len,
                tab_size,
            );
            return Some(Cursor::at(new_line, new_col));
        }
    }

    None
}

/// Move cursor down by one visual line.
///
/// Returns new cursor position if movement occurred, None otherwise.
/// `preferred_column` is the visual offset within a visual row (0-based).
pub fn move_down(
    cursor: &Cursor,
    buffer: &TextBuffer,
    preferred_column: Option<usize>,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> Option<Cursor> {
    // Calculate visual offset from current position if not provided
    let visual_offset = preferred_column.unwrap_or_else(|| {
        if let Some(line_text) = buffer.line(cursor.line) {
            let line_text = line_text.trim_end_matches('\n');
            let line_len = line_text.graphemes(true).count();
            let cursor_col = cursor.column.min(line_len);
            let (_visual_rows, wrap_points) =
                word_wrap::get_line_wrap_points(line_text, content_width, use_smart_wrap, tab_size);
            let current_visual_row = wrap_points.iter().filter(|&&wp| wp <= cursor_col).count();
            let (visual_row_start, _) =
                get_visual_row_bounds(current_visual_row, &wrap_points, line_len);
            word_wrap::row_offset_columns(line_text, visual_row_start, cursor_col, tab_size)
        } else {
            cursor.column
        }
    });

    // Try to move within current line first
    if let Some(line_text) = buffer.line(cursor.line) {
        let line_text = line_text.trim_end_matches('\n');
        let line_len = line_text.graphemes(true).count();
        let cursor_col = cursor.column.min(line_len);

        let (total_visual_rows, wrap_points) =
            word_wrap::get_line_wrap_points(line_text, content_width, use_smart_wrap, tab_size);

        let current_visual_row = wrap_points.iter().filter(|&&wp| wp <= cursor_col).count();

        if current_visual_row + 1 < total_visual_rows {
            // Move down within same physical line
            let target_visual_row = current_visual_row + 1;
            let (visual_row_start, visual_row_end) =
                get_visual_row_bounds(target_visual_row, &wrap_points, line_len);
            let new_col = column_in_visual_row(
                line_text,
                visual_offset,
                visual_row_start,
                visual_row_end,
                line_len,
                tab_size,
            );
            return Some(Cursor::at(cursor.line, new_col));
        }
    }

    // Move to next physical line
    let max_line = buffer.line_count().saturating_sub(1);
    if cursor.line < max_line {
        let new_line = cursor.line + 1;

        if let Some(line_text) = buffer.line(new_line) {
            let line_text = line_text.trim_end_matches('\n');
            let line_len = line_text.graphemes(true).count();

            if line_len == 0 {
                return Some(Cursor::at(new_line, 0));
            }

            let (_visual_rows, wrap_points) =
                word_wrap::get_line_wrap_points(line_text, content_width, use_smart_wrap, tab_size);

            // First visual row
            let visual_row_end = if !wrap_points.is_empty() {
                wrap_points[0]
            } else {
                line_len
            };

            let new_col = column_in_visual_row(
                line_text,
                visual_offset,
                0,
                visual_row_end,
                line_len,
                tab_size,
            );
            return Some(Cursor::at(new_line, new_col));
        }
    }

    None
}

/// Move cursor to start of current visual line.
///
/// Returns new column position.
pub fn move_to_visual_line_start(
    cursor: &Cursor,
    buffer: &TextBuffer,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> usize {
    if let Some(line_text) = buffer.line(cursor.line) {
        let line_text = line_text.trim_end_matches('\n');
        let line_len = line_text.graphemes(true).count();
        let cursor_col = cursor.column.min(line_len);

        let (_visual_rows, wrap_points) =
            word_wrap::get_line_wrap_points(line_text, content_width, use_smart_wrap, tab_size);

        // Find which visual row the cursor is on
        let current_visual_row = wrap_points.iter().filter(|&&wp| wp <= cursor_col).count();

        // Get start of this visual row
        let (visual_row_start, _) =
            get_visual_row_bounds(current_visual_row, &wrap_points, line_len);
        return visual_row_start;
    }

    0
}

/// Move cursor to end of current visual line.
///
/// Returns new column position.
pub fn move_to_visual_line_end(
    cursor: &Cursor,
    buffer: &TextBuffer,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> usize {
    if let Some(line_text) = buffer.line(cursor.line) {
        let line_text = line_text.trim_end_matches('\n');
        let line_len = line_text.graphemes(true).count();
        let cursor_col = cursor.column.min(line_len);

        let (_visual_rows, wrap_points) =
            word_wrap::get_line_wrap_points(line_text, content_width, use_smart_wrap, tab_size);

        // Find which visual row the cursor is on
        let current_visual_row = wrap_points.iter().filter(|&&wp| wp <= cursor_col).count();

        // Get end of this visual row
        let (_, visual_row_end) = get_visual_row_bounds(current_visual_row, &wrap_points, line_len);

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

/// Move cursor up by page_size visual lines.
///
/// Returns final cursor position after moving up by page_size steps or until top of document.
/// If less than a page remains to start, moves cursor to (0, 0).
pub fn page_up(
    cursor: &Cursor,
    buffer: &TextBuffer,
    preferred_column: Option<usize>,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
    page_size: usize,
) -> Cursor {
    let mut current_cursor = *cursor;
    let mut moves_made = 0;

    for _ in 0..page_size {
        let prev_cursor = current_cursor;

        if let Some(new_cursor) = move_up(
            &current_cursor,
            buffer,
            preferred_column,
            content_width,
            use_smart_wrap,
            tab_size,
        ) {
            current_cursor = new_cursor;
            moves_made += 1;
        }

        // Stop if we haven't moved (at top of document)
        if current_cursor == prev_cursor {
            break;
        }
    }

    // If we moved less than a full page, we're near top - go to beginning
    if moves_made < page_size {
        return Cursor::at(0, 0);
    }

    current_cursor
}

/// Move cursor down by page_size visual lines.
///
/// Returns final cursor position after moving down by page_size steps or until bottom of document.
/// If less than a page remains to end, moves cursor to end of file.
pub fn page_down(
    cursor: &Cursor,
    buffer: &TextBuffer,
    preferred_column: Option<usize>,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
    page_size: usize,
) -> Cursor {
    let mut current_cursor = *cursor;
    let max_line = buffer.line_count().saturating_sub(1);
    let mut moves_made = 0;

    for _ in 0..page_size {
        let prev_cursor = current_cursor;

        if let Some(new_cursor) = move_down(
            &current_cursor,
            buffer,
            preferred_column,
            content_width,
            use_smart_wrap,
            tab_size,
        ) {
            current_cursor = new_cursor;
            moves_made += 1;
        }

        // Stop if we haven't moved (at bottom of document)
        if current_cursor == prev_cursor {
            break;
        }

        // Stop if we reached the last line
        if current_cursor.line >= max_line {
            break;
        }
    }

    // If we moved less than a full page, we're near bottom - go to end of file
    if moves_made < page_size {
        let last_line_len = buffer.line_len_graphemes(max_line);
        return Cursor::at(max_line, last_line_len);
    }

    current_cursor
}
