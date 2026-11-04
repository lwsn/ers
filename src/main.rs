mod board;
mod cli;
mod config;
mod ebooks;
mod models;
mod parser;
mod reader;
mod state;
mod util;

use std::backtrace::Backtrace;
use std::io::Write;
use std::process::ExitCode;

use anyhow::Result;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;

/// Append `msg` to `crash.log` in the app directory, best effort.
fn log_crash(msg: &str) {
    let Some(dir) = state::app_prefix() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("crash.log")) {
        let _ = writeln!(f, "[{}] {msg}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}

/// Log panics before the default hook (and ratatui's terminal restore,
/// which chains onto this one) runs.
fn install_panic_logger() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_crash(&format!("PANIC: {info}\n{}", Backtrace::force_capture()));
        default(info);
    }));
}

fn run_tui(mut filepath: String) -> Result<()> {
    let mouse = config::Config::load()?.setting.mouse_support;
    install_panic_logger();
    let mut terminal = ratatui::init();
    reader::enable_key_disambiguation();
    if mouse {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
    }
    let result = loop {
        match reader::start_reading(&mut terminal, &filepath) {
            Ok(Some(next)) => filepath = next,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    if mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    reader::disable_key_disambiguation();
    ratatui::restore();
    if let Err(e) = &result {
        log_crash(&format!("ERROR: {e:#}"));
    }
    result
}

fn main() -> ExitCode {
    let result = cli::parse().and_then(|cmd| match cmd {
        cli::Command::Read(path) => run_tui(path),
        cli::Command::Dump(path) => cli::dump_ebook_content(&path),
        cli::Command::Exit => Ok(()),
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ERROR: {e:#}");
            ExitCode::FAILURE
        }
    }
}
