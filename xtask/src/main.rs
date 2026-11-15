//! Rust developer tasks.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};
use sha2::{Digest, Sha256};

/// The checksum file the updater reads beside the portable assets.
const SUMS_FILE: &str = "SHA256SUMS";

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
    /// Build the release binary with the GUI feature.
    Build,
    /// Copy the release binary to `dist/handfast-<target>(.exe)`.
    Portable,
    /// Build the Debian package with `packaging/linux/make-deb.sh`.
    Deb {
        /// Package version. Defaults to the workspace version with `-` as `~`.
        #[arg(long)]
        version: Option<String>,
        /// Minimum glibc version for the Depends line.
        #[arg(long, default_value = "2.36")]
        glibc: String,
        /// Distribution label in the file name.
        #[arg(long, default_value = "debian12")]
        label: String,
        /// Output directory.
        #[arg(long, default_value = "dist")]
        out: PathBuf,
    },
    /// Build the MSI with candle and light from the WiX Toolset.
    Msi {
        /// Output directory.
        #[arg(long, default_value = "dist")]
        out: PathBuf,
    },
    /// Copy the portable binary and build the host's native package.
    Package,
    /// Write `dist/SHA256SUMS` over every other file in `dist/`.
    Checksums,
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
    match task {
        Task::Portable => portable(),
        Task::Deb {
            version,
            glibc,
            label,
            out,
        } => deb(version.as_deref(), glibc, label, out),
        Task::Msi { out } => msi(out),
        Task::Package => portable() && native_package(),
        Task::Checksums => checksums(),
        Task::Check => false,
        _ => run_cargo(task),
    }
}

fn run_cargo(task: &Task) -> bool {
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
                "--",
                "-D",
                "warnings",
            ]);
        }
        Task::Test => {
            command.args(["test", "--workspace", "--all-targets"]);
        }
        Task::Doc => {
            command
                .env("RUSTDOCFLAGS", "-D warnings")
                .args(["doc", "--workspace", "--no-deps"]);
        }
        Task::Build => {
            command.args([
                "build",
                "--release",
                "--locked",
                "-p",
                "handfast-daemon",
                "--features",
                "gui",
            ]);
        }
        Task::Check
        | Task::Portable
        | Task::Deb { .. }
        | Task::Msi { .. }
        | Task::Package
        | Task::Checksums => return false,
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

fn workspace_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn deb(version: Option<&str>, glibc: &str, label: &str, out: &Path) -> bool {
    let version = match version {
        Some(version) => version.to_string(),
        None => workspace_version().replace('-', "~"),
    };
    println!("building the deb package");
    match Command::new("bash")
        .args(["packaging/linux/make-deb.sh", &version, glibc, label])
        .arg(out)
        .status()
    {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("make-deb.sh failed with {status}");
            false
        }
        Err(error) => {
            eprintln!("failed to spawn bash: {error}");
            false
        }
    }
}

fn msi(out: &Path) -> bool {
    let version = workspace_version()
        .split('-')
        .next()
        .unwrap_or(workspace_version());
    let wxs = Path::new("packaging/windows/handfast.wxs");
    let notices = Path::new("THIRD-PARTY-NOTICES.md");
    if !wxs.is_file() || !notices.is_file() {
        eprintln!("run xtask from the repository root");
        return false;
    }
    let binary = Path::new("target")
        .join("release")
        .join(format!("handfast{}", std::env::consts::EXE_SUFFIX));
    if !binary.is_file() {
        eprintln!("{} is missing; run `xtask build` first", binary.display());
        return false;
    }
    let wix_dir = Path::new("target").join("wix");
    for directory in [wix_dir.as_path(), out] {
        if let Err(error) = std::fs::create_dir_all(directory) {
            eprintln!("failed to create {}: {error}", directory.display());
            return false;
        }
    }

    let wixobj = wix_dir.join("handfast.wixobj");
    let package = out.join(format!("handfast-{version}-x64.msi"));
    let candle = wix_tool("candle.exe");
    let light = wix_tool("light.exe");
    let candle_args = [
        "-nologo".to_string(),
        "-arch".to_string(),
        "x64".to_string(),
        "-ext".to_string(),
        "WixUtilExtension".to_string(),
        format!("-dVersion={version}"),
        format!("-dBinaryPath={}", binary.display()),
        format!("-dNoticesPath={}", notices.display()),
        "-out".to_string(),
        wixobj.display().to_string(),
        wxs.display().to_string(),
    ];
    if !run_tool(&candle, &candle_args) {
        return false;
    }
    let light_args = [
        "-nologo".to_string(),
        "-ext".to_string(),
        "WixUtilExtension".to_string(),
        "-out".to_string(),
        package.display().to_string(),
        wixobj.display().to_string(),
    ];
    run_tool(&light, &light_args)
}

fn run_tool(tool: &Path, args: &[String]) -> bool {
    match Command::new(tool).args(args).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("{} failed with {status}", tool.display());
            false
        }
        Err(error) => {
            eprintln!("failed to spawn {}: {error}", tool.display());
            false
        }
    }
}

fn wix_tool(name: &str) -> PathBuf {
    match std::env::var_os("WIX_BIN") {
        Some(directory) if !directory.is_empty() => PathBuf::from(directory).join(name),
        _ => PathBuf::from(name),
    }
}

#[cfg(windows)]
fn native_package() -> bool {
    msi(Path::new("dist"))
}

#[cfg(unix)]
fn native_package() -> bool {
    deb(None, "2.36", "debian12", Path::new("dist"))
}

fn portable() -> bool {
    let Some(target) = rustc_host() else {
        eprintln!("failed to read the host triple from rustc -vV");
        return false;
    };
    let suffix = std::env::consts::EXE_SUFFIX;
    let source = Path::new("target")
        .join("release")
        .join(format!("handfast{suffix}"));
    let dist = Path::new("dist");
    let destination = dist.join(format!("handfast-{target}{suffix}"));
    if let Err(error) = std::fs::create_dir_all(dist) {
        eprintln!("failed to create {}: {error}", dist.display());
        return false;
    }
    match std::fs::copy(&source, &destination) {
        Ok(_) => {
            println!("wrote {}", destination.display());
            true
        }
        Err(error) => {
            eprintln!(
                "failed to copy {} to {}: {error}",
                source.display(),
                destination.display()
            );
            false
        }
    }
}

fn checksums() -> bool {
    let dist = Path::new("dist");
    let mut files = Vec::new();
    if let Err(error) = collect_files(dist, &mut files) {
        eprintln!("failed to read {}: {error}", dist.display());
        return false;
    }
    files.sort();

    let mut seen = HashSet::new();
    let mut text = String::new();
    for path in &files {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            eprintln!("{} has no UTF-8 file name", path.display());
            return false;
        };
        if name == SUMS_FILE {
            continue;
        }
        if !seen.insert(name.to_string()) {
            eprintln!("{name} appears in more than one artifact directory");
            return false;
        }
        let digest = match sha256_file(path) {
            Ok(digest) => digest,
            Err(error) => {
                eprintln!("failed to hash {}: {error}", path.display());
                return false;
            }
        };
        text.push_str(&digest);
        text.push_str("  ");
        text.push_str(name);
        text.push('\n');
    }

    let sums = dist.join(SUMS_FILE);
    match std::fs::write(&sums, text) {
        Ok(()) => {
            println!("wrote {} over {} files", sums.display(), seen.len());
            true
        }
        Err(error) => {
            eprintln!("failed to write {}: {error}", sums.display());
            false
        }
    }
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest.as_slice() {
        text.push(char::from(DIGITS[usize::from(*byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(*byte & 0x0f)]));
    }
    Ok(text)
}

fn rustc_host() -> Option<String> {
    let output = Command::new("rustc").arg("-vV").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().find_map(|line| {
        line.strip_prefix("host: ")
            .map(|host| host.trim().to_string())
    })
}
