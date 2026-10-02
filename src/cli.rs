//! Command line interface.

use std::io::Write;

use anyhow::{Result, bail};
use clap::{ArgAction, Parser};

use crate::ebooks::get_ebook_obj;
use crate::models::LibraryItem;
use crate::parser::parse_html_lines;
use crate::state::State;
use crate::util::{is_url, matching_chars, truncate};

#[derive(Parser)]
#[command(
    name = "ers",
    about = "Read ebook in terminal",
    version,
    disable_version_flag = true,
    after_help = "examples:\n  \
        ers /path/to/ebook    read /path/to/ebook file\n  \
        ers 3                 read #3 file from reading history\n  \
        ers count monte       read file matching 'count monte'\n                        \
        from reading history"
)]
struct Args {
    /// print reading history
    #[arg(short = 'r', long = "history")]
    history: bool,

    /// dump the content of ebook
    #[arg(short, long)]
    dump: bool,

    /// print version and exit
    #[arg(short = 'v', long, action = ArgAction::Version)]
    version: Option<bool>,

    /// ebook path, history number, pattern or URL
    #[arg(value_name = "PATH | # | PATTERN | URL")]
    ebook: Vec<String>,
}

/// Remove files that no longer exist from the library.
fn cleanup_library(state: &State) -> Result<()> {
    for item in state.get_from_history()? {
        if !std::path::Path::new(&item.filepath).is_file() && !is_url(&item.filepath) {
            state.delete_from_library(&item.filepath)?;
        }
    }
    Ok(())
}

fn get_matching_library_item(state: &State, pattern: &str, threshold: f64) -> Result<Option<LibraryItem>> {
    let pattern = pattern.to_lowercase();
    let plen = pattern.chars().count().max(1) as f64;
    let best = state
        .get_from_history()?
        .into_iter()
        .map(|item| {
            let tomatch = format!(
                "{} - {}",
                item.title.as_deref().unwrap_or("None"),
                item.author.as_deref().unwrap_or("None")
            )
            .to_lowercase();
            let score = matching_chars(&tomatch, &pattern) as f64 / plen;
            (item, score)
        })
        // stable: keep the most recent item on ties
        .fold(None::<(LibraryItem, f64)>, |best, cur| match best {
            Some(b) if b.1 >= cur.1 => Some(b),
            _ => Some(cur),
        });
    Ok(best.filter(|(_, score)| *score >= threshold).map(|(item, _)| item))
}

fn print_reading_history(state: &State) -> Result<()> {
    let items = state.get_from_history()?;
    if items.is_empty() {
        println!("No Reading History.");
        return Ok(());
    }
    let termc = ratatui::crossterm::terminal::size().map(|s| s.0 as usize).unwrap_or(80);
    println!("Reading History:");
    let dig = (items.len() + 1).to_string().len();
    let tcols = termc.saturating_sub(dig + 2).max(4);
    for (n, item) in items.iter().enumerate() {
        println!("{:>dig$} {}", n + 1, truncate(&item.to_string(), "...", tcols, tcols - 3));
    }
    Ok(())
}

pub enum Command {
    Read(String),
    Dump(String),
    Exit,
}

pub fn parse() -> Result<Command> {
    let args = Args::parse();
    let state = State::new()?;
    cleanup_library(&state)?;
    let wrap = |path: String| if args.dump { Command::Dump(path) } else { Command::Read(path) };

    if args.history {
        print_reading_history(&state)?;
        return Ok(Command::Exit);
    }

    match args.ebook.as_slice() {
        [] => match state.get_last_read()? {
            Some(path) => return Ok(wrap(path)),
            None => bail!("Found no last read ebook file."),
        },
        [arg] => {
            if let Ok(nth) = arg.parse::<i64>() {
                let items = state.get_from_history()?;
                return match usize::try_from(nth - 1).ok().and_then(|i| items.get(i)) {
                    Some(item) => Ok(wrap(item.filepath.clone())),
                    None => {
                        println!("ERROR: #{nth} file not found.");
                        print_reading_history(&state)?;
                        std::process::exit(1);
                    }
                };
            } else if is_url(arg) || std::path::Path::new(arg).is_file() {
                return Ok(wrap(arg.clone()));
            }
        }
        _ => {}
    }

    match get_matching_library_item(&state, &args.ebook.join(" "), 0.5)? {
        Some(item) => Ok(wrap(item.filepath)),
        None => bail!("Found no matching ebook from history."),
    }
}

pub fn dump_ebook_content(filepath: &str) -> Result<()> {
    let mut ebook = get_ebook_obj(filepath)?;
    if let Err(e) = ebook.initialize() {
        bail!("Badly-structured ebook.\n{e:#}");
    }
    let mut out = std::io::stdout().lock();
    for content in ebook.contents().to_vec() {
        for line in parse_html_lines(&ebook.get_raw_text(&content)?) {
            if writeln!(out, "{line}\n").is_err() {
                // eg. broken pipe when piped into `head`
                break;
            }
        }
    }
    ebook.cleanup();
    Ok(())
}
