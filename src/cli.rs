use std::{net::SocketAddr, path::PathBuf};

use anyhow::{bail, ensure, Context, Ok as AnyOk};
use clap::Parser;
use easy_sgr::{Color::*, Style::*};
use watchexec::Watchexec;

use crate::{Api, MutexExt};

/// host a markdown file server
#[derive(Parser, Debug)]
#[command(name = "mdflc")]
pub struct Args {
    /// The base path to read
    #[arg(default_value = "./")]
    pub base: PathBuf,
    /// The markdown file to treat as index
    #[arg(short, long, default_value = "./index.md")]
    pub index: PathBuf,
    /// The address to run on
    #[arg(short, long, default_value = "0.0.0.0:6464")]
    pub addr: SocketAddr,
}

/// Reads console
///
/// Finishes once quit command recieved.
pub fn read_console(api: &Api, wx: &Watchexec) -> anyhow::Result<()> {
    // TODO: create better interactive terminal
    let stdin = std::io::stdin();
    let mut buf = String::new();

    loop {
        buf.clear();
        stdin.read_line(&mut buf)?;
        let s = buf.trim();
        if !s.is_empty() && handle_ci(api, wx, s) {
            break;
        }
    }
    Ok(())
}

/// returns true if program should stop
#[must_use]
pub fn handle_ci(api: &Api, wx: &Watchexec, s: &str) -> bool {
    match s {
        "help" | "h" => println!(
            "\
            enter {BlueFg}[s]et [p]ath {{PATH}}{Reset} to set a new path to serve (resets index)\n\
            enter {BlueFg}[s]et [i]ndex {{PATH}}{Reset} to set a new path to serve (resets index)\n\
            enter {BlueFg}[h]elp{Reset} to show help (this text)\n\
            enter {BlueFg}[p]ath{Reset} to show path\n\
            enter {BlueFg}[i]ndex{Reset} to show index\n\
            enter {BlueFg}[o]pen{Reset} to open client in browser\n\
            enter {BlueFg}[u]rl{Reset} to show server url\n\
            enter {BlueFg}[c]lear{Reset} clear screen\n\
            enter {BlueFg}[q]uit{Reset} to quit\
            "
        ),
        "open" | "o" => {
            if webbrowser::open(&api.url).is_ok() {
                println!("{GreenFg}Opening browser...{Reset}");
            } else {
                println!("{YellowFg}Unable to open browser{Reset}");
            }
        }
        "path" | "p" => println!("{BlueFg}{}{Reset}", api.base.unlock().display()),
        "index" | "i" => println!("{BlueFg}{}{Reset}", api.index.unlock()),
        "clear" | "c" => scroll(),
        "url" | "u" => println!("{BlueFg}{}{Reset}", api.addr),
        "quit" | "q" => return true,
        s => match set_path(s, api, wx) {
            Ok(true) => (),
            Ok(false) => println!("{YellowFg}Unknown input: \"{s}\"{Reset}"),
            Err(e) => println!("{YellowFg}Incorrect Input: \"{e}\"{Reset}"),
        },
    }
    false
}

fn set_path(s: &str, api: &Api, wx: &Watchexec) -> anyhow::Result<bool> {
    enum Kind {
        Path,
        Index,
    }

    let (kind, path) = if let Some(s) = s.strip_prefix("set").map(str::trim) {
        if let Some(s) = s.strip_prefix("path") {
            (Kind::Path, s)
        } else if let Some(s) = s.strip_prefix("index") {
            (Kind::Index, s)
        } else {
            bail!("expect 'path' or 'index' after set");
        }
    } else {
        match s.get(..2) {
            Some("sp") => (Kind::Path, s),
            Some("si") => (Kind::Index, s),
            _ => return Ok(false),
        }
    };
    let path = path.trim();
    ensure!(!path.is_empty(), "inputted path was empty");
    let path = PathBuf::from(path).canonicalize()?;

    match kind {
        Kind::Path => {
            if !path.try_exists().context("unknown path")? {
                println!("{RedFg}The given path does not exist{Reset}");
            }
            wx.config.pathset([path.clone()]);
            *api.base.unlock() = path;
        }
        Kind::Index => {
            let path = path
                .strip_prefix(api.base.unlock().as_path())
                .context("index must be a subpath of base")?
                .to_str()
                .context("only utf8 paths allowed")?
                .to_owned();
            *api.index.unlock() = path;
        }
    }

    AnyOk(true)
}

pub(crate) fn scroll() {
    print!("\x1B[2J\x1B[1;1H");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}
