mod board;
mod cli;
mod config;
mod ebooks;
mod models;
mod parser;
mod reader;
mod speakers;
mod state;
mod util;

use std::process::ExitCode;

use anyhow::Result;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;

fn run_tui(mut filepath: String) -> Result<()> {
    let mouse = config::Config::load()?.setting.mouse_support;
    let mut terminal = ratatui::init();
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
    ratatui::restore();
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
