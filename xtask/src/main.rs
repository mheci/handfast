//! Rust developer tasks.

use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Handfast developer tasks")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Check formatting.
    Fmt,
    /// Run clippy with warnings denied.
    Clippy,
    /// Run the workspace test suite.
    Test,
    /// Build documentation with warnings denied.
    Doc,
    /// Run fmt, clippy, tests and docs.
    Check,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let success = match cli.task {
        Task::Check => [Task::Fmt, Task::Clippy, Task::Test, Task::Doc]
            .iter()
            .all(run),
        task => run(&task),
    };
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run(task: &Task) -> bool {
    let mut command = Command::new("cargo");
    match task {
        Task::Fmt => {
            command.args(["fmt", "--all", "--", "--check"]);
        }
        Task::Clippy => {
            command.args([
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ]);
        }
        Task::Test => {
            command.args(["test", "--workspace", "--all-features", "--all-targets"]);
        }
        Task::Doc => {
            command.env("RUSTDOCFLAGS", "-D warnings").args([
                "doc",
                "--workspace",
                "--no-deps",
                "--all-features",
            ]);
        }
        Task::Check => return false,
    }
    println!("running {task:?}");
    match command.status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("task failed with {status}");
            false
        }
        Err(error) => {
            eprintln!("failed to spawn cargo: {error}");
            false
        }
    }
}
