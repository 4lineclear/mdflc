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

use anyhow::{ensure, Context};
use args::Args;
use axum::{
    extract::{Path as AxumPath, State, WebSocketUpgrade},
    http::{header::CONTENT_TYPE, StatusCode},
    response::{Html, IntoResponse, Redirect},
    routing::get,
    Router,
};
use clap::Parser;
use dashmap::DashMap;
use easy_sgr::{Color::*, Style::*};
use pulldown_cmark::{html::write_html_fmt, Options};
use tokio::{net::TcpListener, sync::Notify};
use walkdir::{DirEntry, WalkDir};
use watchexec::{action::ActionHandler, Config, Watchexec};

/// implements clap
pub mod args;

/// server end signal
pub mod signal;

// pub mod tiny;

// TODO: Create own markdown parser
// TODO: Add ability to add/remove/list paths
pub async fn run() -> anyhow::Result<()> {
    let args = Args::parse();
    let base_exists = args
        .base
        .try_exists()
        .context("Couldn't check if given path exists")?;
    ensure!(
        base_exists,
        "The given path \"{}\" does not exist",
        args.base.display()
    );

    let base = args.base.canonicalize().context("Invalid path")?;
    let port = args.addr.port();
    let addr = args.addr;
    let tcp_listener = TcpListener::bind(addr).await?;

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
    config.on_action(move |mut h| {
        if let Err(e) = wx_api.file_update(&mut h) {
            println!("{RedFg}{e}{Reset}");
        }
        h
    });

    let wx = Watchexec::with_config(config)?;
    let wx_handle = wx.main();

    let local_url = format!("http://localhost:{port}/");
    let router = router(api.clone());

    axum::serve(tcp_listener, router)
        .with_graceful_shutdown(signal::signal(wx_handle, local_url))
        .await
        .context("axum server error")?;

    api.server_closed.notify_waiters();

    println!("{BlueFg}mdflc stopped{Reset}");
    Ok(())
}

pub fn router(api: Api) -> Router {
    let index_css = get(([(CONTENT_TYPE, "text/css")], INDEX_CSS));
    let index_js = get(([(CONTENT_TYPE, "text/javascript")], INDEX_JS));
    let favicon = get(([(CONTENT_TYPE, "image/x-icon")], FAVICON));

    Router::new()
        .route("/", get(Redirect::temporary("/index.md")))
        .route("/index.css", index_css)
        .route("/index.js", index_js)
        .route("/favicon.ico", favicon)
        .route("/:md", get(handle_md))
        .route("/refresh-ws", get(handle_ws))
        .with_state(api)
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
    sockets: Arc<AtomicUsize>,
    md: MdFiles,
    template: Template,
    update: Arc<Notify>,
    server_closed: Arc<Notify>,
    base: PathBuf,
}

impl Api {
    pub fn new(base: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            sockets: Arc::new(0usize.into()),
            md: initialize_md(&base)?,
            template: Template::default(),
            update: Arc::default(),
            server_closed: Arc::default(),
            base,
        })
    }

    #[must_use]
    pub fn get_md(&self, url: &str) -> Option<String> {
        self.md
            .get(clean_url(url))
            .map(|r| self.template.html(r.value()))
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
                self.update.notify_waiters();
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
