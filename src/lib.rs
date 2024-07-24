//! simple md server
#![forbid(unsafe_code)]
#![deny(
    // missing_docs,
    future_incompatible,
    rustdoc::all,
    clippy::all,
    clippy::pedantic,
    clippy::nursery
)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::enum_glob_use)]
#![cfg(unix)]

use std::{
    collections::HashSet,
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use anyhow::{bail, ensure, Context, Ok as AnyOk};
use axum::{
    extract::{Path as AxumPath, State, WebSocketUpgrade},
    http::{header::CONTENT_TYPE, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use clap::Parser;
use dashmap::DashMap;
use easy_sgr::{Color::*, Style::*};
use pulldown_cmark::{html::write_html_fmt, Options};
use tokio::{
    net::TcpListener,
    sync::{oneshot, Notify},
};
use tokio::{signal, task::JoinHandle};
use walkdir::{DirEntry, WalkDir};
use watchexec::{action::ActionHandler, error::CriticalError, Config, Watchexec};

// TODO: Create own markdown parser
// TODO: Add ability to add/remove/list paths
// TODO: add glossery, etc.
// TODO: create utility for making ext traits
pub async fn run() -> anyhow::Result<()> {
    let args = Args::parse();

    ensure!(
        args.base.try_exists().unwrap_or(false),
        "The given path \"{}\" does not exist",
        args.base.display()
    );

    let port = args.addr.port();
    let addr = args.addr;
    let tcp_listener = TcpListener::bind(addr).await?;

    let api = Api::new(addr, &args.index, &args.base)?;

    scroll();
    println!(
        "{GreenFg}mdflc started with port {port} and path {}.{Reset}",
        api.base.unlock().display()
    );

    let wx = api.watcher()?;
    let wx_handle = wx.main();

    let (console_stop, console_recv) = oneshot::channel();
    let stdin_api = api.clone();

    let router = router(api.clone());
    let server_handle = tokio::task::spawn(async {
        axum::serve(tcp_listener, router)
            .with_graceful_shutdown(signal(console_recv, wx_handle))
            .await
            .context("axum server error")
    });

    // blocks the current thread until stdin stops
    // running here allows it to clos properly
    read_console(&stdin_api, console_stop, &wx)?;

    server_handle.await??;
    api.server_closed.notify_waiters();

    println!("{BlueFg}mdflc stopped{Reset}");
    Ok(())
}

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

// TODO: create intermixed version of anyhow & thiserror
// add seamless intermixing between the transparent and
// opaque error types

pub fn router(api: Api) -> Router {
    let index_css = get(([(CONTENT_TYPE, "text/css")], INDEX_CSS));
    let index_js = get(([(CONTENT_TYPE, "text/javascript")], INDEX_JS));
    let favicon = get(([(CONTENT_TYPE, "image/x-icon")], FAVICON));
    Router::new()
        .route("/", get(handle_index))
        .route("/index.css", index_css)
        .route("/index.js", index_js)
        .route("/favicon.ico", favicon)
        .route("/:md", get(handle_md))
        .route("/refresh-ws", get(handle_ws))
        .with_state(api)
}

pub async fn handle_index(State(api): State<Api>) -> impl IntoResponse {
    (
        StatusCode::SEE_OTHER,
        [(axum::http::header::LOCATION, &*api.index.unlock())],
    )
        .into_response()
}

pub async fn handle_ws(ws: WebSocketUpgrade, State(api): State<Api>) -> impl IntoResponse {
    ws.on_upgrade(|mut socket| async move {
        println!("{BlueFg}refresh socket opened{Reset}");

        api.sockets.fetch_add(1, Ordering::Relaxed);
        #[allow(clippy::redundant_pub_crate)]
        let _ = tokio::select! {
            biased;
            () = api.server_closed.notified() => socket.close().await,
            () = async { while socket.recv().await.is_some() {} } => Ok(()),
            () = api.update.notified() => socket.send("refresh".into()).await,
        };
        api.sockets.fetch_sub(1, Ordering::Relaxed);

        println!("{BlueFg}refresh socket closed{Reset}");
    })
}

async fn handle_md(url: AxumPath<String>, State(api): State<Api>) -> impl IntoResponse {
    match api.get_md(&url) {
        Some(html) => (StatusCode::OK, Html(html)),
        None => (StatusCode::NOT_FOUND, Html(api.template.not_found)),
    }
}

pub type MdFiles = Arc<DashMap<String, String>>;

const INDEX_HTML: &str = include_str!("../client/index.html");
const INDEX_CSS: &str = include_str!("../client/index.css");
const INDEX_JS: &str = include_str!("../client/index.js");
const FAVICON: &[u8] = include_bytes!("../client/favicon.ico");

#[derive(Debug, Clone)]
pub struct Api {
    /// server urls
    url: String,
    addr: SocketAddr,
    /// parsed md files
    md: MdFiles,
    /// the served route and the default
    base: Arc<Mutex<PathBuf>>,
    index: Arc<Mutex<String>>,
    /// html templating
    template: Template,
    /// The number of opened websockets
    sockets: Arc<AtomicUsize>,
    /// The number of opened websockets
    update: Arc<Notify>,
    server_closed: Arc<Notify>,
}

impl Api {
    pub fn new(addr: SocketAddr, index: &Path, base: &Path) -> anyhow::Result<Self> {
        let base = base.canonicalize()?;
        let index = index.canonicalize()?;
        let index = index
            .strip_prefix(&base)
            .context("Index must be a path within base")?
            .to_str()
            .context("Invalid path")?
            .to_owned();
        Ok(Self {
            url: format!("http://localhost:{}/", addr.port()),
            addr,
            md: initialize_md(&base)?,
            base: Arc::new(base.into()),
            index: Arc::new(index.into()),
            sockets: Arc::new(0usize.into()),
            template: Template::default(),
            update: Arc::default(),
            server_closed: Arc::default(),
        })
    }

    #[must_use]
    pub fn get_md(&self, url: &str) -> Option<String> {
        self.md
            .get(clean_url(url))
            .map(|r| self.template.html(r.value()))
    }

    pub fn file_update(&self, h: &mut ActionHandler) -> anyhow::Result<()> {
        use watchexec_signals::Signal::*;

        let stop_signal = h
            .signals()
            .find(|s| matches!(s, Hangup | ForceStop | Interrupt | Quit | Terminate));
        if let Some(signal) = stop_signal {
            h.quit_gracefully(signal, Duration::from_millis(500));
            return Ok(());
        }

        let filter = |(path, _): (&Path, _)| {
            if !path.is_file() {
                return None;
            }

            let key = path
                .strip_prefix(self.base.unlock().as_path())
                .ok()?
                .to_str()?
                .to_owned();
            let key = clean_url(&key);
            Some((path.to_owned(), key.to_owned()))
        };

        #[allow(clippy::needless_collect)]
        for (path, key) in h.paths().filter_map(filter).collect::<HashSet<_>>() {
            write_md_from_file(&mut self.md.entry(key).or_default(), &path)?;
        }

        if self.sockets.load(Ordering::Relaxed) != 0 {
            self.update.notify_waiters();
        }

        Ok(())
    }

    fn watcher(&self) -> anyhow::Result<Watchexec> {
        let wx_api = self.clone();
        let config = Config::default();
        config.throttle(Duration::from_millis(100));
        config.pathset([self.base.unlock().clone()]);
        config.on_action(move |mut h| {
            if let Err(e) = wx_api.file_update(&mut h) {
                println!("{RedFg}{e}{Reset}");
            }
            h
        });

        Ok(Watchexec::with_config(config)?)
    }
}

#[derive(Debug, Clone)]
pub struct Template {
    before: &'static str,
    after: &'static str,
    not_found: String,
}

impl Default for Template {
    fn default() -> Self {
        let replace = "{{md}}";

        let Some(start) = INDEX_HTML.find(replace) else {
            unreachable!("the index.html included with the binary is invalid");
        };
        let Some(before) = INDEX_HTML.get(..start) else {
            unreachable!("the index.html included with the binary is invalid");
        };
        let Some(after) = INDEX_HTML.get((start + replace.len())..) else {
            unreachable!("the index.html included with the binary is invalid");
        };

        let not_found = format!("{before}<h1>Error 404: Page not found</h1>{after}");

        Self {
            before,
            after,
            not_found,
        }
    }
}

impl Template {
    #[must_use]
    pub fn html(&self, s: &str) -> String {
        let capacity = self.before.len() + s.len() + self.after.len();
        let mut html = String::with_capacity(capacity);
        html.push_str(self.before);
        html.push_str(s);
        html.push_str(self.after);
        html
    }
}

#[must_use]
pub fn clean_url(url: &str) -> &str {
    let url = url.strip_prefix('/').unwrap_or(url);
    let url = url.strip_suffix(".md").unwrap_or(url);
    url
}

pub fn initialize_md(base: &Path) -> anyhow::Result<MdFiles> {
    let md = MdFiles::default();

    if base.is_file() {
        let mut value = String::new();
        write_md_from_file(&mut value, base)?;
        md.insert("index".into(), value);
        return Ok(md);
    }

    let filter = |file: Result<DirEntry, _>| {
        let file = file.ok().filter(|f| f.file_type().is_file())?;
        file.path()
            .strip_prefix(base)
            .ok()?
            .to_str()?
            .strip_suffix(".md")
            .map(String::from)
            .map(|s| (s, file))
    };

    for (key, file) in WalkDir::new(base).into_iter().filter_map(filter) {
        let mut value = String::new();
        write_md_from_file(&mut value, file.path())?;
        md.insert(key, value);
    }

    Ok(md)
}

pub fn write_md_from_file(out: &mut String, path: &Path) -> anyhow::Result<()> {
    let text = fs::read_to_string(path)?;
    let parser_iter = pulldown_cmark::Parser::new_ext(&text, Options::all());
    let additional = out.capacity().saturating_sub(text.len());

    out.reserve(additional);
    out.clear();
    write_html_fmt(out, parser_iter)?;
    Ok(())
}

/// The finishing of this future indicates a shutdown signal
///
/// # Panics
///
/// Panics if either the `ctrl_c` signal or `sigterm`
/// signal for unix fails to be installed
#[allow(clippy::cognitive_complexity)]
pub async fn signal(
    console_recv: oneshot::Receiver<()>,
    wx_handle: JoinHandle<Result<(), CriticalError>>,
) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[allow(clippy::redundant_pub_crate)]
    let () = tokio::select! {
        () = ctrl_c => {
            println!("{BlueFg}Ctrl-C received, app shutdown commencing{Reset}");
        },
        () = terminate => {
            println!("{BlueFg}SIGTERM received, app shutdown commencing{Reset}");
        },
        e = console_recv => {
            e.context("stdin error").unwrap();
            println!("{BlueFg}Console exit recieved, app shutdown commencing{Reset}");
        },
        e = wx_handle => {
            e.context("Handle Error").unwrap().context("Watchexec Error").unwrap();
            println!("{BlueFg}Watchexec handle stopped{Reset}");
        }
    };
}

/// Reads console
///
/// Finishes once quit command recieved.
fn read_console(api: &Api, stop: oneshot::Sender<()>, wx: &Watchexec) -> std::io::Result<()> {
    // TODO: create better interactive terminal
    let stdin = std::io::stdin();
    let mut buf = String::new();

    loop {
        buf.clear();
        stdin.read_line(&mut buf)?;
        let s = buf.trim();
        if !s.is_empty() && handle_ci(api, wx, s) {
            let _ = stop.send(());
            break;
        }
    }
    Ok(())
}

/// returns true if program should stop
fn handle_ci(api: &Api, wx: &Watchexec, s: &str) -> bool {
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

/// utility for mutex
pub trait MutexExt<'a, T: ?Sized> {
    /// An extension to [`Mutex::lock`]
    ///
    /// All this does is [unwrap] the output of [`Mutex::lock`].
    ///
    /// [unwrap]: Result::unwrap
    fn unlock(&'a self) -> MutexGuard<'a, T>;
}

impl<'a, T: ?Sized + 'a> MutexExt<'a, T> for Mutex<T> {
    fn unlock(&'a self) -> MutexGuard<'a, T> {
        self.lock().expect("mutex error")
    }
}
