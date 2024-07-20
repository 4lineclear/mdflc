use std::path::PathBuf;

use clap::Parser;

/// host a markdown file server
#[derive(Parser, Debug)]
pub struct Args {
    /// The path to read
    pub path: PathBuf,
    /// An optional port
    #[arg(short, long, default_value_t = 6464)]
    pub port: u16,
}
