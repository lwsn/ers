//! Settings and keymaps.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::app_prefix;

pub const DOUBLE_SPREAD_PADDING_LEFT: i64 = 10;
pub const DOUBLE_SPREAD_PADDING_MIDDLE: i64 = 7;
pub const DOUBLE_SPREAD_PADDING_RIGHT: i64 = 10;

/// Image viewers, sorted by most widely used.
pub const VIEWER_PRESET_LIST: [&str; 8] =
    ["feh", "imv", "gio", "gnome-open", "gvfs-open", "xdg-open", "kde-open", "firefox"];

pub const DICT_PRESET_LIST: [&str; 3] = ["wkdict", "sdcv", "dict"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Settings {
    pub default_viewer: String,
    pub dictionary_client: String,
    pub show_progress_indicator: bool,
    pub page_scroll_animation: bool,
    /// Delay in milliseconds between frames of the page-turn animation
    pub page_scroll_animation_rate: u64,
    /// Blank rows above the text
    pub top_padding: i64,
    /// Blank rows below the text
    pub bottom_padding: i64,
    pub mouse_support: bool,
    pub start_with_double_spread: bool,
    /// -1 is the default terminal fg/bg color
    #[serde(rename = "DefaultColorFG")]
    pub default_color_fg: i32,
    #[serde(rename = "DefaultColorBG")]
    pub default_color_bg: i32,
    #[serde(rename = "DarkColorFG")]
    pub dark_color_fg: i32,
    #[serde(rename = "DarkColorBG")]
    pub dark_color_bg: i32,
    #[serde(rename = "LightColorFG")]
    pub light_color_fg: i32,
    #[serde(rename = "LightColorBG")]
    pub light_color_bg: i32,
    pub seamless_between_chapters: bool,
    #[serde(rename = "PreferredTTSEngine")]
    pub preferred_tts_engine: Option<String>,
    #[serde(rename = "TTSEngineArgs")]
    pub tts_engine_args: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_viewer: "auto".into(),
            dictionary_client: "auto".into(),
            show_progress_indicator: true,
            page_scroll_animation: true,
            page_scroll_animation_rate: 0,
            top_padding: 0,
            bottom_padding: 0,
            mouse_support: false,
            start_with_double_spread: false,
            default_color_fg: -1,
            default_color_bg: -1,
            dark_color_fg: 252,
            dark_color_bg: 235,
            light_color_fg: 238,
            light_color_bg: 253,
            seamless_between_chapters: false,
            preferred_tts_engine: None,
            tts_engine_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Tab,
    Enter,
    Esc,
    Backspace,
    CtrlC,
    Resize,
}

impl Key {
    pub fn digit(self) -> Option<char> {
        match self {
            Key::Char(c) if c.is_ascii_digit() => Some(c),
            _ => None,
        }
    }
}

macro_rules! actions {
    ($($name:ident = $default:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Action { $($name),* }

        impl Action {
            /// All actions with their default key, in help-menu order.
            pub const ALL: &'static [(Action, &'static str, &'static str)] =
                &[$((Action::$name, stringify!($name), $default)),*];
        }
    };
}

actions! {
    ScrollUp = "k",
    ScrollDown = "j",
    PageUp = "h",
    PageDown = "l",
    NextChapter = "L",
    PrevChapter = "H",
    BeginningOfCh = "g",
    EndOfCh = "G",
    Shrink = "-",
    Enlarge = "+",
    SetWidth = "=",
    Metadata = "M",
    DefineWord = "d",
    TableOfContents = "t",
    Follow = "f",
    OpenImage = "o",
    RegexSearch = "/",
    ShowHideProgress = "s",
    MarkPosition = "m",
    JumpToPosition = "`",
    GoToPercent = "%",
    TopPadding = ",",
    BottomPadding = ".",
    AddBookmark = "b",
    ShowBookmarks = "B",
    Quit = "q",
    Help = "?",
    SwitchColor = "c",
    TTSToggle = "!",
    DoubleSpreadToggle = "D",
    Library = "R",
}

fn builtin_keys(action: Action) -> &'static [Key] {
    use Key::*;
    match action {
        Action::ScrollUp => &[Up],
        Action::ScrollDown => &[Down],
        Action::PageUp => &[Key::PageUp, Left, Backspace],
        Action::PageDown => &[Key::PageDown, Char(' '), Right],
        Action::BeginningOfCh => &[Home],
        Action::EndOfCh => &[End],
        Action::TableOfContents => &[Tab],
        Action::Follow => &[Enter],
        Action::Quit => &[CtrlC, Esc],
        _ => &[],
    }
}

#[derive(Debug, Clone)]
pub struct Keymap {
    keys: HashMap<Action, Vec<Key>>,
}

impl Keymap {
    pub fn has(&self, action: Action, key: Option<Key>) -> bool {
        key.is_some_and(|k| self.keys[&action].contains(&k))
    }

    pub fn get(&self, action: Action) -> &[Key] {
        &self.keys[&action]
    }

    pub fn first(&self, action: Action) -> Key {
        self.keys[&action][0]
    }
}

pub struct Config {
    pub setting: Settings,
    pub keymap: Keymap,
    /// (action name, user key) in help-menu order, to build the help text
    pub keymap_user: Vec<(&'static str, String)>,
}

#[derive(Serialize)]
struct ConfigFile<'a> {
    #[serde(rename = "Setting")]
    setting: &'a Settings,
    #[serde(rename = "Keymap")]
    keymap: serde_json::Map<String, serde_json::Value>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = app_prefix().map(|p| p.join("configuration.json"));
        let mut setting = Settings::default();
        let mut keymap_user: Vec<(&'static str, String)> =
            Action::ALL.iter().map(|(_, name, def)| (*name, def.to_string())).collect();

        match &path {
            Some(path) if path.is_file() => {
                let text = std::fs::read_to_string(path)?;
                let json: serde_json::Value = serde_json::from_str(&text)
                    .with_context(|| format!("invalid config file {}", path.display()))?;
                if let Some(s) = json.get("Setting") {
                    setting = serde_json::from_value(s.clone())
                        .with_context(|| format!("invalid \"Setting\" in {}", path.display()))?;
                }
                if let Some(serde_json::Value::Object(km)) = json.get("Keymap") {
                    for (name, key) in keymap_user.iter_mut() {
                        if let Some(serde_json::Value::String(v)) = km.get(*name)
                            && !v.is_empty() {
                                *key = v.clone();
                            }
                    }
                }
            }
            Some(path) => {
                let file = ConfigFile {
                    setting: &setting,
                    keymap: keymap_user
                        .iter()
                        .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.clone())))
                        .collect(),
                };
                let _ = std::fs::write(path, serde_json::to_string_pretty(&file)?);
            }
            None => {}
        }

        if cfg!(windows) {
            setting.page_scroll_animation = false;
        }

        let mut keys = HashMap::new();
        for ((action, _, _), (_, user_key)) in Action::ALL.iter().zip(&keymap_user) {
            let mut list: Vec<Key> = user_key.chars().next().map(Key::Char).into_iter().collect();
            for k in builtin_keys(*action) {
                if !list.contains(k) {
                    list.push(*k);
                }
            }
            keys.insert(*action, list);
        }

        Ok(Self { setting, keymap: Keymap { keys }, keymap_user })
    }
}
