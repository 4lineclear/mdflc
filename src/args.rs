use std::{net::SocketAddr, path::PathBuf};

use clap::Parser;

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
