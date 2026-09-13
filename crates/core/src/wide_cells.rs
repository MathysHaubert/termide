//! Frame fix-up for emoji that a variation selector made two columns wide.
//!
//! `ratatui-core`'s frame diff treats a wide cell whose symbol contains
//! U+FE0F specially: besides the cell itself it emits the trailing cell as
//! an explicit blank, assuming the terminal kept the emoji one column wide
//! and would otherwise leave stale content there. Terminals that do widen
//! after VS16 (Ghostty, WezTerm, iTerm2) already advanced the cursor past
//! their own spacer, so that blank lands one column further right and
//! everything after it on the row shifts — the "doubled letters and shifted
//! borders" artifact. A cell flagged `skip` is never emitted, and the diff
//! honours the flag for those trailing updates too, so marking the tail of
//! every such emoji restores the plain "wide symbol, skip one cell" output.

use ratatui::buffer::Buffer;
use unicode_width::UnicodeWidthStr;

/// Flag the trailing cell of every VS16-widened emoji in `buf` as `skip`.
///
/// Call after a frame is rendered and before it is flushed. No-op when the
/// host terminal keeps such emoji narrow (see
/// [`unicode_width::variation_selectors_change_width`]): then no cell of
/// theirs is two columns wide and the diff has nothing to work around.
pub fn mark_variation_selector_tails(buf: &mut Buffer) {
    if !unicode_width::variation_selectors_change_width() {
        return;
    }
    let width = usize::from(buf.area.width);
    if width < 2 {
        return;
    }
    let cells = &mut buf.content;
    for i in 0..cells.len().saturating_sub(1) {
        // A wide cell never sits in the last column of a row.
        if (i + 1) % width == 0 {
            continue;
        }
        let symbol = cells[i].symbol();
        if symbol.contains('\u{FE0F}') && symbol.width() > 1 {
            cells[i + 1].skip = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    #[test]
    fn tail_is_skipped_only_when_the_host_widens() {
        // One test for both modes: the flag is process-global and the two
        // halves would race each other on separate threads.
        let area = Rect::new(0, 0, 6, 1);

        unicode_width::set_variation_selectors_change_width(true);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "a\u{23F1}\u{FE0F}b", Style::default());
        mark_variation_selector_tails(&mut buf);
        assert!(buf[(2, 0)].skip, "the spacer after the emoji is skipped");
        assert!(!buf[(1, 0)].skip);
        assert!(!buf[(3, 0)].skip);
        let previous = Buffer::filled(area, ratatui::buffer::Cell::new("x"));
        let updates: Vec<u16> = previous.diff(&buf).into_iter().map(|(x, _, _)| x).collect();
        assert_eq!(updates, vec![0, 1, 3, 4, 5]);

        unicode_width::set_variation_selectors_change_width(false);
        let mut buf = Buffer::empty(area);
        buf.set_string(0, 0, "a\u{23F1}\u{FE0F}b", Style::default());
        mark_variation_selector_tails(&mut buf);
        assert!(buf.content.iter().all(|c| !c.skip));
        unicode_width::set_variation_selectors_change_width(true);
    }
}
