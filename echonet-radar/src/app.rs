//! The window: one session at a time, one immutable snapshot per frame.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use echonet_radar_core::{
    CommandError, Fault, ObjectKey, Pulse, RadarHandle, RadarSession, RadarSnapshot, SessionConfig,
    SessionStatus,
};

use crate::chart;
use crate::net::{self, Interface};
use crate::readiness::{self, Readiness};
use crate::settings::{self, InterfaceChoice, Settings};
use crate::views;

/// How long a settings change waits before it is written and acted on.
const SETTINGS_DEBOUNCE: Duration = Duration::from_secs(1);

/// What the app was given at launch.
pub struct Startup {
    /// The settings this run starts from.
    pub settings: Settings,
    /// A notice from loading the settings, shown beside the session notices.
    pub notice: Option<String>,
    /// Where settings are written; `None` keeps them in memory.
    pub path: Option<PathBuf>,
    /// The interface choice the file held, so a command-line override is not
    /// written back.
    pub stored_interface: InterfaceChoice,
    /// The `--interface` name, when the run was given one.
    pub interface_override: Option<String>,
}

/// A click the start card reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartAction {
    /// Nothing was clicked.
    #[default]
    None,
    /// The user asked to bind and listen.
    Start,
}

/// Why a start attempt did not produce a running session.
#[derive(Debug, Clone)]
pub enum StartFailure {
    /// A stored interface name is no longer present.
    InterfaceMissing(String),
    /// The session rejected the configuration or could not bind.
    Fault(Fault),
}

/// Buttons the status bar reports.
#[derive(Debug, Clone, Copy, Default)]
pub struct AppActions {
    /// Ask the service for a value poll now.
    pub poll: bool,
    /// Ask the service for a discovery request now.
    pub rediscover: bool,
    /// Stop the session and start a new one.
    pub restart: bool,
}

/// The app-side [`Pulse`] over egui's repaint request.
pub struct EguiPulse(egui::Context);

impl Pulse for EguiPulse {
    fn pulse(&self) {
        self.0.request_repaint();
    }
}

struct Running {
    session: RadarSession,
    handle: RadarHandle,
}

/// The window state.
pub struct RadarApp {
    settings: Settings,
    settled: Settings,
    stored_interface: InterfaceChoice,
    interface_override: Option<String>,
    settings_path: Option<PathBuf>,
    interfaces: Vec<Interface>,
    start_action: StartAction,
    start_failure: Option<StartFailure>,
    running: Option<Running>,
    snapshot: Arc<RadarSnapshot>,
    readiness: Readiness,
    selection: Option<ObjectKey>,
    history: VecDeque<Option<i64>>,
    notice: Option<String>,
    dirty_since: Option<Instant>,
    last_sample: Option<Instant>,
    actions: AppActions,
    session_seen: Option<u64>,
    selection_judged: bool,
    running_socket: Option<(InterfaceChoice, Duration, Duration)>,
    ctx: egui::Context,
}

impl RadarApp {
    /// Build the window state without binding anything. The start card is what
    /// binds, so the OS permission prompt arrives with the user's click.
    #[must_use]
    pub fn new(
        startup: Startup,
        interfaces: Vec<Interface>,
        ctx: egui::Context,
    ) -> Self {
        let settled = {
            let mut state = startup.settings.clone();
            if startup.interface_override.is_some() {
                state.interface.clone_from(&startup.stored_interface);
            }
            state
        };
        Self {
            selection: startup.settings.selected,
            settings: startup.settings,
            settled,
            stored_interface: startup.stored_interface,
            interface_override: startup.interface_override,
            settings_path: startup.path,
            interfaces,
            start_action: StartAction::None,
            start_failure: None,
            running: None,
            snapshot: Arc::new(empty_snapshot()),
            readiness: Readiness::NotStarted,
            history: VecDeque::new(),
            notice: startup.notice,
            dirty_since: None,
            last_sample: None,
            actions: AppActions::default(),
            session_seen: None,
            selection_judged: false,
            running_socket: None,
            ctx,
        }
    }

    /// One frame, without an `eframe::Frame` so tests can drive it.
    pub fn render(
        &mut self,
        ui: &mut egui::Ui,
    ) {
        self.poll_session();
        if self.running.is_none() {
            self.render_start_card(ui);
        } else {
            self.keyboard(ui.ctx());
            self.show_started(ui);
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
        self.settle();
    }

    fn render_start_card(
        &mut self,
        ui: &mut egui::Ui,
    ) {
        if let Some(notice) = &self.notice {
            egui::Panel::top("notice").show(ui, |ui| {
                ui.label(notice);
            });
        }
        let failure = self.start_failure.clone();
        views::start::start_card(
            ui,
            &mut self.settings.interface,
            &self.interfaces,
            &mut self.start_action,
            failure.as_ref(),
        );
        if self.start_action == StartAction::Start {
            self.start_action = StartAction::None;
            self.start();
        }
    }

    fn show_started(
        &mut self,
        ui: &mut egui::Ui,
    ) {
        let interface = self.interface_label();
        egui::Panel::top("status-bar").show(ui, |ui| {
            views::status::status_bar(
                ui,
                &self.readiness,
                &self.snapshot,
                &mut self.actions,
                &interface,
            );
        });
        egui::Panel::bottom("settings-panel").show(ui, |ui| {
            egui::CollapsingHeader::new("Settings and diagnostics").show(ui, |ui| {
                views::settings::settings(ui, &mut self.settings, &self.interfaces);
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(notice) = &self.notice {
                ui.label(notice);
            }
            chart::draw(ui, &self.history);
            views::devices::device_list(
                ui,
                &self.snapshot,
                &mut self.selection,
                self.settings.update_interval,
            );
            if let Some(key) = self.selection {
                views::properties::property_pane(ui, &self.snapshot, key);
            }
        });
    }

    fn settle(&mut self) {
        self.clear_stale_override();
        if self.write_state() != self.settled {
            self.dirty_since.get_or_insert_with(Instant::now);
        }
        let Some(since) = self.dirty_since else {
            return;
        };
        if since.elapsed() < SETTINGS_DEBOUNCE {
            return;
        }
        self.dirty_since = None;
        self.persist();
        if self.running.is_some() && self.running_socket.as_ref() != Some(&self.socket_settings()) {
            self.restart();
        }
    }

    fn clear_stale_override(&mut self) {
        let Some(name) = &self.interface_override else {
            return;
        };
        if self.settings.interface != InterfaceChoice::Named(name.clone()) {
            self.interface_override = None;
        }
    }

    fn socket_settings(&self) -> (InterfaceChoice, Duration, Duration) {
        (
            self.settings.interface.clone(),
            self.settings.discovery_interval,
            self.settings.update_interval,
        )
    }

    fn interface_label(&self) -> String {
        match net::resolve_in(&self.settings.interface, &self.interfaces) {
            Ok(address) if address.is_unspecified() => String::from("Automatic"),
            Ok(address) => format!("{} ({address})", self.settings.interface.label()),
            Err(name) => format!("{name} (missing)"),
        }
    }

    fn poll_session(&mut self) {
        if let Some(running) = self.running.as_mut()
            && running.handle.has_changed()
        {
            self.snapshot = running.handle.snapshot();
        }
        if self.running.is_some() {
            self.readiness = readiness::step(&self.readiness, &self.snapshot);
        }
        self.adopt_selection();
        if self.running.is_some() {
            self.sample_history();
        }
    }

    /// Clear a restored selection once discovery has shown the device is gone.
    ///
    /// An empty snapshot says nothing yet: on a restart the first frames arrive
    /// before any device answers, and clearing there would make the stored
    /// selection useless.
    fn adopt_selection(&mut self) {
        if self.session_seen != Some(self.snapshot.session) {
            self.session_seen = Some(self.snapshot.session);
            self.selection_judged = false;
        }
        if !self.selection_judged && !self.snapshot.objects.is_empty() {
            self.selection_judged = true;
            if let Some(key) = self.selection
                && !self.snapshot.objects.contains_key(&key)
            {
                self.selection = None;
            }
        }
        if self.selection != self.settings.selected {
            self.settings.selected = self.selection;
            self.mark_dirty();
        }
    }

    fn sample_history(&mut self) {
        let now = Instant::now();
        let due = self
            .last_sample
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1));
        if due {
            chart::push_sample(
                &mut self.history,
                &self.snapshot,
                SystemTime::now(),
                self.settings.update_interval,
            );
            self.last_sample = Some(now);
        }
    }

    fn mark_dirty(&mut self) {
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    /// The settings a write would carry: a command-line override keeps the
    /// stored interface, because `--interface` applies to one run only.
    fn write_state(&self) -> Settings {
        let mut stored = self.settings.clone();
        if self.interface_override.is_some() {
            stored.interface.clone_from(&self.stored_interface);
        }
        stored
    }

    fn persist(&mut self) {
        let stored = self.write_state();
        if let Some(path) = self.settings_path.as_deref()
            && let Err(error) = settings::save_to(path, &stored)
        {
            tracing::warn!(path = %path.display(), %error, "settings write failed");
            self.notice = Some(format!(
                "Could not save settings to {}: {error}",
                path.display()
            ));
        }
        self.stored_interface.clone_from(&stored.interface);
        // A failed write must not retry on every frame; the next change retries.
        self.settled = stored;
    }

    fn start(&mut self) {
        let interface = match net::resolve(&self.settings.interface) {
            Ok(interface) => interface,
            Err(missing) => {
                self.start_failure = Some(StartFailure::InterfaceMissing(missing.clone()));
                self.notice = Some(format!(
                    "Interface {missing} is not present; Automatic is selected instead."
                ));
                self.settings.interface = InterfaceChoice::Auto;
                self.interface_override = None;
                self.mark_dirty();
                return;
            },
        };
        let config = SessionConfig {
            interface,
            discovery_interval: self.settings.discovery_interval,
            update_interval: self.settings.update_interval,
            ..SessionConfig::desktop()
        };
        tracing::info!(
            interface = %interface,
            discovery_s = config.discovery_interval.as_secs(),
            update_s = config.update_interval.as_secs(),
            "starting radar session"
        );
        match RadarSession::start(config, Arc::new(EguiPulse(self.ctx.clone()))) {
            Ok((session, mut handle)) => {
                self.snapshot = handle.snapshot();
                self.readiness = readiness::step(&Readiness::NotStarted, &self.snapshot);
                self.session_seen = None;
                self.selection_judged = false;
                self.running_socket = Some(self.socket_settings());
                self.running = Some(Running { session, handle });
                self.start_failure = None;
            },
            Err(fault) => {
                tracing::warn!(fault = ?fault, "radar session did not start");
                self.start_failure = Some(StartFailure::Fault(fault));
            },
        }
    }

    fn restart(&mut self) {
        tracing::info!("restarting radar session");
        if let Some(running) = self.running.take()
            && let Err(fault) = running.session.stop()
        {
            tracing::warn!(fault = ?fault, "radar session did not stop cleanly");
            self.notice = Some(format!(
                "The previous session did not stop cleanly: {}",
                readiness::describe_fault(&fault)
            ));
        }
        self.start();
    }

    fn keyboard(
        &mut self,
        ctx: &egui::Context,
    ) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (up, down, enter, poll, restart) = ctx.input(|input| {
            (
                input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::ArrowDown),
                input.key_pressed(egui::Key::Enter),
                input.key_pressed(egui::Key::P),
                input.key_pressed(egui::Key::R),
            )
        });
        if up || down {
            self.move_selection(down);
        }
        if enter && self.selection.is_none() {
            self.selection = views::devices::ordered_keys(&self.snapshot)
                .first()
                .copied();
        }
        if poll {
            self.actions.poll = true;
        }
        if restart {
            self.actions.restart = true;
        }
    }

    fn move_selection(
        &mut self,
        forward: bool,
    ) {
        let keys = views::devices::ordered_keys(&self.snapshot);
        if keys.is_empty() {
            return;
        }
        let index = self
            .selection
            .and_then(|selected| keys.iter().position(|key| *key == selected));
        let next = match (index, forward) {
            (Some(index), true) => (index + 1).min(keys.len() - 1),
            (Some(index), false) => index.saturating_sub(1),
            (None, _) => 0,
        };
        self.selection = keys.get(next).copied();
    }

    fn handle_actions(&mut self) {
        let actions = std::mem::take(&mut self.actions);
        if let Some(running) = &self.running {
            let result: Result<(), CommandError> = if actions.poll {
                running.handle.poll_now()
            } else if actions.rediscover {
                running.handle.rediscover()
            } else {
                Ok(())
            };
            if let Err(error) = result {
                self.notice = Some(error.to_string());
            }
        }
        if actions.restart {
            self.restart();
        }
    }
}

impl eframe::App for RadarApp {
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        _frame: &mut eframe::Frame,
    ) {
        self.render(ui);
        self.handle_actions();
    }
}

impl Drop for RadarApp {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            let _ = running.session.stop();
        }
        self.persist();
    }
}

const fn empty_snapshot() -> RadarSnapshot {
    RadarSnapshot {
        revision: 0,
        session: 0,
        status: SessionStatus::Stopped,
        objects: BTreeMap::new(),
        recent: Vec::new(),
        notices: Vec::new(),
        frames_seen: 0,
        last_frame: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicU64, Ordering};

    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable as _;

    use crate::test_support::{key, object, property, snapshot};

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT_DIR: AtomicU64 = AtomicU64::new(1);
            let path = std::env::temp_dir().join(format!(
                "echonet-radar-app-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("temporary directory");
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("settings.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn app() -> RadarApp {
        RadarApp::new(
            Startup {
                settings: Settings::default(),
                notice: None,
                path: None,
                stored_interface: InterfaceChoice::Auto,
                interface_override: None,
            },
            Vec::new(),
            egui::Context::default(),
        )
    }

    fn texts(output: &mut egui::FullOutput) -> Vec<(String, egui::Color32)> {
        // A frame the test does not render leaves glyph deltas behind, and
        // dropping them panics while a failing assertion unwinds.
        output.textures_delta.clear();
        let mut collected = Vec::new();
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut collected);
        }
        collected
    }

    fn collect(
        shape: &egui::Shape,
        collected: &mut Vec<(String, egui::Color32)>,
    ) {
        match shape {
            egui::Shape::Text(text) => {
                collected.push((text.galley.text().to_owned(), text_color(text)));
            },
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, collected);
                }
            },
            _ => {},
        }
    }

    fn text_color(text: &egui::epaint::TextShape) -> egui::Color32 {
        match text
            .galley
            .job
            .sections
            .first()
            .map(|section| section.format.color)
        {
            Some(color) if color != egui::Color32::PLACEHOLDER => color,
            _ => text.override_text_color.unwrap_or(text.fallback_color),
        }
    }

    fn has_text(
        output: &mut egui::FullOutput,
        needle: &str,
    ) -> bool {
        texts(output)
            .iter()
            .any(|(text, _color)| text.contains(needle))
    }

    fn two_devices(now: SystemTime) -> RadarSnapshot {
        let mut snap = snapshot(now);
        let meter = object(
            Ipv4Addr::new(192, 168, 1, 20),
            0x0288,
            "Low-voltage smart electric energy meter",
            BTreeMap::from([(0xE7, property(&[0, 0, 2, 0x44], now))]),
            now,
        );
        let conditioner = object(
            Ipv4Addr::new(192, 168, 1, 30),
            0x0130,
            "Home air conditioner",
            BTreeMap::from([(0x80, property(&[0x30], now))]),
            now,
        );
        snap.objects.insert(meter.key, meter);
        snap.objects.insert(conditioner.key, conditioner);
        snap
    }

    #[test]
    fn the_start_card_shows_its_button() {
        let ctx = egui::Context::default();
        let mut app = app();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.render(ui));
        assert!(has_text(&mut output, "Start listening"));
        assert!(has_text(&mut output, "ECHONET Radar"));
        assert!(
            app.running.is_none(),
            "the app must not bind before the click"
        );
        assert_eq!(app.readiness, Readiness::NotStarted);
    }

    #[test]
    fn a_snapshot_renders_the_class_name_and_value() {
        let ctx = egui::Context::default();
        let mut app = app();
        let now = SystemTime::now();
        app.snapshot = Arc::new(two_devices(now));
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show_started(ui));
        assert!(has_text(
            &mut output,
            "Low-voltage smart electric energy meter"
        ));
        assert!(has_text(&mut output, "value"));
        assert!(has_text(&mut output, "192.168.1.20"));
        assert!(has_text(&mut output, "2 devices"));
    }

    #[test]
    fn a_stale_row_is_muted() {
        let ctx = egui::Context::default();
        let mut app = app();
        let now = SystemTime::now();
        let mut snap = two_devices(now);
        let stale_seen = now - Duration::from_mins(15);
        let stale = object(
            Ipv4Addr::new(192, 168, 1, 40),
            0x0130,
            "Home air conditioner",
            BTreeMap::from([(0x80, property(&[0x30], stale_seen))]),
            stale_seen,
        );
        snap.objects.insert(stale.key, stale);
        app.snapshot = Arc::new(snap);
        let mut visuals = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            visuals = Some(ui.style().visuals.clone());
            app.show_started(ui);
        });
        let drawn = texts(&mut output);
        let visuals = visuals.expect("a frame with a style");
        let fresh_age = drawn
            .iter()
            .find(|(text, _)| text.ends_with("s ago"))
            .map(|(_, color)| *color)
            .expect("a fresh age is drawn");
        let stale_age = drawn
            .iter()
            .find(|(text, _)| text.contains("min ago"))
            .map(|(_, color)| *color)
            .expect("a stale age is drawn");
        assert_eq!(fresh_age, visuals.text_color());
        assert_eq!(stale_age, visuals.weak_text_color());
        assert_ne!(fresh_age, stale_age, "a stale row must not look fresh");
    }

    #[test]
    fn the_property_pane_follows_selection() {
        let ctx = egui::Context::default();
        let mut app = app();
        let snap = two_devices(SystemTime::now());
        let meter = key(Ipv4Addr::new(192, 168, 1, 20), 0x0288);
        let conditioner = key(Ipv4Addr::new(192, 168, 1, 30), 0x0130);
        app.snapshot = Arc::new(snap);

        app.selection = Some(meter);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show_started(ui));
        let drawn = texts(&mut output);
        assert_eq!(
            drawn
                .iter()
                .filter(|(text, _)| text == "Low-voltage smart electric energy meter")
                .count(),
            2,
            "the row and the property heading both name the class"
        );
        assert!(drawn.iter().any(|(text, _)| text == "0xE7"));

        app.selection = Some(conditioner);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show_started(ui));
        let drawn = texts(&mut output);
        let pane_epcs: Vec<&String> = drawn
            .iter()
            .map(|(text, _)| text)
            .filter(|text| text.starts_with("0x") && text.len() == 4)
            .collect();
        assert_eq!(pane_epcs, vec!["0x80"], "one property row per pane");
    }

    #[test]
    fn clicking_a_row_selects_it() {
        struct ListState {
            snapshot: RadarSnapshot,
            selection: Option<ObjectKey>,
        }
        let snap = two_devices(SystemTime::now());
        let conditioner = key(Ipv4Addr::new(192, 168, 1, 30), 0x0130);
        let mut harness = Harness::new_ui_state(
            |ui, state: &mut ListState| {
                views::devices::device_list(
                    ui,
                    &state.snapshot,
                    &mut state.selection,
                    Duration::from_secs(15),
                );
            },
            ListState {
                snapshot: snap,
                selection: None,
            },
        );
        harness.get_by_label("Home air conditioner").click();
        harness.run();
        assert_eq!(harness.state().selection, Some(conditioner));
    }

    #[test]
    fn the_selection_clears_when_the_lan_lacks_the_device() {
        let mut app = app();
        let now = SystemTime::now();
        let mut snap = snapshot(now);
        let present = object(
            Ipv4Addr::new(192, 168, 1, 30),
            0x0130,
            "Home air conditioner",
            BTreeMap::new(),
            now,
        );
        snap.objects.insert(present.key, present);
        app.selection = Some(key(Ipv4Addr::new(192, 168, 1, 20), 0x0288));
        app.settings.selected = app.selection;
        app.snapshot = Arc::new(snap);
        app.poll_session();
        assert_eq!(app.selection, None);
        assert_eq!(app.settings.selected, None);
    }

    #[test]
    fn an_empty_snapshot_keeps_the_restored_selection() {
        let mut app = app();
        app.selection = Some(key(Ipv4Addr::new(192, 168, 1, 20), 0x0288));
        app.settings.selected = app.selection;
        app.snapshot = Arc::new(snapshot(SystemTime::now()));
        app.poll_session();
        assert_eq!(app.selection, app.settings.selected);
        assert!(app.selection.is_some());
    }

    #[test]
    fn arrow_keys_move_the_selection_in_table_order() {
        let mut app = app();
        app.snapshot = Arc::new(two_devices(SystemTime::now()));
        app.move_selection(true);
        let first = app.selection.expect("a selection after one step");
        assert_eq!(
            first.ip,
            Ipv4Addr::new(192, 168, 1, 30),
            "class name sorts first"
        );
        app.move_selection(true);
        let last = app.selection.expect("a selection after two steps");
        assert_eq!(last.ip, Ipv4Addr::new(192, 168, 1, 20));
        app.move_selection(true);
        assert_eq!(app.selection, Some(last), "the last row is the end");
        app.move_selection(false);
        assert_eq!(
            app.selection.map(|selected| selected.ip),
            Some(Ipv4Addr::new(192, 168, 1, 30))
        );
    }

    #[test]
    fn the_interface_override_is_not_persisted_until_the_picker_changes() {
        let dir = TempDir::new();
        let startup_settings = Settings {
            interface: InterfaceChoice::Named(String::from("br0")),
            ..Settings::default()
        };
        let mut app = RadarApp::new(
            Startup {
                settings: startup_settings,
                notice: None,
                path: Some(dir.file()),
                stored_interface: InterfaceChoice::Auto,
                interface_override: Some(String::from("br0")),
            },
            Vec::new(),
            egui::Context::default(),
        );

        app.persist();
        let (stored, _notice) = settings::load_from(&dir.file());
        assert_eq!(
            stored.interface,
            InterfaceChoice::Auto,
            "the override must not reach the file"
        );
        assert_eq!(
            app.settings.interface,
            InterfaceChoice::Named(String::from("br0"))
        );

        app.settings.interface = InterfaceChoice::Named(String::from("eth0"));
        app.clear_stale_override();
        assert!(app.interface_override.is_none());
        app.persist();
        let (stored, _notice) = settings::load_from(&dir.file());
        assert_eq!(
            stored.interface,
            InterfaceChoice::Named(String::from("eth0"))
        );
    }

    #[test]
    fn a_start_on_a_missing_interface_falls_back_to_automatic() {
        let mut app = app();
        app.settings.interface = InterfaceChoice::Named(String::from("vanish0"));
        app.start();
        assert!(matches!(
            app.start_failure,
            Some(StartFailure::InterfaceMissing(ref name)) if name == "vanish0"
        ));
        assert_eq!(app.settings.interface, InterfaceChoice::Auto);
        assert!(app.notice.is_some());
        assert!(app.running.is_none());
    }

    #[test]
    fn a_settings_edit_is_written_after_the_debounce() {
        let dir = TempDir::new();
        let mut app = RadarApp::new(
            Startup {
                settings: Settings::default(),
                notice: None,
                path: Some(dir.file()),
                stored_interface: InterfaceChoice::Auto,
                interface_override: None,
            },
            Vec::new(),
            egui::Context::default(),
        );
        app.settle();
        assert!(!dir.file().exists(), "an unchanged app must not write");

        app.settings.update_interval = Duration::from_secs(30);
        app.settle();
        assert!(app.dirty_since.is_some(), "a change starts the debounce");
        assert!(!dir.file().exists(), "the write must wait one second");

        std::thread::sleep(SETTINGS_DEBOUNCE + Duration::from_millis(50));
        app.settle();
        assert!(app.dirty_since.is_none());
        let (stored, _notice) = settings::load_from(&dir.file());
        assert_eq!(stored.update_interval, Duration::from_secs(30));
    }

    #[test]
    fn kittest_finds_the_start_button_in_the_full_app() {
        let harness = Harness::new_ui_state(|ui, app: &mut RadarApp| app.render(ui), app());
        harness.get_by_label("Start listening");
        harness.get_by_label("Interface");
    }

    #[test]
    fn a_degraded_status_bar_offers_restart() {
        let mut app = app();
        app.readiness = Readiness::Degraded {
            fault: Fault::Service(String::from("socket closed")),
            last_frame: None,
        };
        app.snapshot = Arc::new(snapshot(SystemTime::now()));
        let mut harness = Harness::new_ui_state(|ui, app: &mut RadarApp| app.show_started(ui), app);
        harness.get_by_label("Restart").click();
        harness.run();
        assert!(harness.state().actions.restart);
    }

    #[test]
    fn the_settings_panel_names_the_group_and_both_bound_addresses() {
        let mut app = app();
        app.snapshot = Arc::new(two_devices(SystemTime::now()));
        let mut harness = Harness::new_ui_state(|ui, app: &mut RadarApp| app.show_started(ui), app);
        harness.get_by_label("Settings and diagnostics").click();
        harness.run();
        harness.get_by_label_contains("224.0.23.0:3610");
        harness.get_by_label_contains("Bound addresses 0.0.0.0:3610");
    }

    #[test]
    fn egui_pulse_requests_a_repaint() {
        let ctx = egui::Context::default();
        let pulse = EguiPulse(ctx.clone());
        let mut output = ctx.run_ui(egui::RawInput::default(), |_ui| pulse.pulse());
        let repaint = output
            .viewport_output
            .values()
            .any(|viewport| viewport.repaint_delay.is_zero());
        output.textures_delta.clear();
        assert!(repaint, "the pulse must ask for a repaint");
    }
}
