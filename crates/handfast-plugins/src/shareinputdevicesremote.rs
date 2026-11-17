//! Shared input receive plugin.
//!
//! A peer that shares its keyboard and mouse sends
//! `kdeconnect.shareinputdevices.request` with the edge it left through and the
//! offset along that edge. The pointer enters this desktop on the opposite edge.
//! The peer's relative `kdeconnect.mousepad.request` moves are injected by the
//! mousepad plugin, and this plugin follows the same moves. When the pointer
//! crosses the far edge, it sends `kdeconnect.shareinputdevices` with the release
//! offset.
//!
//! Named gaps. Wayland has no portable absolute pointer, so every share is
//! declined there, and the peer's offset is echoed back. Only the main display
//! is used, so a multi-monitor layout enters at a corner of the primary
//! display. Scroll packets do not move the tracked pointer, unlike upstream.

use async_trait::async_trait;
use handfast_desktop::DesktopError;
use handfast_proto::{types, Packet};
use handfast_runtime::{
    LogLevel, Plugin, PluginContext, PluginError, PluginEvent, PluginFactory, PluginMeta,
    PluginOutput,
};
use serde_json::{json, Map, Value};

use crate::mousepad;
use crate::roster;

/// Receives shared keyboard and mouse input from a peer.
#[derive(Debug, Default)]
pub struct ShareInputDevicesRemotePlugin {
    share: Option<Share>,
    warned: bool,
}

impl ShareInputDevicesRemotePlugin {
    /// Create a shared input receiver.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            share: None,
            warned: false,
        }
    }
}

/// A screen edge, as the peer's exit edge and the local entry side both name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    Top,
    Left,
    Right,
    Bottom,
}

/// Map the wire `exitEdge` to an edge. Other values are malformed.
///
/// The numbers are `Qt::Edge` values from `qnamespace.h`, and they do not follow
/// the order of the names.
fn parse_edge(value: i64) -> Option<Edge> {
    Some(match value {
        1 => Edge::Top,
        2 => Edge::Left,
        4 => Edge::Right,
        8 => Edge::Bottom,
        _ => return None,
    })
}

/// A position in pixels. Offsets stay fractional, as upstream keeps them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Rect {
    /// The last pixel column. Qt's `QRect::right` counts it the same way.
    fn right(self) -> i32 {
        self.x + self.width - 1
    }

    fn bottom(self) -> i32 {
        self.y + self.height - 1
    }

    fn top_left(self) -> Point {
        Point {
            x: f64::from(self.x),
            y: f64::from(self.y),
        }
    }

    fn top_right(self) -> Point {
        Point {
            x: f64::from(self.right()),
            y: f64::from(self.y),
        }
    }

    fn bottom_left(self) -> Point {
        Point {
            x: f64::from(self.x),
            y: f64::from(self.bottom()),
        }
    }

    /// Keep a position on the display, where the real pointer stops.
    fn clamp(self, point: Point) -> Point {
        Point {
            x: point.x.clamp(f64::from(self.x), f64::from(self.right())),
            y: point.y.clamp(f64::from(self.y), f64::from(self.bottom())),
        }
    }
}

/// The peer left through `exit`, so the pointer enters from the opposite side.
/// Offsets count from the returned corner.
fn entry(exit: Edge, screen: Rect) -> (Edge, Point) {
    match exit {
        Edge::Left => (Edge::Right, screen.top_right()),
        Edge::Right => (Edge::Left, screen.top_left()),
        Edge::Top => (Edge::Bottom, screen.bottom_left()),
        Edge::Bottom => (Edge::Top, screen.top_left()),
    }
}

/// Whether the pointer has gone past the edge that the share entered through.
///
/// The right and bottom tests compare with the exclusive edge, as upstream does,
/// so the pixel just past the last column still belongs to the share.
fn crossed(enter: Edge, screen: Rect, point: Point) -> bool {
    match enter {
        Edge::Left => point.x < f64::from(screen.x),
        Edge::Right => point.x > f64::from(screen.x + screen.width),
        Edge::Top => point.y < f64::from(screen.y),
        Edge::Bottom => point.y > f64::from(screen.y + screen.height),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Share {
    enter: Edge,
    /// Release offsets count from this corner, not from the screen.
    corner: Point,
    position: Point,
    screen: Rect,
}

impl Share {
    fn start(exit: Edge, deltax: f64, deltay: f64, screen: Rect) -> Self {
        let (enter, corner) = entry(exit, screen);
        let position = screen.clamp(Point {
            x: corner.x + deltax,
            y: corner.y + deltay,
        });
        Self {
            enter,
            corner,
            position,
            screen,
        }
    }

    fn release_offset(&self, point: Point) -> Point {
        Point {
            x: point.x - self.corner.x,
            y: point.y - self.corner.y,
        }
    }

    /// Follow one relative move. Once the pointer crosses the far edge, the share
    /// ends and the release offset is returned. Otherwise the position stays on
    /// the display.
    fn move_by(&mut self, dx: f64, dy: f64) -> Option<Point> {
        let target = Point {
            x: self.position.x + dx,
            y: self.position.y + dy,
        };
        if crossed(self.enter, self.screen, target) {
            return Some(self.release_offset(target));
        }
        self.position = self.screen.clamp(target);
        None
    }
}

fn entry_point(exit: Edge, deltax: f64, deltay: f64, screen: Rect) -> (i32, i32) {
    let position = Share::start(exit, deltax, deltay, screen).position;
    (position.x.round() as i32, position.y.round() as i32)
}

/// A parsed `kdeconnect.shareinputdevices.request`.
struct Request {
    exit: Edge,
    deltax: f64,
    deltay: f64,
}

fn delta_field(body: &Map<String, Value>, field: &str) -> Result<f64, &'static str> {
    match body.get(field) {
        None => Ok(0.0),
        Some(value) => value
            .as_f64()
            .ok_or("shareinputdevices delta is not a number"),
    }
}

/// A packet without `exitEdge` is not an error. Upstream ignores it, and so does
/// this plugin.
fn parse_request(body: &Map<String, Value>) -> Result<Option<Request>, &'static str> {
    let Some(value) = body.get("exitEdge") else {
        return Ok(None);
    };
    let exit = value
        .as_i64()
        .and_then(parse_edge)
        .ok_or("shareinputdevices exitEdge is not a Qt edge")?;
    Ok(Some(Request {
        exit,
        deltax: delta_field(body, "deltax")?,
        deltay: delta_field(body, "deltay")?,
    }))
}

fn release(offset: Point) -> Result<PluginOutput, PluginError> {
    let packet = Packet::new(
        types::SHAREINPUTDEVICES,
        json!({"releaseDeltax": offset.x, "releaseDeltay": offset.y}),
    )
    .map_err(|error| PluginError::InvalidPacket(error.to_string()))?;
    Ok(PluginOutput::new().with_packet(packet))
}

fn decline_warning(message: &str) -> PluginEvent {
    PluginEvent::Log {
        level: LogLevel::Warn,
        message: format!("shared input declined: {message}"),
    }
}

impl ShareInputDevicesRemotePlugin {
    /// Forget the share and release the peer at `offset`. The request's own deltas
    /// go back, as the Android receiver echoes them. `None` means the request could
    /// not be parsed, so the release is zero.
    fn decline(
        &mut self,
        offset: Option<Point>,
        warning: Option<PluginEvent>,
    ) -> Result<PluginOutput, PluginError> {
        self.share = None;
        let output = release(offset.unwrap_or(Point { x: 0.0, y: 0.0 }))?;
        Ok(match warning {
            Some(event) => output.with_event(event),
            None => output,
        })
    }

    /// Decline after the backend refused. An unsupported backend warns once.
    fn refused(&mut self, offset: Point, error: DesktopError) -> Result<PluginOutput, PluginError> {
        let warning = match error {
            DesktopError::Unsupported(_) if self.warned => None,
            DesktopError::Unsupported(message) => {
                self.warned = true;
                Some(decline_warning(message))
            }
            other => Some(decline_warning(&other.to_string())),
        };
        self.decline(Some(offset), warning)
    }

    async fn request(
        &mut self,
        ctx: &PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        let request = match parse_request(&packet.body) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(PluginOutput::new()),
            Err(message) => return self.decline(None, Some(decline_warning(message))),
        };
        let offset = Point {
            x: request.deltax,
            y: request.deltay,
        };
        self.share = None;
        let Some(input) = ctx.desktop.input.as_ref() else {
            return self.decline(
                Some(offset),
                Some(decline_warning("input adapter unavailable")),
            );
        };
        let screen = match input.main_display().await {
            Ok((width, height)) if width >= 1 && height >= 1 => Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            Ok(_) => {
                return self.decline(
                    Some(offset),
                    Some(decline_warning("the main display reports no size")),
                )
            }
            Err(error) => return self.refused(offset, error),
        };
        let (x, y) = entry_point(request.exit, request.deltax, request.deltay, screen);
        match input.absolute_pointer(x, y).await {
            Ok(()) => {
                self.share = Some(Share::start(
                    request.exit,
                    request.deltax,
                    request.deltay,
                    screen,
                ));
                Ok(PluginOutput::new())
            }
            Err(error) => self.refused(offset, error),
        }
    }

    fn pointer_moved(&mut self, packet: &Packet) -> Result<PluginOutput, PluginError> {
        let Some(share) = self.share.as_mut() else {
            return Ok(PluginOutput::new());
        };
        let Some(event) = mousepad::pointer_move(packet) else {
            return Ok(PluginOutput::new());
        };
        match share.move_by(event.dx, event.dy) {
            Some(offset) => {
                self.share = None;
                release(offset)
            }
            None => Ok(PluginOutput::new()),
        }
    }
}

#[async_trait]
impl Plugin for ShareInputDevicesRemotePlugin {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SHAREINPUTDEVICESREMOTE
    }

    async fn on_packet(
        &mut self,
        ctx: &mut PluginContext<'_>,
        packet: &Packet,
    ) -> Result<PluginOutput, PluginError> {
        match packet.ptype.as_str() {
            types::SHAREINPUTDEVICES_REQUEST => self.request(ctx, packet).await,
            types::MOUSEPAD_REQUEST => self.pointer_moved(packet),
            types::SHAREINPUTDEVICES => {
                self.share = None;
                Ok(PluginOutput::new())
            }
            _ => Ok(PluginOutput::new()),
        }
    }

    async fn on_disconnect(
        &mut self,
        _ctx: &mut PluginContext<'_>,
    ) -> Result<PluginOutput, PluginError> {
        self.share = None;
        Ok(PluginOutput::new())
    }
}

/// Builds shared input receiver instances.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShareInputDevicesRemoteFactory;

impl PluginFactory for ShareInputDevicesRemoteFactory {
    fn meta(&self) -> &'static PluginMeta {
        &roster::SHAREINPUTDEVICESREMOTE
    }

    fn create(&self) -> Box<dyn Plugin> {
        Box::new(ShareInputDevicesRemotePlugin::new())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use handfast_desktop::mock::InputEvent;
    use handfast_desktop::{
        ButtonAction, Desktop, Input, KeyEvent, MockDesktop, PointerButton, PointerEvent,
    };
    use handfast_proto::{DeviceId, DeviceName, DeviceType};
    use handfast_runtime::Device;

    use super::*;

    /// The main display that the mock reports.
    const SCREEN: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    const REQUEST: &str =
        include_str!("../../handfast-testkit/tests/vectors/shareinputdevices_request.json");
    const RELEASE: &str =
        include_str!("../../handfast-testkit/tests/vectors/shareinputdevices_release.json");

    fn device() -> Device {
        Device::new(
            DeviceId::generate(),
            DeviceName::new("Test"),
            DeviceType::Phone,
        )
    }

    fn golden(text: &str) -> Packet {
        serde_json::from_str(text).unwrap()
    }

    fn request_packet(body: Value) -> Packet {
        Packet::from_body(types::SHAREINPUTDEVICES_REQUEST, &body).unwrap()
    }

    fn move_packet(body: Value) -> Packet {
        Packet::from_body(types::MOUSEPAD_REQUEST, &body).unwrap()
    }

    fn release_packet(x: f64, y: f64) -> Packet {
        Packet::from_body(
            types::SHAREINPUTDEVICES,
            &json!({"releaseDeltax": x, "releaseDeltay": y}),
        )
        .unwrap()
    }

    fn has_warning(output: &PluginOutput) -> bool {
        output.events.iter().any(|event| {
            matches!(
                event,
                PluginEvent::Log {
                    level: LogLevel::Warn,
                    ..
                }
            )
        })
    }

    async fn handle(
        plugin: &mut ShareInputDevicesRemotePlugin,
        desktop: &Desktop,
        packet: &Packet,
    ) -> PluginOutput {
        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop,
        };
        plugin.on_packet(&mut ctx, packet).await.unwrap()
    }

    /// An adapter with only the required methods. Absolute positioning and the
    /// display size fall back to `Unsupported`, as on Wayland.
    struct NoAbsoluteInput;

    #[async_trait]
    impl Input for NoAbsoluteInput {
        async fn pointer(&self, _event: PointerEvent) -> Result<(), DesktopError> {
            Ok(())
        }

        async fn button(
            &self,
            _button: PointerButton,
            _action: ButtonAction,
        ) -> Result<(), DesktopError> {
            Ok(())
        }

        async fn key(&self, _event: KeyEvent) -> Result<(), DesktopError> {
            Ok(())
        }
    }

    struct RefusingInput;

    #[async_trait]
    impl Input for RefusingInput {
        async fn pointer(&self, _event: PointerEvent) -> Result<(), DesktopError> {
            Ok(())
        }

        async fn button(
            &self,
            _button: PointerButton,
            _action: ButtonAction,
        ) -> Result<(), DesktopError> {
            Ok(())
        }

        async fn key(&self, _event: KeyEvent) -> Result<(), DesktopError> {
            Ok(())
        }

        async fn main_display(&self) -> Result<(i32, i32), DesktopError> {
            Ok((1920, 1080))
        }

        async fn absolute_pointer(&self, _x: i32, _y: i32) -> Result<(), DesktopError> {
            Err(DesktopError::Backend("the pointer is locked".to_string()))
        }
    }

    #[test]
    fn wire_edges_are_qt_edge_numbers() {
        assert_eq!(parse_edge(1), Some(Edge::Top));
        assert_eq!(parse_edge(2), Some(Edge::Left));
        assert_eq!(parse_edge(4), Some(Edge::Right));
        assert_eq!(parse_edge(8), Some(Edge::Bottom));
        for value in [0, 3, 6, 16, -1] {
            assert_eq!(parse_edge(value), None, "{value}");
        }
    }

    #[test]
    fn each_exit_edge_enters_from_the_opposite_side() {
        assert_eq!(
            entry(Edge::Left, SCREEN),
            (Edge::Right, Point { x: 1919.0, y: 0.0 })
        );
        assert_eq!(
            entry(Edge::Right, SCREEN),
            (Edge::Left, Point { x: 0.0, y: 0.0 })
        );
        assert_eq!(
            entry(Edge::Top, SCREEN),
            (Edge::Bottom, Point { x: 0.0, y: 1079.0 })
        );
        assert_eq!(
            entry(Edge::Bottom, SCREEN),
            (Edge::Top, Point { x: 0.0, y: 0.0 })
        );
    }

    #[test]
    fn entry_point_adds_the_offset_to_the_corner() {
        assert_eq!(entry_point(Edge::Left, 0.0, 300.0, SCREEN), (1919, 300));
        assert_eq!(entry_point(Edge::Right, 0.0, 540.0, SCREEN), (0, 540));
        assert_eq!(entry_point(Edge::Top, 200.0, 0.0, SCREEN), (200, 1079));
        assert_eq!(entry_point(Edge::Bottom, 40.0, 0.0, SCREEN), (40, 0));
        assert_eq!(entry_point(Edge::Right, 0.0, 540.6, SCREEN), (0, 541));
    }

    #[test]
    fn entry_point_stays_on_the_display() {
        assert_eq!(entry_point(Edge::Left, 0.0, 5000.0, SCREEN), (1919, 1079));
        assert_eq!(entry_point(Edge::Right, -50.0, -50.0, SCREEN), (0, 0));
        assert_eq!(entry_point(Edge::Top, 9000.0, 0.0, SCREEN), (1919, 1079));
    }

    #[test]
    fn the_far_edge_is_crossed_only_past_its_bound() {
        let at = |x: f64, y: f64| Point { x, y };
        assert!(!crossed(Edge::Left, SCREEN, at(0.0, 0.0)));
        assert!(crossed(Edge::Left, SCREEN, at(-0.5, 0.0)));
        assert!(!crossed(Edge::Right, SCREEN, at(1920.0, 0.0)));
        assert!(crossed(Edge::Right, SCREEN, at(1921.0, 0.0)));
        assert!(!crossed(Edge::Top, SCREEN, at(0.0, 0.0)));
        assert!(crossed(Edge::Top, SCREEN, at(0.0, -1.0)));
        assert!(!crossed(Edge::Bottom, SCREEN, at(0.0, 1080.0)));
        assert!(crossed(Edge::Bottom, SCREEN, at(0.0, 1081.0)));
    }

    #[test]
    fn release_offsets_count_from_the_entry_corner() {
        let mut share = Share::start(Edge::Left, 0.0, 300.0, SCREEN);
        assert_eq!(share.move_by(2.0, 10.0), Some(Point { x: 2.0, y: 310.0 }));

        let mut share = Share::start(Edge::Right, 0.0, 540.0, SCREEN);
        assert_eq!(
            share.move_by(-30.0, 200.0),
            Some(Point { x: -30.0, y: 740.0 })
        );

        let mut share = Share::start(Edge::Bottom, 40.0, 0.0, SCREEN);
        assert_eq!(share.move_by(0.0, -12.0), Some(Point { x: 40.0, y: -12.0 }));

        let mut share = Share::start(Edge::Top, 200.0, 0.0, SCREEN);
        assert_eq!(share.move_by(0.0, 6.0), Some(Point { x: 200.0, y: 6.0 }));
    }

    #[test]
    fn malformed_requests_are_refused() {
        for body in [
            json!({"exitEdge": 3}),
            json!({"exitEdge": 0}),
            json!({"exitEdge": 16}),
            json!({"exitEdge": "left"}),
            json!({"exitEdge": 2.5}),
            json!({"exitEdge": 2, "deltax": "far"}),
            json!({"exitEdge": 2, "deltay": null}),
        ] {
            let parsed = parse_request(body.as_object().unwrap());
            assert!(parsed.is_err(), "{body} parsed");
        }
    }

    #[test]
    fn requests_take_integer_or_float_deltas_and_default_to_zero() {
        let body = json!({"exitEdge": 4, "deltax": 3, "deltay": 4.5});
        let parsed = parse_request(body.as_object().unwrap()).unwrap().unwrap();
        assert_eq!(
            (parsed.exit, parsed.deltax, parsed.deltay),
            (Edge::Right, 3.0, 4.5)
        );

        let body = json!({"exitEdge": 2});
        let parsed = parse_request(body.as_object().unwrap()).unwrap().unwrap();
        assert_eq!((parsed.deltax, parsed.deltay), (0.0, 0.0));

        let body = json!({"deltax": 1.0});
        assert!(matches!(parse_request(body.as_object().unwrap()), Ok(None)));
    }

    #[tokio::test]
    async fn a_request_puts_the_pointer_at_the_entry_point() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesRemotePlugin::new();

        let output = handle(&mut plugin, &mock.desktop(), &golden(REQUEST)).await;

        assert_eq!(
            mock.input_events(),
            vec![InputEvent::AbsolutePointer { x: 1919, y: 300 }]
        );
        assert!(output.packets.is_empty());
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn crossing_the_far_edge_sends_the_release_vector() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        handle(&mut plugin, &desktop, &golden(REQUEST)).await;

        let output = handle(
            &mut plugin,
            &desktop,
            &move_packet(json!({"dx": 2.0, "dy": 10.0})),
        )
        .await;

        assert_eq!(output.packets, vec![golden(RELEASE)]);
    }

    #[tokio::test]
    async fn the_share_ends_after_the_release() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        handle(&mut plugin, &desktop, &golden(REQUEST)).await;
        handle(
            &mut plugin,
            &desktop,
            &move_packet(json!({"dx": 2.0, "dy": 10.0})),
        )
        .await;

        let output = handle(
            &mut plugin,
            &desktop,
            &move_packet(json!({"dx": 2.0, "dy": 0.0})),
        )
        .await;

        assert!(output.packets.is_empty());
    }

    #[tokio::test]
    async fn the_peer_ending_the_share_clears_it() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        handle(&mut plugin, &desktop, &golden(REQUEST)).await;
        handle(&mut plugin, &desktop, &golden(RELEASE)).await;

        let output = handle(
            &mut plugin,
            &desktop,
            &move_packet(json!({"dx": 2.0, "dy": 10.0})),
        )
        .await;

        assert!(output.packets.is_empty());
    }

    #[tokio::test]
    async fn disconnect_clears_the_share() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        handle(&mut plugin, &desktop, &golden(REQUEST)).await;

        let device = device();
        let mut ctx = PluginContext {
            device: &device,
            desktop: &desktop,
        };
        let closed = plugin.on_disconnect(&mut ctx).await.unwrap();
        assert!(closed.packets.is_empty());

        let output = handle(
            &mut plugin,
            &desktop,
            &move_packet(json!({"dx": 2.0, "dy": 10.0})),
        )
        .await;
        assert!(output.packets.is_empty());
    }

    #[tokio::test]
    async fn scrolls_do_not_move_the_tracked_pointer() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        handle(&mut plugin, &desktop, &golden(REQUEST)).await;

        let output = handle(
            &mut plugin,
            &desktop,
            &move_packet(json!({"scroll": true, "dx": 50.0, "dy": 0.0})),
        )
        .await;
        assert!(output.packets.is_empty());

        let output = handle(
            &mut plugin,
            &desktop,
            &move_packet(json!({"dx": 2.0, "dy": 10.0})),
        )
        .await;
        assert_eq!(output.packets, vec![golden(RELEASE)]);
    }

    #[tokio::test]
    async fn the_tracked_pointer_stops_at_the_display_edge() {
        let mock = MockDesktop::new();
        let desktop = mock.desktop();
        let mut plugin = ShareInputDevicesRemotePlugin::new();
        handle(&mut plugin, &desktop, &golden(REQUEST)).await;

        // Without the clamp, the tracker would drift past the display and miss the release.
        let output = handle(
            &mut plugin,
            &desktop,
            &move_packet(json!({"dx": -5000.0, "dy": 0.0})),
        )
        .await;
        assert!(output.packets.is_empty());
        let output = handle(
            &mut plugin,
            &desktop,
            &move_packet(json!({"dx": 1.0, "dy": 0.0})),
        )
        .await;
        assert!(output.packets.is_empty());

        let output = handle(
            &mut plugin,
            &desktop,
            &move_packet(json!({"dx": 1920.0, "dy": 0.0})),
        )
        .await;
        assert_eq!(output.packets, vec![release_packet(2.0, 300.0)]);
    }

    #[tokio::test]
    async fn moves_without_a_share_are_ignored() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesRemotePlugin::new();

        let output = handle(
            &mut plugin,
            &mock.desktop(),
            &move_packet(json!({"dx": 2.0, "dy": 10.0})),
        )
        .await;

        assert!(output.packets.is_empty());
        assert!(output.events.is_empty());
        assert!(mock.input_events().is_empty());
    }

    #[tokio::test]
    async fn an_unsupported_backend_declines_and_warns_once() {
        let desktop = Desktop {
            input: Some(Arc::new(NoAbsoluteInput)),
            ..Desktop::default()
        };
        let mut plugin = ShareInputDevicesRemotePlugin::new();

        let first = handle(&mut plugin, &desktop, &golden(REQUEST)).await;
        assert_eq!(first.packets, vec![release_packet(0.0, 300.0)]);
        assert!(has_warning(&first));

        let second = handle(&mut plugin, &desktop, &golden(REQUEST)).await;
        assert_eq!(second.packets, vec![release_packet(0.0, 300.0)]);
        assert!(!has_warning(&second));
    }

    #[tokio::test]
    async fn a_refused_move_declines_and_the_plugin_keeps_running() {
        let desktop = Desktop {
            input: Some(Arc::new(RefusingInput)),
            ..Desktop::default()
        };
        let mut plugin = ShareInputDevicesRemotePlugin::new();

        let output = handle(&mut plugin, &desktop, &golden(REQUEST)).await;

        assert_eq!(output.packets, vec![release_packet(0.0, 300.0)]);
        assert!(has_warning(&output));
    }

    #[tokio::test]
    async fn a_missing_input_adapter_declines() {
        let mut plugin = ShareInputDevicesRemotePlugin::new();

        let output = handle(&mut plugin, &Desktop::default(), &golden(REQUEST)).await;

        assert_eq!(output.packets, vec![release_packet(0.0, 300.0)]);
        assert!(has_warning(&output));
    }

    #[tokio::test]
    async fn a_malformed_request_is_declined_without_a_move() {
        let mock = MockDesktop::new();
        let mut plugin = ShareInputDevicesRemotePlugin::new();

        let output = handle(
            &mut plugin,
            &mock.desktop(),
            &request_packet(json!({"exitEdge": 3})),
        )
        .await;

        assert!(mock.input_events().is_empty());
        assert_eq!(output.packets, vec![release_packet(0.0, 0.0)]);
        assert!(has_warning(&output));
    }

    #[tokio::test]
    async fn a_decline_echoes_the_request_offset() {
        let desktop = Desktop {
            input: Some(Arc::new(NoAbsoluteInput)),
            ..Desktop::default()
        };
        let mut plugin = ShareInputDevicesRemotePlugin::new();

        let output = handle(
            &mut plugin,
            &desktop,
            &request_packet(json!({"exitEdge": 4, "deltax": 25.0, "deltay": 40.0})),
        )
        .await;

        assert_eq!(output.packets, vec![release_packet(25.0, 40.0)]);
    }

    #[test]
    fn meta_matches_the_roster() {
        assert_eq!(
            ShareInputDevicesRemotePlugin::new().meta(),
            &roster::SHAREINPUTDEVICESREMOTE
        );
        assert_eq!(
            ShareInputDevicesRemoteFactory.meta(),
            &roster::SHAREINPUTDEVICESREMOTE
        );
    }
}
