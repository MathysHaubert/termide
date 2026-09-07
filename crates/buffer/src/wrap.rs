//! Word wrapping utilities for smart line breaking at word boundaries
//!
//! This module provides functions for intelligent line wrapping that respects
//! word boundaries when possible, falling back to hard breaks for words wider
//! than the viewport.

use unicode_width::UnicodeWidthStr;

/// Calculate the optimal wrap point for a line segment using graphemes
///
/// This function tries to find a word boundary (non-alphanumeric character)
/// to break the line at, but will force a break at max_width if:
/// - No word boundary is found (single long word)
/// - The word would be wider than the viewport
///
/// Uses display width and grapheme clusters for proper Unicode handling
/// (CJK characters, combining characters like Hindi vowel signs, etc.)
///
/// # Arguments
/// * `graphemes` - The line grapheme clusters to wrap
/// * `start` - Starting position in the grapheme array
/// * `max_width` - Maximum display width before wrapping (content width)
/// * `line_len` - Total length of the line (grapheme count)
///
/// # Returns
/// The grapheme index where the line should be wrapped
pub fn calculate_wrap_point(
    graphemes: &[&str],
    start: usize,
    max_width: usize,
    line_len: usize,
) -> usize {
    if start >= line_len {
        return line_len;
    }

    // Find the grapheme index where display width exceeds max_width
    let mut display_width = 0;
    let mut ideal_end = start;

    for (i, grapheme) in graphemes
        .iter()
        .enumerate()
        .skip(start)
        .take(line_len - start)
    {
        let grapheme_width = grapheme.width();

        if display_width + grapheme_width > max_width {
            ideal_end = i;
            break;
        }

        display_width += grapheme_width;
        ideal_end = i + 1;
    }

    // If we reached end of line, no wrapping needed
    if ideal_end >= line_len {
        return line_len;
    }

    // Check if grapheme is a word boundary (first char is non-alphanumeric)
    let is_boundary = |g: &str| g.chars().next().is_none_or(|c| !c.is_alphanumeric());

    // If the grapheme at ideal_end is a word boundary, we can break there
    // Note: ideal_end points to a grapheme that doesn't fit, so don't include it
    if ideal_end < line_len && is_boundary(graphemes[ideal_end]) {
        return ideal_end;
    }

    // Search backwards from ideal_end for a word boundary
    for i in (start..ideal_end).rev() {
        if is_boundary(graphemes[i]) {
            // Found a boundary - wrap after this grapheme
            // But avoid wrapping right after start (would create empty visual line)
            if i > start {
                return i + 1;
            }
        }
    }

    // No word boundary found - this means we have a single long word
    // Force break at ideal_end to prevent horizontal overflow
    ideal_end.max(start + 1) // Ensure at least one grapheme is included
}

/// Check if a character is a word boundary
///
/// Word boundaries are characters that are neither alphanumeric nor `_`.
/// Underscore counts as part of a word so that double-clicking a
/// `snake_case` identifier selects all of it — matching this editor's own vim
/// motions (`word_boundary::char_type`) and the terminal's double-click
/// (`panel_terminal::selection::is_word_char`), which both already treat it
/// that way.
///
/// Only word selection uses this; `calculate_wrap_point` carries its own
/// predicate, where `_` deliberately stays a break point so a long
/// `snake_case_identifier` wraps there instead of mid-word.
pub fn is_word_boundary(c: char) -> bool {
    !(c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_wrap_point_basic() {
        use unicode_segmentation::UnicodeSegmentation;

        let text = "hello world test";
        let graphemes: Vec<&str> = text.graphemes(true).collect();

        // Should wrap after "hello "
        let wrap_point = calculate_wrap_point(&graphemes, 0, 10, graphemes.len());
        assert_eq!(wrap_point, 6); // After space
    }

    #[test]
    fn test_calculate_wrap_point_long_word() {
        use unicode_segmentation::UnicodeSegmentation;

        let text = "verylongword";
        let graphemes: Vec<&str> = text.graphemes(true).collect();

        // Should force break at max_width
        let wrap_point = calculate_wrap_point(&graphemes, 0, 5, graphemes.len());
        assert_eq!(wrap_point, 5);
    }

    #[test]
    fn test_is_word_boundary() {
        assert!(is_word_boundary(' '));
        assert!(is_word_boundary('.'));
        assert!(is_word_boundary(','));
        assert!(is_word_boundary('!'));

        assert!(!is_word_boundary('a'));
        assert!(!is_word_boundary('Z'));
        assert!(!is_word_boundary('5'));
        assert!(!is_word_boundary('ж')); // Cyrillic
        assert!(!is_word_boundary('中')); // Chinese
                                          // Underscore belongs to the word: a double-click on `snake_case`
                                          // selects the whole identifier.
        assert!(!is_word_boundary('_'));
    }
}
