//! Per-user autostart service.
//!
//! The daemon must run inside the user's session: the desktop adapters need the
//! clipboard, the session bus, MPRIS, the PipeWire socket and the user's own
//! state directory. A SYSTEM service or a root systemd unit loses all of them,
//! so the service is a Task Scheduler logon task on Windows and a systemd user
//! unit on Linux.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use handfast_proto::DeviceId;

use crate::control::{
    send_request, ControlError, ControlRequest, ControlResponse, DaemonStatus, RemoteMediaCommand,
};

/// The Task Scheduler task name.
pub const TASK_NAME: &str = "Handfast";

/// The systemd user unit name.
pub const UNIT_NAME: &str = "handfast.service";

/// How long `stop` waits for the control endpoint to stop answering.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);

const STOP_POLL: Duration = Duration::from_millis(100);

/// A snapshot of the installed and running service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceStatus {
    /// Whether the unit or task is registered.
    pub installed: bool,
    /// Whether a daemon answered on the control channel.
    pub running: bool,
    /// The daemon process ID, when running.
    pub pid: Option<u32>,
    /// Whole seconds the daemon has been up, when running.
    pub uptime_secs: Option<u64>,
}

impl std::fmt::Display for ServiceStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(formatter, "installed: {}", self.installed)?;
        writeln!(formatter, "running: {}", self.running)?;
        match (self.pid, self.uptime_secs) {
            (Some(pid), Some(uptime)) => {
                writeln!(formatter, "pid: {pid}")?;
                write!(formatter, "uptime: {uptime}s")
            }
            _ => {
                writeln!(formatter, "pid: -")?;
                write!(formatter, "uptime: -")
            }
        }
    }
}

/// The captured output of one external command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Whether the command exited zero.
    pub success: bool,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

/// Runs the platform service manager.
pub trait CommandRunner {
    /// Run `program` with `args` and capture its output.
    ///
    /// # Errors
    ///
    /// Fails when the program cannot be started.
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput>;
}

/// The real runner, which spawns the platform service manager.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemCommands;

impl CommandRunner for SystemCommands {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
        let output = Command::new(program).args(args).output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Errors from the service commands.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// A filesystem or process operation failed.
    #[error("service io: {0}")]
    Io(#[from] io::Error),

    /// The service manager rejected a command.
    #[error("{command} failed: {message}")]
    Command {
        /// The command that failed.
        command: String,
        /// The manager's message.
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cmd {
    program: &'static str,
    args: Vec<String>,
}

impl Cmd {
    fn new(program: &'static str, args: &[&str]) -> Self {
        Self {
            program,
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }

    fn display(&self) -> String {
        let mut text = self.program.to_string();
        for arg in &self.args {
            text.push(' ');
            text.push_str(arg);
        }
        text
    }
}

/// The same-user control channel, as the service commands need it.
pub trait DaemonControl {
    /// Read one status snapshot.
    ///
    /// # Errors
    ///
    /// Fails when no daemon answers or the response is not a status.
    fn status(&self) -> Result<DaemonStatus, ControlError>;

    /// Ask the daemon to shut down through its bounded path.
    ///
    /// # Errors
    ///
    /// Fails when no daemon answers or the response is not a shutdown.
    fn shutdown(&self) -> Result<(), ControlError>;
}

/// The real control channel, over the same-user socket or pipe.
///
/// The client owns a current-thread runtime so the service commands stay
/// synchronous. Call it from outside an async context.
pub struct LocalControl {
    data_dir: PathBuf,
    runtime: tokio::runtime::Runtime,
    request_timeout: Duration,
}

impl LocalControl {
    /// Build a control client for `data_dir`.
    ///
    /// # Errors
    ///
    /// Fails when the runtime cannot be built.
    pub fn new(data_dir: PathBuf) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self {
            data_dir,
            runtime,
            request_timeout: STOP_TIMEOUT,
        })
    }

    fn request(&self, request: &ControlRequest) -> Result<ControlResponse, ControlError> {
        self.runtime.block_on(async {
            match tokio::time::timeout(self.request_timeout, send_request(&self.data_dir, request))
                .await
            {
                Ok(result) => result,
                Err(_) => Err(ControlError::Timeout("reply")),
            }
        })
    }

    /// Answer one pending pairing request.
    ///
    /// # Errors
    ///
    /// Fails when no daemon answers, the request is unknown or answered, the
    /// fingerprint no longer matches the open prompt, or the response is not an
    /// acknowledgement.
    pub fn pairing_decision(
        &self,
        device_id: &DeviceId,
        accept: bool,
        fingerprint: &str,
    ) -> Result<(), ControlError> {
        let request = ControlRequest::PairingDecision {
            device_id: device_id.as_str().to_string(),
            accept,
            fingerprint: fingerprint.to_string(),
        };
        match self.request(&request)? {
            ControlResponse::Decided => Ok(()),
            ControlResponse::Err { message } => Err(ControlError::Remote(message)),
            ControlResponse::Ok { .. }
            | ControlResponse::Status { .. }
            | ControlResponse::ShuttingDown
            | ControlResponse::Commands { .. }
            | ControlResponse::Contacts { .. }
            | ControlResponse::SmsConversations { .. }
            | ControlResponse::SmsThread { .. }
            | ControlResponse::SftpEntries { .. } => Err(ControlError::Remote(
                "unexpected control response".to_string(),
            )),
        }
    }

    /// Ask a connected device to ring.
    ///
    /// # Errors
    ///
    /// Fails when no daemon answers, the device is not connected, or the
    /// response is not an acknowledgement.
    pub fn ring(&self, device_id: &DeviceId) -> Result<(), ControlError> {
        let request = ControlRequest::Ring {
            device_id: device_id.as_str().to_string(),
        };
        match self.request(&request)? {
            ControlResponse::Ok { .. } => Ok(()),
            ControlResponse::Err { message } => Err(ControlError::Remote(message)),
            ControlResponse::Status { .. }
            | ControlResponse::ShuttingDown
            | ControlResponse::Decided
            | ControlResponse::Commands { .. }
            | ControlResponse::Contacts { .. }
            | ControlResponse::SmsConversations { .. }
            | ControlResponse::SmsThread { .. }
            | ControlResponse::SftpEntries { .. } => Err(ControlError::Remote(
                "unexpected control response".to_string(),
            )),
        }
    }

    /// Change one audio sink on a connected device.
    ///
    /// # Errors
    ///
    /// Fails when no daemon answers, the device is not connected or has not
    /// reported the sink, the percent is over 100, both or neither of `percent`
    /// and `muted` are set, or the response is not an acknowledgement.
    pub fn set_remote_volume(
        &self,
        device_id: &DeviceId,
        sink: &str,
        percent: Option<u8>,
        muted: Option<bool>,
    ) -> Result<(), ControlError> {
        let request = ControlRequest::RemoteVolume {
            device_id: device_id.as_str().to_string(),
            sink: sink.to_string(),
            percent,
            muted,
        };
        match self.request(&request)? {
            ControlResponse::Ok { .. } => Ok(()),
            ControlResponse::Err { message } => Err(ControlError::Remote(message)),
            ControlResponse::Status { .. }
            | ControlResponse::ShuttingDown
            | ControlResponse::Decided
            | ControlResponse::Commands { .. }
            | ControlResponse::Contacts { .. }
            | ControlResponse::SmsConversations { .. }
            | ControlResponse::SmsThread { .. }
            | ControlResponse::SftpEntries { .. } => Err(ControlError::Remote(
                "unexpected control response".to_string(),
            )),
        }
    }

    /// Control one media player on a connected device.
    ///
    /// # Errors
    ///
    /// Fails when no daemon answers, the device is not connected or has not
    /// reported the player, the command is invalid, or the response is not an
    /// acknowledgement.
    pub fn media(
        &self,
        device_id: &DeviceId,
        command: RemoteMediaCommand,
    ) -> Result<(), ControlError> {
        let request = ControlRequest::Media {
            device_id: device_id.as_str().to_string(),
            command,
        };
        match self.request(&request)? {
            ControlResponse::Ok { .. } => Ok(()),
            ControlResponse::Err { message } => Err(ControlError::Remote(message)),
            ControlResponse::Status { .. }
            | ControlResponse::ShuttingDown
            | ControlResponse::Decided
            | ControlResponse::Commands { .. }
            | ControlResponse::Contacts { .. }
            | ControlResponse::SmsConversations { .. }
            | ControlResponse::SmsThread { .. }
            | ControlResponse::SftpEntries { .. } => Err(ControlError::Remote(
                "unexpected control response".to_string(),
            )),
        }
    }
}

impl DaemonControl for LocalControl {
    fn status(&self) -> Result<DaemonStatus, ControlError> {
        match self.request(&ControlRequest::Status)? {
            ControlResponse::Status { status } => Ok(*status),
            ControlResponse::Err { message } => Err(ControlError::Remote(message)),
            ControlResponse::Ok { .. }
            | ControlResponse::ShuttingDown
            | ControlResponse::Decided
            | ControlResponse::Commands { .. }
            | ControlResponse::Contacts { .. }
            | ControlResponse::SmsConversations { .. }
            | ControlResponse::SmsThread { .. }
            | ControlResponse::SftpEntries { .. } => Err(ControlError::Remote(
                "unexpected control response".to_string(),
            )),
        }
    }

    fn shutdown(&self) -> Result<(), ControlError> {
        match self.request(&ControlRequest::Shutdown)? {
            ControlResponse::ShuttingDown => Ok(()),
            ControlResponse::Err { message } => Err(ControlError::Remote(message)),
            ControlResponse::Ok { .. }
            | ControlResponse::Status { .. }
            | ControlResponse::Decided
            | ControlResponse::Commands { .. }
            | ControlResponse::Contacts { .. }
            | ControlResponse::SmsConversations { .. }
            | ControlResponse::SmsThread { .. }
            | ControlResponse::SftpEntries { .. } => Err(ControlError::Remote(
                "unexpected control response".to_string(),
            )),
        }
    }
}

/// The per-user autostart service for one executable and data directory.
pub struct Service<'a> {
    exe: PathBuf,
    data_dir: PathBuf,
    log: PathBuf,
    #[cfg(any(unix, test))]
    unit_path: PathBuf,
    runner: &'a dyn CommandRunner,
    stop_timeout: Duration,
}

impl<'a> Service<'a> {
    /// Describe the service for `exe`, `data_dir` and the daemon `log`.
    ///
    /// Relative paths are resolved against the current directory, so the unit
    /// and the task never depend on the working directory at logon.
    ///
    /// # Errors
    ///
    /// Fails when a path cannot be made absolute.
    pub fn new(
        exe: PathBuf,
        data_dir: PathBuf,
        log: PathBuf,
        runner: &'a dyn CommandRunner,
    ) -> io::Result<Self> {
        Ok(Self {
            exe: std::path::absolute(exe)?,
            data_dir: std::path::absolute(data_dir)?,
            log: std::path::absolute(log)?,
            #[cfg(any(unix, test))]
            unit_path: unit_path(),
            runner,
            stop_timeout: STOP_TIMEOUT,
        })
    }

    /// Write the unit or task, enable it and start it.
    ///
    /// Returns the lines to print. A second run leaves the same state.
    ///
    /// # Errors
    ///
    /// Fails when the unit file or the task definition cannot be written, when
    /// the service manager rejects a command, or when the task cannot be
    /// started.
    pub fn install(&self, control: &dyn DaemonControl) -> Result<Vec<String>, ServiceError> {
        #[cfg(unix)]
        let _ = control;
        #[cfg(unix)]
        return self.install_unit();
        #[cfg(windows)]
        return self.install_task(control);
    }

    /// Stop, disable and remove the unit or task.
    ///
    /// A service that is not installed counts as success.
    ///
    /// # Errors
    ///
    /// Fails when the unit file cannot be removed or the service manager
    /// rejects a command.
    pub fn uninstall(&self) -> Result<(), ServiceError> {
        #[cfg(unix)]
        return self.uninstall_unit();
        #[cfg(windows)]
        return self.uninstall_task();
    }

    /// Start the unit or task. A daemon that already answers is success.
    ///
    /// # Errors
    ///
    /// Fails when the service manager rejects the start command.
    pub fn start(&self, control: &dyn DaemonControl) -> Result<(), ServiceError> {
        #[cfg(unix)]
        return self.start_with(control, &unix_start_command());
        #[cfg(windows)]
        return self.start_with(control, &windows_run_task());
    }

    fn start_with(&self, control: &dyn DaemonControl, command: &Cmd) -> Result<(), ServiceError> {
        if control.status().is_ok() {
            return Ok(());
        }
        self.run_checked(command)?;
        Ok(())
    }

    /// Send `Shutdown`, wait for the endpoint to stop answering, then fall
    /// back to the platform stop command.
    ///
    /// A daemon that is not running counts as success. A daemon that rejects
    /// the request or does not reply is stopped by the fallback, and a failed
    /// fallback is an error.
    ///
    /// # Errors
    ///
    /// Fails when the fallback command is rejected.
    pub fn stop(&self, control: &dyn DaemonControl) -> Result<(), ServiceError> {
        match control.shutdown() {
            Ok(()) => {}
            Err(ControlError::NotRunning) => return Ok(()),
            Err(_) => return self.stop_service(),
        }
        let deadline = Instant::now() + self.stop_timeout;
        loop {
            if matches!(control.status(), Err(ControlError::NotRunning)) {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep(STOP_POLL.min(deadline - now));
        }
        self.stop_service()
    }

    fn stop_service(&self) -> Result<(), ServiceError> {
        #[cfg(unix)]
        self.run_checked(&unix_stop_command())?;
        #[cfg(windows)]
        self.run_checked(&windows_end_task())?;
        Ok(())
    }

    /// Read the installed and running state.
    #[must_use]
    pub fn status(&self, control: &dyn DaemonControl) -> ServiceStatus {
        let installed = self.is_installed();
        match control.status() {
            Ok(status) => ServiceStatus {
                installed,
                running: true,
                pid: Some(status.pid),
                uptime_secs: Some(status.uptime_secs),
            },
            Err(_) => ServiceStatus {
                installed,
                running: false,
                pid: None,
                uptime_secs: None,
            },
        }
    }

    fn is_installed(&self) -> bool {
        #[cfg(unix)]
        return self.installed_unix();
        #[cfg(windows)]
        return self.installed_windows();
    }

    #[cfg(any(unix, test))]
    fn install_unit(&self) -> Result<Vec<String>, ServiceError> {
        self.write_unit(&unit_file(&self.exe, &self.data_dir, &self.log))?;
        self.run_checked(&unix_daemon_reload())?;
        self.run_checked(&unix_enable_unit())?;
        Ok(vec![format!("installed {}", self.unit_path.display())])
    }

    #[cfg(any(unix, test))]
    fn uninstall_unit(&self) -> Result<(), ServiceError> {
        if self.installed_unix() {
            self.run_checked(&unix_disable_unit())?;
            self.run_checked(&unix_stop_command())?;
            if self.unit_path.is_file() {
                std::fs::remove_file(&self.unit_path)?;
            }
        }
        self.run_checked(&unix_daemon_reload())?;
        Ok(())
    }

    #[cfg(any(unix, test))]
    fn installed_unix(&self) -> bool {
        // A packaged install ships the unit under /usr/lib/systemd/user.
        self.unit_path.is_file() || self.succeeds("systemctl", &["--user", "cat", UNIT_NAME])
    }

    #[cfg(windows)]
    fn install_task(&self, control: &dyn DaemonControl) -> Result<Vec<String>, ServiceError> {
        let sid = handfast_desktop::winacl::current_user_sid_string()?;
        self.install_task_with_sid(&sid, control)
    }

    #[cfg(windows)]
    fn uninstall_task(&self) -> Result<(), ServiceError> {
        let sid = handfast_desktop::winacl::current_user_sid_string()?;
        self.uninstall_task_with_sid(&sid)
    }

    #[cfg(any(windows, test))]
    fn install_task_with_sid(
        &self,
        sid: &str,
        control: &dyn DaemonControl,
    ) -> Result<Vec<String>, ServiceError> {
        use std::sync::atomic::{AtomicU64, Ordering};

        // Concurrent installs must not share one definition file.
        static TASK_FILE: AtomicU64 = AtomicU64::new(0);
        let nonce = TASK_FILE.fetch_add(1, Ordering::Relaxed);
        let xml = task_xml(&self.exe, &self.data_dir, &self.log, sid);
        let temp =
            std::env::temp_dir().join(format!("handfast-task-{}-{nonce}.xml", std::process::id()));
        write_task_xml(&temp, &xml)?;
        let created = self.run_checked(&windows_create_task(&temp));
        let _ = std::fs::remove_file(&temp);
        created?;
        self.start_with(control, &windows_run_task())?;

        let mut notes = vec![format!("installed task {TASK_NAME}")];
        let rule = firewall_rule_name(sid);
        if self.is_elevated() {
            self.run_ignored(&firewall_delete_command(&rule));
            if self
                .run_checked(&firewall_add_command(&rule, &self.exe))
                .is_ok()
            {
                notes.push(format!("installed firewall rule {rule}"));
            } else {
                notes.push(format!(
                    "could not install the Windows firewall rule; add it as an administrator with:\n{}",
                    firewall_command(&self.exe, &rule)
                ));
            }
        } else {
            let name = format!("name={rule}");
            if !self.succeeds("netsh", &["advfirewall", "firewall", "show", "rule", &name]) {
                notes.push(format!(
                    "the Windows firewall rule is missing; add it as an administrator with:\n{}",
                    firewall_command(&self.exe, &rule)
                ));
            }
        }
        Ok(notes)
    }

    #[cfg(any(windows, test))]
    fn uninstall_task_with_sid(&self, sid: &str) -> Result<(), ServiceError> {
        if self.installed_windows() {
            self.run_ignored(&windows_end_task());
            self.run_checked(&windows_delete_task())?;
        }
        if self.is_elevated() {
            self.run_ignored(&firewall_delete_command(&firewall_rule_name(sid)));
        }
        Ok(())
    }

    #[cfg(any(windows, test))]
    fn installed_windows(&self) -> bool {
        self.succeeds("schtasks", &["/Query", "/TN", TASK_NAME])
    }

    #[cfg(any(unix, test))]
    fn write_unit(&self, content: &str) -> Result<(), ServiceError> {
        use std::sync::atomic::{AtomicU64, Ordering};

        if std::fs::read_to_string(&self.unit_path).is_ok_and(|existing| existing == content) {
            return Ok(());
        }
        let Some(parent) = self.unit_path.parent() else {
            return Err(ServiceError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has no parent directory", self.unit_path.display()),
            )));
        };
        std::fs::create_dir_all(parent)?;
        static UNIT_FILE: AtomicU64 = AtomicU64::new(0);
        let nonce = UNIT_FILE.fetch_add(1, Ordering::Relaxed);
        let temp = self
            .unit_path
            .with_file_name(format!("{UNIT_NAME}.{}-{nonce}.tmp", std::process::id()));
        std::fs::write(&temp, content)?;
        std::fs::rename(&temp, &self.unit_path)?;
        Ok(())
    }

    fn run(&self, command: &Cmd) -> io::Result<CommandOutput> {
        let args: Vec<&str> = command.args.iter().map(String::as_str).collect();
        self.runner.run(command.program, &args)
    }

    fn run_checked(&self, command: &Cmd) -> Result<CommandOutput, ServiceError> {
        let output = self.run(command)?;
        if output.success {
            return Ok(output);
        }
        let message = if output.stderr.trim().is_empty() {
            output.stdout.trim().to_string()
        } else {
            output.stderr.trim().to_string()
        };
        Err(ServiceError::Command {
            command: command.display(),
            message,
        })
    }

    #[cfg(any(windows, test))]
    fn run_ignored(&self, command: &Cmd) {
        let _ = self.run(command);
    }

    fn succeeds(&self, program: &str, args: &[&str]) -> bool {
        self.runner
            .run(program, args)
            .map(|output| output.success)
            .unwrap_or(false)
    }

    #[cfg(any(windows, test))]
    fn is_elevated(&self) -> bool {
        self.succeeds("net", &["session"])
    }
}

/// Render the systemd user unit for `exe`, `data_dir` and `log`.
#[must_use]
pub fn unit_file(exe: &Path, data_dir: &Path, log: &Path) -> String {
    format!(
        "\
[Unit]
Description=Handfast KDE Connect daemon
Documentation=https://github.com/mheci/handfast
After=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=simple
ExecStart={} daemon --data-dir {} --log-file {}
Restart=on-failure
RestartSec=5
NoNewPrivileges=yes
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6

[Install]
WantedBy=default.target
",
        systemd_quote(exe),
        systemd_quote(data_dir),
        systemd_quote(log)
    )
}

/// Render the logon task definition for `exe`, `data_dir`, `log` and `sid`.
#[must_use]
pub fn task_xml(exe: &Path, data_dir: &Path, log: &Path, sid: &str) -> String {
    let arguments = format!(
        "daemon --data-dir \"{}\" --log-file \"{}\"",
        data_dir.display(),
        log.display()
    );
    format!(
        "\
<?xml version=\"1.0\" encoding=\"UTF-16\"?>
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id=\"Author\">
      <UserId>{}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context=\"Author\">
    <Exec>
      <Command>{}</Command>
      <Arguments>{}</Arguments>
    </Exec>
  </Actions>
</Task>
",
        xml_escape(sid),
        xml_escape(sid),
        xml_escape(&exe.to_string_lossy()),
        xml_escape(&arguments)
    )
}

/// Write a task definition as UTF-16 with a byte-order mark.
///
/// Task Scheduler exports its own definitions in that shape, so
/// `schtasks /Create /XML` receives the same bytes it writes.
///
/// # Errors
///
/// Fails when the file cannot be written.
pub fn write_task_xml(path: &Path, xml: &str) -> io::Result<()> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(path, bytes)
}

/// Stable firewall rule name for one user.
///
/// The name carries the user's SID, so two users never share a rule and an MSI
/// install and a later elevated `service install` by the same user address the
/// same rule. Windows Installer exposes that SID as its `UserSID` property,
/// which is what the MSI's own rule actions use.
#[must_use]
pub fn firewall_rule_name(sid: &str) -> String {
    format!("Handfast-{sid}")
}

#[cfg(any(windows, test))]
fn firewall_add_command(rule: &str, exe: &Path) -> Cmd {
    Cmd {
        program: "netsh",
        args: vec![
            "advfirewall".to_string(),
            "firewall".to_string(),
            "add".to_string(),
            "rule".to_string(),
            format!("name={rule}"),
            "dir=in".to_string(),
            "action=allow".to_string(),
            format!("program={}", exe.display()),
            "enable=yes".to_string(),
        ],
    }
}

#[cfg(any(windows, test))]
fn firewall_delete_command(rule: &str) -> Cmd {
    Cmd {
        program: "netsh",
        args: vec![
            "advfirewall".to_string(),
            "firewall".to_string(),
            "delete".to_string(),
            "rule".to_string(),
            format!("name={rule}"),
        ],
    }
}

/// The administrator command that opens the Windows firewall for `exe`.
#[must_use]
pub fn firewall_command(exe: &Path, rule: &str) -> String {
    format!(
        "netsh advfirewall firewall add rule name=\"{}\" dir=in action=allow program=\"{}\" enable=yes",
        rule,
        exe.display()
    )
}

#[cfg(any(unix, test))]
fn unit_path() -> PathBuf {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        Some(config) => PathBuf::from(config),
        None => config_home(),
    };
    config.join("systemd/user").join(UNIT_NAME)
}

#[cfg(any(unix, test))]
fn config_home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".config"),
        |home| PathBuf::from(home).join(".config"),
    )
}

fn systemd_quote(path: &Path) -> String {
    let text = path.display().to_string().replace('%', "%%");
    let needs_quotes = text
        .chars()
        .any(|character| character.is_whitespace() || character == '"' || character == '\'');
    if needs_quotes {
        format!("\"{text}\"")
    } else {
        text
    }
}

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(any(unix, test))]
fn unix_daemon_reload() -> Cmd {
    Cmd::new("systemctl", &["--user", "daemon-reload"])
}

#[cfg(any(unix, test))]
fn unix_enable_unit() -> Cmd {
    Cmd::new("systemctl", &["--user", "enable", "--now", UNIT_NAME])
}

#[cfg(any(unix, test))]
fn unix_disable_unit() -> Cmd {
    Cmd::new("systemctl", &["--user", "disable", UNIT_NAME])
}

#[cfg(any(unix, test))]
fn unix_start_command() -> Cmd {
    Cmd::new("systemctl", &["--user", "start", UNIT_NAME])
}

#[cfg(any(unix, test))]
fn unix_stop_command() -> Cmd {
    Cmd::new("systemctl", &["--user", "stop", UNIT_NAME])
}

#[cfg(any(windows, test))]
fn windows_create_task(xml: &Path) -> Cmd {
    let xml = xml.to_string_lossy().into_owned();
    Cmd::new(
        "schtasks",
        &["/Create", "/XML", &xml, "/TN", TASK_NAME, "/F"],
    )
}

#[cfg(any(windows, test))]
fn windows_end_task() -> Cmd {
    Cmd::new("schtasks", &["/End", "/TN", TASK_NAME])
}

#[cfg(any(windows, test))]
fn windows_delete_task() -> Cmd {
    Cmd::new("schtasks", &["/Delete", "/TN", TASK_NAME, "/F"])
}

#[cfg(any(windows, test))]
fn windows_run_task() -> Cmd {
    Cmd::new("schtasks", &["/Run", "/TN", TASK_NAME])
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use crate::control::{IdentitySummary, PairedDevice, TransferSummary};

    #[derive(Default)]
    struct FakeCommands {
        calls: RefCell<Vec<(String, Vec<String>)>>,
        failing: Vec<&'static str>,
    }

    impl FakeCommands {
        fn failing(prefixes: &[&'static str]) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                failing: prefixes.to_vec(),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.borrow().clone()
        }
    }

    impl CommandRunner for FakeCommands {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
            let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
            let line = format!("{program} {}", args.join(" "));
            let success = !self.failing.iter().any(|prefix| line.starts_with(prefix));
            self.calls.borrow_mut().push((program.to_string(), args));
            Ok(CommandOutput {
                success,
                stdout: String::new(),
                stderr: if success {
                    String::new()
                } else {
                    format!("{line} failed")
                },
            })
        }
    }

    #[derive(Clone, Copy)]
    enum Reply {
        Answer,
        NotRunning,
        Timeout,
        Rejected,
    }

    impl Reply {
        fn status(self) -> Result<DaemonStatus, ControlError> {
            match self {
                Reply::Answer => Ok(sample_status()),
                Reply::NotRunning => Err(ControlError::NotRunning),
                Reply::Timeout => Err(ControlError::Timeout("reply")),
                Reply::Rejected => Err(ControlError::Remote("rejected".to_string())),
            }
        }

        fn shutdown(self) -> Result<(), ControlError> {
            match self {
                Reply::Answer => Ok(()),
                Reply::NotRunning => Err(ControlError::NotRunning),
                Reply::Timeout => Err(ControlError::Timeout("reply")),
                Reply::Rejected => Err(ControlError::Remote("rejected".to_string())),
            }
        }
    }

    struct FakeControl {
        status_replies: RefCell<VecDeque<Reply>>,
        shutdown_reply: Reply,
        shutdowns: Cell<usize>,
    }

    impl FakeControl {
        fn answering(answers: &[Reply]) -> Self {
            Self {
                status_replies: RefCell::new(answers.iter().copied().collect()),
                shutdown_reply: Reply::Answer,
                shutdowns: Cell::new(0),
            }
        }

        fn stopped() -> Self {
            Self {
                status_replies: RefCell::new(VecDeque::new()),
                shutdown_reply: Reply::NotRunning,
                shutdowns: Cell::new(0),
            }
        }

        fn refusing() -> Self {
            Self {
                status_replies: RefCell::new(VecDeque::new()),
                shutdown_reply: Reply::Rejected,
                shutdowns: Cell::new(0),
            }
        }

        fn timing_out() -> Self {
            Self {
                status_replies: RefCell::new(VecDeque::new()),
                shutdown_reply: Reply::Timeout,
                shutdowns: Cell::new(0),
            }
        }
    }

    impl DaemonControl for FakeControl {
        fn status(&self) -> Result<DaemonStatus, ControlError> {
            self.status_replies
                .borrow_mut()
                .pop_front()
                .unwrap_or(Reply::NotRunning)
                .status()
        }

        fn shutdown(&self) -> Result<(), ControlError> {
            self.shutdowns.set(self.shutdowns.get() + 1);
            self.shutdown_reply.shutdown()
        }
    }

    fn sample_status() -> DaemonStatus {
        DaemonStatus {
            version: "0.2.0".to_string(),
            pid: 4242,
            uptime_secs: 90,
            identity: IdentitySummary {
                device_id: handfast_proto::DeviceId::new("0123456789abcdef0123456789abcdef")
                    .unwrap(),
                device_name: "test desktop".to_string(),
                device_type: "desktop".to_string(),
                fingerprint: "00".to_string(),
                tcp_port: 1716,
            },
            incoming_dir: PathBuf::from("/incoming"),
            connected: Vec::new(),
            paired: Vec::<PairedDevice>::new(),
            pending_pairings: Vec::new(),
            transfers: Vec::<TransferSummary>::new(),
            remote_sinks: Vec::new(),
            remote_players: Vec::new(),
            remote_commands: Vec::new(),
            truncated: false,
        }
    }

    fn service_for<'a>(
        exe: &str,
        data_dir: &Path,
        unit_path: PathBuf,
        runner: &'a FakeCommands,
    ) -> Service<'a> {
        Service {
            exe: PathBuf::from(exe),
            data_dir: data_dir.to_path_buf(),
            log: data_dir.join("handfast.log"),
            unit_path,
            runner,
            stop_timeout: Duration::from_millis(1),
        }
    }

    fn read_task_xml(path: &Path) -> String {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "a UTF-16 little-endian mark");
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn service_resolves_relative_paths() {
        let runner = FakeCommands::default();
        let service = Service::new(
            PathBuf::from("handfast"),
            PathBuf::from("data"),
            PathBuf::from("data/handfast.log"),
            &runner,
        )
        .unwrap();

        assert!(service.exe.is_absolute(), "{}", service.exe.display());
        assert!(
            service.data_dir.is_absolute(),
            "{}",
            service.data_dir.display()
        );
        assert!(service.log.is_absolute(), "{}", service.log.display());
    }

    #[test]
    fn unit_file_names_the_daemon_data_and_log_paths() {
        let unit = unit_file(
            Path::new("/opt/handfast/handfast"),
            Path::new("/home/me/.local/share/handfast"),
            Path::new("/home/me/.local/share/handfast/handfast.log"),
        );
        assert_eq!(
            unit,
            r#"[Unit]
Description=Handfast KDE Connect daemon
Documentation=https://github.com/mheci/handfast
After=graphical-session.target
PartOf=graphical-session.target

[Service]
Type=simple
ExecStart=/opt/handfast/handfast daemon --data-dir /home/me/.local/share/handfast --log-file /home/me/.local/share/handfast/handfast.log
Restart=on-failure
RestartSec=5
NoNewPrivileges=yes
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6

[Install]
WantedBy=default.target
"#
        );
    }

    #[test]
    fn unit_file_quotes_a_path_with_spaces() {
        let unit = unit_file(
            Path::new("/opt/my apps/handfast"),
            Path::new("/home/me/My Data"),
            Path::new("/home/me/My Data/handfast.log"),
        );
        assert!(
            unit.contains(
                "ExecStart=\"/opt/my apps/handfast\" daemon --data-dir \"/home/me/My Data\" --log-file \"/home/me/My Data/handfast.log\""
            ),
            "{unit}"
        );
    }

    #[test]
    fn unit_file_escapes_percent_specifiers() {
        let unit = unit_file(
            Path::new("/opt/100%/handfast"),
            Path::new("/home/me/My Data%"),
            Path::new("/home/me/My Data%/handfast.log"),
        );
        assert!(
            unit.contains(
                "ExecStart=/opt/100%%/handfast daemon --data-dir \"/home/me/My Data%%\" --log-file \"/home/me/My Data%%/handfast.log\""
            ),
            "{unit}"
        );
    }

    #[test]
    fn task_xml_carries_the_logon_contract() {
        let xml = task_xml(
            Path::new(r"C:\Users\me\AppData\Local\Programs\Handfast\handfast.exe"),
            Path::new(r"C:\Users\me\AppData\Local\handfast"),
            Path::new(r"C:\Users\me\AppData\Local\handfast\handfast.log"),
            "S-1-5-21-1-2-3-1001",
        );
        assert_eq!(
            xml,
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>S-1-5-21-1-2-3-1001</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>S-1-5-21-1-2-3-1001</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>C:\Users\me\AppData\Local\Programs\Handfast\handfast.exe</Command>
      <Arguments>daemon --data-dir &quot;C:\Users\me\AppData\Local\handfast&quot; --log-file &quot;C:\Users\me\AppData\Local\handfast\handfast.log&quot;</Arguments>
    </Exec>
  </Actions>
</Task>
"#
        );
    }

    #[test]
    fn task_xml_escapes_markup() {
        let xml = task_xml(
            Path::new(r"C:\Tools & More's\handfast.exe"),
            Path::new(r"C:\Data <1>"),
            Path::new(r"C:\Data <1>\handfast.log"),
            "S-1-5-21-1-2-3-1001",
        );
        assert!(
            xml.contains(r"<Command>C:\Tools &amp; More&apos;s\handfast.exe</Command>"),
            "{xml}"
        );
        assert!(
            xml.contains(r#"<Arguments>daemon --data-dir &quot;C:\Data &lt;1&gt;&quot; --log-file &quot;C:\Data &lt;1&gt;\handfast.log&quot;</Arguments>"#),
            "{xml}"
        );
    }

    #[test]
    fn task_xml_file_is_utf16_with_a_mark() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("handfast-task.xml");
        let xml = task_xml(
            Path::new(r"C:\Handfast\handfast.exe"),
            Path::new(r"C:\Handfast\data"),
            Path::new(r"C:\Handfast\data\handfast.log"),
            "S-1-5-21-1-2-3-1001",
        );
        write_task_xml(&path, &xml).unwrap();
        assert_eq!(read_task_xml(&path), xml);
    }

    #[cfg(any(unix, test))]
    #[test]
    fn unix_commands_use_the_user_manager() {
        assert_eq!(
            unix_daemon_reload(),
            Cmd::new("systemctl", &["--user", "daemon-reload"])
        );
        assert_eq!(
            unix_enable_unit(),
            Cmd::new("systemctl", &["--user", "enable", "--now", UNIT_NAME])
        );
        assert_eq!(
            unix_disable_unit(),
            Cmd::new("systemctl", &["--user", "disable", UNIT_NAME])
        );
        assert_eq!(
            unix_start_command(),
            Cmd::new("systemctl", &["--user", "start", UNIT_NAME])
        );
        assert_eq!(
            unix_stop_command(),
            Cmd::new("systemctl", &["--user", "stop", UNIT_NAME])
        );
    }

    #[cfg(any(windows, test))]
    #[test]
    fn windows_commands_use_schtasks() {
        assert_eq!(
            windows_create_task(Path::new(r"C:\Temp\task.xml")),
            Cmd::new(
                "schtasks",
                &[
                    "/Create",
                    "/XML",
                    r"C:\Temp\task.xml",
                    "/TN",
                    TASK_NAME,
                    "/F"
                ]
            )
        );
        assert_eq!(
            windows_end_task(),
            Cmd::new("schtasks", &["/End", "/TN", TASK_NAME])
        );
        assert_eq!(
            windows_delete_task(),
            Cmd::new("schtasks", &["/Delete", "/TN", TASK_NAME, "/F"])
        );
        assert_eq!(
            windows_run_task(),
            Cmd::new("schtasks", &["/Run", "/TN", TASK_NAME])
        );
    }

    #[cfg(any(unix, test))]
    #[test]
    fn install_writes_the_unit_and_reloads() {
        let directory = tempfile::tempdir().unwrap();
        let unit_path = directory.path().join("systemd/user").join(UNIT_NAME);
        let runner = FakeCommands::default();
        let data_dir = directory.path().join("data");
        let service = service_for(
            "/opt/handfast/handfast",
            &data_dir,
            unit_path.clone(),
            &runner,
        );

        let notes = service.install_unit().unwrap();

        assert_eq!(
            std::fs::read_to_string(&unit_path).unwrap(),
            unit_file(
                Path::new("/opt/handfast/handfast"),
                &data_dir,
                &data_dir.join("handfast.log")
            ),
            "the unit carries the resolved paths"
        );
        assert_eq!(
            runner.calls(),
            vec![
                (
                    "systemctl".to_string(),
                    vec!["--user".to_string(), "daemon-reload".to_string()]
                ),
                (
                    "systemctl".to_string(),
                    vec![
                        "--user".to_string(),
                        "enable".to_string(),
                        "--now".to_string(),
                        UNIT_NAME.to_string()
                    ]
                ),
            ]
        );
        assert_eq!(notes, vec![format!("installed {}", unit_path.display())]);
    }

    #[cfg(any(unix, test))]
    #[test]
    fn install_twice_leaves_one_unit_and_the_same_state() {
        let directory = tempfile::tempdir().unwrap();
        let unit_path = directory.path().join("systemd/user").join(UNIT_NAME);
        let runner = FakeCommands::default();
        let data_dir = directory.path().join("data");
        let service = service_for(
            "/opt/handfast/handfast",
            &data_dir,
            unit_path.clone(),
            &runner,
        );

        service.install_unit().unwrap();
        let first = std::fs::read_to_string(&unit_path).unwrap();
        service.install_unit().unwrap();

        assert_eq!(std::fs::read_to_string(&unit_path).unwrap(), first);
        assert_eq!(runner.calls().len(), 4, "each install reloads and enables");
    }

    #[cfg(any(windows, test))]
    #[test]
    fn install_writes_registers_and_starts_the_task() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let data_dir = directory.path().join("data");
        let service = service_for(
            r"C:\Handfast\handfast.exe",
            &data_dir,
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::stopped();

        let notes = service
            .install_task_with_sid("S-1-5-21-1-2-3-1001", &control)
            .unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 5, "{calls:?}");
        assert_eq!(calls[0].0, "schtasks");
        assert_eq!(calls[0].1[0], "/Create");
        assert_eq!(calls[0].1[1], "/XML");
        assert!(
            calls[0].1[2].ends_with(".xml"),
            "the XML path is passed: {calls:?}"
        );
        assert_eq!(calls[0].1[3..], ["/TN", TASK_NAME, "/F"]);
        assert_eq!(calls[1].1, ["/Run", "/TN", TASK_NAME]);
        assert_eq!(calls[2].0, "net");
        assert_eq!(calls[2].1, ["session"]);
        let rule = firewall_rule_name("S-1-5-21-1-2-3-1001");
        assert_eq!(calls[3].0, "netsh");
        assert_eq!(
            calls[3].1[..4],
            ["advfirewall", "firewall", "delete", "rule"]
        );
        assert_eq!(calls[3].1[4], format!("name={rule}"));
        assert_eq!(calls[4].0, "netsh");
        assert_eq!(calls[4].1[..4], ["advfirewall", "firewall", "add", "rule"]);
        assert_eq!(calls[4].1[4], format!("name={rule}"));
        assert_eq!(calls[4].1[7], format!("program={}", service.exe.display()));
        assert_eq!(calls[4].1[8], "enable=yes");
        assert_eq!(notes[0], format!("installed task {TASK_NAME}"));
        assert_eq!(notes[1], format!("installed firewall rule {rule}"));
        assert!(
            !Path::new(&calls[0].1[2]).exists(),
            "the temp definition is removed"
        );
    }

    #[cfg(any(windows, test))]
    #[test]
    fn install_leaves_a_running_daemon_alone() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let data_dir = directory.path().join("data");
        let service = service_for(
            r"C:\Handfast\handfast.exe",
            &data_dir,
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::answering(&[Reply::Answer]);

        service
            .install_task_with_sid("S-1-5-21-1-2-3-1001", &control)
            .unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 4, "{calls:?}");
        assert_eq!(calls[0].1[0], "/Create");
        assert!(
            !calls.iter().any(|call| call.1[0] == "/Run"),
            "a running daemon needs no second start: {calls:?}"
        );
    }

    #[cfg(any(windows, test))]
    #[test]
    fn install_prints_the_firewall_command_when_the_rule_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::failing(&["net session", "netsh advfirewall firewall show"]);
        let data_dir = directory.path().join("data");
        let service = service_for(
            r"C:\Handfast\handfast.exe",
            &data_dir,
            directory.path().join(UNIT_NAME),
            &runner,
        );

        let notes = service
            .install_task_with_sid("S-1-5-21-1-2-3-1001", &FakeControl::stopped())
            .unwrap();

        let rule = firewall_rule_name("S-1-5-21-1-2-3-1001");
        let expected = firewall_command(Path::new(r"C:\Handfast\handfast.exe"), &rule);
        assert!(
            expected.starts_with(
                "netsh advfirewall firewall add rule name=\"Handfast-S-1-5-21-1-2-3-1001\""
            ),
            "{expected}"
        );
        assert!(
            expected
                .contains("dir=in action=allow program=\"C:\\Handfast\\handfast.exe\" enable=yes"),
            "{expected}"
        );
        assert!(
            notes.iter().any(|note| note.contains(&expected)),
            "{notes:?}"
        );
        assert!(
            !runner
                .calls()
                .iter()
                .any(|call| call.0 == "netsh" && call.1[2] == "add"),
            "a non-elevated install does not add the rule"
        );
    }

    #[cfg(any(windows, test))]
    #[test]
    fn elevated_install_replaces_the_firewall_rule() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let data_dir = directory.path().join("data");
        let service = service_for(
            r"C:\Handfast\handfast.exe",
            &data_dir,
            directory.path().join(UNIT_NAME),
            &runner,
        );

        let notes = service
            .install_task_with_sid("S-1-5-21-1-2-3-1001", &FakeControl::stopped())
            .unwrap();

        let calls = runner.calls();
        let rule = format!("name={}", firewall_rule_name("S-1-5-21-1-2-3-1001"));
        assert!(
            calls
                .iter()
                .any(|call| call.0 == "netsh" && call.1[2] == "delete" && call.1[4] == rule),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|call| call.0 == "netsh" && call.1[2] == "add" && call.1[4] == rule),
            "{calls:?}"
        );
        assert!(
            notes
                .iter()
                .any(|note| note.starts_with("installed firewall rule Handfast-")),
            "{notes:?}"
        );
    }

    #[cfg(any(windows, test))]
    #[test]
    fn unelevated_install_stays_quiet_when_the_rule_exists() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::failing(&["net session"]);
        let data_dir = directory.path().join("data");
        let service = service_for(
            r"C:\Handfast\handfast.exe",
            &data_dir,
            directory.path().join(UNIT_NAME),
            &runner,
        );

        let notes = service
            .install_task_with_sid("S-1-5-21-1-2-3-1001", &FakeControl::stopped())
            .unwrap();

        assert_eq!(notes, vec![format!("installed task {TASK_NAME}")]);
        assert!(
            !runner
                .calls()
                .iter()
                .any(|call| call.0 == "netsh" && call.1[2] == "add"),
            "a non-elevated install does not add the rule"
        );
    }

    #[test]
    fn firewall_rule_name_is_the_sid_and_stable() {
        let first = firewall_rule_name("S-1-5-21-1-2-3-1001");
        let again = firewall_rule_name("S-1-5-21-1-2-3-1001");
        let other = firewall_rule_name("S-1-5-21-1-2-3-2002");

        assert_eq!(first, "Handfast-S-1-5-21-1-2-3-1001");
        assert_eq!(first, again, "one user keeps one name across installs");
        assert_ne!(first, other, "two users never share a rule");
    }

    #[cfg(any(windows, test))]
    #[test]
    fn install_and_uninstall_address_the_same_rule() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let data_dir = directory.path().join("data");
        let service = service_for(
            r"C:\Handfast\handfast.exe",
            &data_dir,
            directory.path().join(UNIT_NAME),
            &runner,
        );

        service
            .install_task_with_sid("S-1-5-21-1-2-3-1001", &FakeControl::stopped())
            .unwrap();
        service
            .uninstall_task_with_sid("S-1-5-21-1-2-3-1001")
            .unwrap();

        let calls = runner.calls();
        let rule = format!("name={}", firewall_rule_name("S-1-5-21-1-2-3-1001"));
        let names: Vec<&String> = calls
            .iter()
            .filter(|call| call.0 == "netsh")
            .map(|call| &call.1[4])
            .collect();
        assert_eq!(
            names,
            vec![&rule, &rule, &rule],
            "delete, add and uninstall delete carry one name"
        );
    }

    #[cfg(any(windows, test))]
    #[test]
    fn uninstall_removes_the_firewall_rule_when_elevated() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let data_dir = directory.path().join("data");
        let service = service_for(
            r"C:\Handfast\handfast.exe",
            &data_dir,
            directory.path().join(UNIT_NAME),
            &runner,
        );

        service
            .uninstall_task_with_sid("S-1-5-21-1-2-3-1001")
            .unwrap();

        let calls = runner.calls();
        let last = calls.last().expect("uninstall runs commands");
        assert_eq!(last.0, "netsh");
        assert_eq!(last.1[2], "delete");
        assert_eq!(
            last.1[4],
            format!("name={}", firewall_rule_name("S-1-5-21-1-2-3-1001")),
            "{calls:?}"
        );
    }

    #[cfg(any(unix, test))]
    #[test]
    fn uninstall_disables_stops_removes_and_reloads() {
        let directory = tempfile::tempdir().unwrap();
        let unit_path = directory.path().join("systemd/user").join(UNIT_NAME);
        std::fs::create_dir_all(unit_path.parent().unwrap()).unwrap();
        std::fs::write(&unit_path, "unit").unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            unit_path.clone(),
            &runner,
        );

        service.uninstall_unit().unwrap();

        assert!(!unit_path.exists(), "the unit file is removed");
        let calls = runner.calls();
        assert_eq!(calls[0].1, ["--user", "disable", UNIT_NAME]);
        assert_eq!(calls[1].1, ["--user", "stop", UNIT_NAME]);
        assert_eq!(calls[2].1, ["--user", "daemon-reload"]);
    }

    #[cfg(any(unix, test))]
    #[test]
    fn installed_unix_reads_the_unit_file_or_the_user_manager() {
        let directory = tempfile::tempdir().unwrap();
        let unit_path = directory.path().join(UNIT_NAME);
        let runner = FakeCommands::failing(&["systemctl --user cat"]);
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            unit_path.clone(),
            &runner,
        );
        assert!(!service.installed_unix(), "no unit file and no unit");

        std::fs::write(&unit_path, "unit").unwrap();
        assert!(service.installed_unix(), "the unit file counts");

        std::fs::remove_file(&unit_path).unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            unit_path,
            &runner,
        );
        assert!(
            service.installed_unix(),
            "a packaged unit under /usr/lib/systemd/user counts"
        );
    }

    #[cfg(any(unix, test))]
    #[test]
    fn uninstall_without_a_unit_still_reloads() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::failing(&["systemctl --user cat"]);
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );

        service.uninstall_unit().unwrap();

        assert_eq!(runner.calls().len(), 2, "{:?}", runner.calls());
        assert_eq!(runner.calls()[0].1, ["--user", "cat", UNIT_NAME]);
        assert_eq!(runner.calls()[1].1, ["--user", "daemon-reload"]);
    }

    #[cfg(any(unix, test))]
    #[test]
    fn uninstall_disables_a_packaged_unit() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );

        service.uninstall_unit().unwrap();

        let calls = runner.calls();
        assert_eq!(calls[0].1, ["--user", "cat", UNIT_NAME]);
        assert_eq!(calls[1].1, ["--user", "disable", UNIT_NAME]);
        assert_eq!(calls[2].1, ["--user", "stop", UNIT_NAME]);
        assert_eq!(calls[3].1, ["--user", "daemon-reload"]);
    }

    #[cfg(any(unix, test))]
    #[test]
    fn uninstall_reports_a_failed_disable() {
        let directory = tempfile::tempdir().unwrap();
        let unit_path = directory.path().join(UNIT_NAME);
        std::fs::write(&unit_path, "unit").unwrap();
        let runner = FakeCommands::failing(&["systemctl --user disable"]);
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            unit_path.clone(),
            &runner,
        );

        let error = service.uninstall_unit().unwrap_err();

        assert!(matches!(error, ServiceError::Command { .. }), "{error}");
        assert!(unit_path.exists(), "a failed disable removes nothing");
    }

    #[cfg(any(windows, test))]
    #[test]
    fn uninstall_ends_and_deletes_the_task() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            r"C:\Handfast\handfast.exe",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );

        service
            .uninstall_task_with_sid("S-1-5-21-1-2-3-1001")
            .unwrap();

        let calls = runner.calls();
        assert_eq!(calls[0].1, ["/Query", "/TN", TASK_NAME]);
        assert_eq!(calls[1].1, ["/End", "/TN", TASK_NAME]);
        assert_eq!(calls[2].1, ["/Delete", "/TN", TASK_NAME, "/F"]);
    }

    #[cfg(any(windows, test))]
    #[test]
    fn uninstall_without_a_task_still_removes_the_firewall_rule() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::failing(&["schtasks /Query"]);
        let service = service_for(
            r"C:\Handfast\handfast.exe",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );

        service
            .uninstall_task_with_sid("S-1-5-21-1-2-3-1001")
            .unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[0].0, "schtasks");
        assert_eq!(calls[1].0, "net");
        assert_eq!(calls[2].0, "netsh");
        assert_eq!(calls[2].1[2], "delete");
        assert_eq!(
            calls[2].1[4],
            format!("name={}", firewall_rule_name("S-1-5-21-1-2-3-1001")),
            "{calls:?}"
        );
        assert!(
            !calls.iter().any(|call| call.0 == "schtasks"
                && (call.1[0] == "/End" || call.1[0] == "/Delete")),
            "an absent task runs no task commands"
        );
    }

    #[test]
    fn start_runs_the_start_command_when_no_daemon_answers() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::stopped();

        service.start(&control).unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        #[cfg(unix)]
        assert_eq!(calls[0].1, ["--user", "start", UNIT_NAME]);
        #[cfg(windows)]
        assert_eq!(calls[0].1, ["/Run", "/TN", TASK_NAME]);
    }

    #[test]
    fn start_leaves_a_running_daemon_alone() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::answering(&[Reply::Answer]);

        service.start(&control).unwrap();

        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    #[test]
    fn stop_returns_when_the_endpoint_stops_answering() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::answering(&[Reply::Answer]);

        service.stop(&control).unwrap();

        assert_eq!(control.shutdowns.get(), 1, "stop sends Shutdown first");
        assert!(
            runner.calls().is_empty(),
            "a daemon that exits needs no fallback: {:?}",
            runner.calls()
        );
    }

    #[test]
    fn stop_falls_back_when_the_daemon_keeps_answering() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::answering(&[Reply::Answer, Reply::Answer, Reply::Answer]);

        service.stop(&control).unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        #[cfg(unix)]
        assert_eq!(calls[0].1, ["--user", "stop", UNIT_NAME]);
        #[cfg(windows)]
        assert_eq!(calls[0].1, ["/End", "/TN", TASK_NAME]);
    }

    #[test]
    fn stop_without_a_daemon_runs_no_command() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::stopped();

        service.stop(&control).unwrap();

        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
    }

    #[test]
    fn stop_falls_back_when_the_daemon_rejects_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::refusing();

        service.stop(&control).unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        #[cfg(unix)]
        assert_eq!(calls[0].1, ["--user", "stop", UNIT_NAME]);
        #[cfg(windows)]
        assert_eq!(calls[0].1, ["/End", "/TN", TASK_NAME]);
    }

    #[test]
    fn stop_falls_back_when_the_request_times_out() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::timing_out();

        service.stop(&control).unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        #[cfg(unix)]
        assert_eq!(calls[0].1, ["--user", "stop", UNIT_NAME]);
        #[cfg(windows)]
        assert_eq!(calls[0].1, ["/End", "/TN", TASK_NAME]);
    }

    #[test]
    fn stop_reports_a_failed_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::failing(&["systemctl --user stop", "schtasks /End"]);
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::refusing();

        let error = service.stop(&control).unwrap_err();

        assert!(matches!(error, ServiceError::Command { .. }), "{error}");
    }

    #[test]
    fn status_reports_installed_running_pid_and_uptime() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::default();
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::answering(&[Reply::Answer]);

        let status = service.status(&control);

        assert_eq!(
            status,
            ServiceStatus {
                installed: true,
                running: true,
                pid: Some(4242),
                uptime_secs: Some(90),
            }
        );
        assert_eq!(
            status.to_string(),
            "installed: true\nrunning: true\npid: 4242\nuptime: 90s"
        );
    }

    #[test]
    fn status_reports_a_stopped_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let runner = FakeCommands::failing(&["schtasks /Query", "systemctl --user cat"]);
        let service = service_for(
            "/opt/handfast/handfast",
            directory.path(),
            directory.path().join(UNIT_NAME),
            &runner,
        );
        let control = FakeControl::stopped();

        let status = service.status(&control);

        assert_eq!(
            status,
            ServiceStatus {
                installed: false,
                running: false,
                pid: None,
                uptime_secs: None,
            }
        );
        assert_eq!(
            status.to_string(),
            "installed: false\nrunning: false\npid: -\nuptime: -"
        );
    }
}
