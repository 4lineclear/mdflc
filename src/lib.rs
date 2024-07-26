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
    io::IsTerminal,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use anyhow::{ensure, Context, Ok as AnyOk};
use axum::{
    extract::{Path as AxumPath, State, WebSocketUpgrade},
    http::{
        header::{CONTENT_TYPE, LOCATION},
        StatusCode,
    },
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

/// the cli
pub mod cli;

// TODO: Create own markdown parser
// TODO: Add ability to add/remove/list paths
// TODO: add glossery, etc.
// TODO: create utility for making ext traits
// TODO: create intermixed version of anyhow & thiserror
// add seamless intermixing between the transparent and
// opaque error types
// TODO: user added custom css
// TODO: create new spa-like loading system
pub async fn run() -> anyhow::Result<()> {
    let args = cli::Args::parse();

    ensure!(
        args.base.try_exists().unwrap_or(false),
        "The given path \"{}\" does not exist",
        args.base.display()
    );

    let port = args.addr.port();
    let addr = args.addr;
    let tcp_listener = TcpListener::bind(addr).await?;

    let api = Arc::new(Api::new(addr, &args.index, &args.base)?);

    cli::scroll();
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

    if std::io::stdin().is_terminal() {
        // spawn in thread so we can exit using other methods
        std::thread::spawn(move || {
            if let Err(e) = cli::read_console(&stdin_api, &wx) {
                eprintln!("{YellowFg}interactive console shutdown: {Reset}{RedFg}\"{e}\"{Reset}");
            } else {
                let _ = console_stop.send(());
            }
        });
    }

    server_handle.await??;
    api.server_closed.notify_waiters();

    println!("{BlueFg}mdflc stopped{Reset}");
    AnyOk(())
}

pub fn router(api: Arc<Api>) -> Router {
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

pub async fn handle_index(State(api): ApiState) -> impl IntoResponse {
    (StatusCode::SEE_OTHER, [(LOCATION, &*api.index.unlock())]).into_response()
}

async fn handle_md(url: AxumPath<String>, State(api): ApiState) -> impl IntoResponse {
    api.get_md(&url).map_or_else(
        || (StatusCode::NOT_FOUND, Html(api.template.not_found.clone())),
        |html| (StatusCode::OK, Html(html)),
    )
}

pub async fn handle_ws(ws: WebSocketUpgrade, State(api): ApiState) -> impl IntoResponse {
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

/// a collection of paths to parsed markdown files
pub type MdFiles = Arc<DashMap<String, String>>;

const INDEX_HTML: &str = include_str!("../client/index.html");
const INDEX_CSS: &str = include_str!("../client/index.css");
const INDEX_JS: &str = include_str!("../client/index.js");
const FAVICON: &[u8] = include_bytes!("../client/favicon.ico");

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

type ApiState = State<Arc<Api>>;

#[derive(Debug)]
pub struct Api {
    /// server urls
    url: String,
    #[allow(dead_code)]
    addr: SocketAddr,
    /// parsed md files
    md: MdFiles,
    /// the served route and the default
    base: Mutex<PathBuf>,
    index: Mutex<String>,
    /// html templating
    template: Template,
    /// The number of opened websockets
    sockets: AtomicUsize,
    /// The number of opened websockets
    update: Notify,
    server_closed: Notify,
}

impl Api {
    pub fn new(addr: SocketAddr, index: &Path, base: &Path) -> anyhow::Result<Self> {
        let base = base.canonicalize().context("invalid base path")?;
        let index = index
            .canonicalize()
            .context("invalid index path")?
            .strip_prefix(&base)
            .context("index must be a path within base")?
            .to_str()
            .context("only utf8 paths allowed")?
            .to_owned();

        Ok(Self {
            url: format!("http://localhost:{}/", addr.port()),
            addr,
            md: initialize_md(&base)?,
            base: base.into(),
            index: index.into(),
            sockets: AtomicUsize::default(),
            template: Template::default(),
            update: Notify::default(),
            server_closed: Notify::default(),
        })
    }

    #[must_use]
    pub fn get_md(&self, url: &str) -> Option<String> {
        self.md
            .get(clean_url(url))
            .map(|r| self.template.html(r.value()))
    }

    /// Handles file updates made by [`watchexec`]
    pub fn file_update(&self, h: &ActionHandler) -> anyhow::Result<()> {
        // don't read files twice
        let mut files = HashSet::new();

        for (path, _) in h.paths() {
            if !path.is_file() {
                continue;
            }

            if !files.insert(path) {
                continue;
            }

            let Some(key) = path
                .strip_prefix(self.base.unlock().as_path())
                .ok()
                .and_then(Path::to_str)
                .and_then(|s| s.strip_suffix(".md"))
            else {
                continue;
            };

            write_md_from_file(&mut self.md.entry(key.to_owned()).or_default(), path)?;
        }

        // send update only once
        if !files.is_empty() && self.sockets.load(Ordering::Relaxed) != 0 {
            self.update.notify_waiters();
        }

        Ok(())
    }

    fn watcher(self: &Arc<Self>) -> anyhow::Result<Watchexec> {
        let wx_api = self.clone();
        let config = Config::default();

        config.throttle(Duration::from_millis(100));
        config.pathset([self.base.unlock().clone()]);
        config.on_action(move |h| {
            if let Err(e) = wx_api.file_update(&h) {
                eprintln!("{RedFg}{e}{Reset}");
            }
            h
        });

        Ok(Watchexec::with_config(config)?)
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

pub trait MutexExt<'a, T: ?Sized> {
    fn unlock(&'a self) -> MutexGuard<'a, T>;
}

impl<'a, T: ?Sized + 'a> MutexExt<'a, T> for Mutex<T> {
    fn unlock(&'a self) -> MutexGuard<'a, T> {
        self.lock().expect("mutex error")
    }
}
