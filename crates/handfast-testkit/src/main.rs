//! Rust conformance peer and vector checker.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use handfast_testkit::{
    payload_peer::run_payload_self_test, peer::run_self_test, tls_peer::run_tls_self_test,
    vectors::load_dir,
};

#[derive(Debug, Parser)]
#[command(
    name = "handfast-testkit",
    version,
    about = "Rust KDE Connect conformance peer"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the loopback client/server self-test.
    SelfTest,
    /// Run TLS 1.2 and 1.3 handshakes with ECDSA and RSA certificates.
    TlsSelfTest,
    /// Run the loopback payload sender and receiver self-test.
    PayloadSelfTest,
    /// Validate golden vectors.
    Vectors {
        /// Vector directory. Defaults to the crate's tests/vectors.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command.unwrap_or(Command::SelfTest)).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("handfast-testkit: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::SelfTest => {
            let report = run_self_test().await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::TlsSelfTest => {
            let report = run_tls_self_test().await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::PayloadSelfTest => {
            let report = run_payload_self_test().await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Vectors { dir } => {
            let dir = dir
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors"));
            let vectors = load_dir(&dir)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "directory": dir,
                    "vectors": vectors.len(),
                    "names": vectors.iter().map(|vector| vector.name.as_str()).collect::<Vec<_>>(),
                }))?
            );
        }
    }
    Ok(())
}
