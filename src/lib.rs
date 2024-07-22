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

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use args::Args;
use async_std::channel::{unbounded, Receiver, Sender};
use axum::{
    extract::{ws::Message, Path as AxumPath, State, WebSocketUpgrade},
    http::{header::CONTENT_TYPE, StatusCode},
    response::{Html, IntoResponse, Redirect},
    routing::get,
    Router,
};
use clap::Parser;
use dashmap::DashMap;
use easy_sgr::{Color::*, Style::*};
use miette::{ensure, IntoDiagnostic};
use pulldown_cmark::{html::write_html_fmt, Options};
use tokio::net::TcpListener;
use walkdir::{DirEntry, WalkDir};
use watchexec::{action::ActionHandler, Config, Watchexec};

/// implements clap
pub mod args;

/// server end signal
pub mod signal;

// pub mod tiny;

pub async fn run() -> miette::Result<()> {
    miette::set_panic_hook();
    let args = Args::parse();

    ensure!(
        args.path.exists(),
        "The given path \"{}\" does not exist",
        args.path.to_string_lossy()
    );
    let base = args.path.canonicalize().into_diagnostic()?;
    let port = args.port;
    let addr = format!("0.0.0.0:{port}");
    let tcp_listener = TcpListener::bind(addr).await.into_diagnostic()?;

    signal::scroll();
    println!(
        "{GreenFg}\
            mdflc started with port {port} and path {}.\
         {Reset}",
        base.display()
    );

    let api = Api::new(base)?;

    let wx_api = api.clone();
    let config = Config::default();
    config.throttle(Duration::from_millis(100));
    config.pathset([api.base.clone()]);
    config.on_action_async(move |mut h| {
        let api = wx_api.clone();

        Box::new(async move {
            if let Err(e) = api.file_update(&mut h).await {
                println!("{RedFg}{e}{Reset}");
            }
            h
        })
    });

    let wx = Watchexec::with_config(config)?;
    let wx_handle = wx.main();

    let local_url = format!("http://localhost:{port}/");
    let router = router(api.clone());

    println!("{GreenFg}Watcher started{Reset}");
    println!("{GreenFg}Server Starting{Reset}");
    axum::serve(tcp_listener, router)
        .with_graceful_shutdown(signal::signal(wx_handle, local_url))
        .await
        .into_diagnostic()?;
    println!("{GreenFg}Server Stopped{Reset}");
    println!("{GreenFg}mdflc stopping{Reset}");

    Ok(())
}

pub fn router(api: Api) -> Router {
    let index_md = get(|| async { Redirect::permanent("index.md") });
    let index_css = get(([(CONTENT_TYPE, "text/css")], INDEX_CSS));
    let index_js = get(([(CONTENT_TYPE, "text/javascript")], INDEX_JS));
    let favicon = get(([(CONTENT_TYPE, "image/x-icon")], FAVICON));

    Router::new()
        // redirect
        .route("/", index_md)
        // other stuff
        .route("/index.css", index_css)
        .route("/index.js", index_js)
        .route("/favicon.ico", favicon)
        // the real index
        .route("/:md", get(handle_md))
        .route("/refresh-ws", get(handle_ws))
        .with_state(api)
}

pub async fn handle_ws(ws: WebSocketUpgrade, State(api): State<Api>) -> impl IntoResponse {
    // NOTE: could probably remove the below select
    ws.on_upgrade(|mut socket| async move {
        println!("{BlueFg}refresh socket opened{Reset}");
        api.sockets.fetch_add(1, Ordering::Relaxed);
        #[allow(clippy::redundant_pub_crate)]
        loop {
            tokio::select! {
                e = api.recv.recv() => { if e.is_err() { break; } },
                r = socket.recv() => { if r.is_none() { break; } },
            }
            if socket.send(Message::Text("reload".into())).await.is_err() {
                break;
            }
        }
        api.sockets.fetch_sub(1, Ordering::Relaxed);
        println!("{BlueFg}refresh socket closed{Reset}");
    })
}

async fn handle_md(url: AxumPath<String>, State(api): State<Api>) -> impl IntoResponse {
    if let Some(contents) = api.md.get(clean_url(&url)) {
        let html = api.template.html(contents.value());
        return (StatusCode::OK, Html(html)).into_response();
    }

    (StatusCode::NOT_FOUND, Html(api.template.not_found)).into_response()
}

pub type MdFiles = Arc<DashMap<String, String>>;

const INDEX_HTML: &str = include_str!("../client/index.html");
const INDEX_CSS: &str = include_str!("../client/index.css");
const INDEX_JS: &str = include_str!("../client/index.js");

const FAVICON: &[u8] = include_bytes!("../client/favicon.ico");

#[derive(Debug, Clone)]
pub struct Api {
    sockets: Arc<AtomicUsize>,
    md: MdFiles,
    template: Template,
    recv: Receiver<()>,
    send: Sender<()>,
    base: PathBuf,
}

impl Api {
    pub fn new(base: PathBuf) -> miette::Result<Self> {
        let (send, recv) = unbounded();

        Ok(Self {
            sockets: Arc::new(0usize.into()),
            md: initialize_md(&base)?,
            template: Template::default(),
            recv,
            send,
            base,
        })
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

impl Api {
    pub async fn file_update(&self, h: &mut ActionHandler) -> miette::Result<()> {
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
                .strip_prefix(&self.base)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            let key = clean_url(&key);
            Some((path.to_owned(), key.to_owned()))
        };

        #[allow(clippy::needless_collect)]
        for (path, key) in h.paths().filter_map(filter).collect::<HashSet<_>>() {
            write_md_from_file(&mut self.md.entry(key).or_default(), &path)?;
            if self.sockets.load(Ordering::Relaxed) != 0 {
                self.send.send(()).await.into_diagnostic()?;
            }
        }

        Ok(())
    }
}

#[must_use]
pub fn clean_url(url: &str) -> &str {
    let url = url.strip_prefix('/').unwrap_or(url);
    let url = url.strip_suffix(".md").unwrap_or(url);
    url
}

pub fn initialize_md(base: &Path) -> miette::Result<MdFiles> {
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

pub fn write_md_from_file(out: &mut String, path: &Path) -> miette::Result<()> {
    let text = fs::read_to_string(path).into_diagnostic()?;
    let parser_iter = pulldown_cmark::Parser::new_ext(&text, Options::all());
    let additional = out.capacity().saturating_sub(text.len());

    out.reserve(additional);
    out.clear();
    write_html_fmt(out, parser_iter).into_diagnostic()?;
    Ok(())
}
