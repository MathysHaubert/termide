//! Word wrap calculations for editor content.
//!
//! This module provides utilities for calculating line wrapping in the editor,
//! including smart wrapping (breaking at word boundaries) and hard wrapping
//! (breaking at fixed column width).

use std::collections::HashMap;

use lsp_types::Diagnostic;
use termide_buffer::{calculate_wrap_point, TextBuffer};
use termide_git::GitDiffCache;
use unicode_segmentation::UnicodeSegmentation;

/// Calculate wrap points for a single line of text.
///
/// Returns (visual_row_count, wrap_points) where wrap_points contains
/// the grapheme indices where each new visual line starts.
///
/// Uses display width and grapheme clusters for proper Unicode handling.
/// This function iterates exactly like rendering does to ensure consistency.
pub fn get_line_wrap_points(
    line_text: &str,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> (usize, Vec<usize>) {
    if content_width == 0 {
        return (1, Vec::new());
    }

    // Check display width, not char/grapheme count
    let display_width = termide_buffer::display_width(line_text, tab_size);
    if display_width == 0 {
        return (1, Vec::new());
    }

    if display_width <= content_width {
        return (1, Vec::new()); // No wrapping needed
    }

    // Iterate exactly like rendering does to ensure wrap points match
    let graphemes: Vec<&str> = line_text.graphemes(true).collect();
    let line_len = graphemes.len();
    let mut wrap_points = Vec::new();
    let mut grapheme_offset = 0;

    while grapheme_offset < line_len {
        let chunk_end = if use_smart_wrap {
            calculate_wrap_point(
                &graphemes,
                grapheme_offset,
                content_width,
                line_len,
                tab_size,
            )
        } else {
            calculate_simple_wrap_point(&graphemes, grapheme_offset, content_width, tab_size)
        };

        // Push chunk_end as start of NEXT visual line (not grapheme_offset!)
        if chunk_end > grapheme_offset && chunk_end < line_len {
            wrap_points.push(chunk_end);
        }

        // Prevent infinite loop
        if chunk_end == grapheme_offset {
            grapheme_offset += 1;
        } else {
            grapheme_offset = chunk_end;
        }
    }

    (wrap_points.len() + 1, wrap_points)
}

/// Calculate simple wrap point for a single visual line (iterative version).
///
/// Returns the grapheme index where the visual line ends.
/// This mirrors the logic in wrap_rendering.rs for consistency: tab stops
/// restart at `start`, the first column of the visual row.
fn calculate_simple_wrap_point(
    graphemes: &[&str],
    start: usize,
    max_width: usize,
    tab_size: usize,
) -> usize {
    let mut display_width = 0;

    for (i, grapheme) in graphemes.iter().enumerate().skip(start) {
        let grapheme_width = termide_buffer::grapheme_columns(grapheme, display_width, tab_size);

        if display_width + grapheme_width > max_width {
            return i;
        }

        display_width += grapheme_width;
    }

    graphemes.len()
}

/// Get which visual row within a single line the cursor is on (cached version).
///
/// Uses the wrap cache to avoid recalculating wrap points.
pub(crate) fn get_cursor_visual_row_in_line_cached(
    cache: &mut RenderingCache,
    buffer: &TextBuffer,
    line: usize,
    column: usize,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> usize {
    if content_width == 0 {
        return 0;
    }

    let (_, wrap_points) =
        get_line_wrap_points_cached(cache, buffer, line, content_width, use_smart_wrap, tab_size);

    // Clamp column to line length (using cached grapheme count)
    let line_len = get_line_grapheme_count_cached(
        cache,
        buffer,
        line,
        content_width,
        use_smart_wrap,
        tab_size,
    );
    let column_clamped = column.min(line_len);

    // Find which visual row contains the cursor column
    wrap_points.partition_point(|&wp| wp <= column_clamped)
}

/// Convert visual row to buffer position accounting for both word wrap and diagnostic virtual lines.
///
/// Returns (buffer_line, column_offset, chunk_end, is_virtual_line).
/// - `buffer_line`: The buffer line index
/// - `column_offset`: Grapheme index where this visual line starts
/// - `chunk_end`: Grapheme index where this visual line ends (exclusive)
/// - `is_virtual_line`: True if this row is a diagnostic virtual line
///
/// Diagnostic virtual lines appear after all wrapped segments of their associated buffer line.
/// Multi-row diagnostics are handled correctly.
pub fn visual_row_to_buffer_position_with_diagnostics(
    buffer: &TextBuffer,
    visual_row: usize,
    viewport_top: usize,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
    diagnostics: &[Diagnostic],
) -> (usize, usize, usize, bool) {
    // Group diagnostics by line with total row count (accounting for multi-row diagnostics)
    let diagnostics_by_line = count_diagnostic_rows_by_line(diagnostics, buffer, content_width);

    if content_width == 0 {
        // No wrap, but still need to account for diagnostic lines
        let mut current_visual_row = 0;
        let mut line_idx = viewport_top;

        while line_idx < buffer.line_count() {
            // Real line
            if current_visual_row == visual_row {
                let line_len = buffer
                    .line(line_idx)
                    .map(|s| s.trim_end_matches('\n').graphemes(true).count())
                    .unwrap_or(0);
                return (line_idx, 0, line_len, false);
            }
            current_visual_row += 1;

            // Diagnostic virtual rows for this line (may be more than number of diagnostics)
            let diag_row_count = diagnostics_by_line.get(&line_idx).copied().unwrap_or(0);
            for _ in 0..diag_row_count {
                if current_visual_row == visual_row {
                    let line_len = buffer
                        .line(line_idx)
                        .map(|s| s.trim_end_matches('\n').graphemes(true).count())
                        .unwrap_or(0);
                    return (line_idx, 0, line_len, true);
                }
                current_visual_row += 1;
            }

            line_idx += 1;
        }

        let last_line = buffer.line_count().saturating_sub(1);
        let line_len = buffer
            .line(last_line)
            .map(|s| s.trim_end_matches('\n').graphemes(true).count())
            .unwrap_or(0);
        return (last_line, 0, line_len, false);
    }

    let mut current_visual_row = 0;
    let mut line_idx = viewport_top;

    while line_idx < buffer.line_count() {
        if let Some(line_text) = buffer.line(line_idx) {
            let line_text = line_text.trim_end_matches('\n');
            let graphemes: Vec<&str> = line_text.graphemes(true).collect();
            let line_len = graphemes.len();

            // Handle empty lines (1 visual row)
            if line_len == 0 {
                if current_visual_row == visual_row {
                    return (line_idx, 0, 0, false);
                }
                current_visual_row += 1;
            } else {
                // Iterate through wrapped segments
                let mut grapheme_offset = 0;

                while grapheme_offset < line_len {
                    let chunk_end = if use_smart_wrap {
                        calculate_wrap_point(
                            &graphemes,
                            grapheme_offset,
                            content_width,
                            line_len,
                            tab_size,
                        )
                    } else {
                        calculate_simple_wrap_point(
                            &graphemes,
                            grapheme_offset,
                            content_width,
                            tab_size,
                        )
                    };

                    if current_visual_row == visual_row {
                        // Found the target visual row - it's a real line segment
                        return (line_idx, grapheme_offset, chunk_end, false);
                    }

                    current_visual_row += 1;

                    // Safety: prevent infinite loop
                    if chunk_end == grapheme_offset {
                        grapheme_offset += 1;
                    } else {
                        grapheme_offset = chunk_end;
                    }
                }
            }

            // After all wrapped segments, check for diagnostic virtual rows
            let diag_row_count = diagnostics_by_line.get(&line_idx).copied().unwrap_or(0);
            for _ in 0..diag_row_count {
                if current_visual_row == visual_row {
                    // This visual row is a diagnostic virtual line
                    return (line_idx, 0, line_len, true);
                }
                current_visual_row += 1;
            }
        } else {
            // If line doesn't exist, treat as empty (1 visual row)
            if current_visual_row == visual_row {
                return (line_idx, 0, 0, false);
            }
            current_visual_row += 1;
        }

        line_idx += 1;
    }

    // If we've exhausted all lines, return the last line
    let last_line = buffer.line_count().saturating_sub(1);
    let last_line_len = buffer
        .line(last_line)
        .map(|s| s.trim_end_matches('\n').graphemes(true).count())
        .unwrap_or(0);
    (last_line, 0, last_line_len, false)
}

/// Count total diagnostic visual rows per buffer line.
///
/// This accounts for multi-row diagnostic messages that wrap based on content_width.
pub(crate) fn count_diagnostic_rows_by_line(
    diagnostics: &[Diagnostic],
    _buffer: &TextBuffer,
    content_width: usize,
) -> HashMap<usize, usize> {
    use crate::git;
    use std::collections::HashSet;

    let mut result: HashMap<usize, usize> = HashMap::with_capacity(diagnostics.len());
    let mut seen: HashSet<(usize, u64)> = HashSet::with_capacity(diagnostics.len());

    for diag in diagnostics {
        let line = diag.range.start.line as usize;
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        diag.message.hash(&mut hasher);
        let key = (line, hasher.finish());
        if !seen.insert(key) {
            continue;
        }

        // Calculate diagnostic info similar to git::group_diagnostics_by_line
        let start_col = diag.range.start.character as usize;
        let end_col = diag.range.end.character as usize;

        // Get underline length (simplified - use end_col - start_col)
        let underline_len = end_col.saturating_sub(start_col).max(1);

        // Extract code
        let code = diag.code.as_ref().map(|c| match c {
            lsp_types::NumberOrString::Number(n) => n.to_string(),
            lsp_types::NumberOrString::String(s) => s.clone(),
        });

        // Calculate how many rows this diagnostic needs
        let rows = git::calculate_diagnostic_rows(
            start_col,
            underline_len,
            code.as_deref(),
            &diag.message,
            content_width,
        );

        *result.entry(line).or_insert(0) += rows;
    }

    result
}

// =============================================================================
// Cached Versions of Word Wrap Functions
// =============================================================================
//
// These functions use the RenderingCache to avoid redundant calculations.
// They check the cache first and only compute if needed.

use crate::state::rendering_cache::RenderingCache;

/// Get wrap points for a line, using cache if available.
///
/// Returns (visual_rows, wrap_points) for the given line.
/// Uses cache lookup first, computes and caches if miss.
/// Cache validation ensures data was computed with matching content_width and use_smart_wrap.
pub(crate) fn get_line_wrap_points_cached(
    cache: &mut RenderingCache,
    buffer: &TextBuffer,
    line: usize,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> (usize, Vec<usize>) {
    // Check if cache has valid data for this line with matching width settings
    if let Some(cached) = cache.get_wrap_data(line, content_width, use_smart_wrap, tab_size) {
        return (cached.visual_rows, cached.wrap_points.clone());
    }

    // Cache miss - compute wrap points
    let line_cow = buffer.line_cow(line);
    let line_text = line_cow
        .as_deref()
        .map(|s| s.trim_end_matches('\n'))
        .unwrap_or("");

    let grapheme_count = line_text.graphemes(true).count();
    let (visual_rows, wrap_points) =
        get_line_wrap_points(line_text, content_width, use_smart_wrap, tab_size);

    // Store in cache with width settings. The cache holds one tab size for
    // all its entries, so a lookup made with another one is not stored.
    if cache.tab_size == tab_size {
        cache.set_wrap_data(
            line,
            visual_rows,
            wrap_points.clone(),
            grapheme_count,
            content_width,
            use_smart_wrap,
        );
    }

    (visual_rows, wrap_points)
}

/// Get visual row count for a line, using cache if available.
///
/// This avoids the Vec<usize> clone that `get_line_wrap_points_cached` requires,
/// making it more efficient when only the row count is needed (e.g., scrolling calculations).
pub(crate) fn get_visual_rows_cached(
    cache: &mut RenderingCache,
    buffer: &TextBuffer,
    line: usize,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> usize {
    if let Some(cached) = cache.get_wrap_data(line, content_width, use_smart_wrap, tab_size) {
        return cached.visual_rows;
    }

    // Cache miss — compute and cache, return only visual_rows
    let (visual_rows, _) =
        get_line_wrap_points_cached(cache, buffer, line, content_width, use_smart_wrap, tab_size);
    visual_rows
}

/// Get the grapheme count for a line, using cache if available.
///
/// This avoids repeated `graphemes(true).count()` calls by returning
/// the count stored in the wrap cache.
pub(crate) fn get_line_grapheme_count_cached(
    cache: &mut RenderingCache,
    buffer: &TextBuffer,
    line: usize,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> usize {
    // Ensure wrap data is cached (populates grapheme_count)
    if cache
        .get_wrap_data(line, content_width, use_smart_wrap, tab_size)
        .is_none()
    {
        // Populate cache
        get_visual_rows_cached(cache, buffer, line, content_width, use_smart_wrap, tab_size);
    }

    cache
        .get_wrap_data(line, content_width, use_smart_wrap, tab_size)
        .map(|c| c.grapheme_count)
        .unwrap_or(0)
}

/// Calculate total visual rows in buffer using cumulative cache.
///
/// Uses O(1) lookup if cumulative cache is valid, otherwise builds cache first.
pub(crate) fn calculate_total_visual_rows_cached(
    cache: &mut RenderingCache,
    buffer: &TextBuffer,
    content_width: usize,
    word_wrap_enabled: bool,
    use_smart_wrap: bool,
    tab_size: usize,
) -> usize {
    if content_width == 0 || !word_wrap_enabled {
        return buffer.line_count();
    }

    // Update wrap settings (invalidates cache if changed)
    cache.update_wrap_settings(content_width, use_smart_wrap, tab_size);

    // Try to use cumulative cache (verify it covers all buffer lines)
    if cache.cumulative_covers_line_count(buffer.line_count()) {
        if let Some(total) = cache.get_total_visual_rows() {
            return total;
        }
    }

    // Build cumulative cache
    cache.build_cumulative_cache(buffer);

    cache.get_total_visual_rows().unwrap_or(buffer.line_count())
}

/// Text of buffer line `line` without its line break (empty past the end).
fn line_text_at(buffer: &TextBuffer, line: usize) -> String {
    buffer
        .line(line)
        .map(|s| s.trim_end_matches('\n').to_string())
        .unwrap_or_default()
}

/// Screen columns between grapheme `row_start` and grapheme `col` of `line`,
/// with tab stops restarting at `row_start` as on a drawn row: how far right
/// the cursor sits on its visual row.
pub(crate) fn row_offset_columns(
    line: &str,
    row_start: usize,
    col: usize,
    tab_size: usize,
) -> usize {
    line.graphemes(true)
        .take(col)
        .skip(row_start)
        .fold(0, |width, g| {
            width + termide_buffer::grapheme_columns(g, width, tab_size)
        })
}

/// The grapheme of `line` drawn `offset` screen columns right of grapheme
/// `row_start` (tab stops restarting there), no further than `max_col`:
/// where vertical movement lands to keep the cursor in the same screen
/// column. An offset inside a tab lands on the tab.
pub(crate) fn column_at_row_offset(
    line: &str,
    row_start: usize,
    max_col: usize,
    offset: usize,
    tab_size: usize,
) -> usize {
    let mut width = 0;
    for (idx, g) in line.graphemes(true).enumerate().skip(row_start) {
        if idx >= max_col {
            return max_col;
        }
        let cols = termide_buffer::grapheme_columns(g, width, tab_size);
        if width + cols > offset {
            return idx;
        }
        width += cols;
    }
    max_col.min(line.graphemes(true).count()).max(row_start)
}

/// Move cursor up by one visual line, using cached wrap data.
///
/// Returns Some((line, col)) if movement was possible, None if at top.
pub(crate) fn move_up_cached(
    cache: &mut RenderingCache,
    buffer: &TextBuffer,
    cursor_pos: (usize, usize),
    preferred_column: Option<usize>,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> Option<(usize, usize)> {
    use unicode_segmentation::UnicodeSegmentation;

    let (cursor_line, cursor_col) = cursor_pos;

    if content_width == 0 {
        // No word wrap - simple line movement
        if cursor_line == 0 {
            return None;
        }
        let offset = preferred_column.unwrap_or_else(|| {
            row_offset_columns(&line_text_at(buffer, cursor_line), 0, cursor_col, tab_size)
        });
        let target = line_text_at(buffer, cursor_line - 1);
        let line_len = target.graphemes(true).count();
        let col = column_at_row_offset(&target, 0, line_len, offset, tab_size);
        return Some((cursor_line - 1, col));
    }

    // Get wrap data for current line
    let (_, wrap_points) = get_line_wrap_points_cached(
        cache,
        buffer,
        cursor_line,
        content_width,
        use_smart_wrap,
        tab_size,
    );

    let line_len = get_line_grapheme_count_cached(
        cache,
        buffer,
        cursor_line,
        content_width,
        use_smart_wrap,
        tab_size,
    );
    let cursor_col = cursor_col.min(line_len);

    // Find which visual row within this line the cursor is on
    let current_visual_row = wrap_points.partition_point(|&wp| wp <= cursor_col);

    // Get bounds for current visual row
    let (visual_row_start, _) = get_visual_row_bounds(current_visual_row, &wrap_points, line_len);

    // Calculate visual offset (screen columns) within current visual row
    let cur_text = line_text_at(buffer, cursor_line);
    let visual_offset = preferred_column
        .unwrap_or_else(|| row_offset_columns(&cur_text, visual_row_start, cursor_col, tab_size));

    if current_visual_row > 0 {
        // Move up within same physical line
        let (prev_start, prev_end) =
            get_visual_row_bounds(current_visual_row - 1, &wrap_points, line_len);
        let max_col = prev_end.saturating_sub(1).max(prev_start);
        let new_col = column_at_row_offset(&cur_text, prev_start, max_col, visual_offset, tab_size);
        Some((cursor_line, new_col))
    } else if cursor_line > 0 {
        // Move to previous physical line
        let prev_line = cursor_line - 1;
        let (prev_visual_rows, prev_wrap_points) = get_line_wrap_points_cached(
            cache,
            buffer,
            prev_line,
            content_width,
            use_smart_wrap,
            tab_size,
        );

        let prev_line_len = get_line_grapheme_count_cached(
            cache,
            buffer,
            prev_line,
            content_width,
            use_smart_wrap,
            tab_size,
        );

        // Target the last visual row of previous line
        let target_visual_row = prev_visual_rows.saturating_sub(1);
        let (prev_start, prev_end) =
            get_visual_row_bounds(target_visual_row, &prev_wrap_points, prev_line_len);

        let max_col = if prev_end == prev_line_len {
            prev_end
        } else {
            prev_end.saturating_sub(1)
        };
        let new_col = column_at_row_offset(
            &line_text_at(buffer, prev_line),
            prev_start,
            max_col.max(prev_start),
            visual_offset,
            tab_size,
        );
        Some((prev_line, new_col))
    } else {
        None // At top of buffer
    }
}

/// Move cursor down by one visual line, using cached wrap data.
///
/// Returns Some((line, col)) if movement was possible, None if at bottom.
pub(crate) fn move_down_cached(
    cache: &mut RenderingCache,
    buffer: &TextBuffer,
    cursor_pos: (usize, usize),
    preferred_column: Option<usize>,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
) -> Option<(usize, usize)> {
    use unicode_segmentation::UnicodeSegmentation;

    let (cursor_line, cursor_col) = cursor_pos;
    let line_count = buffer.line_count();

    if content_width == 0 {
        // No word wrap - simple line movement
        if cursor_line + 1 >= line_count {
            return None;
        }
        let offset = preferred_column.unwrap_or_else(|| {
            row_offset_columns(&line_text_at(buffer, cursor_line), 0, cursor_col, tab_size)
        });
        let target = line_text_at(buffer, cursor_line + 1);
        let line_len = target.graphemes(true).count();
        let col = column_at_row_offset(&target, 0, line_len, offset, tab_size);
        return Some((cursor_line + 1, col));
    }

    // Get wrap data for current line
    let (total_visual_rows, wrap_points) = get_line_wrap_points_cached(
        cache,
        buffer,
        cursor_line,
        content_width,
        use_smart_wrap,
        tab_size,
    );

    let line_len = get_line_grapheme_count_cached(
        cache,
        buffer,
        cursor_line,
        content_width,
        use_smart_wrap,
        tab_size,
    );
    let cursor_col = cursor_col.min(line_len);

    // Find which visual row within this line the cursor is on
    let current_visual_row = wrap_points.partition_point(|&wp| wp <= cursor_col);

    // Get bounds for current visual row
    let (visual_row_start, _) = get_visual_row_bounds(current_visual_row, &wrap_points, line_len);

    // Calculate visual offset (screen columns) within current visual row
    let cur_text = line_text_at(buffer, cursor_line);
    let visual_offset = preferred_column
        .unwrap_or_else(|| row_offset_columns(&cur_text, visual_row_start, cursor_col, tab_size));

    if current_visual_row + 1 < total_visual_rows {
        // Move down within same physical line
        let (next_start, next_end) =
            get_visual_row_bounds(current_visual_row + 1, &wrap_points, line_len);
        // On the last visual row (end == line_len), cursor can be after the last char.
        // On intermediate rows, the char at the wrap point belongs to the next row.
        let max_col = if next_end == line_len {
            next_end
        } else {
            next_end.saturating_sub(1)
        };
        let new_col = column_at_row_offset(
            &cur_text,
            next_start,
            max_col.max(next_start),
            visual_offset,
            tab_size,
        );
        Some((cursor_line, new_col))
    } else if cursor_line + 1 < line_count {
        // Move to next physical line
        let next_line = cursor_line + 1;
        let (_, next_wrap_points) = get_line_wrap_points_cached(
            cache,
            buffer,
            next_line,
            content_width,
            use_smart_wrap,
            tab_size,
        );

        let next_line_len = get_line_grapheme_count_cached(
            cache,
            buffer,
            next_line,
            content_width,
            use_smart_wrap,
            tab_size,
        );

        // Target the first visual row of next line
        let (next_start, next_end) = get_visual_row_bounds(0, &next_wrap_points, next_line_len);

        let max_col = if next_end == next_line_len {
            next_end
        } else {
            next_end.saturating_sub(1)
        };
        let new_col = column_at_row_offset(
            &line_text_at(buffer, next_line),
            next_start,
            max_col.max(next_start),
            visual_offset,
            tab_size,
        );
        Some((next_line, new_col))
    } else {
        None // At bottom of buffer
    }
}

/// Page up by visual lines, using cached wrap data.
///
/// Returns (line, col) for the new cursor position.
#[allow(clippy::too_many_arguments)]
pub(crate) fn page_up_cached(
    cache: &mut RenderingCache,
    buffer: &TextBuffer,
    cursor_pos: (usize, usize),
    preferred_column: Option<usize>,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
    page_size: usize,
) -> (usize, usize) {
    let (mut line, mut col) = cursor_pos;

    for _ in 0..page_size {
        if let Some((new_line, new_col)) = move_up_cached(
            cache,
            buffer,
            (line, col),
            preferred_column,
            content_width,
            use_smart_wrap,
            tab_size,
        ) {
            line = new_line;
            col = new_col;
        } else {
            break; // At top
        }
    }

    (line, col)
}

/// Page down by visual lines, using cached wrap data.
///
/// Returns (line, col) for the new cursor position.
#[allow(clippy::too_many_arguments)]
pub(crate) fn page_down_cached(
    cache: &mut RenderingCache,
    buffer: &TextBuffer,
    cursor_pos: (usize, usize),
    preferred_column: Option<usize>,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
    page_size: usize,
) -> (usize, usize) {
    let (mut line, mut col) = cursor_pos;

    for _ in 0..page_size {
        if let Some((new_line, new_col)) = move_down_cached(
            cache,
            buffer,
            (line, col),
            preferred_column,
            content_width,
            use_smart_wrap,
            tab_size,
        ) {
            line = new_line;
            col = new_col;
        } else {
            break; // At bottom
        }
    }

    (line, col)
}

/// Helper: Get the start and end grapheme indices for a visual row.
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

/// Convert visual row to buffer position using cached wrap data.
///
/// This is the cached version of `visual_row_to_buffer_position_with_diagnostics`.
/// Uses the wrap cache to avoid redundant calculations.
///
/// Parameters:
/// - `cache`: The rendering cache for wrap data
/// - `buffer`: The text buffer
/// - `visual_row`: Visual row index (includes top_visual_row_offset if scrolled within a line)
/// - `viewport_top`: First visible buffer line
/// - `content_width`: Width for wrapping
/// - `use_smart_wrap`: Whether to use smart wrapping
/// - `diagnostics`: Diagnostic list for virtual lines
/// - `git_diff_cache`: Git diff cache for deletion markers
/// - `show_git_diff`: Whether git diff display is enabled
///
/// Returns (buffer_line, column_offset, chunk_end, is_virtual_line).
#[allow(clippy::too_many_arguments)]
pub(crate) fn visual_row_to_buffer_position_cached(
    cache: &mut RenderingCache,
    buffer: &TextBuffer,
    visual_row: usize,
    viewport_top: usize,
    content_width: usize,
    use_smart_wrap: bool,
    tab_size: usize,
    diagnostics: &[Diagnostic],
    git_diff_cache: &Option<GitDiffCache>,
    show_git_diff: bool,
) -> (usize, usize, usize, bool) {
    if content_width == 0 {
        // No wrap - delegate to non-cached version
        return visual_row_to_buffer_position_with_diagnostics(
            buffer,
            visual_row,
            viewport_top,
            content_width,
            use_smart_wrap,
            tab_size,
            diagnostics,
        );
    }

    // Ensure diagnostic rows cache is populated
    if !cache.is_diagnostic_cache_valid(content_width) {
        let map = count_diagnostic_rows_by_line(diagnostics, buffer, content_width);
        cache.set_diagnostic_rows_cache(map, content_width);
    }

    let mut current_visual_row = 0;
    let mut line_idx = viewport_top;

    while line_idx < buffer.line_count() {
        // Use cached wrap data
        let (visual_rows, wrap_points) = get_line_wrap_points_cached(
            cache,
            buffer,
            line_idx,
            content_width,
            use_smart_wrap,
            tab_size,
        );

        let line_len = get_line_grapheme_count_cached(
            cache,
            buffer,
            line_idx,
            content_width,
            use_smart_wrap,
            tab_size,
        );

        // Check if target is within this line's visual rows
        if visual_row < current_visual_row + visual_rows {
            // Found the target line - determine exact position
            let row_within_line = visual_row - current_visual_row;
            let (start, end) = get_visual_row_bounds(row_within_line, &wrap_points, line_len);
            return (line_idx, start, end, false);
        }
        current_visual_row += visual_rows;

        // Check deletion marker after this line (rendered between text and diagnostics)
        if show_git_diff {
            if let Some(git_diff) = git_diff_cache.as_ref() {
                if git_diff.has_deletion_marker(line_idx) {
                    if visual_row == current_visual_row {
                        // Target is a deletion marker virtual line
                        return (line_idx, 0, line_len, true);
                    }
                    current_visual_row += 1;
                }
            }
        }

        // Check diagnostic virtual rows after this line (from cache)
        let diag_row_count = cache.diagnostic_rows_for_line(line_idx);
        if diag_row_count > 0 && visual_row < current_visual_row + diag_row_count {
            // Target is a diagnostic virtual line
            return (line_idx, 0, line_len, true);
        }
        current_visual_row += diag_row_count;

        line_idx += 1;
    }

    // If we've exhausted all lines, return the last line
    let last_line = buffer.line_count().saturating_sub(1);
    let last_line_len = get_line_grapheme_count_cached(
        cache,
        buffer,
        last_line,
        content_width,
        use_smart_wrap,
        tab_size,
    );
    (last_line, 0, last_line_len, false)
}

#[cfg(test)]
mod tests {
    use super::{column_at_row_offset, row_offset_columns};

    #[test]
    fn row_offsets_count_screen_columns_from_the_row_start() {
        assert_eq!(row_offset_columns("\t\txy", 0, 3, 4), 9);
        // Tab stops restart at the row start: grapheme 1 is the second tab.
        assert_eq!(row_offset_columns("\t\txy", 1, 3, 4), 5);
        assert_eq!(row_offset_columns("abc", 0, 9, 4), 3);
    }

    #[test]
    fn a_row_offset_lands_on_the_grapheme_drawn_there() {
        assert_eq!(column_at_row_offset("\t\txy", 0, 4, 9, 4), 3);
        // Inside a tab: the tab itself.
        assert_eq!(column_at_row_offset("\t\txy", 0, 4, 6, 4), 1);
        // Past the end: the end, or the row's last column.
        assert_eq!(column_at_row_offset("ab", 0, 2, 7, 4), 2);
        assert_eq!(column_at_row_offset("abcdef", 2, 4, 9, 4), 4);
    }
}
