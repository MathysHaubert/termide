//! Inline diff rendering for word-level change highlighting.
//!
//! This module provides functions for building visual lines that display
//! inline differences between original and current text, showing both
//! deleted (red) and inserted (green) text segments.

use ratatui::style::{Color, Modifier, Style};

use termide_git::{InlineChange, InlineChangeType};

/// Segment of visual line with its change type (borrows text from InlineChange).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualSegment<'a> {
    pub text: &'a str,
    pub change_type: InlineChangeType,
}

/// Build a visual line from inline changes.
///
/// Converts inline diff changes into segments suitable for rendering.
/// Deleted text is included in the visual output (will be rendered
/// at the position where it was deleted).
///
/// # Returns
/// Vec of segments in display order, including both deleted and current text.
pub(crate) fn build_visual_line(inline_changes: &[InlineChange]) -> Vec<VisualSegment<'_>> {
    inline_changes
        .iter()
        .map(|change| VisualSegment {
            text: &change.text,
            change_type: change.change_type,
        })
        .collect()
}

/// Apply diff styles to visual segments.
///
/// Merges syntax highlighting styles with diff-specific styling:
/// - Unchanged: keeps original syntax style
/// - Deleted: red background with dimmed foreground, optionally strikethrough
/// - Inserted: green background, preserving syntax foreground color
///
/// # Arguments
/// - `visual_segments` - segments from `build_visual_line()`
/// - `deleted_bg` - background color for deleted text (typically error/red)
/// - `inserted_bg` - background color for inserted text (typically success/green)
/// - `base_fg` - default foreground color for text
pub fn apply_diff_style(
    change_type: InlineChangeType,
    base_style: Style,
    deleted_bg: Color,
    inserted_bg: Color,
) -> Style {
    match change_type {
        InlineChangeType::Unchanged => base_style,
        InlineChangeType::Deleted => {
            // Deleted text: red background, dimmed foreground
            Style::default()
                .bg(deleted_bg)
                .fg(Color::Rgb(180, 140, 140)) // Dimmed red-ish text
                .add_modifier(Modifier::CROSSED_OUT)
        }
        InlineChangeType::Inserted => {
            // Inserted text: green background, keep syntax fg if available
            let fg = base_style.fg.unwrap_or(Color::White);
            Style::default().bg(inserted_bg).fg(fg)
        }
    }
}

/// Convert a visual (screen) column to a grapheme index in the buffer line.
///
/// Walks the line as it is drawn, deleted text included and tabs expanded, so
/// the result is the grapheme under that column. A column on deleted text maps
/// to the buffer position where it was deleted; a column past the end maps to
/// the end of the line.
pub fn visual_to_buffer_col(
    visual_col: usize,
    inline_changes: &[InlineChange],
    tab_size: usize,
) -> usize {
    use unicode_segmentation::UnicodeSegmentation;

    let mut col = 0;
    let mut buffer_idx = 0;
    for change in inline_changes {
        let deleted = change.change_type == InlineChangeType::Deleted;
        for g in change.text.graphemes(true) {
            let width = termide_buffer::grapheme_columns(g, col, tab_size);
            if col + width > visual_col {
                return buffer_idx;
            }
            col += width;
            if !deleted {
                buffer_idx += 1;
            }
        }
    }

    buffer_idx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_change(text: &str, change_type: InlineChangeType) -> InlineChange {
        InlineChange {
            text: text.to_string(),
            change_type,
        }
    }

    #[test]
    fn test_build_visual_line() {
        let changes = vec![
            make_change("Hello ", InlineChangeType::Unchanged),
            make_change("world", InlineChangeType::Deleted),
            make_change("beautiful world", InlineChangeType::Inserted),
        ];

        let segments = build_visual_line(&changes);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].text, "Hello ");
        assert_eq!(segments[1].text, "world");
        assert_eq!(segments[2].text, "beautiful world");
    }

    #[test]
    fn test_visual_to_buffer_col() {
        let changes = vec![
            make_change("Hello ", InlineChangeType::Unchanged),
            make_change("old", InlineChangeType::Deleted),
            make_change("new", InlineChangeType::Inserted),
        ];

        // Visual col 0 -> buffer col 0
        assert_eq!(visual_to_buffer_col(0, &changes, 4), 0);

        // Visual col in deleted region -> buffer col after unchanged
        assert_eq!(visual_to_buffer_col(7, &changes, 4), 6);
    }

    /// Deleted text drawn before a tab moves its tab stop, and the click
    /// mapping follows the line as drawn.
    #[test]
    fn visual_to_buffer_col_expands_tabs_after_deleted_text() {
        // Drawn: "ab" (deleted) then "\tx": the tab spans columns 2..4.
        let changes = vec![
            make_change("ab", InlineChangeType::Deleted),
            make_change("\tx", InlineChangeType::Unchanged),
        ];
        assert_eq!(visual_to_buffer_col(1, &changes, 4), 0);
        assert_eq!(visual_to_buffer_col(3, &changes, 4), 0);
        assert_eq!(visual_to_buffer_col(4, &changes, 4), 1);
        assert_eq!(visual_to_buffer_col(9, &changes, 4), 2);
    }

    #[test]
    fn test_apply_diff_style_unchanged() {
        let base = Style::default().fg(Color::Cyan);
        let result = apply_diff_style(InlineChangeType::Unchanged, base, Color::Red, Color::Green);
        assert_eq!(result, base);
    }

    #[test]
    fn test_apply_diff_style_deleted() {
        let base = Style::default().fg(Color::White);
        let result = apply_diff_style(InlineChangeType::Deleted, base, Color::Red, Color::Green);
        assert_eq!(result.bg, Some(Color::Red));
        assert!(result.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    #[test]
    fn test_apply_diff_style_inserted() {
        let base = Style::default().fg(Color::Cyan);
        let result = apply_diff_style(InlineChangeType::Inserted, base, Color::Red, Color::Green);
        assert_eq!(result.bg, Some(Color::Green));
        assert_eq!(result.fg, Some(Color::Cyan)); // Preserves syntax color
    }
}
