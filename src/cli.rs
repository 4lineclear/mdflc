use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fmt::Debug,
    net::SocketAddr,
    path::PathBuf,
};

use anyhow::{bail, ensure, Context, Ok as AnyOk};
use clap::Parser;
use easy_sgr::{Color::*, Style::*};
use rustyline::{
    completion::Completer,
    error::ReadlineError,
    highlight::Highlighter,
    hint::Hinter,
    history::{History, MemHistory},
    line_buffer::LineBuffer,
    validate::{ValidationContext, ValidationResult, Validator},
    Changeset, CompletionType, Config, Editor, Helper,
};
use watchexec::Watchexec;

use crate::{Api, MutexExt};

/// host a markdown file server
#[derive(Parser, Debug)]
#[command(name = "mdflc")]
pub struct Args {
    /// The base path to read
    #[arg(default_value = "./")]
    pub base: PathBuf,
    /// The markdown file to treat as index, relative to base
    #[arg(short, long, default_value = "index.md")]
    pub index: PathBuf,
    /// The address to run on
    #[arg(short, long, default_value = "0.0.0.0:6464")]
    pub addr: SocketAddr,
}

/// Reads console
///
/// Finishes once quit command recieved.
pub fn read_console(api: &Api, wx: &Watchexec) -> anyhow::Result<()> {
    use rustyline::error::ReadlineError::*;
    let config = Config::default();
    let mut rl: Editor<(), MemHistory> =
        Editor::with_history(config, MemHistory::with_config(config))?;

    loop {
        match rl.readline(">> ") {
            Ok(s) => {
                rl.history_mut().add(&s)?;
                let s = s.trim();
                if !s.is_empty() && handle_ci(api, wx, s) {
                    break;
                }
            }
            Err(e) => match e {
                Eof | Interrupted => break,
                e => eprintln!("repl error: \"{e}\""),
            },
        }
    }

    Ok(())
}

pub struct Repl {
    pub commands: Vec<Command>,
    // NOTE: maybe switch to vec
    pub paths: HashMap<String, usize>,
}

impl Helper for Repl {}

impl Completer for Repl {
    type Candidate = String;

    fn complete(
        &self, // FIXME should be `&mut self`
        line: &str,
        pos: usize,
        ctx: &rustyline::Context<'_>,
    ) -> Result<(usize, Vec<Self::Candidate>), ReadlineError> {
        let _ = (line, pos, ctx);
        Ok((0, Vec::with_capacity(0)))
    }

    fn update(&self, line: &mut LineBuffer, start: usize, elected: &str, cl: &mut Changeset) {
        let end = line.pos();
        line.replace(start..end, elected, cl);
    }
}

impl Hinter for Repl {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, ctx: &rustyline::Context<'_>) -> Option<Self::Hint> {
        let _ = (line, pos, ctx);
        None
    }
}

impl Highlighter for Repl {
    fn highlight<'l>(&self, line: &'l str, pos: usize) -> std::borrow::Cow<'l, str> {
        let _ = pos;
        std::borrow::Cow::Borrowed(line)
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> std::borrow::Cow<'b, str> {
        let _ = default;
        std::borrow::Cow::Borrowed(prompt)
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> std::borrow::Cow<'h, str> {
        std::borrow::Cow::Borrowed(hint)
    }

    fn highlight_candidate<'c>(
        &self,
        candidate: &'c str, // FIXME should be Completer::Candidate
        completion: CompletionType,
    ) -> std::borrow::Cow<'c, str> {
        let _ = completion;
        std::borrow::Cow::Borrowed(candidate)
    }

    fn highlight_char(&self, line: &str, pos: usize, forced: bool) -> bool {
        let _ = (line, pos, forced);
        false
    }
}

impl Validator for Repl {
    fn validate(&self, ctx: &mut ValidationContext) -> Result<ValidationResult, ReadlineError> {
        let _ = ctx;
        Ok(ValidationResult::Valid(None))
    }
}

/// The path of strings that leads to a command
#[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum CommandPath {
    /// A command that comes from a single string
    Unit {
        /// the base path
        long: SmartStr,
        /// an optional short path
        short: Option<SmartStr>,
    },
    /// A single string plus a "path"
    Multi {
        /// The first unit
        start: SmartStr,
        /// The descendent paths
        paths: Vec<CommandPath>,
    },
}

impl CommandPath {
    /// parse yeah
    #[must_use]
    #[allow(dead_code)]
    pub fn parse(&self, _s: &str) -> Option<Match> {
        // match self {
        //     CommandPath::Unit { long, short } => todo!(),
        //     CommandPath::Multi { start, paths } => todo!(),
        // }
        todo!()
    }
}

#[allow(dead_code)]
pub enum Match<'a> {
    /// String matched, `.0` is the leftover
    Match(&'a str),
    /// The start matched, `.0` is the leftover
    Incomplete(&'a str, &'a str),
    /// String is not
    None,
}

pub type SmartStr = Cow<'static, str>;

#[allow(dead_code)]
#[derive(Debug)]
pub struct Command {
    name: String,
    desc: String,
    paths: HashSet<String>,
    run: Box<dyn Runnable>,
}

impl Command {
    #[must_use]
    pub fn new(name: String, desc: String, run: impl Into<Box<dyn Runnable>>) -> Self {
        Self {
            name,
            desc,
            paths: HashSet::new(),
            run: run.into(),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn desc(&self) -> &str {
        &self.desc
    }

    #[must_use]
    pub const fn paths(&self) -> &HashSet<String> {
        &self.paths
    }
}

pub trait Runnable: Debug {
    fn run(&self, s: &str, api: &Api, wx: &Watchexec) -> anyhow::Result<bool>;
}

impl<F> Runnable for F
where
    F: Fn(&str, &Api, &Watchexec) -> anyhow::Result<bool> + Debug,
{
    fn run(&self, s: &str, api: &Api, wx: &Watchexec) -> anyhow::Result<bool> {
        self(s, api, wx)
    }
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
        "url" | "u" => println!("{BlueFg}{}{Reset}", api.url),
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
