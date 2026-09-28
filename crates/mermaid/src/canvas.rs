//! A growable character grid used to compose diagram pseudographics.

/// A growable grid of characters addressed by `(x, y)`.
pub(crate) struct Canvas {
    rows: Vec<Vec<char>>,
}

impl Canvas {
    pub(crate) fn new() -> Self {
        Self { rows: Vec::new() }
    }

    fn ensure(&mut self, x: usize, y: usize) {
        if y >= self.rows.len() {
            self.rows.resize(y + 1, Vec::new());
        }
        if x >= self.rows[y].len() {
            self.rows[y].resize(x + 1, ' ');
        }
    }

    pub(crate) fn put(&mut self, x: usize, y: usize, ch: char) {
        self.ensure(x, y);
        self.rows[y][x] = ch;
    }

    /// Write text left-to-right starting at `(x, y)`.
    pub(crate) fn text(&mut self, x: usize, y: usize, s: &str) {
        for (i, ch) in s.chars().enumerate() {
            self.put(x + i, y, ch);
        }
    }

    pub(crate) fn hline(&mut self, x0: usize, x1: usize, y: usize, ch: char) {
        for x in x0..=x1 {
            self.put(x, y, ch);
        }
    }

    pub(crate) fn vline(&mut self, x: usize, y0: usize, y1: usize, ch: char) {
        for y in y0..=y1 {
            self.put(x, y, ch);
        }
    }

    /// Draw a single-line box with top-left at `(x, y)` and the given inner
    /// width, with the label centered. `corners` selects the corner glyphs.
    pub(crate) fn draw_box(&mut self, x: usize, y: usize, inner_w: usize, label: &str) {
        self.draw_box_styled(x, y, inner_w, label, ['┌', '┐', '└', '┘']);
    }

    /// Like [`Self::draw_box`] but with custom corner glyphs (shape hint).
    pub(crate) fn draw_box_styled(
        &mut self,
        x: usize,
        y: usize,
        inner_w: usize,
        label: &str,
        corners: [char; 4],
    ) {
        let w = inner_w + 2;
        self.put(x, y, corners[0]);
        self.put(x + w - 1, y, corners[1]);
        self.hline(x + 1, x + w - 2, y, '─');
        self.put(x, y + 1, '│');
        self.put(x + w - 1, y + 1, '│');
        let pad = inner_w.saturating_sub(label.chars().count()) / 2;
        self.text(x + 1 + pad, y + 1, label);
        self.put(x, y + 2, corners[2]);
        self.put(x + w - 1, y + 2, corners[3]);
        self.hline(x + 1, x + w - 2, y + 2, '─');
    }

    /// Draw a box `h` rows tall with the label centred both ways — a plain
    /// node grown to seat the edges meeting its sides.
    pub(crate) fn draw_tall_box(
        &mut self,
        x: usize,
        y: usize,
        inner_w: usize,
        h: usize,
        label: &str,
        corners: [char; 4],
    ) {
        let w = inner_w + 2;
        self.put(x, y, corners[0]);
        self.put(x + w - 1, y, corners[1]);
        self.hline(x + 1, x + w - 2, y, '─');
        for row in y + 1..y + h - 1 {
            self.put(x, row, '│');
            self.put(x + w - 1, row, '│');
        }
        let pad = inner_w.saturating_sub(label.chars().count()) / 2;
        self.text(x + 1 + pad, y + h / 2, label);
        self.put(x, y + h - 1, corners[2]);
        self.put(x + w - 1, y + h - 1, corners[3]);
        self.hline(x + 1, x + w - 2, y + h - 1, '─');
    }

    /// Draw a box with a centered title and, if `body` is non-empty, a second
    /// compartment of left-aligned lines below a separator rule. Used for class
    /// members / ER attributes. `corners` selects the corner glyphs.
    pub(crate) fn draw_panel(
        &mut self,
        x: usize,
        y: usize,
        inner_w: usize,
        title: &str,
        body: &[String],
        corners: [char; 4],
    ) {
        let w = inner_w + 2;
        self.put(x, y, corners[0]);
        self.put(x + w - 1, y, corners[1]);
        self.hline(x + 1, x + w - 2, y, '─');
        // Title (centered).
        self.put(x, y + 1, '│');
        self.put(x + w - 1, y + 1, '│');
        let pad = inner_w.saturating_sub(title.chars().count()) / 2;
        self.text(x + 1 + pad, y + 1, title);
        let mut row = y + 2;
        if !body.is_empty() {
            // Separator rule between compartments.
            self.put(x, row, '├');
            self.put(x + w - 1, row, '┤');
            self.hline(x + 1, x + w - 2, row, '─');
            row += 1;
            for line in body {
                self.put(x, row, '│');
                self.put(x + w - 1, row, '│');
                self.text(x + 1, row, line);
                row += 1;
            }
        }
        self.put(x, row, corners[2]);
        self.put(x + w - 1, row, corners[3]);
        self.hline(x + 1, x + w - 2, row, '─');
    }

    /// Fill `│` on empty cells of a column between rows (e.g. a lifeline behind
    /// other content).
    pub(crate) fn lifeline(&mut self, x: usize, y0: usize, y1: usize) {
        for y in y0..=y1 {
            self.ensure(x, y);
            if self.rows[y][x] == ' ' {
                self.rows[y][x] = '│';
            }
        }
    }

    pub(crate) fn into_lines(self) -> Vec<String> {
        self.rows
            .into_iter()
            .map(|r| {
                let mut s: String = r.into_iter().collect();
                let trimmed = s.trim_end();
                s.truncate(trimmed.len());
                s
            })
            .collect()
    }
}

/// Display width of a label (character count; assumes mostly single-width).
pub(crate) fn label_width(s: &str) -> usize {
    s.chars().count()
}
