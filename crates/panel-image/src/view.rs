//! Viewport geometry for the image panel: fit-to-panel scale, zoom steps
//! and panning. Pure functions over pixel and cell sizes so the rules can
//! be tested without a terminal.

use ratatui::layout::Rect;

/// Zoom factor applied per step: `1.25^step`, step 0 is fit-to-panel.
const ZOOM_BASE: f64 = 1.25;
/// Smallest zoom step; below this the image is a few cells wide anyway.
const MIN_STEP: i32 = -8;
/// Largest zoom step; enough to inspect single pixels of a large image.
const MAX_STEP: i32 = 16;
/// Panning moves the window by this fraction of its visible size.
const PAN_DIVISOR: u32 = 8;

/// Where the user is looking: zoom level and the top-left of the visible
/// window in source pixels. The default is the whole image fitted to the
/// panel, aspect ratio preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Viewport {
    step: i32,
    offset: (u32, u32),
}

/// Rendering plan for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Visible source window as `(x, y, width, height)` in image pixels.
    pub crop: (u32, u32, u32, u32),
    /// Cells inside the panel area the window is drawn into, centred.
    pub rect: Rect,
    /// Source pixels to screen pixels, as a percentage (100 = 1:1).
    pub percent: u32,
}

/// Scale that fits the whole image into `area_px`, preserving aspect ratio.
fn fit_scale(image: (u32, u32), area_px: (u32, u32)) -> f64 {
    let (iw, ih) = (image.0.max(1) as f64, image.1.max(1) as f64);
    (area_px.0 as f64 / iw).min(area_px.1 as f64 / ih)
}

impl Viewport {
    /// `true` when the whole image fits the panel (no zoom applied).
    pub fn is_fit(&self) -> bool {
        self.step == 0
    }

    /// Back to fit-to-panel.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Change the zoom by `delta` steps, keeping the centre of the visible
    /// window in place. `area` and `font` describe the panel the image was
    /// last drawn in; without them the zoom is anchored at the top-left.
    pub fn zoom_by(&mut self, delta: i32, image: (u32, u32), area: Rect, font: (u16, u16)) {
        let new_step = (self.step + delta).clamp(MIN_STEP, MAX_STEP);
        if new_step == self.step {
            return;
        }
        let before = self.layout(image, area, font);
        self.step = new_step;
        let after = self.layout(image, area, font);
        let (Some(before), Some(after)) = (before, after) else {
            return;
        };
        // Keep the source point under the window centre fixed.
        let cx = before.crop.0 as i64 + before.crop.2 as i64 / 2;
        let cy = before.crop.1 as i64 + before.crop.3 as i64 / 2;
        self.offset = (
            (cx - after.crop.2 as i64 / 2).max(0) as u32,
            (cy - after.crop.3 as i64 / 2).max(0) as u32,
        );
    }

    /// Move the visible window by `dx`/`dy` units of one eighth of its size.
    /// A no-op along an axis where the whole image is already visible.
    pub fn pan(&mut self, dx: i32, dy: i32, image: (u32, u32), area: Rect, font: (u16, u16)) {
        let Some(layout) = self.layout(image, area, font) else {
            return;
        };
        let (_, _, vw, vh) = layout.crop;
        let step_x = (vw / PAN_DIVISOR).max(1) as i64;
        let step_y = (vh / PAN_DIVISOR).max(1) as i64;
        let max_x = image.0.saturating_sub(vw) as i64;
        let max_y = image.1.saturating_sub(vh) as i64;
        let x = (layout.crop.0 as i64 + dx as i64 * step_x).clamp(0, max_x);
        let y = (layout.crop.1 as i64 + dy as i64 * step_y).clamp(0, max_y);
        self.offset = (x as u32, y as u32);
    }

    /// Compute what to draw for an image of `image` pixels inside `area`
    /// cells of `font` pixels each. `None` when nothing can be drawn.
    pub fn layout(&self, image: (u32, u32), area: Rect, font: (u16, u16)) -> Option<Layout> {
        if image.0 == 0 || image.1 == 0 || area.width == 0 || area.height == 0 {
            return None;
        }
        let font = (font.0.max(1), font.1.max(1));
        let area_px = (
            area.width as u32 * font.0 as u32,
            area.height as u32 * font.1 as u32,
        );
        let scale = fit_scale(image, area_px) * ZOOM_BASE.powi(self.step);
        if scale <= 0.0 || !scale.is_finite() {
            return None;
        }

        // Visible window in source pixels, clamped to the image.
        let vw = ((area_px.0 as f64 / scale).floor() as u32).clamp(1, image.0);
        let vh = ((area_px.1 as f64 / scale).floor() as u32).clamp(1, image.1);
        let x = self.offset.0.min(image.0 - vw);
        let y = self.offset.1.min(image.1 - vh);

        // Window size on screen, then in whole cells, never beyond the area.
        let out_w = ((vw as f64 * scale).round() as u32).clamp(1, area_px.0);
        let out_h = ((vh as f64 * scale).round() as u32).clamp(1, area_px.1);
        let cells_w = out_w.div_ceil(font.0 as u32).min(area.width as u32) as u16;
        let cells_h = out_h.div_ceil(font.1 as u32).min(area.height as u32) as u16;
        let rect = Rect::new(
            area.x + (area.width - cells_w) / 2,
            area.y + (area.height - cells_h) / 2,
            cells_w,
            cells_h,
        );

        Some(Layout {
            crop: (x, y, vw, vh),
            rect,
            percent: (scale * 100.0).round().max(1.0) as u32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FONT: (u16, u16) = (10, 20);
    const AREA: Rect = Rect {
        x: 2,
        y: 1,
        width: 80,
        height: 24,
    };

    #[test]
    fn fit_shows_the_whole_image_centred() {
        // 80x24 cells at 10x20 px = 800x480 px. A 1600x600 image is limited
        // by width: scale 0.5, 800x300 px = 80x15 cells.
        let l = Viewport::default().layout((1600, 600), AREA, FONT).unwrap();
        assert_eq!(l.crop, (0, 0, 1600, 600));
        assert_eq!(l.rect, Rect::new(2, 1 + (24 - 15) / 2, 80, 15));
        assert_eq!(l.percent, 50);
    }

    #[test]
    fn fit_upscales_a_small_image() {
        // 100x100 px in 800x480 px: limited by height, scale 4.8 -> 480x480 px
        // = 48x24 cells, centred horizontally.
        let l = Viewport::default().layout((100, 100), AREA, FONT).unwrap();
        assert_eq!(l.crop, (0, 0, 100, 100));
        assert_eq!(l.rect, Rect::new(2 + (80 - 48) / 2, 1, 48, 24));
        assert_eq!(l.percent, 480);
    }

    #[test]
    fn zoom_in_crops_around_the_centre() {
        let mut v = Viewport::default();
        v.zoom_by(1, (1600, 600), AREA, FONT);
        let l = v.layout((1600, 600), AREA, FONT).unwrap();
        // scale 0.625: window 1280x600 (height clamps to the image).
        assert_eq!(l.crop.2, 1280);
        assert_eq!(l.crop.3, 600);
        assert_eq!(l.crop.0, (1600 - 1280) / 2);
        assert_eq!(l.crop.1, 0);
        assert_eq!(l.rect.width, 80);
        assert!(l.rect.height <= 24);
        assert_eq!(l.percent, 63);
    }

    #[test]
    fn zoom_out_shrinks_below_the_panel_and_stays_centred() {
        let mut v = Viewport::default();
        v.zoom_by(-1, (1600, 600), AREA, FONT);
        let l = v.layout((1600, 600), AREA, FONT).unwrap();
        assert_eq!(l.crop, (0, 0, 1600, 600));
        assert!(l.rect.width < 80);
        assert!(l.rect.x > AREA.x);
        assert!(l.rect.right() <= AREA.right());
        assert!(l.rect.bottom() <= AREA.bottom());
    }

    #[test]
    fn zoom_steps_are_clamped() {
        let mut v = Viewport::default();
        v.zoom_by(100, (1600, 600), AREA, FONT);
        assert_eq!(v.step, MAX_STEP);
        v.zoom_by(-1000, (1600, 600), AREA, FONT);
        assert_eq!(v.step, MIN_STEP);
        v.reset();
        assert!(v.is_fit());
    }

    #[test]
    fn pan_is_clamped_to_the_image_and_ignored_when_it_fits() {
        let img = (1600, 600);
        let mut v = Viewport::default();
        v.pan(5, 5, img, AREA, FONT);
        assert_eq!(v.layout(img, AREA, FONT).unwrap().crop, (0, 0, 1600, 600));

        v.zoom_by(4, img, AREA, FONT);
        let before = v.layout(img, AREA, FONT).unwrap().crop;
        v.pan(1, 0, img, AREA, FONT);
        let after = v.layout(img, AREA, FONT).unwrap().crop;
        assert_eq!(after.0, before.0 + before.2 / PAN_DIVISOR);
        assert_eq!(after.1, before.1);

        v.pan(1000, 1000, img, AREA, FONT);
        let edge = v.layout(img, AREA, FONT).unwrap().crop;
        assert_eq!(edge.0 + edge.2, 1600);
        assert_eq!(edge.1 + edge.3, 600);
        v.pan(-1000, -1000, img, AREA, FONT);
        let origin = v.layout(img, AREA, FONT).unwrap().crop;
        assert_eq!((origin.0, origin.1), (0, 0));
    }

    #[test]
    fn rect_never_exceeds_the_area_at_any_zoom() {
        let img = (1234, 777);
        for step in MIN_STEP..=MAX_STEP {
            let v = Viewport {
                step,
                offset: (0, 0),
            };
            let l = v.layout(img, AREA, FONT).unwrap();
            assert!(l.rect.width >= 1 && l.rect.width <= AREA.width);
            assert!(l.rect.height >= 1 && l.rect.height <= AREA.height);
            assert!(l.rect.x >= AREA.x && l.rect.right() <= AREA.right());
            assert!(l.rect.y >= AREA.y && l.rect.bottom() <= AREA.bottom());
            assert!(l.crop.0 + l.crop.2 <= img.0);
            assert!(l.crop.1 + l.crop.3 <= img.1);
        }
    }

    #[test]
    fn degenerate_inputs_draw_nothing() {
        let v = Viewport::default();
        assert!(v.layout((0, 10), AREA, FONT).is_none());
        assert!(v.layout((10, 10), Rect::new(0, 0, 0, 5), FONT).is_none());
        // A zero font size is treated as one pixel per cell rather than panicking.
        assert!(v.layout((10, 10), AREA, (0, 0)).is_some());
    }
}
