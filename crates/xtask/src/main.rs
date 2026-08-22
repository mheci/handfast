//! `cargo xtask` — release tooling for Handfast.
//!
//! * `cargo xtask dist [--out DIR]` — assemble a distributable directory
//!   (`binaries + SHA256SUMS`) from a prior `cargo build --release`.
//! * `cargo xtask completions <SHELL> [--out DIR]` — generate shell completions
//!   by invoking the built `hfctl` binary (keeps CLI definitions in one place).

#![deny(clippy::unwrap_used)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};
use clap::{Parser, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "Handfast release tooling")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, clap::Subcommand)]
enum Cmd {
    /// Assemble dist/ from target/release artifacts.
    Dist {
        /// Output directory (default: dist/).
        #[arg(long, default_value = "dist")]
        out: PathBuf,
    },
    /// Generate shell completions via the built hfctl binary.
    Completions {
        shell: Shell,
        /// Output directory (default: dist/completions).
        #[arg(long, default_value = "dist/completions")]
        out: PathBuf,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Elvish,
}

impl Shell {
    fn hfctl_name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::PowerShell => "powershell",
            Shell::Elvish => "elvish",
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.cmd {
        Cmd::Dist { out } => dist(&out),
        Cmd::Completions { shell, out } => completions(shell, &out),
    }
}

const BINARIES: [&str; 3] = ["handfastd", "hfctl", "handfast-gui"];

fn release_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into()))
        .join("release")
}

fn dist(out: &Path) -> anyhow::Result<()> {
    let src = release_dir();
    std::fs::create_dir_all(out).context("creating dist dir")?;

    let mut hashes: Vec<(String, String)> = Vec::new();
    for name in BINARIES {
        let candidate = src.join(name);
        if !candidate.is_file() {
            bail!(
                "{name} not found at {} — run `cargo build --release --all-features` first",
                candidate.display()
            );
        }
        let dest = out.join(name);
        std::fs::copy(&candidate, &dest)
            .with_context(|| format!("copying {}", candidate.display()))?;
        let digest = sha256_file(&dest)?;
        hashes.push((digest, name.to_string()));
        println!("packed {name}");
    }

    let mut sums = String::new();
    for (digest, name) in &hashes {
        sums.push_str(&format!("{digest}  {name}\n"));
    }
    std::fs::write(out.join("SHA256SUMS"), sums).context("writing SHA256SUMS")?;
    println!("dist complete: {}", out.display());
    Ok(())
}

fn completions(shell: Shell, out: &Path) -> anyhow::Result<()> {
    let hfctl = release_dir().join("hfctl");
    if !hfctl.is_file() {
        bail!(
            "hfctl not found at {} — build release first",
            hfctl.display()
        );
    }
    std::fs::create_dir_all(out).context("creating completions dir")?;
    let output = Command::new(&hfctl)
        .arg("completions")
        .arg(shell.hfctl_name())
        .output()
        .context("running hfctl completions")?;
    if !output.status.success() {
        bail!(
            "hfctl completions failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let file_name = match shell {
        Shell::Bash => "hfctl.bash",
        Shell::Zsh => "_hfctl",
        Shell::Fish => "hfctl.fish",
        Shell::PowerShell => "_hfctl.ps1",
        Shell::Elvish => "hfctl.elv",
    };
    let mut f = std::fs::File::create(out.join(file_name)).context("creating completion file")?;
    f.write_all(&output.stdout).context("writing completion")?;
    println!("wrote {}", out.join(file_name).display());
    Ok(())
}

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

/// Lowercase hex without external dependencies.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
