use std::collections::{BTreeMap, HashMap};

use chrono::NaiveDateTime;
use ratatui::style::Modifier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Default)]
pub struct BookMetadata {
    pub title: Option<String>,
    pub creator: Option<String>,
    pub description: Option<String>,
    pub publisher: Option<String>,
    pub date: Option<String>,
    pub language: Option<String>,
    pub format: Option<String>,
    pub identifier: Option<String>,
    pub source: Option<String>,
}

impl BookMetadata {
    pub const FIELDS: [&'static str; 9] = [
        "title",
        "creator",
        "description",
        "publisher",
        "date",
        "language",
        "format",
        "identifier",
        "source",
    ];

    pub fn field_mut(&mut self, name: &str) -> Option<&mut Option<String>> {
        Some(match name {
            "title" => &mut self.title,
            "creator" => &mut self.creator,
            "description" => &mut self.description,
            "publisher" => &mut self.publisher,
            "date" => &mut self.date,
            "language" => &mut self.language,
            "format" => &mut self.format,
            "identifier" => &mut self.identifier,
            "source" => &mut self.source,
            _ => return None,
        })
    }

    pub fn fields(&self) -> [(&'static str, &Option<String>); 9] {
        [
            ("title", &self.title),
            ("creator", &self.creator),
            ("description", &self.description),
            ("publisher", &self.publisher),
            ("date", &self.date),
            ("language", &self.language),
            ("format", &self.format),
            ("identifier", &self.identifier),
            ("source", &self.source),
        ]
    }
}

#[derive(Debug, Clone)]
pub struct LibraryItem {
    pub last_read: NaiveDateTime,
    pub filepath: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub reading_progress: Option<f64>,
}

impl std::fmt::Display for LibraryItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let progress = match self.reading_progress {
            None => "N/A".to_string(),
            Some(p) => format!("{}%", (p * 100.0) as i64),
        };
        let filename = match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => self.filepath.replacen(&home, "~", 1),
            _ => self.filepath.clone(),
        };
        let book_name = match (&self.title, &self.author) {
            (Some(title), Some(author)) => format!("{title} - {author} ({filename})"),
            (None, Some(author)) if !author.is_empty() => format!("{filename} - {author}"),
            _ => filename,
        };
        let last_read = self.last_read.format("%I:%M%p %b %d");
        write!(f, "{progress:>4} {last_read}: {book_name}")
    }
}

/// Reading position.
///
/// `row` must always be set because the seamless mode needs it to convert
/// between rows relative to a content and rows absolute to the whole book.
/// When `rel_pctg` or `section` is set, it overrides `row`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadingState {
    pub content_index: usize,
    pub textwidth: i64,
    pub row: i64,
    pub rel_pctg: Option<f64>,
    pub section: Option<String>,
}

impl ReadingState {
    pub fn new(content_index: usize, textwidth: i64, row: i64) -> Self {
        Self { content_index, textwidth, row, rel_pctg: None, section: None }
    }
}

#[derive(Debug, Clone)]
pub struct SearchData {
    pub direction: Direction,
    pub value: String,
}

/// `all`: total letters in book. `cumulative[n]`: total letters of all
/// contents before content `n`.
#[derive(Debug, Clone)]
pub struct LettersCount {
    pub all: usize,
    pub cumulative: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CharPos {
    pub row: usize,
    pub col: usize,
}

/// A marking in text, inclusive on both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextMark {
    pub start: CharPos,
    pub end: Option<CharPos>,
}

impl TextMark {
    /// False if the mark is unterminated (eg. `<i>` missing `</i>`) or reversed.
    pub fn is_valid(&self) -> bool {
        match self.end {
            Some(end) if self.start.row == end.row => self.start.col <= end.col,
            Some(end) => self.start.row < end.row,
            None => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSpan {
    pub start: CharPos,
    pub n_letters: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineStyle {
    pub row: usize,
    pub col: usize,
    pub n_letters: usize,
    pub attr: Modifier,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TocEntry {
    pub label: String,
    pub content_index: usize,
    pub section: Option<String>,
}

/// How a content should be displayed on screen.
#[derive(Debug, Clone, Default)]
pub struct TextStructure {
    pub text_lines: Vec<String>,
    /// line number -> image path inside the ebook
    pub image_maps: BTreeMap<usize, String>,
    /// section id -> line number
    pub section_rows: HashMap<String, usize>,
    pub formatting: Vec<InlineStyle>,
}

impl TextStructure {
    pub fn merge(&mut self, other: TextStructure) {
        self.text_lines.extend(other.text_lines);
        self.image_maps.extend(other.image_maps);
        self.section_rows.extend(other.section_rows);
        self.formatting.extend(other.formatting);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(row: usize, col: usize) -> CharPos {
        CharPos { row, col }
    }

    #[test]
    fn text_mark_validation() {
        let mark = |s, e| TextMark { start: s, end: e };
        assert!(mark(pos(3, 1), Some(pos(3, 5))).is_valid());
        assert!(mark(pos(3, 5), Some(pos(3, 5))).is_valid());
        assert!(!mark(pos(3, 5), Some(pos(3, 2))).is_valid());
        assert!(mark(pos(3, 5), Some(pos(5, 2))).is_valid());
        assert!(!mark(pos(8, 5), Some(pos(5, 2))).is_valid());
        assert!(!mark(pos(0, 3), None).is_valid());
    }
}
