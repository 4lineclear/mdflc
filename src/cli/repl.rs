use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Debug,
    io::{self, stdin, stdout, Bytes, Read},
    slice::Iter,
};

use anyhow::Ok as AnyOk;
use crossterm::{cursor::MoveToColumn, execute, style::Print};
use watchexec::Watchexec;

use crate::Api;

#[derive(Debug, Default)]
pub struct Repl {
    pub commands: Vec<Command>,
    pub paths: HashMap<String, usize>,
    pub state: State,
    mem: Vec<String>,
}

impl Repl {
    /// Command builder
    #[must_use]
    pub fn with(mut self, cmd: impl Into<Command>) -> Self {
        self.commands.push(cmd.into());
        self
    }

    pub fn run(self, api: &Api, wx: &Watchexec) -> anyhow::Result<()> {
        use std::io::Write;
        // for ch in StreamLines::new(io::stdin().lock().bytes()) {
        //     let Some(ch) = ch? else {
        //         continue;
        //     };
        //     println!("{ch}");
        // }

        let stdout = &mut stdout().lock();
        crossterm::terminal::enable_raw_mode()?;
        for byte in stdin().lock().bytes() {
            let byte = byte?;
            execute!(stdout, Print(format!("{byte}\n")), MoveToColumn(0))?;
        }

        // let stdin = std::io::stdin();
        // let mut buf = String::new();
        // loop {
        //     buf.clear();
        //     stdin.read_line(&mut buf)?;
        //     let s = buf.trim();
        //
        //     let Some(&i) = self.paths.get(s) else {
        //         continue;
        //     };
        //
        //     if self.commands[i].run.run(api, wx)? {
        //         break;
        //     }
        // }

        AnyOk(())
    }
}

struct StreamLines<R> {
    bytes: Bytes<R>,
    buf: ByteBuf,
}

impl<R> StreamLines<R> {
    fn new(bytes: Bytes<R>) -> Self {
        Self {
            bytes,
            buf: ByteBuf::default(),
        }
    }
}

impl<R: Read> Iterator for StreamLines<R> {
    type Item = io::Result<Option<char>>;

    fn next(&mut self) -> Option<Self::Item> {
        let err = || {
            Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid utf8 entered",
            )))
        };
        let map_ch = |ch| match ch {
            Some(ch) => Some(Ok(Some(ch))),
            None => err(),
        };

        let Ok(b) = self.bytes.next()? else {
            if self.buf.is_empty() {
                return None;
            }
            let ch = self.buf.to_char();
            self.buf = ByteBuf::E;
            return map_ch(ch);
        };

        if !self.buf.is_empty() && is_char_boundary(b) {
            let ch = self.buf.to_char();
            self.buf = ByteBuf::A([b]);
            return map_ch(ch);
        }

        let Some(buf) = self.buf.add(b) else {
            return err();
        };

        self.buf = buf;
        Some(Ok(None))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub enum ByteBuf {
    A([u8; 1]),
    B([u8; 2]),
    C([u8; 3]),
    D([u8; 4]),
    #[default]
    E,
}

impl ByteBuf {
    #[must_use]
    pub fn add(self, n: u8) -> Option<Self> {
        Some(match self {
            ByteBuf::E => ByteBuf::A([n]),
            ByteBuf::A([b1]) => ByteBuf::B([b1, n]),
            ByteBuf::B([b1, b2]) => ByteBuf::C([b1, b2, n]),
            ByteBuf::C([b1, b2, b3]) => ByteBuf::D([b1, b2, b3, n]),
            ByteBuf::D(_) => return None,
        })
    }
    #[must_use]
    pub fn to_char(self) -> Option<char> {
        let map = |b: Iter<_>| b.fold(0, |b, n| (b << 8) + (*n as u32));
        let i = match self {
            ByteBuf::A(b) => map(b.iter()),
            ByteBuf::B(b) => map(b.iter()),
            ByteBuf::C(b) => map(b.iter()),
            ByteBuf::D(b) => map(b.iter()),
            ByteBuf::E => return None,
        };
        char::from_u32(i)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::E)
    }
}

#[inline]
#[must_use]
pub const fn is_char_boundary(b: u8) -> bool {
    // This is bit magic equivalent to: b < 128 || b >= 192
    (b as i8) >= -0x40
}

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
    fn run(&self, api: &Api, wx: &Watchexec) -> anyhow::Result<bool>;
}

impl<F> Runnable for F
where
    F: Fn(&Api, &Watchexec) -> anyhow::Result<bool> + Debug,
{
    fn run(&self, api: &Api, wx: &Watchexec) -> anyhow::Result<bool> {
        self(api, wx)
    }
}

#[derive(Debug, Default)]
pub enum State {
    #[default]
    Normal,
    Scrolling,
}

#[derive(Debug, Default)]
pub struct Memory {
    mem: VecDeque<String>,
    max: usize,
}

impl Memory {
    pub fn store(&mut self, line: String) {
        self.mem.push_back(line);
        while self.mem.len() >= self.max {
            self.mem.pop_front();
        }
    }
}
