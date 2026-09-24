//! Shared tinting for diff lines, so every panel that shows a diff (the git
//! diff panel, the agent's edit results) paints added and removed lines alike.

use ratatui::style::Color;

/// Blend two colors together.
/// `ratio` 0.0 = all color1, 1.0 = all color2
pub fn blend_colors(color1: Color, color2: Color, ratio: f32) -> Color {
    let (r1, g1, b1) = rgb(color1);
    let (r2, g2, b2) = rgb(color2);

    let ratio = ratio.clamp(0.0, 1.0);
    let inv = 1.0 - ratio;

    Color::Rgb(
        (r1 as f32 * inv + r2 as f32 * ratio) as u8,
        (g1 as f32 * inv + g2 as f32 * ratio) as u8,
        (b1 as f32 * inv + b2 as f32 * ratio) as u8,
    )
}

/// The faint background of an added (`accent` = success) or removed
/// (`accent` = error) diff line: the accent washed out towards `bg`.
pub fn diff_line_bg(accent: Color, bg: Color) -> Color {
    blend_colors(accent, bg, 0.85)
}

fn rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::White => (255, 255, 255),
        Color::Black => (0, 0, 0),
        Color::Gray => (128, 128, 128),
        Color::Red => (255, 0, 0),
        Color::Green => (0, 255, 0),
        Color::Yellow => (255, 255, 0),
        Color::Blue => (0, 0, 255),
        Color::Magenta => (255, 0, 255),
        Color::Cyan => (0, 255, 255),
        _ => (128, 128, 128),
    }
}
