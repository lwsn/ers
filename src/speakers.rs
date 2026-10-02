//! Text-to-speech engines.

use std::io::Write;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

use crate::util::which;

pub trait Speaker {
    fn speak(&mut self, text: &str) -> Result<()>;
    fn is_done(&mut self) -> bool;
    fn stop(&mut self);
    fn cleanup(&mut self) {}
}

fn is_exited(child: &mut Option<Child>) -> bool {
    child.as_mut().is_none_or(|c| !matches!(c.try_wait(), Ok(None)))
}

fn terminate(child: &mut Option<Child>) {
    if let Some(c) = child.as_mut() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

struct Mimic {
    args: Vec<String>,
    process: Option<Child>,
}

impl Speaker for Mimic {
    fn speak(&mut self, text: &str) -> Result<()> {
        let mut child = Command::new("mimic")
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        child.stdin.take().context("no stdin")?.write_all(text.as_bytes())?;
        self.process = Some(child);
        Ok(())
    }
    fn is_done(&mut self) -> bool {
        is_exited(&mut self.process)
    }
    fn stop(&mut self) {
        terminate(&mut self.process);
    }
}

struct Pico {
    args: Vec<String>,
    process: Option<Child>,
    tmp: Option<tempfile::TempPath>,
}

impl Speaker for Pico {
    fn speak(&mut self, text: &str) -> Result<()> {
        let tmp = tempfile::Builder::new().suffix(".wav").tempfile()?.into_temp_path();
        let out = Command::new("pico2wave")
            .args(&self.args)
            .arg("-w")
            .arg(&*tmp)
            .arg(text)
            .output()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() && !stderr.contains("invalid pointer") {
            anyhow::bail!("pico2wave failed: {stderr}");
        }
        self.process = Some(
            Command::new("play")
                .arg(&*tmp)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        self.tmp = Some(tmp);
        Ok(())
    }
    fn is_done(&mut self) -> bool {
        is_exited(&mut self.process)
    }
    fn stop(&mut self) {
        terminate(&mut self.process);
    }
    fn cleanup(&mut self) {
        self.tmp = None;
    }
}

struct GttsMpv {
    args: Vec<String>,
    gtts: Option<Child>,
    mpv: Option<Child>,
}

impl Speaker for GttsMpv {
    fn speak(&mut self, text: &str) -> Result<()> {
        let mut gtts = Command::new("gtts-cli")
            .arg("-")
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let gtts_out = gtts.stdout.take().context("no stdout")?;
        let mpv = Command::new("mpv")
            .arg("-")
            .stdin(gtts_out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        gtts.stdin.take().context("no stdin")?.write_all(text.as_bytes())?;
        self.gtts = Some(gtts);
        self.mpv = Some(mpv);
        Ok(())
    }
    fn is_done(&mut self) -> bool {
        is_exited(&mut self.mpv)
    }
    fn stop(&mut self) {
        terminate(&mut self.gtts);
        terminate(&mut self.mpv);
    }
}

/// First available TTS engine, trying `preferred` (by command name) first.
pub fn construct_speaker(preferred: Option<&str>, args: &[String]) -> Option<Box<dyn Speaker>> {
    let args = args.to_vec();
    let mut engines: Vec<(&str, bool)> = vec![
        ("mimic", which("mimic").is_some()),
        ("pico2wave", which("pico2wave").is_some() && which("play").is_some()),
        ("gtts-mpv", which("gtts-cli").is_some() && which("mpv").is_some()),
    ];
    if let Some(p) = preferred {
        engines.sort_by_key(|(cmd, _)| *cmd != p);
    }
    let (cmd, _) = engines.into_iter().find(|(_, available)| *available)?;
    Some(match cmd {
        "mimic" => Box::new(Mimic { args, process: None }),
        "pico2wave" => Box::new(Pico { args, process: None, tmp: None }),
        _ => Box::new(GttsMpv { args, gtts: None, mpv: None }),
    })
}
