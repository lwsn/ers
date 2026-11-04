//! The interactive reader and its popup windows.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::board::Board;
use crate::config::{
    save_layout, Action, Config, DICT_PRESET_LIST, DOUBLE_SPREAD_PADDING_LEFT, DOUBLE_SPREAD_PADDING_MIDDLE,
    DOUBLE_SPREAD_PADDING_RIGHT, Key, Keymap, Settings, VIEWER_PRESET_LIST,
};
use crate::ebooks::{Ebook, get_ebook_obj};
use crate::models::{
    Direction, InlineStyle, LettersCount, ReadingState, SearchData, TextStructure, TocEntry,
};
use crate::parser::{count_letters_in_html, decode_entities, parse_html};
use crate::state::State;
use crate::util::{center, count_letters, is_url, resolve_path, shell_quote, str_width, which, wrap};

/// Columns the page width changes by per Enlarge/Shrink key press.
const WIDTH_STEP: i64 = 10;
const MIN_TEXTWIDTH: i64 = 20;
/// Width of the widest progress indicator, "100.0%".
const PROGRESS_MAX_WIDTH: i64 = 6;
/// Padding levels the TopPadding/BottomPadding keys cycle through.
const PADDING_LEVELS: [i64; 4] = [0, 2, 4, 8];
/// Fewest text rows a padding change may leave.
const MIN_TEXT_ROWS: i64 = 3;

pub enum ReadOutcome {
    State(ReadingState),
    OpenFile(String),
    Quit,
}

enum Input {
    Key(Key),
    Mouse(MouseEvent),
}

enum PromptResult {
    Text(String),
    Key(Key),
    Cancel,
}

enum SearchOutcome {
    Key(Option<Key>),
    State(ReadingState),
}

/// Result of a choice window: a key that closed it, a chosen index, or an
/// index to delete.
#[derive(Default)]
struct Choice {
    key: Option<Key>,
    index: Option<usize>,
    delete: Option<usize>,
}

pub fn find_current_content_index(
    toc_entries: &[TocEntry],
    section_rows: &HashMap<String, usize>,
    index: usize,
    y: i64,
) -> usize {
    let mut ntoc = 0;
    for (n, entry) in toc_entries.iter().enumerate() {
        if entry.content_index <= index {
            let sec_row = entry.section.as_ref().and_then(|s| section_rows.get(s)).copied().unwrap_or(0);
            if y >= sec_row as i64 {
                ntoc = n;
            }
        }
    }
    ntoc
}

/// Page turns made in quick succession with one key.
struct PageRun {
    key: Key,
    /// time of the latest turn
    last: Instant,
    /// where the first turn started
    origin: ReadingState,
    /// whether `origin` is in the jump history yet
    recorded: bool,
}

/// The progress indicators, drawn on the top or bottom row.
struct Indicators {
    bottom: bool,
    left: String,
    center: Option<String>,
    right: String,
}

impl Indicators {
    fn draw(&self, buf: &mut Buffer, style: Style) {
        // faint: the terminal blends the text toward the background, so it
        // recedes in any color scheme without picking colors ourselves
        let style = style.add_modifier(Modifier::DIM);
        let area = buf.area;
        let y = if self.bottom { area.height.saturating_sub(1) } else { 0 };
        let width = |s: &str| s.chars().count() as u16;
        buf.set_string(0, y, &self.left, style);
        let right_x = area.width.saturating_sub(width(&self.right));
        buf.set_string(right_x, y, &self.right, style);
        // centered on the screen, shortened to keep a gap to the others
        if let Some(c) = &self.center {
            let room = right_x.saturating_sub(width(&self.left)).saturating_sub(2);
            let c = truncate_with_ellipsis(c, room as usize);
            let x = (area.width.saturating_sub(width(&c)) / 2)
                .clamp(width(&self.left) + 1, right_x.saturating_sub(width(&c) + 1).max(width(&self.left) + 1));
            if !c.is_empty() {
                buf.set_string(x, y, &c, style);
            }
        }
    }
}

/// `s` cut to at most `max` chars, ending in "…" if shortened.
fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        s.chars().take(max - 1).chain(['…']).collect()
    }
}

/// How many positions ctrl+o / ctrl+i can go back / forward.
const JUMP_HISTORY_LEN: usize = 10;
/// Longest pause between repeats of a jump key for them to count as one
/// jump; above the usual key-repeat delay (eg. 660ms on X11).
const JUMP_REPEAT_GAP: Duration = Duration::from_millis(750);

static KEY_DISAMBIGUATION: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Ask the terminal to report ctrl+i apart from Tab (kitty keyboard
/// protocol), where supported. Call after entering the alternate screen,
/// and pair with `disable_key_disambiguation` before leaving it.
pub fn enable_key_disambiguation() {
    use ratatui::crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
    use ratatui::crossterm::{execute, terminal};
    if *KEY_DISAMBIGUATION.get_or_init(|| terminal::supports_keyboard_enhancement().unwrap_or(false)) {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}

pub fn disable_key_disambiguation() {
    use ratatui::crossterm::event::PopKeyboardEnhancementFlags;
    if KEY_DISAMBIGUATION.get() == Some(&true) {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
}

/// Push `rs` onto `stack`, dropping the oldest entry when full and
/// skipping it if it's where the newest entry already points.
fn push_bounded(stack: &mut Vec<ReadingState>, rs: ReadingState) {
    if stack.last().is_some_and(|l| l.content_index == rs.content_index && l.row == rs.row) {
        return;
    }
    if stack.len() == JUMP_HISTORY_LEN {
        stack.remove(0);
    }
    stack.push(rs);
}

fn pgup(current_row: i64, window_height: i64) -> i64 {
    (current_row - window_height).max(0)
}

fn pgdn(current_row: i64, total_lines: i64, window_height: i64) -> i64 {
    if current_row + window_height <= total_lines - window_height {
        current_row + window_height
    } else {
        (total_lines - window_height).max(0)
    }
}

fn pgend(total_lines: i64, window_height: i64) -> i64 {
    (total_lines - window_height).max(0)
}

/// First non-blank row at or after `row`, so pages don't start with empty
/// lines. Returns `row` unchanged if only blank lines follow.
fn skip_blank_rows(lines: &[String], row: i64) -> i64 {
    let start = row.max(0) as usize;
    lines
        .get(start..)
        .and_then(|rest| rest.iter().position(|l| !l.trim().is_empty()))
        .map_or(row, |n| (start + n) as i64)
}

pub fn count_letters_of(ebook: &mut dyn Ebook) -> LettersCount {
    let mut per_content = Vec::new();
    let mut cumulative = Vec::new();
    for content in ebook.contents().to_vec() {
        cumulative.push(per_content.iter().sum());
        let html = ebook.get_raw_text(&content).unwrap_or_default();
        per_content.push(count_letters_in_html(&html));
    }
    LettersCount { all: per_content.iter().sum(), cumulative }
}

/// Split "TableOfContents" into "Table Of Contents", keeping acronyms.
fn split_camel_case(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() {
            let prev_lower = chars[i - 1].is_lowercase();
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            if prev_lower || (chars[i - 1].is_uppercase() && next_lower) {
                out.push(' ');
            }
        }
        out.push(c);
    }
    out
}

fn color(n: i32) -> Color {
    if n < 0 { Color::Reset } else { Color::Indexed(n.min(255) as u8) }
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out)
}

/// Parse user input like "42", "42%" or "12.5" into a percentage in 0..=100.
fn parse_percent(input: &str) -> Option<f64> {
    let p: f64 = input.trim().trim_end_matches('%').trim().parse().ok()?;
    (p.is_finite() && (0.0..=100.0).contains(&p)).then_some(p)
}

/// Row to put at the top of the page so that the progress indicator (which
/// counts the letters before the top of the page) reads at least `target`.
/// `letters_prefix[i]` is the number of letters in the first `i` lines.
/// Pages after the one starting at `row` until the chapter ends: at the
/// first of `chapter_starts` (sorted) past `row`, or at `totlines`.
fn pages_left_in_chapter(
    chapter_starts: impl IntoIterator<Item = usize>,
    row: usize,
    totlines: usize,
    page: usize,
) -> usize {
    let end = chapter_starts.into_iter().find(|&r| r > row).unwrap_or(totlines);
    end.saturating_sub(row).div_ceil(page).saturating_sub(1)
}

fn row_for_letters(letters_prefix: &[usize], target: usize, max_row: i64) -> i64 {
    let row = letters_prefix.partition_point(|&p| p < target) as i64;
    row.clamp(0, max_row.max(0))
}

fn letters_prefix_of(lines: &[String]) -> Vec<usize> {
    let mut prefix = Vec::with_capacity(lines.len() + 1);
    prefix.push(0);
    for line in lines {
        prefix.push(prefix.last().unwrap() + count_letters(line));
    }
    prefix
}

enum GoToTarget {
    Row(i64),
    State(ReadingState),
    Unavailable,
}

pub struct Reader<'t> {
    term: &'t mut DefaultTerminal,
    /// last frame of the reading screen; popups are drawn on top of it
    base: Option<Buffer>,
    setting: Settings,
    keymap: Keymap,
    keymap_user: Vec<(&'static str, String)>,
    seamless: bool,
    /// keys that make windows exit and return the said key
    win_keys: Vec<Key>,
    ebook: Box<dyn Ebook>,
    state: State,
    page_animation: Option<Direction>,
    show_reading_progress: bool,
    reading_progress: Option<f64>,
    /// (row, TOC index) of the TOC sections starting in the loaded text,
    /// sorted, to tell which chapter we're in and where it ends
    chapter_starts: Vec<(usize, usize)>,
    /// TOC index of the chapter the loaded text starts in, if it begins
    /// before the first of `chapter_starts`
    chapter_before: Option<usize>,
    /// TOC entry labels, whitespace collapsed
    toc_labels: Vec<String>,
    search_data: Option<SearchData>,
    spread: i64,
    jump_list: HashMap<char, ReadingState>,
    /// positions before recent jumps (chapter, TOC, search, ...), newest
    /// last, for ctrl+o
    jump_history: Vec<ReadingState>,
    /// positions left with ctrl+o, newest last, for ctrl+i
    jump_forward: Vec<ReadingState>,
    /// key and time of the latest jump, to tell a held key from new jumps
    last_jump: Option<(Key, Instant)>,
    /// the page turns in progress, to record a run of them as a jump
    page_run: Option<PageRun>,
    letters_thread: Option<JoinHandle<LettersCount>>,
    letters_count: Option<LettersCount>,
    color_pair: u8,
    /// only used when seamless
    totlines_per_content: Vec<usize>,
    /// blank rows above the text, as set by the user
    pad_top: i64,
    /// blank rows below the text
    pad_bottom: i64,
    /// rows actually reserved above the text: `pad_top`, or 1 if that is 0
    /// and the progress indicator needs its own row
    top_pad: i64,
}

impl<'t> Reader<'t> {
    pub fn new(term: &'t mut DefaultTerminal, ebook: Box<dyn Ebook>, config: Config, state: State) -> Self {
        let keymap = config.keymap;
        let pad_top = config.setting.top_padding.max(0);
        let pad_bottom = config.setting.bottom_padding.max(0);
        let mut win_keys = vec![Key::Resize];
        for a in [Action::TableOfContents, Action::Metadata, Action::Help] {
            win_keys.extend_from_slice(keymap.get(a));
        }
        Self {
            term,
            base: None,
            seamless: config.setting.seamless_between_chapters,
            show_reading_progress: config.setting.show_progress_indicator,
            spread: if config.setting.start_with_double_spread { 2 } else { 1 },
            setting: config.setting,
            keymap,
            keymap_user: config.keymap_user,
            win_keys,
            ebook,
            state,
            page_animation: None,
            reading_progress: None,
            chapter_starts: Vec::new(),
            chapter_before: None,
            toc_labels: Vec::new(),
            search_data: None,
            jump_list: HashMap::new(),
            jump_history: Vec::new(),
            jump_forward: Vec::new(),
            last_jump: None,
            page_run: None,
            letters_thread: None,
            letters_count: None,
            color_pair: 1,
            totlines_per_content: Vec::new(),
            pad_top,
            pad_bottom,
            top_pad: 0,
        }
    }

    // -----------------------------------------------------------------
    // terminal helpers
    // -----------------------------------------------------------------

    fn size(&self) -> (i64, i64) {
        let s = self.term.size().unwrap_or_default();
        (s.height as i64, s.width as i64)
    }

    /// Book title from metadata, or the file name if there is none.
    fn book_title(&self) -> String {
        let title = self.ebook.get_meta().title.map(|t| strip_tags(&t)).unwrap_or_default();
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        if !title.is_empty() {
            return title;
        }
        let path = self.ebook.path();
        path.trim_end_matches('/').rsplit(['/', '\\']).next().unwrap_or(path).to_string()
    }

    /// Progress indicator text, rounded down so 100.0% only shows at the very end.
    fn progress_label(&self) -> Option<String> {
        self.reading_progress
            .filter(|_| self.show_reading_progress)
            .map(|p| format!("{:.1}%", (p * 1000.0).floor() / 10.0))
    }

    /// Pages left in the chapter after this one, eg. "-12p". A chapter ends
    /// where the next TOC section starts, or with the content file.
    fn chapter_label(&self, rs: &ReadingState, totlines: usize) -> Option<String> {
        if !self.show_reading_progress {
            return None;
        }
        let page = (self.text_rows() * self.spread).max(1) as usize;
        let starts = self.chapter_starts.iter().map(|&(r, _)| r);
        Some(format!("-{}p", pages_left_in_chapter(starts, rs.row.max(0) as usize, totlines, page)))
    }

    /// Title of the current chapter, from the TOC.
    fn chapter_title(&self, rs: &ReadingState) -> Option<String> {
        let row = rs.row.max(0) as usize;
        let n = self.chapter_starts.iter().take_while(|&&(r, _)| r <= row).last().map(|&(_, n)| n);
        n.or(self.chapter_before).and_then(|n| self.toc_labels.get(n)).filter(|l| !l.is_empty()).cloned()
    }

    /// Whether the indicators go on the bottom row instead of the top.
    fn indicators_at_bottom(pad_top: i64, pad_bottom: i64) -> bool {
        pad_bottom > pad_top
    }

    /// What goes on the indicator row: pages left in the chapter (or a
    /// count being typed), the chapter title when the row is padding
    /// rather than beside the text, and the book progress.
    fn indicators(&self, rs: &ReadingState, totlines: usize, countstring: &str) -> Indicators {
        let bottom = Self::indicators_at_bottom(self.pad_top, self.pad_bottom);
        let row_is_padding = if bottom { self.pad_bottom > 0 } else { self.top_pad > 0 };
        Indicators {
            bottom,
            left: if countstring.is_empty() {
                self.chapter_label(rs, totlines).unwrap_or_default()
            } else {
                countstring.to_string()
            },
            center: self.chapter_title(rs).filter(|_| self.show_reading_progress && row_is_padding),
            right: self.progress_label().unwrap_or_default(),
        }
    }

    /// Screen rows available for text.
    fn text_rows(&self) -> i64 {
        self.size().0 - self.top_pad - self.pad_bottom
    }

    /// The part of the screen the text is drawn in.
    fn text_area(&self) -> Rect {
        let (rows, cols) = self.size();
        let top = self.top_pad.clamp(0, rows.max(0));
        let height = (rows - top - self.pad_bottom).max(0);
        Rect::new(0, top as u16, cols.max(0) as u16, height as u16)
    }

    /// Text rows covered by the bottom status line (eg. while searching):
    /// none if it fits in the bottom padding.
    fn status_rows(&self) -> i64 {
        (self.pad_bottom == 0) as i64
    }

    /// Single-page text width for a screen `cols` wide: the configured
    /// width capped to the screen, or the full screen if none is set.
    fn text_width(&self, cols: i64) -> i64 {
        let max = (cols - 4).max(MIN_TEXTWIDTH);
        self.setting.width.map_or(max, |w| w.clamp(MIN_TEXTWIDTH, max))
    }

    /// Remember the width and paddings for next time.
    fn save_layout(&mut self) {
        if let Err(e) = save_layout(self.setting.width, self.pad_top, self.pad_bottom) {
            self.show_win_error("Error Saving Settings", &format!("{e:#}"), &[]).ok();
        }
    }

    /// Rows to reserve above the text for `pad_top`, plus one for the
    /// indicators if they go on top and don't fit beside the text.
    fn effective_top_pad(&self, pad_top: i64, pad_bottom: i64, textwidth: i64, cols: i64) -> i64 {
        let indicators_on_top = !Self::indicators_at_bottom(pad_top, pad_bottom);
        pad_top.max((indicators_on_top && self.needs_top_pad(textwidth, cols)) as i64)
    }

    /// Whether the progress indicator needs its own row because it doesn't
    /// fit to the right of the text.
    fn needs_top_pad(&self, textwidth: i64, cols: i64) -> bool {
        self.show_reading_progress && cols - Board::text_end(cols, textwidth, self.spread) <= PROGRESS_MAX_WIDTH
    }

    fn base_style(&self) -> Style {
        let (fg, bg) = match self.color_pair {
            2 => (self.setting.dark_color_fg, self.setting.dark_color_bg),
            3 => (self.setting.light_color_fg, self.setting.light_color_bg),
            _ => (self.setting.default_color_fg, self.setting.default_color_bg),
        };
        Style::default().fg(color(fg)).bg(color(bg))
    }

    /// Ctrl+Z: give the terminal back to the shell and stop like any other
    /// job (raw mode keeps the terminal from doing this itself). Once resumed
    /// with `fg`, take the terminal back and clear it for a full repaint.
    fn suspend(&mut self) -> Result<()> {
        #[cfg(unix)]
        {
            use ratatui::crossterm::{cursor, execute, terminal};
            let mut out = std::io::stdout();
            if self.setting.mouse_support {
                execute!(out, event::DisableMouseCapture)?;
            }
            disable_key_disambiguation();
            execute!(out, terminal::LeaveAlternateScreen, cursor::Show)?;
            terminal::disable_raw_mode()?;

            // SAFETY: raising a signal on the current process; the default
            // SIGTSTP action stops us here until SIGCONT
            unsafe {
                libc::raise(libc::SIGTSTP);
            }

            terminal::enable_raw_mode()?;
            execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
            enable_key_disambiguation();
            if self.setting.mouse_support {
                execute!(out, event::EnableMouseCapture)?;
            }
            self.term.clear()?;
        }
        Ok(())
    }

    fn read_input(&mut self, timeout: Option<Duration>) -> Result<Option<Input>> {
        loop {
            if let Some(t) = timeout
                && !event::poll(t)? {
                    return Ok(None);
                }
            let key = match event::read()? {
                Event::Resize(_, _) => Key::Resize,
                Event::Mouse(m) => return Ok(Some(Input::Mouse(m))),
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    match k.code {
                        KeyCode::Char('c') if ctrl => Key::CtrlC,
                        KeyCode::Char('o') if ctrl => Key::CtrlO,
                        // only with key disambiguation, else it arrives as Tab
                        KeyCode::Char('i') if ctrl => Key::CtrlI,
                        KeyCode::Char('z') if ctrl && cfg!(unix) => {
                            self.suspend()?;
                            // everything needs redrawing (and the terminal may
                            // have been resized meanwhile)
                            Key::Resize
                        }
                        KeyCode::Char(_) if ctrl => continue,
                        KeyCode::Char(c) => Key::Char(c),
                        KeyCode::Up => Key::Up,
                        KeyCode::Down => Key::Down,
                        KeyCode::Left => Key::Left,
                        KeyCode::Right => Key::Right,
                        KeyCode::PageUp => Key::PageUp,
                        KeyCode::PageDown => Key::PageDown,
                        KeyCode::Home => Key::Home,
                        KeyCode::End => Key::End,
                        KeyCode::Tab => Key::Tab,
                        KeyCode::Enter => Key::Enter,
                        KeyCode::Esc => Key::Esc,
                        KeyCode::Backspace => Key::Backspace,
                        _ => continue,
                    }
                }
                _ => continue,
            };
            return Ok(Some(Input::Key(key)));
        }
    }

    /// Blocking read of a key, ignoring mouse events.
    fn getch(&mut self) -> Result<Option<Key>> {
        loop {
            match self.read_input(None)? {
                Some(Input::Key(k)) => return Ok(Some(k)),
                Some(Input::Mouse(m)) if matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_)) => {}
                _ => return Ok(None),
            }
        }
    }

    /// Draw on top of the last reading screen.
    fn draw_overlay(&mut self, render: impl FnOnce(&mut Frame, Style)) -> Result<()> {
        let base = self.base.clone();
        let style = self.base_style();
        self.term.draw(|f| {
            match &base {
                Some(b) if b.area == f.area() => f.buffer_mut().content.clone_from(&b.content),
                _ => {
                    let area = f.area();
                    f.buffer_mut().set_style(area, style);
                }
            }
            render(f, style);
        })?;
        Ok(())
    }

    fn draw_screen(&mut self, render: impl FnOnce(&mut Frame, Style)) -> Result<()> {
        let style = self.base_style();
        let completed = self.term.draw(|f| {
            let area = f.area();
            f.buffer_mut().set_style(area, style);
            render(f, style);
        })?;
        self.base = Some(completed.buffer.clone());
        Ok(())
    }

    fn show_loader(&mut self, subtext: &str) -> Result<()> {
        let subtext = subtext.to_string();
        self.draw_screen(|f, style| {
            let area = f.area();
            let middle = area.height.saturating_sub(1) / 2;
            let lines = [center("\u{231B}", area.width as usize), center(&subtext, area.width as usize)];
            for (i, l) in lines.iter().enumerate() {
                let y = middle + i as u16;
                if y < area.height {
                    f.buffer_mut().set_stringn(0, y, l, area.width as usize, style);
                }
            }
        })
    }

    // -----------------------------------------------------------------
    // windows
    // -----------------------------------------------------------------

    fn window_frame(f: &mut Frame, style: Style, title: &str) -> Rect {
        let area = f.area();
        let win = Rect::new(2, 2, area.width.saturating_sub(4), area.height.saturating_sub(4));
        f.render_widget(Clear, win);
        f.render_widget(Block::bordered().style(style), win);
        let max = (area.width as usize).saturating_sub(8);
        let title: String = title.chars().take(max).collect();
        let buf = f.buffer_mut();
        buf.set_stringn(4, 3, &title, max, style);
        buf.set_stringn(4, 4, "-".repeat(str_width(&title)), max, style);
        win
    }

    fn choice_win(
        &mut self,
        title: &str,
        options: &[String],
        mut index: usize,
        current: Option<usize>,
        own_keys: &[Key],
        allowdel: bool,
    ) -> Result<Choice> {
        let totlines = options.len();
        let mut y = 0usize;
        let mut countstring = String::new();
        let mut key: Option<Key> = None;
        let is_yes_no = options.len() == 2 && options[0] == "(Y)es" && options[1] == "(N)o";

        loop {
            let (rows, cols) = self.size();
            let list_top = 6 + allowdel as i64;
            let padhi = (rows - 4 - list_top).max(1) as usize;
            let list_width = (cols - 11).max(1) as usize;

            if key.is_some_and(|k| self.keymap.has(Action::Quit, Some(k)) || own_keys.contains(&k)) {
                return Ok(Choice::default());
            }
            let count = countstring.parse::<usize>().unwrap_or(1);
            if let Some(d) = key.and_then(Key::digit) {
                countstring.push(d);
            } else if key.is_some() {
                let km = &self.keymap;
                let last = totlines.saturating_sub(1);
                let max_y = totlines.saturating_sub(padhi);
                if km.has(Action::ScrollUp, key) {
                    index = index.saturating_sub(count);
                } else if km.has(Action::ScrollDown, key) {
                    index = (index + count).min(last);
                } else if km.has(Action::PageUp, key) {
                    // move the selection and the view by whole pages
                    let jump = padhi * count;
                    index = index.saturating_sub(jump);
                    y = y.saturating_sub(jump);
                } else if km.has(Action::PageDown, key) {
                    let jump = padhi * count;
                    index = (index + jump).min(last);
                    y = (y + jump).min(max_y);
                } else if km.has(Action::Follow, key) {
                    return Ok(Choice { index: Some(index), ..Default::default() });
                } else if km.has(Action::BeginningOfCh, key) {
                    index = 0;
                } else if km.has(Action::EndOfCh, key) {
                    index = totlines.saturating_sub(1);
                } else if key == Some(Key::Char('D')) && allowdel {
                    return Ok(Choice { index: Some(index.saturating_sub(1)), delete: Some(index), ..Default::default() });
                } else if key == Some(Key::Char('d')) && allowdel {
                    let confirm = self.choice_win(
                        &format!("Delete '{}'?", options[index]),
                        &["(Y)es".to_string(), "(N)o".to_string()],
                        0,
                        None,
                        &[Key::Char('n')],
                        false,
                    )?;
                    if confirm.key.is_some() {
                        key = confirm.key;
                        continue;
                    } else if confirm.index == Some(0) {
                        return Ok(Choice { index: Some(index.saturating_sub(1)), delete: Some(index), ..Default::default() });
                    }
                } else if is_yes_no && matches!(key, Some(Key::Char('Y' | 'y' | 'N' | 'n'))) {
                    let yes = matches!(key, Some(Key::Char('Y' | 'y')));
                    return Ok(Choice { index: Some(if yes { 0 } else { 1 }), ..Default::default() });
                } else if key.is_some_and(|k| self.win_keys.contains(&k) && !own_keys.contains(&k)) {
                    return Ok(Choice { key, index: Some(index), ..Default::default() });
                }
                countstring.clear();
            }

            while index < y {
                y -= 1;
            }
            while index >= y + padhi {
                y += 1;
            }

            self.draw_overlay(|f, style| {
                Self::window_frame(f, style, title);
                let buf = f.buffer_mut();
                if allowdel {
                    buf.set_stringn(4, 5, "HINT: Press 'd' to delete.", list_width, style);
                }
                for (n, opt) in options.iter().enumerate().skip(y).take(padhi) {
                    // ">>" marks the current entry (eg. the chapter being read);
                    // the selection is shown by highlighting alone
                    let pre = if Some(n) == current { ">>" } else { "  " };
                    let text = format!("{pre}{}", opt.replace('\n', " "));
                    let text: String = text.chars().take(list_width).collect();
                    let s = if n == index { style.add_modifier(Modifier::REVERSED) } else { style };
                    buf.set_stringn(6, (list_top as usize + n - y) as u16, &text, list_width, s);
                }
            })?;

            key = match self.read_input(None)? {
                Some(Input::Key(k)) => Some(k),
                Some(Input::Mouse(m)) => {
                    let row = m.row as i64;
                    let in_list = row >= list_top && row < rows - 4 && ((row - list_top) as usize) < totlines - y;
                    match m.kind {
                        MouseEventKind::ScrollUp => Some(self.keymap.first(Action::ScrollUp)),
                        MouseEventKind::ScrollDown => Some(self.keymap.first(Action::ScrollDown)),
                        MouseEventKind::Down(MouseButton::Left) if in_list => {
                            let clicked = (row - list_top) as usize + y;
                            if clicked == index {
                                Some(self.keymap.first(Action::Follow))
                            } else {
                                index = clicked;
                                None
                            }
                        }
                        MouseEventKind::Down(MouseButton::Right) => Some(self.keymap.first(Action::Quit)),
                        _ => None,
                    }
                }
                None => None,
            };
        }
    }

    /// Scrollable text window; returns a window key that closed it, if any.
    fn text_win(&mut self, title: &str, raw_texts: &str, own_keys: &[Key]) -> Result<Option<Key>> {
        let mut y: i64 = 0;
        let mut key: Option<Key> = None;
        loop {
            let (rows, cols) = self.size();
            let wrap_width = (cols - 10).max(1) as usize;
            let texts: Vec<String> = raw_texts
                .lines()
                .flat_map(|l| {
                    let w: Vec<String> = wrap(l, wrap_width).into_iter().map(|w| w.text).collect();
                    if w.is_empty() { vec![String::new()] } else { w }
                })
                .collect();
            let totlines = texts.len() as i64;
            let padhi = (rows - 10).max(1);

            if key.is_some_and(|k| self.keymap.has(Action::Quit, Some(k)) || own_keys.contains(&k)) {
                return Ok(None);
            }
            let km = &self.keymap;
            if km.has(Action::ScrollUp, key) && y > 0 {
                y -= 1;
            } else if km.has(Action::ScrollDown, key) && y < totlines - padhi {
                y += 1;
            } else if km.has(Action::PageUp, key) {
                y = pgup(y, padhi);
            } else if km.has(Action::PageDown, key) {
                y = pgdn(y, totlines, padhi);
            } else if km.has(Action::BeginningOfCh, key) {
                y = 0;
            } else if km.has(Action::EndOfCh, key) {
                y = pgend(totlines, padhi);
            } else if key.is_some_and(|k| self.win_keys.contains(&k) && !own_keys.contains(&k)) {
                return Ok(key);
            }

            self.draw_overlay(|f, style| {
                Self::window_frame(f, style, title);
                let buf = f.buffer_mut();
                for (n, line) in texts.iter().skip(y as usize).take(padhi as usize).enumerate() {
                    buf.set_stringn(6, 6 + n as u16, line, wrap_width, style);
                }
            })?;

            key = match self.read_input(None)? {
                Some(Input::Key(k)) => Some(k),
                Some(Input::Mouse(m)) => match m.kind {
                    MouseEventKind::ScrollUp => Some(self.keymap.first(Action::ScrollUp)),
                    MouseEventKind::ScrollDown => Some(self.keymap.first(Action::ScrollDown)),
                    MouseEventKind::Down(MouseButton::Right) => Some(self.keymap.first(Action::Quit)),
                    _ => None,
                },
                None => None,
            };
        }
    }

    fn show_win_error(&mut self, title: &str, msg: &str, keys: &[Key]) -> Result<Option<Key>> {
        self.text_win(title, msg, keys)
    }

    fn show_win_metadata(&mut self) -> Result<Option<Key>> {
        let path = self.ebook.path().to_string();
        let mut mdata = match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => format!(
                "[File Info]\nPATH: {path}\nSIZE: {:.2} MB\n \n[Book Info]\n",
                m.len() as f64 / 1024f64.powi(2)
            ),
            _ => format!("[File Info]\nPATH: {path}\n \n[Book Info]\n"),
        };
        let meta = self.ebook.get_meta();
        for (name, value) in meta.fields() {
            if let Some(v) = value.as_ref().filter(|v| !v.is_empty()) {
                let mut title = name.to_string();
                title[..1].make_ascii_uppercase();
                mdata += &format!("{title}: {}\n", strip_tags(v));
            }
        }
        let keys = self.keymap.get(Action::Metadata).to_vec();
        self.text_win("Metadata", &mdata, &keys)
    }

    fn show_win_help(&mut self) -> Result<Option<Key>> {
        let mut src = String::from("Key Bindings:\n");
        let dig = self.keymap_user.iter().map(|(_, k)| k.chars().count()).max().unwrap_or(1) + 2;
        for (name, key) in &self.keymap_user {
            src += &format!("{key:>dig$}  {}\n", split_camel_case(name));
        }
        src += &format!("{:>dig$}  Jump Back\n", "^O");
        src += &format!("{:>dig$}  Jump Forward\n", "^I");
        let keys = self.keymap.get(Action::Help).to_vec();
        self.text_win("Help", &src, &keys)
    }

    fn input_prompt(&mut self, prompt: &str) -> Result<PromptResult> {
        let mut text = String::new();
        loop {
            let (_, cols) = self.size();
            let shown = if str_width(prompt) + str_width(&text) < cols as usize {
                text.clone()
            } else {
                let keep = (cols as usize).saturating_sub(str_width(prompt) + 4);
                let chars: Vec<char> = text.chars().collect();
                format!("...{}", chars[chars.len().saturating_sub(keep)..].iter().collect::<String>())
            };
            self.draw_overlay(|f, style| {
                let area = f.area();
                let y = area.height.saturating_sub(1);
                let line = Rect::new(0, y, area.width, 1);
                f.render_widget(Clear, line);
                f.buffer_mut().set_style(line, style);
                let (x, _) = f.buffer_mut().set_stringn(0, y, prompt, area.width as usize, style.add_modifier(Modifier::REVERSED));
                let (x, _) = f.buffer_mut().set_stringn(x, y, &shown, area.width.saturating_sub(x) as usize, style);
                f.set_cursor_position(Position::new(x.min(area.width.saturating_sub(1)), y));
            })?;
            if let Some(Input::Key(k)) = self.read_input(None)? { match k {
                Key::Esc | Key::CtrlC => return Ok(PromptResult::Cancel),
                Key::Enter => {
                    return Ok(if text.is_empty() { PromptResult::Cancel } else { PromptResult::Text(text) });
                }
                Key::Backspace => {
                    text.pop();
                }
                Key::Resize => return Ok(PromptResult::Key(Key::Resize)),
                Key::Char(c) => text.push(c),
                Key::Tab => text.push('\t'),
                _ => {}
            } }
        }
    }

    fn show_win_choices_bookmarks(&mut self) -> Result<(Option<Key>, Option<usize>)> {
        let path = self.ebook.path().to_string();
        let own = self.keymap.get(Action::ShowBookmarks).to_vec();
        let mut idx = 0;
        loop {
            let bookmarks: Vec<String> = self.state.get_bookmarks(&path)?.into_iter().map(|b| b.0).collect();
            if bookmarks.is_empty() {
                return Ok((Some(own[0]), None));
            }
            let choice = self.choice_win("Bookmarks", &bookmarks, idx.min(bookmarks.len() - 1), None, &own, true)?;
            match choice.delete {
                Some(todel) => {
                    self.state.delete_bookmark(&path, &bookmarks[todel])?;
                    idx = choice.index.unwrap_or(0);
                }
                None => return Ok((choice.key, choice.index)),
            }
        }
    }

    fn show_win_library(&mut self) -> Result<(Option<Key>, Option<usize>)> {
        let own = self.keymap.get(Action::Library).to_vec();
        loop {
            let items = self.state.get_from_history()?;
            if items.is_empty() {
                return Ok((Some(own[0]), None));
            }
            let labels: Vec<String> = items.iter().map(|i| i.to_string()).collect();
            let current = items.iter().position(|i| i.filepath == self.ebook.path());
            let choice = self.choice_win("Library", &labels, 0, current, &own, true)?;
            match choice.delete {
                Some(todel) => self.state.delete_from_library(&items[todel].filepath)?,
                None => return Ok((choice.key, choice.index)),
            }
        }
    }

    // -----------------------------------------------------------------
    // external programs
    // -----------------------------------------------------------------

    fn ext_dict_app(&self) -> Option<String> {
        let configured = &self.setting.dictionary_client;
        if which(configured.split_whitespace().next().unwrap_or("")).is_some() {
            return Some(configured.clone());
        }
        let app = DICT_PRESET_LIST.iter().find(|a| which(a).is_some())?;
        Some(if *app == "sdcv" { "sdcv -n".to_string() } else { app.to_string() })
    }

    fn image_viewer(&self) -> Option<String> {
        let configured = &self.setting.default_viewer;
        let viewer = if which(configured.split_whitespace().next().unwrap_or("")).is_some() {
            configured.clone()
        } else if cfg!(windows) {
            "start".to_string()
        } else if cfg!(target_os = "macos") {
            "open".to_string()
        } else {
            VIEWER_PRESET_LIST.iter().find(|v| which(v).is_some())?.to_string()
        };
        Some(if viewer == "gio" { "gio open".to_string() } else { viewer })
    }

    fn shell(cmdline: &str) -> Command {
        if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", cmdline]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", cmdline]);
            c
        }
    }

    fn open_image(&mut self, name: &str, bytes: &[u8]) -> Result<Option<Key>> {
        let viewer = self.image_viewer().ok_or_else(|| anyhow!("no image viewer found"))?;
        let suffix = std::path::Path::new(name)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let mut tmp = tempfile::Builder::new().suffix(&suffix).tempfile()?;
        std::io::Write::write_all(&mut tmp, bytes)?;
        let path = tmp.into_temp_path();
        let quoted = if cfg!(windows) { format!("\"{}\"", path.display()) } else { shell_quote(&path.to_string_lossy()) };
        Self::shell(&format!("{viewer} {quoted}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        // keep the temp file around until a key is pressed, since some
        // viewers return before they have read the file
        let k = self.getch()?;
        drop(path);
        Ok(k)
    }

    fn define_word(&mut self, word: &str) -> Result<Option<Key>> {
        let app = self.ext_dict_app().ok_or_else(|| anyhow!("no dictionary client found"))?;
        self.draw_overlay(|f, style| {
            let area = f.area();
            let (hi, wi) = (5u16, 16u16);
            let r = Rect::new(area.width.saturating_sub(wi) / 2, area.height.saturating_sub(hi) / 2, wi, hi)
                .intersection(area);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(vec![Line::from(""), Line::from("  Loading...")]).block(Block::bordered()).style(style),
                r,
            );
        })?;
        let out = Self::shell(&format!("{app} {}", shell_quote(word))).stdin(Stdio::null()).output()?;
        let keys = self.keymap.get(Action::DefineWord).to_vec();
        if out.stderr.is_empty() {
            let title = format!("Definition: {}", word.to_uppercase());
            self.text_win(&title, &String::from_utf8_lossy(&out.stdout), &keys)
        } else {
            self.text_win(&format!("Error: {app}"), &String::from_utf8_lossy(&out.stderr), &keys)
        }
    }

    // -----------------------------------------------------------------
    // search
    // -----------------------------------------------------------------

    fn draw_with_status(&mut self, board: &Board, rs: &ReadingState, letters_prefix: &[usize], msg: &str) -> Result<()> {
        let msg = msg.to_string();
        let text_area = self.text_area();
        let status_rows = self.status_rows() as usize;
        self.calculate_reading_progress(letters_prefix, rs);
        let indicators = self.indicators(rs, letters_prefix.len() - 1, "");
        self.draw_screen(|f, style| {
            let area = f.area();
            board.render(f.buffer_mut(), text_area, rs.row.max(0) as usize, status_rows, style);
            // the status line takes the bottom row
            if !indicators.bottom {
                indicators.draw(f.buffer_mut(), style);
            }
            let y = area.height.saturating_sub(1);
            f.buffer_mut().set_stringn(0, y, &msg, area.width as usize, style.add_modifier(Modifier::REVERSED));
        })
    }

    fn searching(
        &mut self,
        board: &mut Board,
        src: &[String],
        letters_prefix: &[usize],
        mut rs: ReadingState,
        tot: usize,
    ) -> Result<SearchOutcome> {
        let (_, cols) = self.size();
        let rows = self.text_rows();
        if self.search_data.is_none() {
            match self.input_prompt(" Regex:")? {
                PromptResult::Text(value) => {
                    self.search_data = Some(SearchData { direction: Direction::Forward, value })
                }
                PromptResult::Key(k) => return Ok(SearchOutcome::Key(Some(k))),
                PromptResult::Cancel => return Ok(SearchOutcome::Key(None)),
            }
        }
        let value = self.search_data.as_ref().unwrap().value.clone();
        let pattern = match regex::RegexBuilder::new(&value).case_insensitive(true).build() {
            Ok(p) => p,
            Err(e) => {
                self.search_data = None;
                return Ok(SearchOutcome::Key(self.show_win_error("!Regex Error", &e.to_string(), &[])?));
            }
        };

        // (row, col, n_letters) in chars
        let mut found: Vec<(usize, usize, usize)> = Vec::new();
        for (n, line) in src.iter().enumerate() {
            for m in pattern.find_iter(line) {
                if m.is_empty() {
                    continue;
                }
                let col = line[..m.start()].chars().count();
                found.push((n, col, m.as_str().chars().count()));
            }
        }

        let ci = rs.content_index;
        let next = |ci: usize| SearchOutcome::State(ReadingState::new(ci, rs.textwidth, 0));
        let set_direction = |s: &mut Option<SearchData>, d| {
            if let Some(sd) = s.as_mut() {
                sd.direction = d;
            }
        };

        if found.is_empty() {
            let direction = self.search_data.as_ref().unwrap().direction;
            if direction == Direction::Forward && ci + 1 < tot {
                return Ok(next(ci + 1));
            } else if direction == Direction::Backward && ci > 0 {
                return Ok(next(ci - 1));
            }
            let mut s: Option<Key> = None;
            loop {
                if self.keymap.has(Action::Quit, s) {
                    self.search_data = None;
                    return Ok(SearchOutcome::State(rs));
                } else if s == Some(Key::Char('n')) && ci == 0 && ci + 1 < tot {
                    set_direction(&mut self.search_data, Direction::Forward);
                    return Ok(next(ci + 1));
                } else if s == Some(Key::Char('N')) && ci + 1 == tot && ci > 0 {
                    set_direction(&mut self.search_data, Direction::Backward);
                    return Ok(next(ci - 1));
                } else if s == Some(Key::Resize) {
                    return Ok(SearchOutcome::Key(s));
                }
                let shown: String = value.chars().take((cols - 22).max(0) as usize).collect();
                self.draw_with_status(board, &rs, letters_prefix, &format!(" Finished searching: {shown} "))?;
                s = self.getch()?;
            }
        }

        let mut sidx = found.len() - 1;
        if self.search_data.as_ref().unwrap().direction == Direction::Forward {
            if rs.row > found[found.len() - 1].0 as i64 && ci + 1 < tot {
                return Ok(next(ci + 1));
            }
            if let Some(n) = found.iter().position(|f| f.0 as i64 >= rs.row) {
                sidx = n;
            }
        }

        let progress_msg = |sidx: usize| {
            format!(" Searching: {value} --- Res {}/{} Ch {}/{} ", sidx + 1, found.len(), ci + 1, tot)
        };
        let mut msg = progress_msg(sidx);
        let mut s: Option<Key> = None;
        let page = ((rows - self.status_rows()) * self.spread).max(1);
        loop {
            if self.keymap.has(Action::Quit, s) {
                self.search_data = None;
                board.feed_temporary_style(Vec::new());
                return Ok(SearchOutcome::State(rs));
            } else if s == Some(Key::Char('n')) {
                set_direction(&mut self.search_data, Direction::Forward);
                if sidx == found.len() - 1 {
                    if ci + 1 < tot {
                        return Ok(next(ci + 1));
                    }
                    s = None;
                    msg = format!(" Finished searching: {value} ");
                    continue;
                }
                sidx += 1;
                msg = progress_msg(sidx);
            } else if s == Some(Key::Char('N')) {
                set_direction(&mut self.search_data, Direction::Backward);
                if sidx == 0 {
                    if ci > 0 {
                        return Ok(next(ci - 1));
                    }
                    s = None;
                    msg = format!(" Finished searching: {value} ");
                    continue;
                }
                sidx -= 1;
                msg = progress_msg(sidx);
            } else if s == Some(Key::Resize) {
                return Ok(SearchOutcome::Key(s));
            }

            let target = found[sidx].0 as i64;
            while !(rs.row..rs.row + page).contains(&target) {
                if target > rs.row {
                    rs.row += page;
                } else {
                    rs.row = (rs.row - page).max(0);
                }
            }

            board.feed_temporary_style(
                found
                    .iter()
                    .enumerate()
                    .map(|(n, &(row, col, len))| InlineStyle {
                        row,
                        col,
                        n_letters: len,
                        attr: if n == sidx { Modifier::REVERSED } else { Modifier::empty() },
                    })
                    .collect(),
            );
            self.draw_with_status(board, &rs, letters_prefix, &msg)?;
            s = self.getch()?;
        }
    }

    // -----------------------------------------------------------------
    // state
    // -----------------------------------------------------------------

    fn savestate(&mut self, rs: &ReadingState) -> Result<()> {
        let rs = if self.seamless { self.abs_to_rel(rs) } else { rs.clone() };
        let path = self.ebook.path().to_string();
        self.state.set_last_reading_state(&path, &rs)?;
        let meta = self.ebook.get_meta();
        self.state.update_library(&path, meta.title.as_deref(), meta.creator.as_deref(), self.reading_progress)
    }

    pub fn cleanup(&mut self) {
        self.ebook.cleanup();
        // a still-running letter counting thread is detached; it holds its
        // own copy of the ebook
        self.letters_thread = None;
    }

    fn run_counting_letters(&mut self) {
        let path = self.ebook.path().to_string();
        if is_url(&path) {
            self.letters_count = Some(count_letters_of(&mut *self.ebook));
            return;
        }
        self.letters_thread = Some(std::thread::spawn(move || {
            let counted = get_ebook_obj(&path).and_then(|mut e| {
                e.initialize()?;
                Ok(count_letters_of(&mut *e))
            });
            counted.unwrap_or(LettersCount { all: 0, cumulative: Vec::new() })
        }));
    }

    fn try_assign_letters_count(&mut self, force_wait: bool) {
        if self.letters_thread.as_ref().is_some_and(|h| force_wait || h.is_finished())
            && let Ok(count) = self.letters_thread.take().unwrap().join() {
                self.letters_count = Some(count);
            }
    }

    /// `letters_prefix[i]`: letters in the first `i` lines of the text.
    fn calculate_reading_progress(&mut self, letters_prefix: &[usize], rs: &ReadingState) {
        let page = self.text_rows() * self.spread;
        let totlines = letters_prefix.len() as i64 - 1;
        let is_last_content = self.seamless || rs.content_index + 1 >= self.ebook.contents().len();
        if let Some(lc) = self.letters_count.as_ref().filter(|lc| lc.all > 0) {
            // letters before the top of the page; 100% once the end of the book is on screen
            self.reading_progress = Some(if is_last_content && rs.row + page >= totlines {
                1.0
            } else {
                let cum = lc.cumulative.get(rs.content_index).copied().unwrap_or(0);
                let top = rs.row.clamp(0, totlines) as usize;
                (cum + letters_prefix[top]) as f64 / lc.all as f64
            });
        }
    }

    fn abs_to_rel(&self, rs: &ReadingState) -> ReadingState {
        let t = &self.totlines_per_content;
        if t.is_empty() {
            return rs.clone();
        }
        let all: usize = t.iter().sum();
        let mut index = 0;
        let mut cumulative = 0;
        let mut content_lines;
        loop {
            content_lines = t[index];
            cumulative += content_lines;
            if cumulative as i64 > rs.row || index == t.len() - 1 {
                break;
            }
            index += 1;
        }
        let before = (cumulative - content_lines) as f64;
        ReadingState {
            content_index: index,
            textwidth: rs.textwidth,
            row: rs.row - before as i64,
            rel_pctg: rs.rel_pctg.filter(|p| *p != 0.0).map(|p| p - before / all as f64),
            section: rs.section.clone(),
        }
    }

    fn rel_to_abs(&self, rs: &ReadingState) -> ReadingState {
        let t = &self.totlines_per_content;
        let before: usize = t.iter().take(rs.content_index).sum();
        let all: usize = t.iter().sum::<usize>().max(1);
        let row = rs.row + before as i64;
        ReadingState {
            content_index: 0,
            textwidth: rs.textwidth,
            row,
            rel_pctg: rs.rel_pctg.filter(|p| *p != 0.0).map(|_| row as f64 / all as f64),
            section: rs.section.clone(),
        }
    }

    fn section_ids(&self) -> HashSet<String> {
        self.ebook.toc_entries().iter().filter_map(|e| e.section.clone()).collect()
    }

    fn get_all_book_contents(&mut self, rs: &ReadingState) -> Result<(TextStructure, Vec<TocEntry>, Vec<String>)> {
        let contents = self.ebook.contents().to_vec();
        let toc_entries = self.ebook.toc_entries().to_vec();
        let section_ids = self.section_ids();
        let mut ts = TextStructure::default();
        let mut toc_tmp = Vec::new();
        let mut section_rows_tmp = HashMap::new();
        self.totlines_per_content.clear();

        for (n, content) in contents.iter().enumerate() {
            self.show_loader(&format!("loading contents ({}/{})", n + 1, contents.len()))?;
            let starting_line: usize = self.totlines_per_content.iter().sum();
            let html = self.ebook.get_raw_text(content)?;
            let tmp = parse_html(&html, rs.textwidth.max(1) as usize, &section_ids, starting_line);
            self.totlines_per_content.push(tmp.text_lines.len());

            for entry in toc_entries.iter().filter(|e| e.content_index == n) {
                let section = match &entry.section {
                    Some(s) if tmp.section_rows.contains_key(s) => s.clone(),
                    _ => {
                        let id = format!("__ers_content_{n}_{}", toc_tmp.len());
                        section_rows_tmp.insert(id.clone(), starting_line);
                        id
                    }
                };
                toc_tmp.push(TocEntry { label: entry.label.clone(), content_index: 0, section: Some(section) });
            }
            ts.merge(tmp);
        }
        ts.section_rows.extend(section_rows_tmp);
        Ok((ts, toc_tmp, contents.into_iter().take(1).collect()))
    }

    fn get_current_book_content(&mut self, rs: &ReadingState) -> Result<(TextStructure, Vec<TocEntry>, Vec<String>)> {
        let contents = self.ebook.contents().to_vec();
        let toc_entries = self.ebook.toc_entries().to_vec();
        let html = self.ebook.get_raw_text(&contents[rs.content_index])?;
        let ts = parse_html(&html, rs.textwidth.max(1) as usize, &self.section_ids(), 0);
        Ok((ts, toc_entries, contents))
    }

    fn render_page(&mut self, board: &Board, rs: &ReadingState, countstring: &str, letters_prefix: &[usize]) -> Result<()> {
        let rate = self.setting.page_scroll_animation_rate;
        // with no delay between frames the animation takes no time at all
        if let Some(direction) = self.page_animation.take().filter(|_| self.setting.page_scroll_animation && rate > 0) {
            let style = self.base_style();
            let text_area = self.text_area();
            let width = rs.textwidth.max(0) as usize;
            let start = Instant::now();
            // the last frame is the whole page, drawn below anyway
            let mut i = 1;
            while i < width {
                // a pending key press cuts the animation short; the key is
                // left in the queue so the main loop handles it right away
                if event::poll(Duration::ZERO)? {
                    break;
                }
                self.term.draw(|f| {
                    let area = f.area();
                    f.buffer_mut().set_style(area, style);
                    board.render_partial(f.buffer_mut(), text_area, rs.row as usize, i, direction, style);
                })?;
                let due = start + Duration::from_millis(rate * i as u64);
                std::thread::sleep(due.saturating_duration_since(Instant::now()));
                // skip the frames a slow terminal fell behind on, so it
                // doesn't drag the animation out
                i = (i + 1).max((start.elapsed().as_millis() / rate as u128) as usize + 1);
            }
        }

        self.try_assign_letters_count(false);
        self.calculate_reading_progress(letters_prefix, rs);

        let indicators = self.indicators(rs, letters_prefix.len() - 1, countstring);
        let text_area = self.text_area();
        self.draw_screen(|f, style| {
            board.render(f.buffer_mut(), text_area, rs.row.max(0) as usize, 0, style);
            indicators.draw(f.buffer_mut(), style);
        })
    }

    /// Where to go to show `pct` percent of the book.
    fn go_to_percent(&mut self, pct: f64, rs: &ReadingState, letters_prefix: &[usize]) -> Result<GoToTarget> {
        self.try_assign_letters_count(true);
        let Some(lc) = self.letters_count.clone().filter(|lc| lc.all > 0) else {
            return Ok(GoToTarget::Unavailable);
        };
        let target = (pct / 100.0 * lc.all as f64).round() as usize;
        let page = self.text_rows() * self.spread;
        // rows can go down to the last line of a content, but in the last
        // content only to its last full page
        let max_row = |prefix: &[usize], last: bool| {
            let totlines = prefix.len() as i64 - 1;
            if last { pgend(totlines, page) } else { totlines - 1 }
        };
        if self.seamless {
            // the whole book is loaded as one text
            let row = row_for_letters(letters_prefix, target, max_row(letters_prefix, true));
            return Ok(GoToTarget::Row(row));
        }

        let n = lc.cumulative.len();
        let end = |i: usize| lc.cumulative.get(i + 1).copied().unwrap_or(lc.all);
        // first content (with any text) that reaches the target; 0% is the very start
        let ci = if target == 0 {
            0
        } else {
            (0..n).find(|&i| end(i) >= target && end(i) > lc.cumulative[i]).unwrap_or(n - 1)
        };
        let local = target - lc.cumulative[ci].min(target);
        let last = ci == n - 1;
        if ci == rs.content_index {
            return Ok(GoToTarget::Row(row_for_letters(letters_prefix, local, max_row(letters_prefix, last))));
        }
        let content = self.ebook.contents()[ci].clone();
        let html = self.ebook.get_raw_text(&content)?;
        let ts = parse_html(&html, rs.textwidth.max(1) as usize, &HashSet::new(), 0);
        let prefix = letters_prefix_of(&ts.text_lines);
        let row = row_for_letters(&prefix, local, max_row(&prefix, last));
        Ok(GoToTarget::State(ReadingState::new(ci, rs.textwidth, row)))
    }

    /// Wait for input. While letters are still being counted, redraw once
    /// the count is ready so the progress indicator appears without a key press.
    fn wait_input(
        &mut self,
        board: &Board,
        rs: &ReadingState,
        countstring: &str,
        letters_prefix: &[usize],
    ) -> Result<Option<Input>> {
        while self.letters_thread.is_some() {
            if let Some(input) = self.read_input(Some(Duration::from_millis(100)))? {
                return Ok(Some(input));
            }
            if self.letters_thread.as_ref().is_some_and(|h| h.is_finished()) {
                self.render_page(board, rs, countstring, letters_prefix)?;
            }
        }
        self.read_input(None)
    }

    /// Remember `rs` as a place to jump back to with ctrl+o. A new jump
    /// starts a new branch, so there's nothing left to go forward to.
    ///
    /// Repeats of the same jump `key` in quick succession (eg. holding `L`)
    /// count as one jump, remembered from where the first one started.
    /// Jumps picked in a prompt or window pass `None` and never merge.
    fn push_jump(&mut self, rs: ReadingState, key: Option<Key>) {
        let now = Instant::now();
        let repeat = self.last_jump.is_some_and(|(k, t)| Some(k) == key && now - t < JUMP_REPEAT_GAP);
        self.last_jump = key.map(|k| (k, now));
        if repeat {
            return;
        }
        push_bounded(&mut self.jump_history, rs);
        self.jump_forward.clear();
    }

    /// A page turn away from `origin`. Turning a single page is reading,
    /// not jumping, so only a run of quick turns with the same key (eg.
    /// holding it) or a counted one (eg. `5h`) is remembered for ctrl+o,
    /// from where it started.
    fn note_page_turn(&mut self, origin: ReadingState, key: Option<Key>, count: i64) {
        let Some(key) = key else { return };
        if count > 1 {
            self.page_run = None;
            self.push_jump(origin, None);
            return;
        }
        let now = Instant::now();
        match &mut self.page_run {
            Some(run) if run.key == key && now - run.last < JUMP_REPEAT_GAP => {
                run.last = now;
                if !run.recorded {
                    run.recorded = true;
                    let origin = run.origin.clone();
                    self.push_jump(origin, None);
                }
            }
            _ => self.page_run = Some(PageRun { key, last: now, origin, recorded: false }),
        }
    }

    /// Step through the jump history: back (ctrl+o) or forward (ctrl+i),
    /// remembering `current` so the opposite key returns to it.
    fn step_jump_history(&mut self, back: bool, current: ReadingState) -> Option<ReadingState> {
        let (from, to) = if back {
            (&mut self.jump_history, &mut self.jump_forward)
        } else {
            (&mut self.jump_forward, &mut self.jump_history)
        };
        let target = from.pop()?;
        push_bounded(to, current);
        Some(target)
    }

    /// Block while the screen is too small to read on (eg. 0x0 while a
    /// window manager moves the window between monitors). Returns the new
    /// (rows, cols), or `None` if the user quit meanwhile.
    fn wait_for_usable_size(&mut self) -> Result<Option<(i64, i64)>> {
        loop {
            let (rows, cols) = self.size();
            if cols >= 22 && rows >= 12 {
                return Ok(Some((rows, cols)));
            }
            let style = self.base_style();
            self.term.draw(|f| {
                let area = f.area();
                f.buffer_mut().set_style(area, style);
                f.buffer_mut().set_stringn(0, 0, "Screen too small (min 22x12)", area.width as usize, style);
            })?;
            if let Some(Input::Key(k)) = self.read_input(None)?
                && self.keymap.has(Action::Quit, Some(k))
                && k != Key::Esc
            {
                return Ok(None);
            }
        }
    }

    // -----------------------------------------------------------------
    // main loop
    // -----------------------------------------------------------------

    pub fn read(&mut self, mut rs: ReadingState) -> Result<ReadOutcome> {
        let km = self.keymap.clone();
        let mut k: Option<Key> = self.search_data.as_ref().map(|_| km.first(Action::RegexSearch));
        let mut cols = self.size().1;

        let mincols_doublespr = DOUBLE_SPREAD_PADDING_LEFT + 22 + DOUBLE_SPREAD_PADDING_MIDDLE + 22 + DOUBLE_SPREAD_PADDING_RIGHT;
        if cols < mincols_doublespr {
            self.spread = 1;
        }
        if self.spread == 2 {
            rs.textwidth = (cols - DOUBLE_SPREAD_PADDING_LEFT - DOUBLE_SPREAD_PADDING_MIDDLE - DOUBLE_SPREAD_PADDING_RIGHT) / 2;
        } else {
            rs.textwidth = self.text_width(cols);
        }
        let x = if self.spread == 2 { DOUBLE_SPREAD_PADDING_LEFT } else { (cols - rs.textwidth) / 2 };
        self.top_pad = self.effective_top_pad(self.pad_top, self.pad_bottom, rs.textwidth, cols);
        let mut rows = self.text_rows();

        self.show_loader("loading contents")?;
        let (mut ts, toc_entries, contents) = if self.seamless {
            let r = self.get_all_book_contents(&rs)?;
            rs = self.rel_to_abs(&rs);
            r
        } else {
            self.get_current_book_content(&rs)?
        };
        ts.formatting.sort_by_key(|s| s.row);
        let ts = ts;
        let totlines = ts.text_lines.len() as i64;
        // entries without a known section start with their content file
        self.chapter_starts = toc_entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.content_index == rs.content_index)
            .map(|(n, e)| (e.section.as_ref().and_then(|s| ts.section_rows.get(s)).copied().unwrap_or(0), n))
            .collect();
        self.chapter_starts.sort_unstable();
        self.chapter_before = toc_entries.iter().rposition(|e| e.content_index < rs.content_index);
        self.toc_labels =
            toc_entries.iter().map(|e| strip_tags(&e.label).split_whitespace().collect::<Vec<_>>().join(" ")).collect();
        let ncontents = contents.len();

        if rs.row < 0 && totlines <= rows * self.spread {
            rs.row = 0;
        } else if let Some(p) = rs.rel_pctg {
            rs.row = ((p * totlines as f64).round() as i64).clamp(0, totlines - 1);
        } else {
            rs.row = rs.row.rem_euclid(totlines);
        }

        let mut board = Board::new(&ts.text_lines, rs.textwidth, &ts.formatting, self.spread);

        let letters_prefix = letters_prefix_of(&ts.text_lines);

        if let Some(section) = rs.section.as_ref().filter(|s| !s.is_empty()) {
            rs.row = ts.section_rows.get(section).copied().unwrap_or(0) as i64;
        }

        let with_pctg = |rs: &ReadingState| ReadingState { rel_pctg: Some(rs.row as f64 / totlines as f64), ..rs.clone() };
        let mut checkpoint_row: Option<i64> = None;
        let mut countstring = String::new();

        loop {
            let count: i64 = countstring.parse().unwrap_or(1);
            let ntoc = || find_current_content_index(&toc_entries, &ts.section_rows, rs.content_index, rs.row);
            // any other key ends a run of repeated jumps or page turns
            if self.last_jump.is_some_and(|(key, _)| k.is_some() && k != Some(key)) {
                self.last_jump = None;
            }
            if self.page_run.as_ref().is_some_and(|run| k.is_some() && k != Some(run.key)) {
                self.page_run = None;
            }

            if let Some(d) = k.and_then(Key::digit) {
                countstring.push(d);
            } else {
                if km.has(Action::Quit, k) {
                    if k == Some(Key::Esc) && !countstring.is_empty() {
                        countstring.clear();
                    } else {
                        self.try_assign_letters_count(true);
                        self.calculate_reading_progress(&letters_prefix, &rs);
                        self.savestate(&with_pctg(&rs))?;
                        return Ok(ReadOutcome::Quit);
                    }
                } else if km.has(Action::DoubleSpreadToggle, k) {
                    if cols < mincols_doublespr {
                        self.show_win_error(
                            "Screen is too small",
                            &format!("Min: {mincols_doublespr} cols x 12 rows"),
                            &[Key::Char('D')],
                        )?;
                    }
                    self.spread = (self.spread % 2) + 1;
                    return Ok(ReadOutcome::State(ReadingState {
                        rel_pctg: Some(rs.row as f64 / totlines as f64),
                        ..ReadingState::new(rs.content_index, rs.textwidth, rs.row)
                    }));
                } else if km.has(Action::ScrollUp, k) {
                    if self.spread == 2 {
                        k = Some(km.first(Action::PageUp));
                        continue;
                    }
                    if count > 1 {
                        checkpoint_row = Some(rs.row - 1);
                    }
                    if rs.row >= count {
                        rs.row -= count;
                    } else if rs.row == 0 && rs.content_index != 0 {
                        self.page_animation = Some(Direction::Backward);
                        return Ok(ReadOutcome::State(ReadingState::new(rs.content_index - 1, rs.textwidth, -rows)));
                    } else {
                        rs.row = 0;
                    }
                } else if km.has(Action::PageUp, k) {
                    if rs.row != 0 || rs.content_index != 0 {
                        self.note_page_turn(with_pctg(&rs), k, count);
                    }
                    if rs.row == 0 && rs.content_index != 0 {
                        self.page_animation = Some(Direction::Backward);
                        // a negative row counts from the end: read() puts the
                        // last page of the previous chapter on screen
                        let page = (rows * self.spread).max(1);
                        return Ok(ReadOutcome::State(ReadingState::new(rs.content_index - 1, rs.textwidth, -page)));
                    } else if rs.row >= rows * self.spread * count {
                        self.page_animation = Some(Direction::Backward);
                        rs.row = skip_blank_rows(&ts.text_lines, rs.row - rows * self.spread * count);
                    } else {
                        rs.row = 0;
                    }
                } else if km.has(Action::ScrollDown, k) {
                    if self.spread == 2 {
                        k = Some(km.first(Action::PageDown));
                        continue;
                    }
                    if count > 1 {
                        checkpoint_row = Some(rs.row + rows - 1);
                    }
                    if rs.row + count <= totlines - rows {
                        rs.row += count;
                    } else if rs.row >= totlines - rows && rs.content_index != ncontents - 1 {
                        self.page_animation = Some(Direction::Forward);
                        return Ok(ReadOutcome::State(ReadingState::new(rs.content_index + 1, rs.textwidth, 0)));
                    }
                } else if km.has(Action::PageDown, k) {
                    if totlines - rs.row > rows * self.spread || rs.content_index != ncontents - 1 {
                        self.note_page_turn(with_pctg(&rs), k, 1);
                    }
                    if totlines - rs.row > rows * self.spread {
                        self.page_animation = Some(Direction::Forward);
                        rs.row = skip_blank_rows(&ts.text_lines, rs.row + rows * self.spread);
                    } else if rs.content_index != ncontents - 1 {
                        self.page_animation = Some(Direction::Forward);
                        return Ok(ReadOutcome::State(ReadingState::new(rs.content_index + 1, rs.textwidth, 0)));
                    }
                } else if km.has(Action::NextChapter, k) {
                    let ntoc = ntoc();
                    if ntoc + 1 < toc_entries.len() {
                        self.push_jump(with_pctg(&rs), k);
                        let next = &toc_entries[ntoc + 1];
                        if rs.content_index == next.content_index {
                            if let Some(r) = next.section.as_ref().and_then(|s| ts.section_rows.get(s)) {
                                rs.row = *r as i64;
                            }
                        } else {
                            return Ok(ReadOutcome::State(ReadingState {
                                section: next.section.clone(),
                                ..ReadingState::new(next.content_index, rs.textwidth, 0)
                            }));
                        }
                    }
                } else if km.has(Action::PrevChapter, k) {
                    let ntoc = ntoc();
                    if ntoc > 0 {
                        self.push_jump(with_pctg(&rs), k);
                        let prev = &toc_entries[ntoc - 1];
                        if rs.content_index == prev.content_index {
                            rs.row = prev.section.as_ref().and_then(|s| ts.section_rows.get(s)).copied().unwrap_or(0) as i64;
                        } else {
                            return Ok(ReadOutcome::State(ReadingState {
                                section: prev.section.clone(),
                                ..ReadingState::new(prev.content_index, rs.textwidth, 0)
                            }));
                        }
                    }
                } else if km.has(Action::BeginningOfCh, k) {
                    self.push_jump(with_pctg(&rs), k);
                    let ntoc = ntoc();
                    rs.row = toc_entries
                        .get(ntoc)
                        .and_then(|e| e.section.as_ref())
                        .and_then(|s| ts.section_rows.get(s))
                        .copied()
                        .unwrap_or(0) as i64;
                } else if km.has(Action::EndOfCh, k) {
                    self.push_jump(with_pctg(&rs), k);
                    let ntoc = ntoc();
                    let section_row = |i: usize| {
                        toc_entries.get(i).and_then(|e| e.section.as_ref()).and_then(|s| ts.section_rows.get(s)).copied()
                    };
                    rs.row = match (section_row(ntoc + 1), section_row(ntoc)) {
                        (Some(next), _) if next as i64 - rows >= 0 => next as i64 - rows,
                        (Some(_), Some(cur)) => cur as i64,
                        _ => pgend(totlines, rows),
                    };
                } else if km.has(Action::TableOfContents, k) {
                    if toc_entries.is_empty() {
                        k = self.show_win_error(
                            "Table of Contents",
                            "N/A: TableOfContents is unavailable for this book.",
                            km.get(Action::TableOfContents),
                        )?;
                        continue;
                    }
                    let labels: Vec<String> = toc_entries.iter().map(|e| e.label.clone()).collect();
                    let current = ntoc();
                    let title = format!("Table of Contents \u{2014} {}", self.book_title());
                    let choice = self.choice_win(
                        &title,
                        &labels,
                        current,
                        Some(current),
                        km.get(Action::TableOfContents),
                        false,
                    )?;
                    if choice.key.is_some() {
                        k = choice.key;
                        continue;
                    } else if let Some(fllwd) = choice.index {
                        self.push_jump(with_pctg(&rs), None);
                        let entry = &toc_entries[fllwd];
                        if rs.content_index == entry.content_index {
                            rs.row = entry.section.as_ref().and_then(|s| ts.section_rows.get(s)).copied().unwrap_or(0) as i64;
                        } else {
                            return Ok(ReadOutcome::State(ReadingState {
                                section: entry.section.clone(),
                                ..ReadingState::new(entry.content_index, rs.textwidth, 0)
                            }));
                        }
                    }
                } else if km.has(Action::Metadata, k) {
                    k = self.show_win_metadata()?;
                    if k.is_some_and(|k| self.win_keys.contains(&k)) {
                        continue;
                    }
                } else if km.has(Action::Help, k) {
                    k = self.show_win_help()?;
                    if k.is_some_and(|k| self.win_keys.contains(&k)) {
                        continue;
                    }
                } else if (km.has(Action::Enlarge, k) || km.has(Action::Shrink, k)) && self.spread == 1 {
                    let step = WIDTH_STEP * count * if km.has(Action::Enlarge, k) { 1 } else { -1 };
                    let max = (cols - 4).max(MIN_TEXTWIDTH);
                    let textwidth = (rs.textwidth + step).clamp(MIN_TEXTWIDTH, max);
                    if textwidth != rs.textwidth {
                        self.setting.width = Some(textwidth);
                        self.save_layout();
                        return Ok(ReadOutcome::State(ReadingState { textwidth, ..with_pctg(&rs) }));
                    }
                } else if km.has(Action::SetWidth, k) && self.spread == 1 {
                    self.setting.width = if countstring.is_empty() {
                        // without a count, toggle between 80 cols and
                        // following the screen width
                        if self.setting.width.is_none() { Some(80) } else { None }
                    } else {
                        Some(count.max(MIN_TEXTWIDTH))
                    };
                    self.save_layout();
                    let textwidth = self.text_width(cols);
                    return Ok(ReadOutcome::State(ReadingState {
                        rel_pctg: Some(rs.row as f64 / totlines as f64),
                        ..ReadingState::new(rs.content_index, textwidth, rs.row)
                    }));
                } else if km.has(Action::RegexSearch, k) {
                    // a search continuing from another chapter already
                    // recorded where it started
                    let origin = self.search_data.is_none().then(|| with_pctg(&rs));
                    let outcome = self.searching(&mut board, &ts.text_lines, &letters_prefix, rs.clone(), ncontents)?;
                    if let (Some(origin), SearchOutcome::State(_)) = (origin, &outcome) {
                        self.push_jump(origin, None);
                    }
                    match outcome {
                        SearchOutcome::Key(key) => {
                            k = key;
                            continue;
                        }
                        SearchOutcome::State(s) if self.search_data.is_some() => return Ok(ReadOutcome::State(s)),
                        SearchOutcome::State(s) => rs = s,
                    }
                } else if km.has(Action::OpenImage, k) && self.image_viewer().is_some() {
                    let lo = rs.row.max(0) as usize;
                    let hi = (rs.row + rows * self.spread).max(0) as usize;
                    let imgs_in_screen: Vec<usize> = ts.image_maps.range(lo..=hi).map(|(r, _)| *r).collect();
                    if imgs_in_screen.is_empty() {
                        k = None;
                        continue;
                    }
                    let mut image_path: Option<String> = None;
                    if imgs_in_screen.len() == 1 {
                        image_path = Some(ts.image_maps[&imgs_in_screen[0]].clone());
                    } else {
                        let rel: Vec<i64> = imgs_in_screen.iter().map(|r| *r as i64 - rs.row).collect();
                        let mut i: i64 = 0;
                        let mut p: Option<Key> = None;
                        while !km.has(Action::Quit, p) && !km.has(Action::Follow, p) {
                            let r = rel[i as usize];
                            let cx = if r / rows == 0 { x } else { cols - DOUBLE_SPREAD_PADDING_RIGHT - rs.textwidth }
                                + rs.textwidth / 2;
                            let pos = Position::new(cx.max(0) as u16, (r % rows + self.top_pad) as u16);
                            self.draw_overlay(|f, _| f.set_cursor_position(pos))?;
                            p = self.getch()?;
                            if km.has(Action::ScrollDown, p) {
                                i += 1;
                            } else if km.has(Action::ScrollUp, p) {
                                i -= 1;
                            }
                            i = i.rem_euclid(rel.len() as i64);
                        }
                        if km.has(Action::Follow, p) {
                            image_path = Some(ts.image_maps[&imgs_in_screen[i as usize]].clone());
                        }
                    }
                    if let Some(mut image_path) = image_path {
                        let result = (|| -> Result<Option<Key>> {
                            if self.ebook.resolves_image_paths() {
                                let ci = if self.seamless { self.abs_to_rel(&rs).content_index } else { rs.content_index };
                                let content_path = self.ebook.contents()[ci].clone();
                                image_path = resolve_path(&content_path, &image_path);
                            }
                            let (name, bytes) = self.ebook.get_img_bytestr(&image_path)?;
                            self.open_image(&name, &bytes)
                        })();
                        match result {
                            Ok(key) => {
                                k = key;
                                continue;
                            }
                            Err(e) => {
                                self.show_win_error("Error Opening Image", &format!("{e:#}"), &[])?;
                            }
                        }
                    }
                } else if km.has(Action::SwitchColor, k) && ["", "0", "1", "2"].contains(&countstring.as_str()) {
                    self.color_pair = if countstring.is_empty() {
                        self.color_pair % 3 + 1
                    } else {
                        count as u8 + 1
                    };
                    return Ok(ReadOutcome::State(ReadingState::new(rs.content_index, rs.textwidth, rs.row)));
                } else if km.has(Action::AddBookmark, k) {
                    match self.input_prompt(" Add bookmark:")? {
                        PromptResult::Text(name) => {
                            let path = self.ebook.path().to_string();
                            if !self.state.insert_bookmark(&path, &name, &with_pctg(&rs))? {
                                k = self.show_win_error(
                                    "Error: Add Bookmarks",
                                    &format!("Bookmark with name '{name}' already exists."),
                                    &[Key::Char('B')],
                                )?;
                                continue;
                            }
                        }
                        PromptResult::Key(key) => {
                            k = Some(key);
                            continue;
                        }
                        PromptResult::Cancel => {
                            k = None;
                            continue;
                        }
                    }
                } else if km.has(Action::ShowBookmarks, k) {
                    let path = self.ebook.path().to_string();
                    if self.state.get_bookmarks(&path)?.is_empty() {
                        k = self.show_win_error(
                            "Bookmarks",
                            "N/A: Bookmarks are not found in this book.",
                            km.get(Action::ShowBookmarks),
                        )?;
                        continue;
                    }
                    let (retk, idxchoice) = self.show_win_choices_bookmarks()?;
                    if retk.is_some() {
                        k = retk;
                        continue;
                    } else if let Some(idx) = idxchoice {
                        self.push_jump(with_pctg(&rs), None);
                        let bookmark = self.state.get_bookmarks(&path)?.swap_remove(idx).1;
                        if bookmark.content_index == rs.content_index && bookmark.textwidth == rs.textwidth {
                            rs = bookmark;
                        } else {
                            return Ok(ReadOutcome::State(ReadingState {
                                rel_pctg: bookmark.rel_pctg,
                                ..ReadingState::new(bookmark.content_index, rs.textwidth, bookmark.row)
                            }));
                        }
                    }
                } else if km.has(Action::DefineWord, k) && self.ext_dict_app().is_some() {
                    match self.input_prompt(" Define:")? {
                        PromptResult::Text(word) => {
                            let defin = self.define_word(&word)?;
                            if defin.is_some_and(|d| self.win_keys.contains(&d)) {
                                k = defin;
                                continue;
                            }
                        }
                        PromptResult::Key(key) => {
                            k = Some(key);
                            continue;
                        }
                        PromptResult::Cancel => {
                            k = None;
                            continue;
                        }
                    }
                } else if km.has(Action::MarkPosition, k) {
                    match self.getch()?.and_then(Key::digit) {
                        Some(d) => {
                            self.jump_list.insert(d, rs.clone());
                        }
                        None => {
                            k = None;
                            continue;
                        }
                    }
                } else if km.has(Action::JumpToPosition, k) {
                    match self.getch()?.and_then(Key::digit).and_then(|d| self.jump_list.get(&d).cloned()) {
                        Some(marked) => {
                            self.push_jump(with_pctg(&rs), None);
                            let same_width = marked.textwidth == rs.textwidth;
                            return Ok(ReadOutcome::State(ReadingState {
                                textwidth: rs.textwidth,
                                rel_pctg: if same_width { None } else { marked.rel_pctg },
                                section: None,
                                ..marked
                            }));
                        }
                        None => {
                            k = None;
                            continue;
                        }
                    }
                } else if km.has(Action::GoToPercent, k) {
                    match self.input_prompt(" Go to %:")? {
                        PromptResult::Text(input) => {
                            let Some(pct) = parse_percent(&input) else {
                                k = self.show_win_error(
                                    "Go To Percent",
                                    &format!("Not a percentage between 0 and 100: '{input}'"),
                                    &[],
                                )?;
                                continue;
                            };
                            match self.go_to_percent(pct, &rs, &letters_prefix)? {
                                GoToTarget::Row(row) => {
                                    self.push_jump(with_pctg(&rs), None);
                                    rs.row = row;
                                }
                                GoToTarget::State(s) => {
                                    self.push_jump(with_pctg(&rs), None);
                                    return Ok(ReadOutcome::State(s));
                                }
                                GoToTarget::Unavailable => {
                                    k = self.show_win_error(
                                        "Go To Percent",
                                        "N/A: Reading progress is unavailable for this book.",
                                        &[],
                                    )?;
                                    continue;
                                }
                            }
                        }
                        PromptResult::Key(key) => {
                            k = Some(key);
                            continue;
                        }
                        PromptResult::Cancel => {
                            k = None;
                            continue;
                        }
                    }
                } else if km.has(Action::ShowHideProgress, k) {
                    self.show_reading_progress = !self.show_reading_progress;
                    self.top_pad = self.effective_top_pad(self.pad_top, self.pad_bottom, rs.textwidth, cols);
                    rows = self.text_rows();
                } else if km.has(Action::TopPadding, k) || km.has(Action::BottomPadding, k) {
                    let top = km.has(Action::TopPadding, k);
                    let current = if top { self.pad_top } else { self.pad_bottom };
                    let next = PADDING_LEVELS.iter().copied().find(|&l| l > current).unwrap_or(PADDING_LEVELS[0]);
                    let (pad_top, pad_bottom) = if top { (next, self.pad_bottom) } else { (self.pad_top, next) };
                    let top_pad = self.effective_top_pad(pad_top, pad_bottom, rs.textwidth, cols);
                    // skip a step that would leave too little room for text
                    if self.size().0 - top_pad - pad_bottom >= MIN_TEXT_ROWS {
                        (self.pad_top, self.pad_bottom, self.top_pad) = (pad_top, pad_bottom, top_pad);
                        rows = self.text_rows();
                        self.save_layout();
                    }
                } else if km.has(Action::Library, k) {
                    self.try_assign_letters_count(true);
                    self.calculate_reading_progress(&letters_prefix, &rs);
                    self.savestate(&with_pctg(&rs))?;
                    let items = self.state.get_from_history()?;
                    if items.is_empty() {
                        k = self.show_win_error("Library", "N/A: No reading history.", km.get(Action::Library))?;
                        continue;
                    }
                    let (retk, choice) = self.show_win_library()?;
                    if retk.is_some() {
                        k = retk;
                        continue;
                    } else if let Some(idx) = choice {
                        let items = self.state.get_from_history()?;
                        if let Some(item) = items.get(idx) {
                            return Ok(ReadOutcome::OpenFile(item.filepath.clone()));
                        }
                    }
                } else if k == Some(Key::CtrlO) || k == Some(Key::CtrlI) {
                    if let Some(target) = self.step_jump_history(k == Some(Key::CtrlO), with_pctg(&rs)) {
                        let same_width = target.textwidth == rs.textwidth;
                        return Ok(ReadOutcome::State(ReadingState {
                            textwidth: rs.textwidth,
                            rel_pctg: if same_width { None } else { target.rel_pctg },
                            section: None,
                            ..target
                        }));
                    }
                } else if k == Some(Key::Resize) {
                    self.savestate(&with_pctg(&rs))?;
                    let Some(size) = self.wait_for_usable_size()? else {
                        return Ok(ReadOutcome::Quit);
                    };
                    (_, cols) = size;
                    let textwidth = self.text_width(cols);
                    return Ok(ReadOutcome::State(if self.spread == 1 && textwidth != rs.textwidth {
                        ReadingState {
                            rel_pctg: Some(rs.row as f64 / totlines as f64),
                            ..ReadingState::new(rs.content_index, textwidth, rs.row)
                        }
                    } else {
                        ReadingState::new(rs.content_index, rs.textwidth, rs.row)
                    }));
                }
                countstring.clear();
            }

            if let Some(cp) = checkpoint_row.filter(|r| *r >= 0) {
                board.feed_temporary_style(vec![InlineStyle {
                    row: cp as usize,
                    col: 0,
                    n_letters: rs.textwidth.max(0) as usize,
                    attr: Modifier::UNDERLINED,
                }]);
            }

            self.render_page(&board, &rs, &countstring, &letters_prefix)?;

            k = match self.wait_input(&board, &rs, &countstring, &letters_prefix)? {
                Some(Input::Key(key)) => Some(key),
                Some(Input::Mouse(m)) => {
                    let ctrl = m.modifiers.contains(KeyModifiers::CONTROL);
                    match m.kind {
                        MouseEventKind::Down(MouseButton::Left) if (m.column as i64) < cols / 2 => {
                            Some(km.first(Action::PageUp))
                        }
                        MouseEventKind::Down(MouseButton::Left) => Some(km.first(Action::PageDown)),
                        MouseEventKind::Down(MouseButton::Right) => Some(km.first(Action::TableOfContents)),
                        MouseEventKind::ScrollUp if ctrl => Some(km.first(Action::Enlarge)),
                        MouseEventKind::ScrollDown if ctrl => Some(km.first(Action::Shrink)),
                        MouseEventKind::ScrollUp => Some(km.first(Action::ScrollUp)),
                        MouseEventKind::ScrollDown => Some(km.first(Action::ScrollDown)),
                        _ => None,
                    }
                }
                None => None,
            };

            if checkpoint_row.take().is_some() {
                board.feed_temporary_style(Vec::new());
            }
        }
    }

    fn run(&mut self) -> Result<Option<String>> {
        self.show_loader("initializing ebook")?;
        if let Err(e) = self.ebook.initialize() {
            bail!("Badly-structured ebook.\n{e:#}");
        }
        if self.ebook.contents().is_empty() {
            bail!("Badly-structured ebook.\nNo contents found.");
        }
        self.run_counting_letters();

        let path = self.ebook.path().to_string();
        let mut rs = self.state.get_last_reading_state(&path)?;
        let (rows, cols) = self.size();
        if cols < 22 || rows < 12 {
            bail!("Screen was too small (min 22cols x 12rows).");
        }
        // the saved row only holds for the width it was saved at
        let textwidth = self.text_width(cols);
        if textwidth == rs.textwidth {
            rs.rel_pctg = None;
        }
        rs.textwidth = textwidth;
        if rs.content_index >= self.ebook.contents().len() {
            rs = ReadingState::new(0, rs.textwidth, 0);
        }

        loop {
            match self.read(rs)? {
                ReadOutcome::Quit => return Ok(None),
                ReadOutcome::OpenFile(p) => return Ok(Some(p)),
                ReadOutcome::State(s) => {
                    rs = if self.seamless { self.abs_to_rel(&s) } else { s };
                    rs.content_index = rs.content_index.min(self.ebook.contents().len() - 1);
                }
            }
        }
    }
}

/// Read `filepath` until the user quits (`None`) or picks another book from
/// the library (`Some(path)`).
pub fn start_reading(term: &mut DefaultTerminal, filepath: &str) -> Result<Option<String>> {
    let ebook = get_ebook_obj(filepath)?;
    let state = State::new()?;
    let config = Config::load()?;
    let mut reader = Reader::new(term, ebook, config, state);
    let result = reader.run();
    reader.cleanup();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_percentages() {
        assert_eq!(parse_percent("42"), Some(42.0));
        assert_eq!(parse_percent(" 12.5% "), Some(12.5));
        assert_eq!(parse_percent("0"), Some(0.0));
        assert_eq!(parse_percent("100"), Some(100.0));
        assert_eq!(parse_percent("101"), None);
        assert_eq!(parse_percent("-1"), None);
        assert_eq!(parse_percent("abc"), None);
        assert_eq!(parse_percent("NaN"), None);
    }

    #[test]
    fn row_for_letters_targets_top_of_page() {
        // 10 lines of 10 letters each
        let prefix: Vec<usize> = (0..=10).map(|i| i * 10).collect();
        assert_eq!(row_for_letters(&prefix, 0, 9), 0);
        // 50 letters come before line 5
        assert_eq!(row_for_letters(&prefix, 50, 9), 5);
        // mid-line targets round forward to the next line
        assert_eq!(row_for_letters(&prefix, 45, 9), 5);
        // clamped to the last allowed row
        assert_eq!(row_for_letters(&prefix, 100, 9), 9);
        assert_eq!(row_for_letters(&prefix, 100, 7), 7);
    }

    #[test]
    fn jump_history_is_bounded_and_deduped() {
        let mut stack = Vec::new();
        for row in 0..15 {
            push_bounded(&mut stack, ReadingState::new(0, 80, row));
        }
        assert_eq!(stack.len(), JUMP_HISTORY_LEN);
        // oldest entries were dropped
        assert_eq!(stack[0].row, 5);
        push_bounded(&mut stack, ReadingState::new(0, 80, 14));
        assert_eq!(stack.len(), JUMP_HISTORY_LEN);
        assert_eq!(stack.last().unwrap().row, 14);
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate_with_ellipsis("Chapter One", 20), "Chapter One");
        assert_eq!(truncate_with_ellipsis("Chapter One", 11), "Chapter One");
        assert_eq!(truncate_with_ellipsis("Chapter One", 8), "Chapter…");
        assert_eq!(truncate_with_ellipsis("Chapter One", 0), "");
    }

    #[test]
    fn counts_pages_left_in_chapter() {
        // chapters start at rows 0, 100 and 250 of 300; 20-row pages
        let starts = [0, 100, 250];
        assert_eq!(pages_left_in_chapter(starts, 0, 300, 20), 4);
        // a partial last page still counts
        assert_eq!(pages_left_in_chapter(starts, 5, 300, 20), 4);
        assert_eq!(pages_left_in_chapter(starts, 80, 300, 20), 0);
        assert_eq!(pages_left_in_chapter(starts, 100, 300, 20), 7);
        // last chapter runs to the end of the text
        assert_eq!(pages_left_in_chapter(starts, 250, 300, 20), 2);
        // no TOC: the whole content file is the chapter
        assert_eq!(pages_left_in_chapter([], 0, 300, 20), 14);
    }

    #[test]
    fn skips_blank_rows_at_page_top() {
        let lines: Vec<String> = ["a", "", "  ", "b", ""].iter().map(|s| s.to_string()).collect();
        assert_eq!(skip_blank_rows(&lines, 0), 0);
        assert_eq!(skip_blank_rows(&lines, 1), 3);
        // only blank lines left: stay put
        assert_eq!(skip_blank_rows(&lines, 4), 4);
    }

    #[test]
    fn splits_camel_case() {
        assert_eq!(split_camel_case("TableOfContents"), "Table Of Contents");
        assert_eq!(split_camel_case("HTMLView"), "HTML View");
        assert_eq!(split_camel_case("Quit"), "Quit");
    }

    #[test]
    fn finds_current_toc_entry() {
        let toc = vec![
            TocEntry { label: "a".into(), content_index: 0, section: None },
            TocEntry { label: "b".into(), content_index: 1, section: None },
            TocEntry { label: "c".into(), content_index: 1, section: Some("s".into()) },
        ];
        let rows: HashMap<String, usize> = [("s".to_string(), 50)].into();
        assert_eq!(find_current_content_index(&toc, &rows, 0, 10), 0);
        assert_eq!(find_current_content_index(&toc, &rows, 1, 10), 1);
        assert_eq!(find_current_content_index(&toc, &rows, 1, 60), 2);
    }
}
