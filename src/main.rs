use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    io::{self, Read},
    path::PathBuf,
};
use strata::{Append, Store, protocol, record::MAX_REQUEST};

#[derive(Parser)]
#[command(version, about = "Append-only JSONL events for agents")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Append a JSON object read from stdin. Prints a receipt after durable storage.
    Append {
        #[arg(long, env = "STRATA_SOCKET")]
        socket: PathBuf,
        #[arg(long = "type")]
        kind: String,
        /// Reuse an ID on retries. Generated if omitted; printed to stderr first.
        #[arg(long)]
        id: Option<String>,
    },
    /// Operator: run the single-writer daemon in the foreground.
    Serve {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long, env = "STRATA_SOCKET")]
        socket: PathBuf,
    },
    /// Operator: verify an offline store or a live daemon's staged Git snapshot.
    Verify {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long, requires = "dir")]
        baseline: Option<PathBuf>,
        #[arg(long, conflicts_with_all = ["dir", "baseline"], requires_all = ["repo", "path"])]
        staged: bool,
        #[arg(long, requires = "staged")]
        repo: Option<PathBuf>,
        /// Store directory relative to the repository root, e.g. events.
        #[arg(long, requires = "staged")]
        path: Option<String>,
        /// Trusted earlier commit; defaults to HEAD.
        #[arg(long, requires = "staged", conflicts_with = "initial")]
        baseline_ref: Option<String>,
        /// Explicitly verify the first snapshot in an unborn repository.
        #[arg(long, requires = "staged")]
        initial: bool,
    },
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Append { socket, kind, id } => {
            let mut bytes = Vec::new();
            io::stdin()
                .take((MAX_REQUEST + 1) as u64)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= MAX_REQUEST, "stdin exceeds 64 KiB");
            let data =
                serde_json::from_slice(&bytes).context("stdin must contain one JSON object")?;
            let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            eprintln!("strata: request id {id} (reuse with --id if the outcome is uncertain)");
            let receipt = protocol::append(&socket, &Append { id, kind, data })?;
            println!("{}", serde_json::to_string(&receipt)?);
        }
        Command::Serve { dir, socket } => {
            protocol::serve(Store::open(dir)?, &socket)?;
        }
        Command::Verify {
            dir,
            baseline,
            staged,
            repo,
            path,
            baseline_ref,
            initial,
        } => {
            if staged {
                println!(
                    "{}",
                    serde_json::to_string(&strata::git::verify_staged(
                        repo.as_deref().context("--repo is required")?,
                        path.as_deref().context("--path is required")?,
                        baseline_ref.as_deref(),
                        initial
                    )?)?
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string(&strata::verify(
                        dir.context("provide --dir or --staged")?,
                        baseline.as_deref()
                    )?)?
                );
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("strata: {error:#}");
        std::process::exit(1);
    }
}
