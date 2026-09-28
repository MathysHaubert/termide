//! Frame fix-up for terminal graphics an overlay was drawn over.
//!
//! Graphics protocols, as `ratatui-image` emits them, put the escape
//! sequence of an image (Sixel, iTerm2) or of one image row (Kitty unicode
//! placeholders) into a single anchor cell and flag the rest of the image
//! `skip`, so the frame diff never writes them. When a modal or dropdown
//! covers part of the image, the diff writes the overlay's cells there. Once
//! it closes those cells are `skip` again and nothing is sent for them, and
//! an anchor the overlay did not cover is unchanged, so it is not sent
//! either: the overlay stays on screen over the image.
//!
//! While any graphics cell is overdrawn, the anchors that survived are
//! flagged `skip` too. The terminal still shows them, but the frame that
//! follows the overlay has plain anchors that differ from these, so the diff
//! sends them again and the image repaints over the leftovers.

use ratatui::buffer::{Buffer, Cell};

/// Graphics cells of a frame, captured after the panels are drawn and before
/// any overlay.
#[derive(Debug, Default)]
pub struct GraphicsCells {
    /// Cells holding an image escape sequence.
    anchors: Vec<usize>,
    /// Cells an image covers without drawing (`skip`).
    covered: Vec<usize>,
}

/// A graphics escape sequence is the only thing that puts ESC into a cell.
fn is_anchor(cell: &Cell) -> bool {
    cell.symbol().starts_with('\x1b')
}

/// Kitty image data (`ESC _ G`, optionally inside a tmux passthrough) is
/// sent only once, in the first frame after the image is encoded; holding
/// that anchor back would lose the image for good.
fn is_kitty_transmit(symbol: &str) -> bool {
    symbol.starts_with("\x1b_G")
        || symbol
            .strip_prefix("\x1bPtmux;\x1b\x1b")
            .is_some_and(|rest| rest.starts_with("_G"))
}

impl GraphicsCells {
    /// Record where graphics sit in `buf`. Empty when the frame has none.
    pub fn capture(buf: &Buffer) -> Self {
        let mut cells = Self::default();
        for (i, cell) in buf.content.iter().enumerate() {
            if is_anchor(cell) {
                cells.anchors.push(i);
            } else if cell.skip {
                cells.covered.push(i);
            }
        }
        if cells.anchors.is_empty() {
            cells.covered.clear();
        }
        cells
    }

    /// Call on the same frame once the overlays are drawn: if any of them
    /// landed on the captured graphics, hold back the surviving anchors so
    /// the next frame without the overlay repaints the images.
    pub fn hold_overdrawn_anchors(&self, buf: &mut Buffer) {
        let cells = &mut buf.content;
        let overdrawn = self.anchors.iter().any(|&i| !is_anchor(&cells[i]))
            || self.covered.iter().any(|&i| !cells[i].skip);
        if !overdrawn {
            return;
        }
        for &i in &self.anchors {
            let cell = &mut cells[i];
            if is_anchor(cell) && !is_kitty_transmit(cell.symbol()) {
                cell.skip = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 8,
        height: 4,
    };

    /// Panels: an image in rows 1..3, columns 2..6, one anchor per row like
    /// Kitty placeholders, the rest of the image skipped.
    fn panels(anchor: &str) -> Buffer {
        let mut buf = Buffer::filled(AREA, Cell::new("."));
        for y in 1..3 {
            buf[(2, y)].set_symbol(anchor);
            for x in 3..6 {
                buf[(x, y)].skip = true;
            }
        }
        buf
    }

    /// A modal over the right part of the image, clear like the real ones.
    fn draw_overlay(buf: &mut Buffer) {
        for y in 1..3 {
            for x in 4..8 {
                buf[(x, y)].reset();
            }
            buf.set_string(4, y, "MMMM", Style::default());
        }
    }

    fn updated(prev: &Buffer, next: &Buffer) -> Vec<(u16, u16)> {
        prev.diff(next)
            .into_iter()
            .map(|(x, y, _)| (x, y))
            .collect()
    }

    #[test]
    fn closing_an_overlay_resends_the_anchors_it_did_not_cover() {
        let image = panels("\x1b[simg");

        let mut with_overlay = panels("\x1b[simg");
        let cells = GraphicsCells::capture(&with_overlay);
        draw_overlay(&mut with_overlay);
        cells.hold_overdrawn_anchors(&mut with_overlay);
        assert!(with_overlay[(2, 1)].skip && with_overlay[(2, 2)].skip);
        assert!(updated(&image, &with_overlay).iter().all(|&(x, _)| x >= 4));

        let mut after = panels("\x1b[simg");
        GraphicsCells::capture(&after).hold_overdrawn_anchors(&mut after);
        let resent = updated(&with_overlay, &after);
        assert!(resent.contains(&(2, 1)) && resent.contains(&(2, 2)));
    }

    #[test]
    fn untouched_graphics_are_left_alone() {
        let mut buf = panels("\x1b[simg");
        let cells = GraphicsCells::capture(&buf);
        buf.set_string(0, 0, "menu", Style::default());
        cells.hold_overdrawn_anchors(&mut buf);
        assert!(!buf[(2, 1)].skip && !buf[(2, 2)].skip);
    }

    #[test]
    fn frames_without_graphics_capture_nothing() {
        let mut buf = Buffer::filled(AREA, Cell::new("."));
        buf[(1, 1)].skip = true;
        let cells = GraphicsCells::capture(&buf);
        assert!(cells.anchors.is_empty() && cells.covered.is_empty());
    }

    #[test]
    fn kitty_image_data_is_never_held_back() {
        for transmit in ["\x1b_Gq=2,m=0;AAAA\x1b\\", "\x1bPtmux;\x1b\x1b_Gq=2,"] {
            let mut buf = panels(transmit);
            let cells = GraphicsCells::capture(&buf);
            draw_overlay(&mut buf);
            cells.hold_overdrawn_anchors(&mut buf);
            assert!(!buf[(2, 1)].skip, "{transmit:?}");
        }
        assert!(!is_kitty_transmit("\x1bPtmux;\x1b\x1b[4X"));
    }
}
