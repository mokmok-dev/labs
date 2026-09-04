//! GPUI shell for the ECHONET Lite radar service.
//!
//! The radar's state machine lives in the [`echonet_radar`] library and runs on
//! its own thread. This binary renders it through a webview:
//!
//! * a local HTTP + WebSocket server streams [`RadarEvent`]s as JSON and turns
//!   client commands back into [`Command`]s, and
//! * a GPUI window hosts the webview so the UI can be written with any web
//!   component library, here `@cloudflare/kumo`.
//!
//! Run `pnpm build` in `echonet-radar/web` once so the compiled UI lands in
//! `web/dist`, which is embedded into the binary with `rust-embed`. For a
//! hot-reload loop, point `RADAR_UI_URL` at a running `pnpm dev` server.

use std::collections::VecDeque;
use std::error::Error;
use std::io::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{StatusCode, Uri, header::CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use echonet_lite_udp::EchoNetSocket;
use echonet_radar::{
    ChangeEvent, Command, DEFAULT_DISCOVERY_INTERVAL, DEFAULT_UPDATE_INTERVAL, RadarConfig,
    RadarEvent, run_service,
};
use gpui_kit::component::{ActiveTheme as _, Root, Theme, TitleBar, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
    Window, WindowOptions,
};
use gpui_kit::{div, px, size};
use gpui_wry::WebView;
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use usage::Cli;

/// Maximum number of change events kept in the bridge history.
const MAX_EVENTS: usize = 1000;

/// Log ECHONET Lite device state changes as a time-series feed.
//
// `default = "..."` is required beside `default_value_t`: the literal feeds the
// emitted portable spec, the Rust expression supplies the runtime value.
#[derive(Debug, Cli)]
#[usage(bin = "echonet-radar", version, completion)]
struct Arguments {
    /// IPv4 interface used for multicast membership.
    #[usage(long, default = "0.0.0.0", value_name = "IP")]
    interface: Ipv4Addr,
    /// Discovery interval in seconds.
    #[usage(
        long,
        default = "60",
        default_value_t = DEFAULT_DISCOVERY_INTERVAL.as_secs(),
        value_name = "SECONDS"
    )]
    discovery_interval_seconds: u64,
    /// Value-polling interval in seconds.
    #[usage(
        long,
        default = "15",
        default_value_t = DEFAULT_UPDATE_INTERVAL.as_secs(),
        value_name = "SECONDS"
    )]
    update_interval_seconds: u64,
}

impl Arguments {
    fn config(self) -> Result<RadarConfig, Box<dyn Error>> {
        let config = RadarConfig {
            interface: self.interface,
            discovery_interval: Duration::from_secs(self.discovery_interval_seconds),
            update_interval: Duration::from_secs(self.update_interval_seconds),
        };
        config.validate()?;
        Ok(config)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = Arguments::parse().config()?;
    let (event_sender, event_receiver) = mpsc::channel();
    let (command_sender, command_receiver) = tokio::sync::mpsc::channel(8);
    let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
    let network_shutdown = shutdown_receiver.clone();
    let network_sender = event_sender.clone();
    let network_config = config;
    let network_thread = thread::Builder::new()
        .name(String::from("echonet-radar-network"))
        .spawn(move || {
            run_network(
                network_config,
                network_sender,
                command_receiver,
                network_shutdown,
            )
        })?;

    let (ready_sender, ready_receiver) = mpsc::channel();
    let web_thread = thread::Builder::new()
        .name(String::from("echonet-radar-web"))
        .spawn(move || {
            run_web(
                event_receiver,
                command_sender,
                shutdown_receiver,
                ready_sender,
            )
        })?;
    drop(event_sender);

    let Ok(local_url) = ready_receiver.recv() else {
        let _ = shutdown_sender.send(true);
        let _ = network_thread.join();
        let web_error = web_thread
            .join()
            .map_err(|_| io::Error::other("web thread panicked"))?;
        let message = match web_error {
            Ok(()) => String::from("web ui server stopped unexpectedly"),
            Err(error) => format!("web ui server failed to start: {error}"),
        };
        return Err(io::Error::other(message).into());
    };
    println!("echonet-radar: serving UI at {local_url}");
    let ui_url = std::env::var("RADAR_UI_URL").map_or_else(
        |_| local_url,
        |dev_url| {
            println!("echonet-radar: RADAR_UI_URL set; webview will load {dev_url}");
            dev_url
        },
    );
    let _ = std::io::stdout().flush();

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        let url = ui_url;
        cx.spawn(async move |cx| {
            let window_options = WindowOptions {
                window_min_size: Some(size(px(960.), px(640.))),
                ..TitleBar::window_options()
            };
            let window = cx.open_window(window_options, move |window, cx| {
                let shell = RadarShell::new(&url, window, cx);
                cx.new(|cx| Root::new(shell, window, cx))
            });
            if let Err(error) = window {
                eprintln!("echonet-radar: failed to open window: {error}");
            }
        })
        .detach();
    });

    let _ = shutdown_sender.send(true);
    let network_result = network_thread
        .join()
        .map_err(|_| io::Error::other("network thread panicked"))?;
    let web_result = web_thread
        .join()
        .map_err(|_| io::Error::other("web thread panicked"))?;
    network_result?;
    web_result?;
    Ok(())
}

fn run_network(
    config: RadarConfig,
    events: mpsc::Sender<RadarEvent>,
    commands: tokio::sync::mpsc::Receiver<Command>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let error_sender = events.clone();
    let result = runtime.block_on(async move {
        let socket = EchoNetSocket::bind_default_multicast(config.interface).await?;
        run_service(socket, config, events, commands, shutdown).await
    });
    if let Err(error) = &result {
        let _ = error_sender.send(RadarEvent::Status(format!("network error: {error}")));
    }
    result
}

/// One observed change, serialized for the web UI.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChangePayload {
    at_ms: u64,
    source: SocketAddr,
    eoj: String,
    epc: u8,
    edt: String,
}

impl ChangePayload {
    fn from_change(change: ChangeEvent) -> Self {
        let at_ms = change
            .at
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        Self {
            at_ms: u64::try_from(at_ms).unwrap_or(0),
            source: change.source,
            eoj: format!(
                "0x{:02X}{:02X}{:02X}",
                change.eoj.class_group, change.eoj.class, change.eoj.instance
            ),
            epc: change.epc,
            edt: change.edt,
        }
    }
}

/// A server-to-client message.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ServerMessage {
    Snapshot {
        changes: Vec<ChangePayload>,
        status: String,
    },
    Change(Box<ChangePayload>),
    Status {
        message: String,
    },
}

/// A client-to-server command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClientMessage {
    PollNow,
}

/// Shared bridge history: recent changes plus the latest status message.
#[derive(Debug, Default)]
struct History {
    changes: VecDeque<ChangePayload>,
    status: String,
}

impl History {
    fn push(
        &mut self,
        change: ChangePayload,
    ) {
        self.changes.push_front(change);
        self.changes.truncate(MAX_EVENTS);
    }
}

type SharedHistory = Arc<Mutex<History>>;

fn lock_history(history: &SharedHistory) -> std::sync::MutexGuard<'_, History> {
    history.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Shared state for the HTTP handlers.
#[derive(Clone)]
struct AppState {
    history: SharedHistory,
    broadcast: tokio::sync::broadcast::Sender<String>,
    commands: tokio::sync::mpsc::Sender<Command>,
}

/// The bridge and UI server. Runs until shutdown is signalled.
fn run_web(
    events: Receiver<RadarEvent>,
    commands: tokio::sync::mpsc::Sender<Command>,
    shutdown: tokio::sync::watch::Receiver<bool>,
    ready: mpsc::Sender<String>,
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let port = std::env::var("RADAR_UI_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0);
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
        let address = listener.local_addr()?;
        let _ = ready.send(format!("http://{address}"));

        let (broadcast, _) = tokio::sync::broadcast::channel(256);
        let history: SharedHistory = Arc::new(Mutex::new(History::default()));
        let pump_history = Arc::clone(&history);
        let pump_broadcast = broadcast.clone();
        thread::Builder::new()
            .name(String::from("echonet-radar-pump"))
            .spawn(move || pump_events(events, &pump_history, &pump_broadcast))?;

        let state = AppState {
            history,
            broadcast,
            commands,
        };
        let app = Router::new()
            .route("/ws", get(ws_handler))
            .fallback(serve_ui)
            .with_state(state);
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal(shutdown))
            .await
    })
}

/// Forward radar events into the broadcast and bridge history.
fn pump_events(
    events: Receiver<RadarEvent>,
    history: &SharedHistory,
    broadcast: &tokio::sync::broadcast::Sender<String>,
) {
    for event in events {
        let message = match event {
            RadarEvent::Change(change) => {
                let payload = ChangePayload::from_change(change);
                lock_history(history).push(payload.clone());
                ServerMessage::Change(Box::new(payload))
            },
            RadarEvent::Status(status) => {
                lock_history(history).status.clone_from(&status);
                ServerMessage::Status { message: status }
            },
        };
        if let Ok(json) = serde_json::to_string(&message) {
            let _ = broadcast.send(json);
        }
    }
}

async fn shutdown_signal(mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stopped| *stopped).await;
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
) {
    let snapshot = {
        let history = lock_history(&state.history);
        serde_json::to_string(&ServerMessage::Snapshot {
            changes: history.changes.iter().cloned().collect(),
            status: history.status.clone(),
        })
    };
    if let Ok(json) = snapshot
        && socket.send(Message::Text(json.into())).await.is_err()
    {
        return;
    }

    let mut events = state.broadcast.subscribe();
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(json) => {
                    if socket.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                },
                Err(_) => break,
            },
            input = socket.recv() => match input {
                Some(Ok(Message::Text(text))) => forward_command(&text, &state.commands),
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {},
            },
        }
    }
}

fn forward_command(
    text: &str,
    commands: &tokio::sync::mpsc::Sender<Command>,
) {
    if matches!(
        serde_json::from_str::<ClientMessage>(text),
        Ok(ClientMessage::PollNow)
    ) {
        let _ = commands.try_send(Command::PollNow);
    }
}

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct UiAssets;

/// Serve the embedded UI, falling back to the SPA entry for paths without an
/// extension.
async fn serve_ui(uri: Uri) -> Response {
    let requested = uri.path().trim_start_matches('/');
    let path = if requested.is_empty() || !requested.contains('.') {
        "index.html"
    } else {
        requested
    };
    match UiAssets::get(path) {
        Some(file) => (
            StatusCode::OK,
            [(CONTENT_TYPE, mime_for(path))],
            file.data.into_owned(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn mime_for(path: &str) -> &'static str {
    let Some((_, extension)) = path.rsplit_once('.') else {
        return "application/octet-stream";
    };
    match extension.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" | "map" => "application/json",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// The GPUI window contents: a title bar above a full-size webview.
struct RadarShell {
    webview: Option<Entity<WebView>>,
    // Kept alive only for its Drop guard, which unsubscribes the appearance
    // observer when the window closes.
    _appearance: gpui_kit::Subscription,
}

impl RadarShell {
    fn new(
        url: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        // Keep the native window (title bar, background) in sync with the OS
        // light/dark appearance; the webview follows on its own.
        Theme::sync_system_appearance(Some(window), cx);
        let webview = create_webview(url, window, cx);
        let appearance = window.observe_window_appearance(|window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
        });
        cx.new(|_| Self {
            webview,
            _appearance: appearance,
        })
    }
}

#[cfg(target_os = "macos")]
fn create_webview(
    url: &str,
    window: &mut Window,
    cx: &mut App,
) -> Option<Entity<WebView>> {
    use wry::WebViewBuilder;

    let builder = WebViewBuilder::new().with_url(url);
    #[cfg(debug_assertions)]
    let builder = builder.with_devtools(true);
    let webview = builder.build_as_child(window).ok()?;
    Some(cx.new(|cx| WebView::new(webview, window, cx)))
}

#[cfg(not(target_os = "macos"))]
fn create_webview(
    _url: &str,
    _window: &mut Window,
    _cx: &mut App,
) -> Option<Entity<WebView>> {
    None
}

impl Render for RadarShell {
    fn render(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title_bar = TitleBar::new().child(
            h_flex()
                .items_center()
                .gap_2()
                .child(div().child("echonet-radar")),
        );
        let content = self.webview.clone().map_or_else(
            || {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .child("WebView is not supported on this platform yet.")
            },
            |webview| div().flex_1().min_h_0().child(webview),
        );
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(title_bar)
            .child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echonet_lite::frame::Eoj;

    fn payload(
        class_group: u8,
        class: u8,
        instance: u8,
        epc: u8,
        edt: &str,
    ) -> ChangePayload {
        ChangePayload::from_change(ChangeEvent {
            at: UNIX_EPOCH,
            source: "192.0.2.1:3610".parse::<SocketAddr>().unwrap(),
            eoj: Eoj::new(class_group, class, instance),
            epc,
            edt: String::from(edt),
        })
    }

    #[test]
    fn change_payload_uses_camel_case_wire_format() {
        let value = serde_json::to_value(payload(0x01, 0x30, 0x01, 0x80, "ON")).unwrap();
        let object = value.as_object().unwrap();
        assert!(object.contains_key("atMs"));
        assert!(object.contains_key("source"));
        assert_eq!(object.get("eoj"), Some(&serde_json::json!("0x013001")));
        assert_eq!(object.get("epc"), Some(&serde_json::json!(0x80)));
        assert_eq!(object.get("edt"), Some(&serde_json::json!("ON")));
    }

    #[test]
    fn snapshot_message_carries_changes_and_status() {
        let message = ServerMessage::Snapshot {
            changes: vec![payload(0x01, 0x30, 0x01, 0x80, "ON")],
            status: String::from("starting"),
        };
        let value = serde_json::to_value(message).unwrap();
        assert_eq!(value.get("type"), Some(&serde_json::json!("snapshot")));
        assert_eq!(value.get("status"), Some(&serde_json::json!("starting")));
        assert!(value.get("changes").unwrap().is_array());
    }

    #[test]
    fn client_poll_now_message_deserializes() {
        assert_eq!(
            serde_json::from_str::<ClientMessage>(r#"{"type":"pollNow"}"#).unwrap(),
            ClientMessage::PollNow
        );
        assert!(serde_json::from_str::<ClientMessage>(r#"{"type":"unknown"}"#).is_err());
    }

    #[test]
    fn history_is_bounded_and_newest_first() {
        let mut history = History::default();
        for index in 0..MAX_EVENTS + 10 {
            history.push(payload(0x01, 0x30, 0x01, 0x80, &index.to_string()));
        }
        assert_eq!(history.changes.len(), MAX_EVENTS);
        assert_eq!(history.changes[0].edt, (MAX_EVENTS + 9).to_string());
        assert_eq!(history.changes[MAX_EVENTS - 1].edt, 10.to_string());
    }

    #[test]
    fn ui_mime_types_are_guessed_from_extension() {
        assert_eq!(mime_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            mime_for("assets/index.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(mime_for("assets/app.css"), "text/css; charset=utf-8");
        assert_eq!(mime_for("icon.svg"), "image/svg+xml");
        assert_eq!(mime_for("font.woff2"), "font/woff2");
        assert_eq!(mime_for("README"), "application/octet-stream");
    }

    #[tokio::test]
    async fn forward_command_relays_poll_now_only() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        forward_command(r#"{"type":"pollNow"}"#, &sender);
        forward_command("garbage", &sender);
        forward_command(r#"{"type":"unknown"}"#, &sender);
        assert_eq!(receiver.recv().await, Some(Command::PollNow));
        assert_eq!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        );
    }

    #[tokio::test]
    async fn embedded_ui_serves_index_for_root_and_slash_paths() {
        for path in ["/", "/index.html"] {
            let uri = axum::http::Uri::from_static(path);
            let response = serve_ui(uri).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get(CONTENT_TYPE).unwrap(),
                "text/html; charset=utf-8"
            );
            let body = axum::body::to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(html.contains("<div id=\"root\">"));
        }
    }

    #[tokio::test]
    async fn embedded_ui_falls_back_to_spa_entry_for_unknown_paths() {
        let uri = axum::http::Uri::from_static("/some/client/route");
        let response = serve_ui(uri).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn embedded_ui_returns_not_found_for_missing_assets() {
        let uri = axum::http::Uri::from_static("/assets/does-not-exist.js");
        let response = serve_ui(uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
