#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

//! `hfctl` — Handfast terminal control.
//!
//! One binary, two faces:
//!
//! * a scriptable CLI for one-shot daemon queries and actions (`hfctl status`,
//!   `hfctl devices`, `hfctl pair …`, …), and
//! * an interactive ratatui interface launched by `hfctl tui` — also the
//!   default when no subcommand is given.
//!
//! All communication goes through [`handfast_ipc`] over the daemon's local
//! socket; this crate contains no display-server or pairing logic.
//!
//! Error contract: modules return typed errors ([`error::Error`],
//! [`std::io::Error`]); only `run` widens them into an [`anyhow::Error`] so
//! `main` can print one flat diagnostic to stderr and exit with status 1.

mod app;
mod cli;
mod cmd;
mod error;
mod model;
mod state;
mod view;

use std::process::ExitCode;

use clap::Parser as _;

use crate::cli::{Cli, Command};

/// Entry point: installs the terminal-restoring panic hook, then dispatches.
///
/// The hook wraps the previous one so panics always leave raw mode and the
/// alternate screen before any message reaches the user's shell.
#[tokio::main]
async fn main() -> ExitCode {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        app::restore_terminal();
        previous_hook(panic_info);
    }));

    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Parse arguments and dispatch the chosen subcommand.
///
/// Clap usage errors are special-cased: they render clap's own help/error
/// output on the appropriate stream and exit with clap's conventional codes
/// (`2` for usage errors, `0` for `--help`/`--version`). Everything else
/// bubbles up through `anyhow`.
async fn run() -> anyhow::Result<ExitCode> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return Ok(if error.use_stderr() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            });
        }
    };

    let command = cli.command.unwrap_or(Command::Tui);
    let socket = cli.socket.unwrap_or_else(handfast_ipc::default_socket_path);

    // Completion generation never touches the socket.
    if let Command::Completions { shell } = &command {
        cli::generate_completions(*shell)?;
        return Ok(ExitCode::SUCCESS);
    }

    let mut client = cmd::connect(&socket).await?;
    match command {
        Command::Tui => {
            tracing::debug!(
                target: "handfast::tui",
                socket = %socket.display(),
                "launching interactive interface"
            );
            app::run(client).await?;
        }
        Command::Status => cmd::print_status(&mut client, &socket).await?,
        Command::Devices => cmd::print_devices(&client).await?,
        Command::Pair { device_id } => cmd::print_pair(&client, &device_id).await?,
        Command::Unpair { device_id } => cmd::print_unpair(&client, &device_id).await?,
        Command::Plugins { action } => {
            cmd::print_plugins_action(&client, action).await?;
        }
        Command::Send {
            device_id,
            file_path,
        } => {
            cmd::print_send(&client, &device_id, &file_path).await?;
        }
        Command::Notifications { action } => {
            cmd::print_notifications_action(&client, action).await?;
        }
        Command::Clipboard { action } => {
            cmd::print_clipboard_action(&client, action).await?;
        }
        Command::Logs { number } => cmd::print_logs(&mut client, number).await?,
        // Already handled above, before opening a connection.
        Command::Completions { .. } => {}
    }
    Ok(ExitCode::SUCCESS)
}
