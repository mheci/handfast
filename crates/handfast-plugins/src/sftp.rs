//! Browse this host's files from the phone over SFTP.
//!
//! The phone asks `kdeconnect.sftp.request {"start": true}` and the desktop
//! starts an SFTP server, then replies with everything the phone needs to
//! connect: `serverRunning`, `errorMessage`, `user`, `port`, `path`,
//! `multiPaths` and `pathNames`.
//!
//! Starting a file server on request is a significant action, so the design
//! decisions here are deliberately conservative:
//!
//! - The server is **off unless configured**. `[sftp] root = "/some/dir"` names
//!   the directory to serve; with no configured root the plugin reports
//!   `serverRunning: false` and an error message instead of exposing anything.
//! - Only one root is served. Upstream can advertise several `multiPaths`; a
//!   list of roots is a list of ways to reach the whole filesystem, so
//!   Handfast advertises exactly the configured one.
//! - The reply always carries `serverRunning`, including on failure, because
//!   the phone's UI reads that field before anything else and waits forever on
//!   a reply without it.
//!
//! Handfast does not implement the SFTP protocol itself. It starts a system
//! `sshd` configured for the served root, over a port the OS assigns, with a
//! one-use account — the same shape upstream uses on Windows, where OpenSSH
//! ships with the OS. Without `sshd` the plugin reports that in
//! `errorMessage`.

use async_trait::async_trait;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::roster;

/// Key under `[sftp]` naming the served directory.
pub const ROOT_KEY: &str = "root";

/// Key under `[sftp]` naming the login user reported to the phone.
pub const USER_KEY: &str = "user";

/// Longest accepted error message sent to the peer.
const MAX_ERROR_BYTES: usize = 512;

/// Result of trying to start the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SftpStart {
    /// The server is listening.
    Running {
        /// Login user.
        user: String,
        /// TCP port.
        port: u16,
        /// Directory served, as an absolute path.
        path: String,
    },
    /// The server could not be started; the reason is shown to the user.
    Failed(String),
}

/// Starts and stops the SFTP server. Tests substitute a fake.
#[async_trait]
pub trait SftpServer: Send + Sync {
    /// Start serving `root` as `user`.
    async fn start(&self, root: &str, user: &str) -> SftpStart;

    /// Stop the server. Harmless when it is not running.
    async fn stop(&self);
}

/// Serves files over SFTP.
pub struct SftpPlugin {
    server: Option<std::sync::Arc<dyn SftpServer>>,
    running: Option<SftpStart>,
}

impl std::fmt::Debug for SftpPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpPlugin")
            .field("configured", &self.server.is_some())
            .field("running", &self.running.is_some())
            .finish()
    }
}

impl SftpPlugin {
    /// Create an SFTP plugin with no server backend.
    ///
    /// A plugin with no backend stays honest: it answers every start request
    /// with `serverRunning: false`, which is exactly what an unconfigured
    /// install should do.
    #[must_use]
    pub fn new() -> Self {
        Self {
            server: None,
            running: None,
        }
    }

    /// Create an SFTP plugin over a server backend.
    #[must_use]
    pub fn with_server(server: std::sync::Arc<dyn SftpServer>) -> Self {
        Self {
            server: Some(server),
            running: None,
        }
    }

    /// Whether a server is currently running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self.running, Some(SftpStart::Running { .. }))
    }

    /// The port the server is listening on, when it is running.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        match &self.running {
            Some(SftpStart::Running { port, .. }) => Some(*port),
            _ => None,
        }
    }

    /// Build the reply for a start or stop request.
    fn reply(state: &Option<SftpStart>) -> Option<Packet> {
        let mut body = Map::new();
        match state {
            Some(SftpStart::Running { user, port, path }) => {
                body.insert("serverRunning".to_string(), json!(true));
                body.insert("user".to_string(), json!(user));
                body.insert("port".to_string(), json!(port));
                body.insert("path".to_string(), json!(path));
                // One root, deliberately: see the module comment.
                body.insert("multiPaths".to_string(), json!([path]));
                body.insert("pathNames".to_string(), json!(["Handfast"]));
            }
            Some(SftpStart::Failed(message)) => {
                body.insert("serverRunning".to_string(), json!(false));
                let message: String = message.chars().take(MAX_ERROR_BYTES).collect();
                body.insert("errorMessage".to_string(), json!(message));
            }
            None => {
                body.insert("serverRunning".to_string(), json!(false));
            }
        }
        Packet::new(types::SFTP, Value::Object(body)).ok()
    }
}

impl Default for SftpPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for SftpPlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SFTP
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        if packet.ptype != types::SFTP_REQUEST {
            return Ok(PluginOutput::new());
        }
        let start = packet
            .body
            .get("start")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if !start {
            if let Some(server) = &self.server {
                server.stop().await;
            }
            let was_running = self.running.take().is_some();
            return Ok(PluginOutput::new()
                .with_optional_packet(SftpPlugin::reply(&None))
                .with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: if was_running {
                        "SFTP server stopped at the peer's request".to_string()
                    } else {
                        "peer asked to stop an SFTP server that was not running".to_string()
                    },
                }));
        }

        let Some(server) = &self.server else {
            // No configured root: refuse, with a reason the phone can show.
            let state = Some(SftpStart::Failed(
                "no SFTP directory is configured on this host".to_string(),
            ));
            self.running = state.clone();
            return Ok(PluginOutput::new()
                .with_optional_packet(SftpPlugin::reply(&state))
                .with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: "peer asked for SFTP but [sftp] root is not configured".to_string(),
                }));
        };

        let root = ctx.config.get(roster::SFTP.name, ROOT_KEY).unwrap_or("");
        let user = ctx
            .config
            .get(roster::SFTP.name, USER_KEY)
            .unwrap_or("handfast");
        if root.trim().is_empty() {
            let state = Some(SftpStart::Failed(
                "no SFTP directory is configured on this host".to_string(),
            ));
            self.running = state.clone();
            return Ok(PluginOutput::new().with_optional_packet(SftpPlugin::reply(&state)));
        }

        let result = server.start(root, user).await;
        let mut output = PluginOutput::new();
        output = output.with_optional_packet(SftpPlugin::reply(&Some(result.clone())));
        match &result {
            SftpStart::Running { port, path, .. } => {
                output = output.with_event(PluginEvent::Log {
                    level: LogLevel::Info,
                    message: format!("SFTP serving {path} on port {port}"),
                });
            }
            SftpStart::Failed(message) => {
                output = output.with_event(PluginEvent::Log {
                    level: LogLevel::Warn,
                    message: format!("SFTP did not start: {message}"),
                });
            }
        }
        self.running = Some(result);
        Ok(output)
    }
}

/// Wraps a [`handfast_desktop::backends::sftp`] backend as an [`SftpServer`].
///
/// The plugin crate cannot depend on `handfast-desktop`'s process management
/// directly, and more importantly the plugin should not own process
/// supervision. The daemon builds the backend and hands it over, so a host
/// without `sshd` simply builds no backend and the plugin reports that.
pub struct DesktopSftpServer {
    backend: std::sync::Arc<dyn handfast_desktop::backends::sftp::SftpServerBackend>,
}

impl std::fmt::Debug for DesktopSftpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DesktopSftpServer")
    }
}

impl DesktopSftpServer {
    /// Wrap a backend.
    #[must_use]
    pub fn new(
        backend: std::sync::Arc<dyn handfast_desktop::backends::sftp::SftpServerBackend>,
    ) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl SftpServer for DesktopSftpServer {
    async fn start(&self, root: &str, user: &str) -> SftpStart {
        let request = handfast_desktop::backends::sftp::ServerRequest {
            root: root.to_string(),
            user: user.to_string(),
        };
        match self.backend.start(&request).await {
            Ok(port) => SftpStart::Running {
                user: user.to_string(),
                port,
                path: root.to_string(),
            },
            Err(error) => SftpStart::Failed(error.to_string()),
        }
    }

    async fn stop(&self) {
        self.backend.stop().await;
    }
}

/// Builds [`SftpPlugin`] instances.
///
/// The factory carries the backend rather than creating one per device: an
/// `sshd` process belongs to the host, and two devices must not each start
/// their own on the same port.
#[derive(Clone, Default)]
pub struct SftpFactory {
    server: Option<std::sync::Arc<dyn SftpServer>>,
}

impl std::fmt::Debug for SftpFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpFactory")
            .field("configured", &self.server.is_some())
            .finish()
    }
}

impl SftpFactory {
    /// A factory with no backend: every start request is refused.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A factory that starts a real server.
    #[must_use]
    pub fn with_server(server: std::sync::Arc<dyn SftpServer>) -> Self {
        Self {
            server: Some(server),
        }
    }
}

impl PluginFactory for SftpFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SFTP
    }

    fn create(&self) -> Box<dyn Plugin> {
        match &self.server {
            Some(server) => Box::new(SftpPlugin::with_server(std::sync::Arc::clone(server))),
            None => Box::new(SftpPlugin::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use handfast_desktop::MockDesktop;
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::{Device, PluginConfig, PluginStatus};

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Phone"),
            DeviceType::Phone,
        )
    }

    struct FakeServer {
        result: SftpStart,
        starts: Mutex<Vec<(String, String)>>,
        stops: AtomicUsize,
    }

    #[async_trait]
    impl SftpServer for FakeServer {
        async fn start(&self, root: &str, user: &str) -> SftpStart {
            self.starts
                .lock()
                .unwrap()
                .push((root.to_string(), user.to_string()));
            self.result.clone()
        }

        async fn stop(&self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn fake(result: SftpStart) -> Arc<FakeServer> {
        Arc::new(FakeServer {
            result,
            starts: Mutex::new(Vec::new()),
            stops: AtomicUsize::new(0),
        })
    }

    async fn send(plugin: &mut SftpPlugin, config: &PluginConfig, body: Value) -> PluginOutput {
        let desktop = MockDesktop::new().desktop();
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
            config,
        };
        let packet = Packet::from_body(types::SFTP_REQUEST, &body).unwrap();
        plugin.on_packet(&mut ctx, &packet).await.unwrap()
    }

    fn configured() -> PluginConfig {
        let mut config = PluginConfig::new();
        config.insert("sftp", "root", "/home/alice/Public");
        config.insert("sftp", "user", "alice");
        config
    }

    #[test]
    fn the_roster_entry_matches_upstream() {
        assert_eq!(roster::SFTP.status, PluginStatus::Implemented);
        assert_eq!(roster::SFTP.incoming, &[types::SFTP_REQUEST]);
        assert_eq!(roster::SFTP.outgoing, &[types::SFTP]);
    }

    #[tokio::test]
    async fn a_start_request_serves_the_configured_root() {
        let server = fake(SftpStart::Running {
            user: "alice".to_string(),
            port: 51234,
            path: "/home/alice/Public".to_string(),
        });
        let mut plugin = SftpPlugin::with_server(Arc::clone(&server) as Arc<dyn SftpServer>);
        let config = configured();

        let output = send(&mut plugin, &config, json!({ "start": true })).await;
        assert_eq!(output.packets.len(), 1);
        let body = &output.packets[0].body;
        assert_eq!(body.get("serverRunning"), Some(&json!(true)));
        assert_eq!(body.get("user"), Some(&json!("alice")));
        assert_eq!(body.get("port"), Some(&json!(51234)));
        assert_eq!(body.get("path"), Some(&json!("/home/alice/Public")));
        // Exactly one root is advertised.
        assert_eq!(body.get("multiPaths"), Some(&json!(["/home/alice/Public"])));
        assert_eq!(body.get("pathNames"), Some(&json!(["Handfast"])));

        assert_eq!(
            server.starts.lock().unwrap().clone(),
            vec![("/home/alice/Public".to_string(), "alice".to_string())]
        );
        assert!(plugin.is_running());
        assert_eq!(plugin.port(), Some(51234));
    }

    #[tokio::test]
    async fn an_unconfigured_root_is_refused_with_a_reason() {
        let mut plugin = SftpPlugin::new();
        let output = send(&mut plugin, &PluginConfig::new(), json!({ "start": true })).await;
        let body = &output.packets[0].body;
        assert_eq!(body.get("serverRunning"), Some(&json!(false)));
        assert!(body
            .get("errorMessage")
            .and_then(Value::as_str)
            .unwrap()
            .contains("not configured"));
        assert!(!plugin.is_running());
    }

    #[tokio::test]
    async fn a_backend_without_a_configured_root_reports_failure() {
        let server = fake(SftpStart::Failed("sshd is not installed".to_string()));
        let mut plugin = SftpPlugin::with_server(Arc::clone(&server) as Arc<dyn SftpServer>);
        let config = configured();
        let output = send(&mut plugin, &config, json!({ "start": true })).await;

        let body = &output.packets[0].body;
        assert_eq!(body.get("serverRunning"), Some(&json!(false)));
        assert_eq!(
            body.get("errorMessage"),
            Some(&json!("sshd is not installed"))
        );
        assert!(!plugin.is_running());
    }

    #[tokio::test]
    async fn a_root_present_in_config_but_empty_is_refused() {
        let server = fake(SftpStart::Running {
            user: "alice".to_string(),
            port: 1,
            path: "/".to_string(),
        });
        let mut plugin = SftpPlugin::with_server(Arc::clone(&server) as Arc<dyn SftpServer>);
        let mut config = PluginConfig::new();
        config.insert("sftp", "root", "   ");
        let output = send(&mut plugin, &config, json!({ "start": true })).await;
        assert_eq!(
            output.packets[0].body.get("serverRunning"),
            Some(&json!(false))
        );
        // Nothing was started with a blank root.
        assert!(server.starts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_stop_request_stops_the_server() {
        let server = fake(SftpStart::Running {
            user: "alice".to_string(),
            port: 51234,
            path: "/home/alice/Public".to_string(),
        });
        let mut plugin = SftpPlugin::with_server(Arc::clone(&server) as Arc<dyn SftpServer>);
        let config = configured();
        send(&mut plugin, &config, json!({ "start": true })).await;

        let output = send(&mut plugin, &config, json!({ "start": false })).await;
        assert_eq!(
            output.packets[0].body.get("serverRunning"),
            Some(&json!(false))
        );
        assert_eq!(server.stops.load(Ordering::SeqCst), 1);
        assert!(!plugin.is_running());
    }

    #[tokio::test]
    async fn stopping_a_stopped_server_is_harmless() {
        let mut plugin = SftpPlugin::new();
        let output = send(&mut plugin, &PluginConfig::new(), json!({ "start": false })).await;
        assert_eq!(
            output.packets[0].body.get("serverRunning"),
            Some(&json!(false))
        );
        assert_eq!(output.events.len(), 1);
    }

    #[tokio::test]
    async fn every_reply_states_whether_the_server_is_running() {
        // The phone's UI reads serverRunning first; a reply without it hangs.
        for body in [
            json!({ "start": true }),
            json!({ "start": false }),
            json!({}),
        ] {
            let mut plugin = SftpPlugin::new();
            let output = send(&mut plugin, &PluginConfig::new(), body).await;
            assert_eq!(output.packets.len(), 1);
            assert!(output.packets[0].body.contains_key("serverRunning"));
        }
    }
}
