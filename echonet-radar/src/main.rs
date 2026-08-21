use std::error::Error;
use std::fmt::Write as _;
use std::io;
use std::net::Ipv4Addr;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use echonet_lite_udp::EchoNetSocket;
use echonet_radar::{
    DEFAULT_DISCOVERY_INTERVAL, DEFAULT_UPDATE_INTERVAL, DEFAULT_UPDATE_JITTER, DeviceKey,
    DeviceSnapshot, RadarConfig, RadarSnapshot, run_service,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

#[derive(Debug, Parser)]
#[command(
    name = "echonet-radar",
    about = "Discover and monitor ECHONET Lite devices in a terminal"
)]
struct Arguments {
    /// IPv4 interface used for multicast membership.
    #[arg(long, default_value = "0.0.0.0", value_name = "IP")]
    interface: Ipv4Addr,
    /// Discovery interval in seconds.
    #[arg(long, default_value_t = DEFAULT_DISCOVERY_INTERVAL.as_secs(), value_name = "SECONDS")]
    discovery_interval_seconds: u64,
    /// Base value-update interval in seconds.
    #[arg(long, default_value_t = DEFAULT_UPDATE_INTERVAL.as_secs(), value_name = "SECONDS")]
    update_interval_seconds: u64,
    /// Maximum positive jitter added to each value update in seconds.
    #[arg(long, default_value_t = DEFAULT_UPDATE_JITTER.as_secs(), value_name = "SECONDS")]
    update_jitter_seconds: u64,
}

impl Arguments {
    fn config(self) -> Result<RadarConfig, Box<dyn Error>> {
        let config = RadarConfig {
            interface: self.interface,
            discovery_interval: Duration::from_secs(self.discovery_interval_seconds),
            update_interval: Duration::from_secs(self.update_interval_seconds),
            update_jitter: Duration::from_secs(self.update_jitter_seconds),
        };
        config.validate()?;
        Ok(config)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = Arguments::parse().config()?;
    let (snapshot_sender, snapshot_receiver) = mpsc::channel();
    let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
    let network_sender = snapshot_sender.clone();
    let network_config = config;
    let network_thread = thread::Builder::new()
        .name(String::from("echonet-radar-network"))
        .spawn(move || run_network(network_config, network_sender, shutdown_receiver))?;
    drop(snapshot_sender);

    let terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = shutdown_sender.send(true);
            let _ = network_thread.join();
            return Err(error.into());
        },
    };
    let terminal_result = run_terminal(terminal, &snapshot_receiver, config);
    let restore_result = ratatui::try_restore();
    let _ = shutdown_sender.send(true);
    let network_result = network_thread
        .join()
        .map_err(|_| io::Error::other("network thread panicked"))?;

    terminal_result?;
    restore_result?;
    network_result?;
    Ok(())
}

fn run_network(
    config: RadarConfig,
    snapshots: std::sync::mpsc::Sender<RadarSnapshot>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let error_sender = snapshots.clone();
    let result = runtime.block_on(async move {
        let socket = EchoNetSocket::bind_default_multicast(config.interface).await?;
        run_service(socket, config, snapshots, shutdown).await
    });
    if let Err(error) = &result {
        let _ = error_sender.send(RadarSnapshot::with_status(format!(
            "network error: {error}"
        )));
    }
    result
}

struct Dashboard {
    snapshot: RadarSnapshot,
    config: RadarConfig,
    selected: Option<DeviceKey>,
    expanded: Option<DeviceKey>,
}

impl Dashboard {
    fn new(config: RadarConfig) -> Self {
        Self {
            snapshot: RadarSnapshot::empty(),
            config,
            selected: None,
            expanded: None,
        }
    }

    fn select_next(&mut self) {
        self.move_selection(false);
    }

    fn select_previous(&mut self) {
        self.move_selection(true);
    }

    fn move_selection(
        &mut self,
        backward: bool,
    ) {
        let count = self.snapshot.devices.len();
        if count == 0 {
            self.selected = None;
            return;
        }
        let index = self.selected.and_then(|key| {
            self.snapshot
                .devices
                .iter()
                .position(|device| device.key == key)
        });
        let next = match index {
            None => 0,
            Some(index) if backward && index == 0 => count - 1,
            Some(index) if !backward && index + 1 >= count => 0,
            Some(index) if backward => index - 1,
            Some(index) => index + 1,
        };
        self.selected = Some(self.snapshot.devices[next].key);
    }

    fn toggle_expanded(&mut self) {
        let Some(key) = self.selected else {
            return;
        };
        self.expanded = if self.expanded == Some(key) {
            None
        } else {
            Some(key)
        };
    }
}

fn run_terminal(
    mut terminal: DefaultTerminal,
    snapshots: &Receiver<RadarSnapshot>,
    config: RadarConfig,
) -> io::Result<()> {
    let mut dashboard = Dashboard::new(config);
    loop {
        match snapshots.try_recv() {
            Ok(snapshot) => dashboard.snapshot = snapshot,
            Err(TryRecvError::Empty) => {},
            Err(TryRecvError::Disconnected) => {
                if dashboard.snapshot.status == "starting"
                    || dashboard.snapshot.status == "waiting for discovery"
                {
                    dashboard.snapshot.status = String::from("network service stopped");
                }
            },
        }

        terminal.draw(|frame| render(frame, &dashboard))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Up => dashboard.select_previous(),
                KeyCode::Down => dashboard.select_next(),
                KeyCode::Enter => dashboard.toggle_expanded(),
                _ => {},
            }
        }
    }
}

fn render(
    frame: &mut Frame,
    dashboard: &Dashboard,
) {
    let areas = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(5),
        Constraint::Length(2),
    ])
    .split(frame.area());
    let now = Instant::now();
    let snapshot = &dashboard.snapshot;

    let title = Line::from(vec![
        Span::styled(
            "echonet-radar",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  ECHONET Lite device radar"),
    ]);
    let schedule = format!(
        "{} | devices: {} | discovery: {}s / next {} | values: {}s + jitter <= {}s / next {}",
        snapshot.status,
        snapshot.devices.len(),
        dashboard.config.discovery_interval.as_secs(),
        format_until(now, snapshot.next_discovery),
        dashboard.config.update_interval.as_secs(),
        dashboard.config.update_jitter.as_secs(),
        format_until(now, snapshot.next_update),
    );
    frame.render_widget(
        Paragraph::new(vec![title, Line::from(schedule)])
            .block(Block::bordered().title(" Status ")),
        areas[0],
    );

    let rows = dashboard
        .snapshot
        .devices
        .iter()
        .map(|device| device_row(device, dashboard));
    let table = Table::new(
        rows,
        [
            Constraint::Length(22),
            Constraint::Length(17),
            Constraint::Min(30),
            Constraint::Length(12),
        ],
    )
    .header(
        Row::new(["Address", "EOJ", "Values", "Updated"]).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .column_spacing(1)
    .block(Block::bordered().title(" Devices "));
    frame.render_widget(table, areas[1]);

    frame.render_widget(
        Paragraph::new(
            "↑/↓ select | Enter expand/collapse | q / Esc: quit | discovery is multicast, value reads are unicast",
        )
        .style(Style::default().fg(Color::DarkGray)),
        areas[2],
    );
}

fn device_row(
    device: &DeviceSnapshot,
    dashboard: &Dashboard,
) -> Row<'static> {
    let expanded = dashboard.expanded == Some(device.key);
    let selected = dashboard.selected == Some(device.key);
    let marker = if expanded {
        "▾ "
    } else if selected {
        "▶ "
    } else {
        "  "
    };
    let address = format!("{marker}{}", device.key.address.ip());
    let eoj = format!(
        "0x{:04X}/0x{:02X}",
        device.key.eoj.class_code(),
        device.key.eoj.instance
    );
    let values = if expanded {
        expanded_values(device)
    } else {
        Cell::from(summary_value(device))
    };
    let updated = device.last_update.map_or_else(
        || String::from("never"),
        |updated| format_age(Instant::now(), updated),
    );
    let row = Row::new([
        Cell::from(address),
        Cell::from(eoj),
        values,
        Cell::from(updated),
    ]);
    let row = if expanded {
        // Ratatui rows default to a single line; give the expanded value list
        // one line per property so the per-EPC detail is visible.
        let height = u16::try_from(device.values.len())
            .unwrap_or(u16::MAX)
            .max(1);
        row.height(height)
    } else {
        row
    };
    if selected {
        row.style(Style::default().fg(Color::Cyan))
    } else {
        row
    }
}

fn summary_value(device: &DeviceSnapshot) -> String {
    if device.values.is_empty() {
        String::from("waiting for value response")
    } else {
        device
            .values
            .iter()
            .map(|value| format!("{}={}", value.name, value.value))
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

fn expanded_values(device: &DeviceSnapshot) -> Cell<'static> {
    if device.values.is_empty() {
        return Cell::from(String::from("waiting for value response"));
    }
    let lines: Vec<Line<'static>> = device
        .values
        .iter()
        .map(|value| {
            let mut spans = vec![Span::styled(
                format!("EPC 0x{:02X} ", value.epc),
                Style::default().fg(Color::DarkGray),
            )];
            // Unknown properties carry the EPC string as their name, which
            // would repeat the EPC prefix already shown.
            if value.name != format!("EPC 0x{:02X}", value.epc) {
                spans.push(Span::raw(format!("{} ", value.name)));
            }
            spans.extend([
                Span::styled(
                    format!("EDT {}", format_edt(&value.edt)),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(format!(" = {}", value.value)),
            ]);
            Line::from(spans)
        })
        .collect();
    Cell::from(Text::from(lines))
}

fn format_edt(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::from("(empty)");
    }
    let mut hex = String::with_capacity(bytes.len() * 3 - 1);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            hex.push(' ');
        }
        let _ = write!(hex, "{byte:02X}");
    }
    hex
}

fn format_until(
    now: Instant,
    deadline: Instant,
) -> String {
    let seconds = deadline.saturating_duration_since(now).as_secs();
    if seconds == 0 {
        String::from("<1s")
    } else {
        format!("{seconds}s")
    }
}

fn format_age(
    now: Instant,
    timestamp: Instant,
) -> String {
    let seconds = now.saturating_duration_since(timestamp).as_secs();
    if seconds == 0 {
        String::from("<1s ago")
    } else {
        format!("{seconds}s ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echonet_lite::frame::Eoj;

    fn key(index: u8) -> DeviceKey {
        DeviceKey {
            address: format!("192.0.2.{index}:3610").parse().unwrap(),
            eoj: Eoj::new(0x01, 0x30, index),
        }
    }

    fn device(index: u8) -> DeviceSnapshot {
        DeviceSnapshot {
            key: key(index),
            values: Vec::new(),
            last_seen: Instant::now(),
            last_update: None,
        }
    }

    fn dashboard(device_count: u8) -> Dashboard {
        Dashboard {
            snapshot: RadarSnapshot {
                devices: (0..device_count).map(device).collect(),
                ..RadarSnapshot::empty()
            },
            config: RadarConfig::default(),
            selected: None,
            expanded: None,
        }
    }

    #[test]
    fn selection_starts_at_first_device_and_wraps() {
        let mut dashboard = dashboard(3);

        dashboard.select_next();
        assert_eq!(dashboard.selected, Some(key(0)));

        dashboard.select_next();
        assert_eq!(dashboard.selected, Some(key(1)));

        dashboard.select_previous();
        assert_eq!(dashboard.selected, Some(key(0)));

        dashboard.select_previous();
        assert_eq!(dashboard.selected, Some(key(2)));
    }

    #[test]
    fn selection_is_ignored_without_devices() {
        let mut dashboard = dashboard(0);
        dashboard.select_next();
        dashboard.select_previous();
        assert_eq!(dashboard.selected, None);
    }

    #[test]
    fn selection_survives_snapshot_reordering() {
        let mut dashboard = Dashboard {
            snapshot: RadarSnapshot {
                devices: vec![device(2), device(6), device(1)],
                ..RadarSnapshot::empty()
            },
            ..dashboard(0)
        };
        dashboard.selected = Some(key(6));
        dashboard.select_next();
        assert_eq!(dashboard.selected, Some(key(1)));

        dashboard.select_previous();
        assert_eq!(dashboard.selected, Some(key(6)));
    }

    #[test]
    fn enter_toggles_the_selected_row_accordion_style() {
        let mut dashboard = dashboard(3);

        // Nothing selected: Enter leaves the accordion closed.
        dashboard.toggle_expanded();
        assert_eq!(dashboard.expanded, None);

        dashboard.select_next();
        dashboard.toggle_expanded();
        assert_eq!(dashboard.expanded, Some(key(0)));

        // Selecting another row and pressing Enter collapses the first.
        dashboard.select_next();
        dashboard.toggle_expanded();
        assert_eq!(dashboard.expanded, Some(key(1)));

        // Enter on the expanded row collapses it.
        dashboard.toggle_expanded();
        assert_eq!(dashboard.expanded, None);
    }

    #[test]
    fn selected_row_survives_device_removal() {
        let mut dashboard = dashboard(2);
        dashboard.selected = Some(key(1));
        dashboard.snapshot.devices.pop();

        dashboard.select_next();
        assert_eq!(dashboard.selected, Some(key(0)));
    }

    #[test]
    fn expanded_edt_is_hex_formatted() {
        assert_eq!(format_edt(&[0x01, 0x1E]), "01 1E");
        assert_eq!(format_edt(&[]), "(empty)");
    }

    fn value(
        epc: u8,
        name: &str,
        value: &str,
        edt: Vec<u8>,
    ) -> echonet_radar::ValueSnapshot {
        echonet_radar::ValueSnapshot {
            epc,
            name: String::from(name),
            value: String::from(value),
            edt,
            updated_at: Instant::now(),
        }
    }

    fn render_text(buffer: &ratatui::buffer::Buffer) -> String {
        let width = buffer.area.width as usize;
        buffer
            .content()
            .chunks(width)
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn collapsed_row_shows_value_summary() {
        let mut dashboard = dashboard(1);
        dashboard.snapshot.devices[0].values = vec![
            value(0x80, "Operation status", "true", vec![0x30]),
            value(0xBB, "Room temperature", "01 1E", vec![0x01, 0x1E]),
        ];
        dashboard.selected = Some(key(0));

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| render(frame, &dashboard)).unwrap();
        let text = render_text(terminal.backend().buffer());

        assert!(text.contains("Operation status=true | Room temperature=01 1E"));
        assert!(!text.contains("EDT"));
        assert!(!text.contains("EPC 0x"));
    }

    #[test]
    fn expanded_row_shows_epc_and_edt_lines() {
        let mut dashboard = dashboard(1);
        dashboard.snapshot.devices[0].values = vec![
            value(0x80, "Operation status", "true", vec![0x30]),
            value(0xBB, "Room temperature", "01 1E", vec![0x01, 0x1E]),
        ];
        dashboard.selected = Some(key(0));
        dashboard.expanded = Some(key(0));

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| render(frame, &dashboard)).unwrap();
        let text = render_text(terminal.backend().buffer());

        assert!(text.contains("▾ "));
        assert!(text.contains("EPC 0x80"));
        assert!(text.contains("EDT 30"));
        assert!(text.contains("= true"));
        assert!(text.contains("EPC 0xBB"));
        assert!(text.contains("EDT 01 1E"));
        assert!(text.contains("Room temperature"));
    }

    #[test]
    fn expanded_row_does_not_duplicate_epc_fallback_name() {
        let mut dashboard = dashboard(1);
        // 0x84 is unknown for class 0x0130, so its name is the EPC fallback.
        dashboard.snapshot.devices[0].values =
            vec![value(0x84, "EPC 0x84", "00 64", vec![0x00, 0x64])];
        dashboard.selected = Some(key(0));
        dashboard.expanded = Some(key(0));

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| render(frame, &dashboard)).unwrap();
        let text = render_text(terminal.backend().buffer());

        let line = text.lines().find(|line| line.contains("EPC 0x84")).unwrap();
        assert_eq!(line.matches("EPC 0x84").count(), 1);
        assert!(line.contains("EDT 00 64"));
        assert!(line.contains("= 00 64"));
    }

    #[test]
    fn expanded_empty_device_keeps_waiting_text() {
        let mut dashboard = dashboard(1);
        dashboard.selected = Some(key(0));
        dashboard.expanded = Some(key(0));

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| render(frame, &dashboard)).unwrap();
        let text = render_text(terminal.backend().buffer());

        assert!(text.contains("waiting for value response"));
    }
}
