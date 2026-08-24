use std::collections::VecDeque;
use std::error::Error;
use std::io;
use std::net::Ipv4Addr;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use echonet_lite_udp::EchoNetSocket;
use echonet_radar::{
    ChangeEvent, Command, DEFAULT_DISCOVERY_INTERVAL, DEFAULT_UPDATE_INTERVAL, RadarConfig,
    RadarEvent, run_service,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

/// Maximum number of change events kept in the feed.
const MAX_EVENTS: usize = 1000;

#[derive(Debug, Parser)]
#[command(
    name = "echonet-radar",
    about = "Log ECHONET Lite device state changes as a time-series feed"
)]
struct Arguments {
    /// IPv4 interface used for multicast membership.
    #[arg(long, default_value = "0.0.0.0", value_name = "IP")]
    interface: Ipv4Addr,
    /// Discovery interval in seconds.
    #[arg(long, default_value_t = DEFAULT_DISCOVERY_INTERVAL.as_secs(), value_name = "SECONDS")]
    discovery_interval_seconds: u64,
    /// Value-polling interval in seconds.
    #[arg(long, default_value_t = DEFAULT_UPDATE_INTERVAL.as_secs(), value_name = "SECONDS")]
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
    let network_sender = event_sender.clone();
    let network_config = config;
    let network_thread = thread::Builder::new()
        .name(String::from("echonet-radar-network"))
        .spawn(move || {
            run_network(
                network_config,
                network_sender,
                command_receiver,
                shutdown_receiver,
            )
        })?;
    drop(event_sender);

    let terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = shutdown_sender.send(true);
            let _ = network_thread.join();
            return Err(error.into());
        },
    };
    let terminal_result = run_terminal(terminal, &event_receiver, &command_sender);
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
    events: std::sync::mpsc::Sender<RadarEvent>,
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

/// The event feed rendered by the terminal, newest change first.
struct Feed {
    events: VecDeque<ChangeEvent>,
    status: String,
    scroll: usize,
}

impl Feed {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            status: String::from("starting"),
            scroll: 0,
        }
    }

    fn push_change(
        &mut self,
        change: ChangeEvent,
    ) {
        self.events.push_front(change);
        while self.events.len() > MAX_EVENTS {
            self.events.pop_back();
        }
    }

    const fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(1);
    }

    const fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }
}

fn run_terminal(
    mut terminal: DefaultTerminal,
    events: &Receiver<RadarEvent>,
    commands: &tokio::sync::mpsc::Sender<Command>,
) -> io::Result<()> {
    let mut feed = Feed::new();
    // Redraw only when new events or input arrive. INF telegrams surface as
    // `Change` events, so they are rendered within one poll cycle.
    let mut dirty = true;
    loop {
        match events.try_recv() {
            Ok(RadarEvent::Change(change)) => {
                feed.push_change(change);
                dirty = true;
            },
            Ok(RadarEvent::Status(status)) => {
                feed.status = status;
                dirty = true;
            },
            Err(TryRecvError::Empty) => {},
            Err(TryRecvError::Disconnected) => {
                feed.status = String::from("network service stopped");
                dirty = true;
            },
        }

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('r' | 'R') => {
                    // Send value GETs to all known devices right away.
                    let _ = commands.try_send(Command::PollNow);
                },
                KeyCode::Up => {
                    feed.scroll_up();
                    dirty = true;
                },
                KeyCode::Down => {
                    feed.scroll_down();
                    dirty = true;
                },
                KeyCode::Home => {
                    feed.scroll = 0;
                    dirty = true;
                },
                KeyCode::End => {
                    feed.scroll = feed.events.len().saturating_sub(1);
                    dirty = true;
                },
                _ => {},
            }
        }

        if dirty {
            terminal.draw(|frame| render(frame, &feed))?;
            dirty = false;
        }
    }
}

fn render(
    frame: &mut Frame,
    feed: &Feed,
) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(frame.area());

    let status = Line::from(vec![
        Span::styled(
            "echonet-radar",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {} | events: {}", feed.status, feed.events.len())),
    ]);
    frame.render_widget(
        Paragraph::new(status).block(Block::bordered().title(" Status ")),
        areas[0],
    );

    let table_area = areas[1];
    let footer = areas[2];
    let visible = table_area.height.saturating_sub(2);
    let rows = feed
        .events
        .iter()
        .skip(feed.scroll)
        .take(usize::from(visible))
        .map(event_row);
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["Time", "EOJ", "EPC", "EDT"]).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .column_spacing(1)
    .block(Block::bordered().title(format!(
        " State changes (newest first, {}/{} ) ",
        feed.events.len().saturating_sub(feed.scroll),
        feed.events.len()
    )));
    frame.render_widget(table, table_area);

    frame.render_widget(
        Paragraph::new("r: poll now | ↑/↓ scroll | Home/End jump | q / Esc: quit")
            .style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}

fn event_row(event: &ChangeEvent) -> Row<'static> {
    let eoj = format!(
        "0x{:02X}{:02X}{:02X}",
        event.eoj.class_group, event.eoj.class, event.eoj.instance
    );
    Row::new([
        Cell::from(format_time(event.at)),
        Cell::from(eoj),
        Cell::from(format!("0x{:02X}", event.epc)),
        Cell::from(event.edt.clone()),
    ])
}

fn format_time(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let h = (seconds / 3600) % 24;
    let m = (seconds / 60) % 60;
    let s = seconds % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use echonet_lite::frame::Eoj;
    use std::net::SocketAddr;

    fn change(
        at: SystemTime,
        class_group: u8,
        class: u8,
        instance: u8,
        epc: u8,
        edt: &str,
    ) -> ChangeEvent {
        ChangeEvent {
            at,
            source: "192.0.2.1:3610".parse::<SocketAddr>().unwrap(),
            eoj: Eoj::new(class_group, class, instance),
            epc,
            edt: String::from(edt),
        }
    }

    #[test]
    fn feed_is_bounded_and_newest_first() {
        let mut feed = Feed::new();
        for index in 0..MAX_EVENTS + 10 {
            feed.push_change(change(
                UNIX_EPOCH,
                0x01,
                0x30,
                0x01,
                0x80,
                &index.to_string(),
            ));
        }
        assert_eq!(feed.events.len(), MAX_EVENTS);
        assert_eq!(feed.events[0].edt, (MAX_EVENTS + 9).to_string());
        assert_eq!(feed.events[MAX_EVENTS - 1].edt, 10.to_string());
    }

    #[test]
    fn scroll_does_not_underflow() {
        let mut feed = Feed::new();
        feed.scroll_up();
        assert_eq!(feed.scroll, 0);
    }

    #[test]
    fn scroll_down_is_bounded_by_content() {
        let mut feed = Feed::new();
        feed.scroll_down();
        feed.scroll_down();
        assert_eq!(feed.scroll, 2);
    }

    #[test]
    fn time_is_formatted_as_utc_hh_mm_ss() {
        // 1970-01-01 12:34:56 UTC.
        let time = UNIX_EPOCH + Duration::from_secs(12 * 3600 + 34 * 60 + 56);
        assert_eq!(format_time(time), "12:34:56");
    }
}
