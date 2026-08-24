//! `hfctl` command-line surface: clap derive types plus shell-completion
//! generation.
//!
//! Parsing is exercised through [`Cli::try_parse_from`] in unit tests so the
//! whole CLI contract round-trips without spawning processes.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

/// Handfast terminal control: drive the daemon from your terminal.
#[derive(Debug, Parser)]
#[command(
    name = "hfctl",
    version,
    about = "Handfast terminal control: control the Handfast daemon from your terminal",
    long_about = None
)]
pub struct Cli {
    /// Override the daemon IPC socket path (default: see handfast-ipc docs).
    ///
    /// Global: may appear before or after the subcommand.
    #[arg(long, global = true, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Subcommand to run; when omitted the interactive interface starts.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Every hfctl subcommand.
#[derive(Debug, Subcommand)]
#[allow(clippy::enum_variant_names)]
pub enum Command {
    /// Launch the interactive terminal interface (the default).
    Tui,

    /// Connect, print a daemon identity/ping summary, then exit.
    Status,

    /// Print a table of known devices (id, name, type, paired, state).
    Devices,

    /// Start pairing with a discovered device.
    Pair {
        /// Target device identifier.
        device_id: String,
    },

    /// Revoke pairing with a device.
    Unpair {
        /// Target device identifier.
        device_id: String,
    },

    /// Inspect or toggle per-device plugins.
    Plugins {
        /// Plugin sub-action.
        #[command(subcommand)]
        action: PluginAction,
    },

    /// Send a local file to a device.
    Send {
        /// Target device identifier.
        device_id: String,
        /// Path of the local file to transfer.
        file_path: PathBuf,
    },

    /// Inspect or dismiss mirrored notifications.
    Notifications {
        /// Notification sub-action.
        #[command(subcommand)]
        action: NotificationAction,
    },

    /// Read or overwrite the local clipboard text.
    Clipboard {
        /// Clipboard sub-action.
        #[command(subcommand)]
        action: ClipboardAction,
    },

    /// Print a table of known transfers (active and finished).
    Transfers,

    /// Cancel one ongoing transfer by id.
    TransferCancel {
        /// Transfer identifier.
        transfer_id: String,
    },

    /// Read or change the local output volume.
    Volume {
        /// Volume sub-action; omit to read the current volume.
        #[command(subcommand)]
        action: Option<VolumeAction>,
    },

    /// Send an SMS from a paired phone.
    Sms {
        /// Target device identifier.
        device_id: String,
        /// Recipient phone number.
        number: String,
        /// Message body text.
        text: String,
    },

    /// Ask a device for its current battery state.
    Battery {
        /// Target device identifier.
        device_id: String,
    },

    /// Inspect commands executable on a device.
    #[command(name = "runcommand")]
    RunCommand {
        /// Run-command sub-action.
        #[command(subcommand)]
        action: RunCommandAction,
    },

    /// Share local text with a device.
    ShareText {
        /// Target device identifier.
        device_id: String,
        /// Text payload.
        text: String,
    },

    /// Open a URL on a device.
    ShareUrl {
        /// Target device identifier.
        device_id: String,
        /// URL to open.
        url: String,
    },

    /// Listen on the event stream for a 2-second window, then print the last
    /// N log records received during that window and exit. No request is
    /// sent: only passively broadcast `LogRecord` events are captured.
    Logs {
        /// How many trailing records to print.
        #[arg(short = 'n', long, default_value_t = 50, value_name = "N")]
        number: usize,
    },

    /// Emit a completion script for hfctl on stdout.
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: Shell,
    },
}

/// Sub-actions of `hfctl plugins`.
#[derive(Debug, Subcommand)]
pub enum PluginAction {
    /// List plugins and their enabled state for a device.
    List {
        /// Target device identifier.
        device_id: String,
    },
    /// Enable one plugin on a device.
    Enable {
        /// Target device identifier.
        device_id: String,
        /// Plugin identifier.
        plugin: String,
    },
    /// Disable one plugin on a device.
    Disable {
        /// Target device identifier.
        device_id: String,
        /// Plugin identifier.
        plugin: String,
    },
}

/// Sub-actions of `hfctl notifications`.
#[derive(Debug, Subcommand)]
pub enum NotificationAction {
    /// List currently mirrored notifications.
    List,
    /// Dismiss one notification by id.
    Dismiss {
        /// Notification identifier.
        notification_id: String,
    },
}

/// Sub-actions of `hfctl clipboard`.
#[derive(Debug, Subcommand)]
pub enum ClipboardAction {
    /// Print the current clipboard text.
    Get,
    /// Overwrite the clipboard with TEXT.
    Set {
        /// New clipboard content.
        text: String,
    },
}

/// Sub-actions of `hfctl volume`.
#[derive(Debug, Subcommand)]
pub enum VolumeAction {
    /// Print the current output volume.
    Get,
    /// Set the output volume percentage.
    Set {
        /// Desired volume percentage (0-100).
        #[arg(value_parser = clap::value_parser!(u8).range(0..=100))]
        percent: u8,
    },
}

/// Sub-actions of `hfctl runcommand`.
#[derive(Debug, Subcommand)]
pub enum RunCommandAction {
    /// List commands available for a device.
    List {
        /// Target device identifier.
        device_id: String,
    },
}

/// Write the completion script for `shell` to stdout.
///
/// Generation itself cannot fail; only the underlying write can.
///
/// # Errors
/// Propagates stdout write failures.
pub fn generate_completions(shell: Shell) -> std::io::Result<()> {
    let mut command = <Cli as clap::CommandFactory>::command();
    let name = command.get_name().to_owned();
    clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::generate;

    /// Parse `args` as if typed after the binary name.
    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("hfctl").chain(args.iter().copied()))
    }

    #[test]
    fn no_subcommand_defaults_to_tui() {
        let cli = parse(&[]).unwrap();
        assert!(cli.command.is_none());
        assert!(cli.socket.is_none());
    }

    #[test]
    fn explicit_tui_parses() {
        assert!(matches!(
            parse(&["tui"]).unwrap().command,
            Some(Command::Tui)
        ));
    }

    #[test]
    fn socket_override_works_before_and_after_subcommand() {
        let cli = parse(&["--socket", "/run/h.sock", "status"]).unwrap();
        assert_eq!(
            cli.socket.as_deref(),
            Some(std::path::Path::new("/run/h.sock"))
        );
        let cli = parse(&["status", "--socket", "/run/h.sock"]).unwrap();
        assert_eq!(
            cli.socket.as_deref(),
            Some(std::path::Path::new("/run/h.sock"))
        );
        // Socket plus no subcommand still means "launch the TUI".
        let cli = parse(&["--socket", "/run/h.sock"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn pair_unpair_carry_device_ids() {
        let cli = parse(&["pair", "abc123"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Pair { ref device_id }) if device_id == "abc123"
        ));
        let cli = parse(&["unpair", "xyz"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Unpair { ref device_id }) if device_id == "xyz"
        ));
    }

    #[test]
    fn plugin_actions_round_trip() {
        let cli = parse(&["plugins", "list", "dev"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plugins { action: PluginAction::List { ref device_id } })
                if device_id == "dev"
        ));
        let cli = parse(&["plugins", "enable", "dev", "ping"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plugins {
                action: PluginAction::Enable { ref device_id, ref plugin }
            }) if device_id == "dev" && plugin == "ping"
        ));
        let cli = parse(&["plugins", "disable", "dev", "ping"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plugins {
                action: PluginAction::Disable { ref device_id, ref plugin }
            }) if device_id == "dev" && plugin == "ping"
        ));
    }

    #[test]
    fn send_requires_device_and_path() {
        assert!(parse(&["send", "only-device"]).is_err());
        let cli = parse(&["send", "dev", "/tmp/photo.jpg"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Send { ref device_id, .. }) if device_id == "dev"
        ));
    }

    #[test]
    fn notification_actions_round_trip() {
        assert!(matches!(
            parse(&["notifications", "list"]).unwrap().command,
            Some(Command::Notifications {
                action: NotificationAction::List
            })
        ));
        let cli = parse(&["notifications", "dismiss", "n42"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Notifications {
                action: NotificationAction::Dismiss { ref notification_id }
            }) if notification_id == "n42"
        ));
    }

    #[test]
    fn clipboard_set_keeps_spaces() {
        let cli = parse(&["clipboard", "set", "hello world"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Clipboard {
                action: ClipboardAction::Set { ref text }
            }) if text == "hello world"
        ));
        assert!(matches!(
            parse(&["clipboard", "get"]).unwrap().command,
            Some(Command::Clipboard {
                action: ClipboardAction::Get
            })
        ));
    }

    #[test]
    fn logs_default_is_50_and_flag_overrides() {
        let cli = parse(&["logs"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Logs { number: 50 })));
        let cli = parse(&["logs", "-n", "7"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Logs { number: 7 })));
        // `-n` demands a value.
        assert!(parse(&["logs", "-n"]).is_err());
    }

    #[test]
    fn completions_accept_all_shell_names_and_reject_others() {
        for name in ["bash", "zsh", "fish", "powershell", "elvish"] {
            assert!(parse(&["completions", name]).is_ok(), "{name} should parse");
        }
        assert!(parse(&["completions", "tcsh"]).is_err());
        assert!(parse(&["completions"]).is_err());
    }

    #[test]
    fn unknown_subcommands_are_rejected() {
        assert!(parse(&["frobnicate"]).is_err());
    }

    #[test]
    fn transfers_parses() {
        assert!(matches!(
            parse(&["transfers"]).unwrap().command,
            Some(Command::Transfers)
        ));
    }

    #[test]
    fn transfer_cancel_requires_transfer_id() {
        assert!(parse(&["transfer-cancel"]).is_err());
        let cli = parse(&["transfer-cancel", "t-9"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::TransferCancel { ref transfer_id }) if transfer_id == "t-9"
        ));
    }

    #[test]
    fn volume_bare_form_defaults_to_get() {
        let cli = parse(&["volume"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Volume { action: None })
        ));
    }

    #[test]
    fn volume_get_and_set_round_trip() {
        assert!(matches!(
            parse(&["volume", "get"]).unwrap().command,
            Some(Command::Volume {
                action: Some(VolumeAction::Get)
            })
        ));
        let cli = parse(&["volume", "set", "42"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Volume {
                action: Some(VolumeAction::Set { percent: 42 })
            })
        ));
    }

    #[test]
    fn volume_set_rejects_missing_nonnumeric_and_out_of_range() {
        assert!(parse(&["volume", "set"]).is_err());
        assert!(parse(&["volume", "set", "loud"]).is_err());
        assert!(parse(&["volume", "set", "101"]).is_err());
        assert!(parse(&["volume", "shout"]).is_err());
        // Boundary values are accepted.
        assert!(parse(&["volume", "set", "0"]).is_ok());
        assert!(parse(&["volume", "set", "100"]).is_ok());
    }

    #[test]
    fn sms_requires_device_number_and_text() {
        assert!(parse(&["sms"]).is_err());
        assert!(parse(&["sms", "dev"]).is_err());
        assert!(parse(&["sms", "dev", "+15550100"]).is_err());
        let cli = parse(&["sms", "dev", "+15550100", "see you later"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Sms {
                ref device_id,
                ref number,
                ref text
            }) if device_id == "dev" && number == "+15550100" && text == "see you later"
        ));
    }

    #[test]
    fn battery_requires_device_id() {
        assert!(parse(&["battery"]).is_err());
        let cli = parse(&["battery", "dev"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Battery { ref device_id }) if device_id == "dev"
        ));
    }

    #[test]
    fn runcommand_only_supports_list() {
        assert!(parse(&["runcommand"]).is_err());
        assert!(parse(&["runcommand", "list"]).is_err());
        assert!(parse(&["runcommand", "run", "dev"]).is_err());
        let cli = parse(&["runcommand", "list", "dev"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::RunCommand {
                action: RunCommandAction::List { ref device_id }
            }) if device_id == "dev"
        ));
    }

    #[test]
    fn share_text_keeps_spaces() {
        assert!(parse(&["share-text"]).is_err());
        assert!(parse(&["share-text", "only-device"]).is_err());
        let cli = parse(&["share-text", "dev", "a bit of text"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::ShareText {
                ref device_id,
                ref text
            }) if device_id == "dev" && text == "a bit of text"
        ));
    }

    #[test]
    fn share_url_requires_device_and_url() {
        assert!(parse(&["share-url"]).is_err());
        assert!(parse(&["share-url", "only-device"]).is_err());
        let cli = parse(&["share-url", "dev", "https://example.net/x?y=1"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::ShareUrl {
                ref device_id,
                ref url
            }) if device_id == "dev" && url == "https://example.net/x?y=1"
        ));
    }

    #[test]
    fn completions_generate_nonempty_output_for_every_shell() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let mut command = <Cli as clap::CommandFactory>::command();
            let mut buffer = Vec::new();
            generate(shell, &mut command, "hfctl", &mut buffer);
            assert!(!buffer.is_empty(), "{shell:?} produced no completions");
        }
    }
}
