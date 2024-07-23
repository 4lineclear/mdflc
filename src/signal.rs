use anyhow::Context;
use easy_sgr::{Color::*, Style::*};
use tokio::{signal, task::JoinHandle};
use watchexec::error::CriticalError;

/// The finishing of this future indicates a shutdown signal
///
/// # Panics
///
/// Panics if either the `ctrl_c` signal or `sigterm`
/// signal for unix fails to be installed
#[allow(clippy::cognitive_complexity)]
pub async fn signal(wx_handle: JoinHandle<Result<(), CriticalError>>, url: String) {
    let console = async { read_console(&url).await.expect("Failed to read console") };
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

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {
            println!("{BlueFg}Ctrl-C received, app shutdown commencing{Reset}");
        },
        () = terminate => {
            println!("{BlueFg}SIGTERM received, app shutdown commencing{Reset}");
        },
        () = console => {
            println!("{BlueFg}Console exit recieved, app shutdown commencing{Reset}");
        },
        e = wx_handle => {
            e.context("Handle Error").unwrap().context("Watchexec Error").unwrap();
            println!("{BlueFg}Watchexec handle stopped{Reset}");
        }
    }
}

/// Reads console
///
/// Finishes once quit command recieved.
async fn read_console(url: &str) -> anyhow::Result<()> {
    let stdin = async_std::io::stdin();
    let mut buf = String::new();
    loop {
        buf.clear();
        stdin
            .read_line(&mut buf)
            .await
            .context("Couldn't read stdin")?;
        if handle_console_input(url, buf.trim()) {
            break;
        }
    }
    Ok(())
}

/// returns true if program should stop
fn handle_console_input(url: &str, s: &str) -> bool {
    match s {
        "h" => println!(
            "\
            enter {BlueFg}h{Reset} to show help (this text)\n\
            enter {BlueFg}o{Reset} to open client in browser\n\
            enter {BlueFg}u{Reset} to show server url\n\
            enter {BlueFg}c{Reset} clear screen\n\
            enter {BlueFg}q{Reset} to quit\
            "
        ),
        "o" => {
            if webbrowser::open(url).is_ok() {
                println!("{GreenFg}Opening browser...{Reset}");
            } else {
                println!("{YellowFg}Unable to open browser{Reset}");
            }
        }
        "c" => scroll(),
        "u" => println!("{BlueFg}{url}{Reset}"),
        "q" => return true,
        _ => (),
    }
    false
}

pub(super) fn scroll() {
    print!("\x1B[2J\x1B[1;1H");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}
