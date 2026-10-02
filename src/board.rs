//! Renders the visible part of the book text.
//!
//! Only the lines that fit on screen are drawn, so arbitrarily long texts
//! (eg. the whole book in seamless mode) stay cheap to render.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::config::{DOUBLE_SPREAD_PADDING_LEFT, DOUBLE_SPREAD_PADDING_MIDDLE};
use crate::models::{Direction, InlineStyle};

pub struct Board<'a> {
    text: &'a [String],
    /// sorted by row
    default_style: &'a [InlineStyle],
    /// sorted by row
    temporary_style: Vec<InlineStyle>,
    textwidth: i64,
    spread: i64,
}

fn styles_for_row(styles: &[InlineStyle], row: usize) -> &[InlineStyle] {
    let start = styles.partition_point(|s| s.row < row);
    let end = styles.partition_point(|s| s.row <= row);
    &styles[start..end]
}

impl<'a> Board<'a> {
    /// `default_style` must be sorted by row.
    pub fn new(text: &'a [String], textwidth: i64, default_style: &'a [InlineStyle], spread: i64) -> Self {
        debug_assert!(default_style.is_sorted_by_key(|s| s.row));
        Self { text, default_style, temporary_style: Vec::new(), textwidth, spread }
    }

    /// Reset temporary styling if `styles` is empty.
    pub fn feed_temporary_style(&mut self, mut styles: Vec<InlineStyle>) {
        styles.sort_by_key(|s| s.row);
        self.temporary_style = styles;
    }

    pub fn x(&self, cols: i64) -> i64 {
        Self::left(cols, self.textwidth, self.spread)
    }

    fn left(cols: i64, textwidth: i64, spread: i64) -> i64 {
        if spread == 2 { DOUBLE_SPREAD_PADDING_LEFT } else { (((cols - textwidth) / 2) + 1).max(0) }
    }

    /// Column just past the rightmost text on screen.
    pub fn text_end(cols: i64, textwidth: i64, spread: i64) -> i64 {
        let left = Self::left(cols, textwidth, spread);
        if spread == 2 { left + textwidth + DOUBLE_SPREAD_PADDING_MIDDLE + textwidth } else { left + textwidth }
    }

    pub fn x_alt(&self) -> i64 {
        DOUBLE_SPREAD_PADDING_LEFT + self.textwidth + DOUBLE_SPREAD_PADDING_MIDDLE
    }

    fn styled_line(&self, row: usize, base: Style) -> Line<'static> {
        let text = &self.text[row];
        let mut chars: Vec<char> = text.chars().collect();
        let mut mods = vec![Modifier::empty(); chars.len()];
        for style in styles_for_row(self.default_style, row)
            .iter()
            .chain(styles_for_row(&self.temporary_style, row))
        {
            let end = style.col + style.n_letters;
            if end > chars.len() {
                chars.resize(end, ' ');
                mods.resize(end, Modifier::empty());
            }
            for m in &mut mods[style.col..end] {
                *m |= style.attr;
            }
        }

        let mut spans = Vec::new();
        let mut start = 0;
        while start < chars.len() {
            let m = mods[start];
            let mut end = start + 1;
            while end < chars.len() && mods[end] == m {
                end += 1;
            }
            let s: String = chars[start..end].iter().collect();
            spans.push(Span::styled(s, base.add_modifier(m)));
            start = end;
        }
        Line::from(spans)
    }

    fn put_line(buf: &mut Buffer, area: Rect, x: i64, y: i64, line: &Line) {
        if x < 0 || x >= area.width as i64 || y < 0 || y >= area.height as i64 {
            return;
        }
        let width = area.width - x as u16;
        buf.set_line(area.x + x as u16, area.y + y as u16, line, width);
    }

    pub fn render(&self, buf: &mut Buffer, area: Rect, row: usize, bottom_padding: usize, base: Style) {
        let page = (area.height as usize).saturating_sub(bottom_padding);
        let total = self.text.len();
        let x = self.x(area.width as i64);
        for n_row in 0..page.min(total.saturating_sub(row)) {
            Self::put_line(buf, area, x, n_row as i64, &self.styled_line(row + n_row, base));
            let alt_row = row + page + n_row;
            if self.spread == 2 && alt_row < total {
                Self::put_line(buf, area, self.x_alt(), n_row as i64, &self.styled_line(alt_row, base));
            }
        }
    }

    /// One frame of the page-turn animation: `n` columns of the new page.
    pub fn render_partial(&self, buf: &mut Buffer, area: Rect, row: usize, n: usize, direction: Direction, base: Style) {
        let page = area.height as usize;
        let total = self.text.len();
        let tw = self.textwidth.max(0) as usize;
        let x = self.x(area.width as i64);
        let slice = |line: &str| -> (i64, String) {
            match direction {
                Direction::Forward => {
                    let mut chars: Vec<char> = line.chars().collect();
                    chars.resize(chars.len().max(tw), ' ');
                    ((tw - n) as i64, chars[..n].iter().collect())
                }
                Direction::Backward => (0, line.chars().skip(tw - n).take(n).collect()),
            }
        };
        for n_row in 0..page.min(total.saturating_sub(row)) {
            let (off, s) = slice(&self.text[row + n_row]);
            Self::put_line(buf, area, x + off, n_row as i64, &Line::styled(s, base));
            let alt_row = row + page + n_row;
            if self.spread == 2 && alt_row < total {
                let (off, s) = slice(&self.text[alt_row]);
                Self::put_line(buf, area, self.x_alt() + off, n_row as i64, &Line::styled(s, base));
            }
        }
    }
}
