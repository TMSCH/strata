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
        /// Quarantine and remove an incomplete last line after reviewing damage.
        #[arg(long)]
        recover_tail: bool,
    },
    /// Operator: verify an offline store, optionally against a trusted snapshot.
    Verify {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        baseline: Option<PathBuf>,
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
        Command::Serve {
            dir,
            socket,
            recover_tail,
        } => {
            protocol::serve(Store::open_with_recovery(dir, recover_tail)?, &socket)?;
        }
        Command::Verify { dir, baseline } => {
            println!(
                "{}",
                serde_json::to_string(&strata::verify(dir, baseline.as_deref())?)?
            );
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
