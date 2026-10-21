use std::io;

use anyhow::Result;
use clap::{Parser, Subcommand};
use dotonce::serve::serve;
use dotonce::store::Store;

#[derive(Parser)]
#[command(
    name = "dotonce",
    version,
    about = "Memory and checks for AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// JSON-lines server for the benchmark adapter: one request per stdin line, one reply per stdout line.
    BenchServe {
        /// Folder for the per-user index files, or ":memory:" to keep every index in memory.
        #[arg(long)]
        dir: String,
        #[arg(long)]
        fresh: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::BenchServe { dir, fresh } => {
            let mut st = Store::open(&dir, fresh)?;
            serve(&mut st, io::stdin().lock(), io::stdout().lock())
        }
    }
}
