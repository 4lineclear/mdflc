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
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use clap::Parser;
use easy_sgr::{Color::*, Style::*};
use miette::{bail, ensure, IntoDiagnostic};
use pulldown_cmark::{html::write_html_fmt, Options};
use tokio::net::TcpListener;
use walkdir::WalkDir;
use watchexec::{action::ActionHandler, Config, Watchexec};

use dashmap::DashMap;

/// implements clap
pub mod args;

/// server end signal
pub mod signal;

pub type MdFiles = Arc<DashMap<String, String>>;

#[derive(Debug, Clone)]
pub struct Api {
    sockets: Arc<AtomicUsize>,
    md: MdFiles,
    /// before markdown
    bm: &'static str,
    /// after markdown
    am: &'static str,
    recv: Receiver<()>,
    send: Sender<()>,
    base: PathBuf,
}

const INDEX_HTML: &str = include_str!("../client/index.html");
const INDEX_CSS: &str = include_str!("../client/index.css");
const INDEX_JS: &str = include_str!("../client/index.js");

const FAVICON: &[u8] = include_bytes!("../client/favicon.ico");

pub async fn run() -> miette::Result<()> {
    miette::set_panic_hook();
    let (bm, am) = setup_template()?;
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

    let (send, recv) = unbounded();

    let md = MdFiles::default();
    initialize_md(&base, md.clone())?;
    let sockets = Arc::new(0usize.into());
    let api = Api {
        sockets,
        md,
        bm,
        am,
        recv,
        send,
        base,
    };

    let wx_api = api.clone();
    let config = Config::default();
    config.throttle(Duration::from_millis(100));
    config.pathset([api.base.clone()]);
    config.on_action_async(move |mut h| {
        let api = wx_api.clone();

        Box::new(async move {
            if let Err(e) = file_update(&mut h, api).await {
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

fn router(api: Api) -> Router {
    let index_css = get(([(CONTENT_TYPE, "text/css")], INDEX_CSS));
    let index_js = get(([(CONTENT_TYPE, "text/javascript")], INDEX_JS));
    let favicon = get(([(CONTENT_TYPE, "image/x-icon")], FAVICON));

    Router::new()
        .route("/index.css", index_css)
        .route("/index.js", index_js)
        .route("/favicon.ico", favicon)
        .route("/refresh-ws", get(refresh_ws))
        .route("/", get(md_handler))
        .route("/index.html", get(md_handler))
        .route("/:md", get(md_handler))
        .with_state(api)
}

fn setup_template() -> miette::Result<(&'static str, &'static str)> {
    let pat = "{{md}}";

    let Some(start) = INDEX_HTML.find(pat) else {
        bail!("the index.html included with the binary is invalid");
    };
    let Some(bm) = INDEX_HTML.get(..start) else {
        bail!("the index.html included with the binary is invalid");
    };
    let Some(am) = INDEX_HTML.get((start + pat.len())..) else {
        bail!("the index.html included with the binary is invalid");
    };

    Ok((bm, am))
}

async fn refresh_ws(ws: WebSocketUpgrade, State(api): State<Api>) -> impl IntoResponse {
    ws.on_upgrade(|mut socket| async move {
        println!("{BlueFg}refresh socket opened{Reset}");
        api.sockets.fetch_add(1, Ordering::Relaxed);
        loop {
            tokio::select! {
                e = api.recv.recv() => { if e.is_err() { break; } },
                r = socket.recv() => { if r.is_none() { break; } },
            }
            if socket.send(Message::Text("".into())).await.is_err() {
                break;
            }
        }
        api.sockets.fetch_sub(1, Ordering::Relaxed);
        println!("{BlueFg}refresh socket closed{Reset}");
    })
}

async fn file_update(h: &mut ActionHandler, api: Api) -> miette::Result<()> {
    use watchexec_signals::Signal::*;

    let stop_signal = h
        .signals()
        .find(|s| matches!(s, Hangup | ForceStop | Interrupt | Quit | Terminate));
    if let Some(signal) = stop_signal {
        h.quit_gracefully(signal, Duration::from_millis(500));
        return Ok(());
    }

    let paths: HashSet<_> = h
        .paths()
        .filter_map(|(path, _)| {
            if !path.is_file() {
                return None;
            }

            let key = path
                .strip_prefix(&api.base)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            let key = clean_url(&key);
            Some((path, key.to_owned()))
        })
        .collect();

    for (path, key) in paths {
        write_md_from_file(&mut api.md.entry(key).or_default(), path)?;
        if api.sockets.load(Ordering::Relaxed) != 0 {
            api.send.try_send(()).into_diagnostic()?;
        }
    }

    Ok(())
}

async fn md_handler(url: Option<AxumPath<String>>, State(api): State<Api>) -> impl IntoResponse {
    let url = match url {
        Some(AxumPath(url)) => url,
        None => "index".into(),
    };

    if let Some(contents) = api.md.get(clean_url(&url)) {
        let md = contents.value();
        let capacity = api.bm.len() + md.len() + api.am.len();
        let mut html = String::with_capacity(capacity);

        html.push_str(api.bm);
        html.push_str(md);
        html.push_str(api.am);

        return (StatusCode::OK, Html(html)).into_response();
    }

    (StatusCode::NOT_FOUND, "<h1>Error 404: Page not found</h1>").into_response()
}

fn clean_url(url: &str) -> &str {
    let url = url.strip_prefix('/').unwrap_or(url);
    let url = url.strip_suffix(".md").unwrap_or(url);
    url
}

fn initialize_md(base: &Path, md_files: MdFiles) -> miette::Result<()> {
    for file in WalkDir::new(base) {
        let file = file.into_diagnostic()?;
        if !file.file_type().is_file() {
            continue;
        }

        let Some(key) = file
            .path()
            .strip_prefix(base)
            .into_diagnostic()?
            .to_string_lossy()
            .strip_suffix(".md")
            .map(String::from)
        else {
            continue;
        };

        let mut value = String::new();
        write_md_from_file(&mut value, file.path())?;
        md_files.insert(key, value);
    }
    Ok(())
}

fn write_md_from_file(out: &mut String, path: &Path) -> miette::Result<()> {
    let text = fs::read_to_string(path).into_diagnostic()?;
    let parser_iter = pulldown_cmark::Parser::new_ext(&text, Options::all());
    let additional = out.capacity().saturating_sub(text.len());

    out.reserve(additional);
    out.clear();
    write_html_fmt(out, parser_iter).into_diagnostic()?;
    Ok(())
}
