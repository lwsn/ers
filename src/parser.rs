//! HTML to lines of text.

use std::collections::{BTreeMap, HashMap, HashSet};

use ratatui::style::Modifier;

use crate::models::{CharPos, InlineStyle, TextMark, TextSpan, TextStructure};
use crate::util::{self, center, unquote, wrap};

// ---------------------------------------------------------------------------
// Tokenizer: a forgiving HTML/XHTML tokenizer with the same event model as
// Python's `html.parser.HTMLParser` (start tag, start-end tag, end tag, data).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Start { name: String, attrs: Vec<(String, String)>, self_closing: bool },
    End { name: String },
    Data(String),
}

pub fn decode_entities(s: &str) -> String {
    if s.contains('&') {
        html_escape::decode_html_entities(s).into_owned()
    } else {
        s.to_string()
    }
}

fn find_from(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}

fn find_ci_from(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
        .map(|p| p + from)
}

pub fn tokenize(src: &str, mut emit: impl FnMut(Token)) {
    let b = src.as_bytes();
    let len = b.len();
    let mut i = 0;
    let mut text_start = 0;

    let flush = |emit: &mut dyn FnMut(Token), from: usize, to: usize| {
        if to > from {
            emit(Token::Data(decode_entities(&src[from..to])));
        }
    };

    while i < len {
        if b[i] != b'<' || i + 1 >= len {
            i += 1;
            continue;
        }
        let next = b[i + 1];
        if b[i..].starts_with(b"<!--") {
            flush(&mut emit, text_start, i);
            i = find_from(b, i + 4, b"-->").map(|p| p + 3).unwrap_or(len);
            text_start = i;
        } else if b[i..].starts_with(b"<![CDATA[") {
            flush(&mut emit, text_start, i);
            let end = find_from(b, i + 9, b"]]>").unwrap_or(len);
            if end > i + 9 {
                emit(Token::Data(src[i + 9..end].to_string()));
            }
            i = (end + 3).min(len);
            text_start = i;
        } else if next == b'!' || next == b'?' {
            flush(&mut emit, text_start, i);
            i = find_from(b, i, b">").map(|p| p + 1).unwrap_or(len);
            text_start = i;
        } else if next == b'/' && b.get(i + 2).is_some_and(|c| c.is_ascii_alphabetic()) {
            flush(&mut emit, text_start, i);
            let end = find_from(b, i, b">").unwrap_or(len);
            let name: String = src[i + 2..end]
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            emit(Token::End { name });
            i = (end + 1).min(len);
            text_start = i;
        } else if next.is_ascii_alphabetic() {
            flush(&mut emit, text_start, i);
            let (token, end) = parse_start_tag(src, i);
            i = end;
            text_start = i;
            if let Token::Start { name, self_closing: false, .. } = &token
                && (name == "script" || name == "style") {
                    // raw text element: skip to its end tag
                    let close = format!("</{name}");
                    let stop = find_ci_from(b, i, close.as_bytes()).unwrap_or(len);
                    let raw = src[i..stop].to_string();
                    emit(token);
                    if !raw.is_empty() {
                        emit(Token::Data(raw));
                    }
                    i = stop;
                    text_start = i;
                    continue;
                }
            emit(token);
        } else {
            i += 1;
        }
    }
    flush(&mut emit, text_start, len);
}

/// Parse a start tag beginning at `src[start] == '<'`. Returns the token and
/// the index just past the closing `>`.
fn parse_start_tag(src: &str, start: usize) -> (Token, usize) {
    let b = src.as_bytes();
    let len = b.len();
    let mut i = start + 1;
    let name_start = i;
    while i < len && !b[i].is_ascii_whitespace() && b[i] != b'>' && b[i] != b'/' {
        i += 1;
    }
    let name = src[name_start..i].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;

    loop {
        while i < len && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= len {
            break;
        }
        match b[i] {
            b'>' => {
                i += 1;
                break;
            }
            b'/' => {
                i += 1;
                if b.get(i) == Some(&b'>') {
                    self_closing = true;
                    i += 1;
                    break;
                }
                continue;
            }
            _ => {}
        }
        let an_start = i;
        while i < len && !b[i].is_ascii_whitespace() && !matches!(b[i], b'=' | b'>') {
            if b[i] == b'/' && b.get(i + 1) == Some(&b'>') {
                break;
            }
            i += 1;
        }
        if i == an_start {
            i += 1;
            continue;
        }
        let attr_name = src[an_start..i].to_ascii_lowercase();
        let mut j = i;
        while j < len && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let mut value = String::new();
        if j < len && b[j] == b'=' {
            j += 1;
            while j < len && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < len && (b[j] == b'"' || b[j] == b'\'') {
                let q = b[j];
                let vstart = j + 1;
                let vend = b[vstart..].iter().position(|&c| c == q).map(|p| p + vstart).unwrap_or(len);
                value = decode_entities(&src[vstart..vend]);
                i = (vend + 1).min(len);
            } else {
                let vstart = j;
                while j < len && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                    j += 1;
                }
                value = decode_entities(&src[vstart..j]);
                i = j;
            }
        }
        attrs.push((attr_name, value));
    }
    (Token::Start { name, attrs, self_closing }, i)
}

// ---------------------------------------------------------------------------
// HTML to lines
// ---------------------------------------------------------------------------

const PARA: [&str; 2] = ["p", "div"];
const INDE: [&str; 4] = ["q", "dt", "dd", "blockquote"];
const PREF: [&str; 1] = ["pre"];
const BULL: [&str; 1] = ["li"];
const HIDE: [&str; 3] = ["script", "style", "head"];
const ITAL: [&str; 2] = ["i", "em"];
const BOLD: [&str; 2] = ["b", "strong"];

pub const ATTR_BOLD: Modifier = Modifier::BOLD;
pub const ATTR_ITALIC: Modifier = Modifier::ITALIC;

fn is_heading(tag: &str) -> bool {
    let b = tag.as_bytes();
    b.len() == 2 && b[0] == b'h' && (b'1'..=b'6').contains(&b[1])
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

pub struct HtmlToLines<'a> {
    text: Vec<String>,
    ishead: bool,
    isinde: bool,
    isbull: bool,
    ispref: bool,
    ishidden: bool,
    idhead: HashSet<usize>,
    idinde: HashSet<usize>,
    idbull: HashSet<usize>,
    idpref: HashSet<usize>,
    idimgs: HashSet<usize>,
    sects: &'a HashSet<String>,
    sectsindex: HashMap<usize, Vec<String>>,
    italic_marks: Vec<TextMark>,
    bold_marks: Vec<TextMark>,
    imgs: HashMap<usize, String>,
}

/// A wrapped piece of a source line placed on an output row.
struct Placed {
    row: usize,
    src_offset: usize,
    len: usize,
    col: usize,
}

impl<'a> HtmlToLines<'a> {
    pub fn new(sects: &'a HashSet<String>) -> Self {
        Self {
            text: vec![String::new()],
            ishead: false,
            isinde: false,
            isbull: false,
            ispref: false,
            ishidden: false,
            idhead: HashSet::new(),
            idinde: HashSet::new(),
            idbull: HashSet::new(),
            idpref: HashSet::new(),
            idimgs: HashSet::new(),
            sects,
            sectsindex: HashMap::new(),
            italic_marks: Vec::new(),
            bold_marks: Vec::new(),
            imgs: HashMap::new(),
        }
    }

    pub fn feed(&mut self, src: &str) {
        tokenize(src, |tok| match tok {
            Token::Start { name, attrs, self_closing } => {
                if self_closing || matches!(name.as_str(), "br" | "img" | "image") {
                    self.handle_startendtag(&name, &attrs);
                } else {
                    self.handle_starttag(&name, &attrs);
                }
            }
            Token::End { name } => self.handle_endtag(&name),
            Token::Data(d) => self.handle_data(&d),
        });
    }

    fn last_pos(&self) -> CharPos {
        CharPos { row: self.text.len() - 1, col: char_len(self.text.last().unwrap()) }
    }

    fn record_sections(&mut self, attrs: &[(String, String)]) {
        for (k, v) in attrs {
            if k == "id" && self.sects.contains(v) {
                let line = self.text.len() - 1;
                self.sectsindex.entry(line).or_default().push(v.clone());
            }
        }
    }

    fn add_image(&mut self, tag: &str, attrs: &[(String, String)]) {
        for (k, v) in attrs {
            if (tag == "img" && k == "src") || (tag == "image" && k.ends_with("href")) {
                let this_line = self.text.len();
                self.idimgs.insert(this_line);
                self.imgs.insert(this_line, unquote(v).into_owned());
                self.text.push("[IMAGE]".into());
                self.text.push(String::new());
            }
        }
    }

    fn handle_starttag(&mut self, tag: &str, attrs: &[(String, String)]) {
        if is_heading(tag) {
            self.ishead = true;
        } else if INDE.contains(&tag) {
            self.isinde = true;
        } else if PREF.contains(&tag) {
            self.ispref = true;
        } else if BULL.contains(&tag) {
            self.isbull = true;
        } else if HIDE.contains(&tag) {
            self.ishidden = true;
        } else if tag == "sup" {
            self.text.last_mut().unwrap().push_str("^{");
        } else if tag == "sub" {
            self.text.last_mut().unwrap().push_str("_{");
        } else if ITAL.contains(&tag) {
            if self.italic_marks.last().is_none_or(|m| m.is_valid()) {
                let start = self.last_pos();
                self.italic_marks.push(TextMark { start, end: None });
            }
        } else if BOLD.contains(&tag)
            && self.bold_marks.last().is_none_or(|m| m.is_valid()) {
                let start = self.last_pos();
                self.bold_marks.push(TextMark { start, end: None });
            }
        self.record_sections(attrs);
    }

    fn handle_startendtag(&mut self, tag: &str, attrs: &[(String, String)]) {
        if tag == "br" {
            self.text.push(String::new());
        } else if tag == "img" || tag == "image" {
            self.add_image(tag, attrs);
        }
        // "id" attribute is sometimes inside a startendtag
        self.record_sections(attrs);
    }

    fn handle_endtag(&mut self, tag: &str) {
        if is_heading(tag) {
            self.text.push(String::new());
            self.text.push(String::new());
            self.ishead = false;
        } else if PARA.contains(&tag) {
            self.text.push(String::new());
        } else if HIDE.contains(&tag) {
            self.ishidden = false;
        } else if INDE.contains(&tag) || PREF.contains(&tag) || BULL.contains(&tag) {
            if !self.text.last().unwrap().is_empty() {
                self.text.push(String::new());
            }
            self.isinde &= !INDE.contains(&tag);
            self.ispref &= !PREF.contains(&tag);
            self.isbull &= !BULL.contains(&tag);
        } else if tag == "sub" || tag == "sup" {
            self.text.last_mut().unwrap().push('}');
        } else if ITAL.contains(&tag) {
            let end = self.last_pos();
            if let Some(m) = self.italic_marks.last_mut() {
                m.end = Some(end);
            }
        } else if BOLD.contains(&tag) {
            let end = self.last_pos();
            if let Some(m) = self.bold_marks.last_mut() {
                m.end = Some(end);
            }
        }
    }

    fn handle_data(&mut self, raw: &str) {
        if raw.is_empty() || self.ishidden {
            return;
        }
        let tmp = if self.text.last().unwrap().is_empty() { raw.trim_start() } else { raw };
        if self.ispref {
            self.text.last_mut().unwrap().push_str(tmp);
        } else {
            let mut collapsed = String::with_capacity(tmp.len());
            let mut in_ws = false;
            for c in tmp.chars() {
                if c.is_whitespace() {
                    if !in_ws {
                        collapsed.push(' ');
                    }
                    in_ws = true;
                } else {
                    collapsed.push(c);
                    in_ws = false;
                }
            }
            self.text.last_mut().unwrap().push_str(&collapsed);
        }
        let idx = self.text.len() - 1;
        if self.ishead {
            self.idhead.insert(idx);
        } else if self.isbull {
            self.idbull.insert(idx);
        } else if self.isinde {
            self.idinde.insert(idx);
        } else if self.ispref {
            self.idpref.insert(idx);
        }
    }

    /// Convert marks into per-line spans of the unwrapped text.
    pub fn mark_to_spans(text: &[String], marks: &[TextMark]) -> Vec<TextSpan> {
        let mut spans = Vec::new();
        for mark in marks {
            let Some(end) = mark.end.filter(|_| mark.is_valid()) else {
                continue;
            };
            if mark.start.row == end.row {
                spans.push(TextSpan { start: mark.start, n_letters: end.col - mark.start.col });
            } else {
                let first_len = char_len(&text[mark.start.row]);
                spans.push(TextSpan {
                    start: mark.start,
                    n_letters: first_len.saturating_sub(mark.start.col),
                });
                for row in mark.start.row + 1..end.row {
                    spans.push(TextSpan {
                        start: CharPos { row, col: 0 },
                        n_letters: char_len(&text[row]),
                    });
                }
                spans.push(TextSpan { start: CharPos { row: end.row, col: 0 }, n_letters: end.col });
            }
        }
        spans
    }

    /// Unwrapped lines of text, eg. for dumping or counting letters.
    pub fn into_lines(self) -> Vec<String> {
        self.text
    }

    pub fn get_structured_text(&self, textwidth: usize, starting_line: usize) -> TextStructure {
        let mut text: Vec<String> = Vec::new();
        let mut images = BTreeMap::new();
        let mut sect = HashMap::new();
        let mut formatting = Vec::new();

        let mut spans_by_row: HashMap<usize, Vec<(TextSpan, Modifier)>> = HashMap::new();
        for (marks, attr) in [(&self.italic_marks, ATTR_ITALIC), (&self.bold_marks, ATTR_BOLD)] {
            for span in Self::mark_to_spans(&self.text, marks) {
                spans_by_row.entry(span.start.row).or_default().push((span, attr));
            }
        }

        for (n, line) in self.text.iter().enumerate() {
            if let Some(ids) = self.sectsindex.get(&n) {
                for id in ids {
                    sect.insert(id.clone(), starting_line + text.len());
                }
            }

            let mut placed: Vec<Placed> = Vec::new();
            let mut push_wrapped = |text: &mut Vec<String>, prefix: &str, first_prefix: &str, base: usize, width: usize, src: &str| {
                for (i, w) in wrap(src, width).into_iter().enumerate() {
                    let pre = if i == 0 { first_prefix } else { prefix };
                    placed.push(Placed {
                        row: text.len(),
                        src_offset: base + w.offset,
                        len: char_len(&w.text),
                        col: char_len(pre),
                    });
                    text.push(format!("{pre}{}", w.text));
                }
            };

            if self.idhead.contains(&n) {
                let startline = text.len();
                for w in wrap(line, textwidth) {
                    let centered = center(&w.text, textwidth);
                    let pad = centered.chars().take_while(|&c| c == ' ').count()
                        - w.text.chars().take_while(|&c| c == ' ').count();
                    placed.push(Placed { row: text.len(), src_offset: w.offset, len: char_len(&w.text), col: pad });
                    text.push(centered);
                }
                text.push(String::new());
                for i in startline..text.len() {
                    formatting.push(InlineStyle {
                        row: starting_line + i,
                        col: 0,
                        n_letters: char_len(&text[i]),
                        attr: ATTR_BOLD,
                    });
                }
            } else if self.idinde.contains(&n) {
                push_wrapped(&mut text, "   ", "   ", 0, textwidth.saturating_sub(3), line);
                text.push(String::new());
            } else if self.idbull.contains(&n) {
                push_wrapped(&mut text, "   ", " - ", 0, textwidth.saturating_sub(3), line);
                text.push(String::new());
            } else if self.idpref.contains(&n) {
                let mut base = 0;
                for sub in line.split('\n') {
                    let sub_trimmed = sub.strip_suffix('\r').unwrap_or(sub);
                    push_wrapped(&mut text, "   ", "   ", base, textwidth.saturating_sub(6), sub_trimmed);
                    base += char_len(sub) + 1;
                }
                text.push(String::new());
            } else if self.idimgs.contains(&n) {
                images.insert(starting_line + text.len(), self.imgs[&n].clone());
                text.push(center(line, textwidth));
                formatting.push(InlineStyle {
                    row: starting_line + text.len() - 1,
                    col: 0,
                    n_letters: char_len(text.last().unwrap()),
                    attr: ATTR_BOLD,
                });
                text.push(String::new());
            } else {
                push_wrapped(&mut text, "", "", 0, textwidth, line);
                text.push(String::new());
            }

            for (span, attr) in spans_by_row.get(&n).into_iter().flatten() {
                let start = span.start.col;
                let end = start + span.n_letters;
                for p in &placed {
                    let s = start.max(p.src_offset);
                    let e = end.min(p.src_offset + p.len);
                    if s < e {
                        formatting.push(InlineStyle {
                            row: starting_line + p.row,
                            col: p.col + s - p.src_offset,
                            n_letters: e - s,
                            attr: *attr,
                        });
                    }
                }
            }
        }

        // chapter suffix
        text.push(center("***", textwidth));

        TextStructure { text_lines: text, image_maps: images, section_rows: sect, formatting }
    }
}

/// Parse html into a [`TextStructure`] wrapped at `textwidth`.
pub fn parse_html(
    html_src: &str,
    textwidth: usize,
    section_ids: &HashSet<String>,
    starting_line: usize,
) -> TextStructure {
    let mut parser = HtmlToLines::new(section_ids);
    parser.feed(html_src);
    parser.get_structured_text(textwidth.max(1), starting_line)
}

/// Parse html into unwrapped paragraphs of text.
pub fn parse_html_lines(html_src: &str) -> Vec<String> {
    let empty = HashSet::new();
    let mut parser = HtmlToLines::new(&empty);
    parser.feed(html_src);
    parser.into_lines()
}

pub fn count_letters_in_html(html_src: &str) -> usize {
    parse_html_lines(html_src).iter().map(|l| util::count_letters(l)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(row: usize, col: usize) -> CharPos {
        CharPos { row, col }
    }

    #[test]
    fn mark_to_span() {
        let text: Vec<String> = [
            "Lorem ipsum dolor sit amet,",
            "consectetur adipiscing elit.",
            "Curabitur rutrum massa",
            "pretium, pulvinar ligula a,",
            "aliquam est. Proin ut lectus",
            "ac massa fermentum commodo.",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let m = |s, e| TextMark { start: s, end: Some(e) };
        assert_eq!(
            HtmlToLines::mark_to_spans(&text, &[m(pos(2, 3), pos(2, 19))]),
            vec![TextSpan { start: pos(2, 3), n_letters: 16 }]
        );
        assert_eq!(
            HtmlToLines::mark_to_spans(&text, &[m(pos(2, 3), pos(3, 5))]),
            vec![
                TextSpan { start: pos(2, 3), n_letters: 19 },
                TextSpan { start: pos(3, 0), n_letters: 5 }
            ]
        );
        assert_eq!(
            HtmlToLines::mark_to_spans(&text, &[m(pos(2, 3), pos(5, 3))]),
            vec![
                TextSpan { start: pos(2, 3), n_letters: 19 },
                TextSpan { start: pos(3, 0), n_letters: 27 },
                TextSpan { start: pos(4, 0), n_letters: 28 },
                TextSpan { start: pos(5, 0), n_letters: 3 },
            ]
        );
    }

    #[test]
    fn tokenizes() {
        let mut toks = Vec::new();
        tokenize(
            r#"<?xml version="1.0"?><!-- c --><p class=x id='a&amp;b'>Hi &amp; <br/>bye</p><script>if (a<b) x()</script>"#,
            |t| toks.push(t),
        );
        assert_eq!(
            toks,
            vec![
                Token::Start {
                    name: "p".into(),
                    attrs: vec![("class".into(), "x".into()), ("id".into(), "a&b".into())],
                    self_closing: false
                },
                Token::Data("Hi & ".into()),
                Token::Start { name: "br".into(), attrs: vec![], self_closing: true },
                Token::Data("bye".into()),
                Token::End { name: "p".into() },
                Token::Start { name: "script".into(), attrs: vec![], self_closing: false },
                Token::Data("if (a<b) x()".into()),
                Token::End { name: "script".into() },
            ]
        );
    }

    #[test]
    fn decodes_named_entities() {
        assert_eq!(decode_entities("a&nbsp;b&mdash;c&hellip;&#8217;"), "a\u{a0}b\u{2014}c\u{2026}\u{2019}");
    }

    #[test]
    fn parses_structure() {
        let html = r#"<html><head><title>T</title><style>p{}</style></head><body>
            <h1 id="s1">Chapter One</h1>
            <p>Hello <i>italic words</i> and <b>bold</b>.</p>
            <ul><li>first item</li></ul>
            <img src="../img/a%20b.png"/>
            </body></html>"#;
        let sects: HashSet<String> = ["s1".to_string()].into();
        let ts = parse_html(html, 20, &sects, 0);
        assert_eq!(
            ts.text_lines,
            vec![
                "    Chapter One     ",
                "",
                "",
                "Hello italic words",
                "and bold.",
                "",
                " - first item",
                "",
                "",
                "      [IMAGE]       ",
                "",
                "",
                "        ***         ",
            ]
        );
        assert_eq!(ts.section_rows.get("s1"), Some(&0));
        assert_eq!(ts.image_maps.get(&9).map(String::as_str), Some("../img/a b.png"));
        assert!(ts.formatting.contains(&InlineStyle { row: 3, col: 6, n_letters: 12, attr: ATTR_ITALIC }));
        assert!(ts.formatting.contains(&InlineStyle { row: 4, col: 4, n_letters: 4, attr: ATTR_BOLD }));
    }

    #[test]
    fn italic_across_wrapped_lines() {
        let empty = HashSet::new();
        let ts = parse_html("<p>aaa <em>bbb ccc ddd</em> eee</p>", 8, &empty, 10);
        assert_eq!(ts.text_lines[..3], ["aaa bbb", "ccc ddd", "eee"]);
        let italics: Vec<_> = ts.formatting.iter().filter(|s| s.attr == ATTR_ITALIC).collect();
        assert_eq!(
            italics,
            vec![
                &InlineStyle { row: 10, col: 4, n_letters: 3, attr: ATTR_ITALIC },
                &InlineStyle { row: 11, col: 0, n_letters: 7, attr: ATTR_ITALIC },
            ]
        );
    }

    #[test]
    fn plain_lines() {
        let lines = parse_html_lines("<p>one\n  two</p><p>three</p>");
        assert_eq!(lines, vec!["one two", "three", ""]);
    }
}
