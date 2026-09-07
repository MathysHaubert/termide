//! Pure text helpers for the document preview panels: character slicing,
//! display-column geometry, substring search, and URL inspection.
//!
//! The HTML and Markdown previews were carrying byte-identical copies of
//! these. They hold no state, so they live here beside the other shared
//! helper modules rather than once per panel.

use unicode_width::UnicodeWidthChar;

/// The `#fragment` part of a URL, if present and non-empty.
pub fn url_fragment(url: &str) -> Option<String> {
    url.split_once('#')
        .map(|(_, f)| f.to_string())
        .filter(|f| !f.is_empty())
}

/// Whether `path` (or URL) ends in a known raster-image extension.
pub fn is_image_path(path: &str) -> bool {
    let ext = path
        .rsplit('.')
        .next()
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "tiff" | "tif"
    )
}

/// Substring of `s` between character indices `[start, end)`.
pub fn slice_chars(s: &str, start: usize, end: usize) -> String {
    s.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

/// Display column at character index `col` (sum of preceding char widths).
pub fn char_col_to_display(s: &str, col: usize) -> usize {
    s.chars().take(col).map(|c| c.width().unwrap_or(0)).sum()
}

/// Character index at (or just past) display column `disp`.
pub fn display_to_char_col(s: &str, disp: u16) -> usize {
    let target = disp as usize;
    let mut acc = 0usize;
    for (i, c) in s.chars().enumerate() {
        if acc >= target {
            return i;
        }
        acc += c.width().unwrap_or(0);
    }
    s.chars().count()
}

/// Character indices where `needle` occurs in `line` (case-insensitive when `ci`).
pub fn find_in_line(line: &str, needle: &str, ci: bool) -> Vec<usize> {
    let hay: Vec<char> = line.chars().collect();
    let pat: Vec<char> = needle.chars().collect();
    let mut out = Vec::new();
    if pat.is_empty() || pat.len() > hay.len() {
        return out;
    }
    let eq = |a: char, b: char| {
        if ci {
            a.eq_ignore_ascii_case(&b) || a.to_lowercase().eq(b.to_lowercase())
        } else {
            a == b
        }
    };
    for i in 0..=hay.len() - pat.len() {
        if (0..pat.len()).all(|j| eq(hay[i + j], pat[j])) {
            out.push(i);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_in_line_case_insensitive() {
        assert_eq!(find_in_line("Foo foo FOO", "foo", true), vec![0, 4, 8]);
        assert_eq!(find_in_line("Foo foo FOO", "foo", false), vec![4]);
    }

    #[test]
    fn find_in_line_handles_degenerate_input() {
        assert!(find_in_line("abc", "", false).is_empty());
        assert!(find_in_line("ab", "abc", false).is_empty());
        assert!(find_in_line("", "a", false).is_empty());
    }

    #[test]
    fn find_in_line_matches_non_ascii_case_insensitively() {
        // The ASCII fast path cannot fold these, so the fallback has to.
        assert_eq!(find_in_line("Привет привет", "ПРИВЕТ", true), vec![0, 7]);
    }

    #[test]
    fn slicing_counts_characters_not_bytes() {
        assert_eq!(slice_chars("привет", 1, 4), "рив");
        // Out-of-range and inverted ranges clamp instead of panicking.
        assert_eq!(slice_chars("ab", 1, 99), "b");
        assert_eq!(slice_chars("ab", 2, 1), "");
    }

    #[test]
    fn display_columns_account_for_wide_characters() {
        // A CJK character occupies two display columns but one char index.
        assert_eq!(char_col_to_display("日本語", 2), 4);
        assert_eq!(display_to_char_col("日本語", 4), 2);
        // A column landing inside a wide character resolves past it, which is
        // what a click on the glyph's right half should do.
        assert_eq!(display_to_char_col("日本語", 3), 2);
        // Past the end clamps to the character count.
        assert_eq!(display_to_char_col("ab", 99), 2);
        assert_eq!(char_col_to_display("ab", 99), 2);
    }

    #[test]
    fn url_fragment_ignores_a_bare_hash() {
        assert_eq!(url_fragment("page.html#top"), Some("top".to_string()));
        assert_eq!(url_fragment("page.html#"), None);
        assert_eq!(url_fragment("page.html"), None);
    }

    #[test]
    fn image_paths_are_matched_by_extension_case_insensitively() {
        assert!(is_image_path("a/b/c.PNG"));
        assert!(is_image_path("https://host/x.jpeg"));
        assert!(!is_image_path("notes.md"));
        assert!(!is_image_path("noextension"));
    }
}
