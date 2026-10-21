use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::format_eoj;
use chrono::{DateTime, Local, Utc};
use echonet_lite::frame::Eoj;
use echonet_lite_udp::{EchoNetSocket, MULTICAST_GROUP, MULTICAST_PORT};
use echonet_radar_core::{
    Command, RadarConfig, RadarEvent, format_cumulative_energy_kwh, format_value,
    instantaneous_watts, power_epc, run_service,
};
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, BorderType, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table,
};

const ACCENT: Color = Color::Cyan;
const LABEL: Color = Color::DarkGray;
const VALUE: Color = Color::White;
const BORDER: Color = Color::DarkGray;
const FRESH: Color = Color::Green;
const STALE: Color = Color::Yellow;
const EXPIRED: Color = Color::Red;

const SAMPLE_CAPACITY: usize = 120;
const SAMPLE_PERIOD: Duration = Duration::from_secs(1);
const STALE_POLLS: u32 = 3;
/// The utility meter first, then the distribution board.
const POWER_PRIORITY: [u16; 2] = [0x0288, 0x0287];
const SOLAR_CLASS_CODE: u16 = 0x0279;
const SOLAR: Color = Color::LightYellow;
const KEY_POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Series {
    Consumption,
    Solar,
}

const SERIES: [Series; 2] = [Series::Consumption, Series::Solar];

impl Series {
    const fn index(self) -> usize {
        match self {
            Self::Consumption => 0,
            Self::Solar => 1,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Consumption => "consumption",
            Self::Solar => "solar",
        }
    }

    const fn color(self) -> Color {
        match self {
            Self::Consumption => VALUE,
            Self::Solar => SOLAR,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    watts: [Option<i64>; SERIES.len()],
}

impl Sample {
    const fn get(
        self,
        series: Series,
    ) -> Option<i64> {
        self.watts[series.index()]
    }
}

enum SeriesState {
    Waiting {
        detail: String,
    },
    Current {
        watts: i64,
        age: Duration,
        detail: Option<String>,
    },
    Stale {
        last: i64,
        age: Duration,
        detail: Option<String>,
    },
}

#[derive(Clone, Copy)]
struct KeyItem {
    epc: u8,
    label: &'static str,
}

const KEY_ITEMS: &[(u16, &[KeyItem])] = &[
    (
        0x0011,
        &[KeyItem {
            epc: 0xE0,
            label: "Temp",
        }],
    ),
    (
        0x0012,
        &[KeyItem {
            epc: 0xE0,
            label: "Humidity",
        }],
    ),
    (
        0x0013,
        &[KeyItem {
            epc: 0xE0,
            label: "Lux",
        }],
    ),
    (
        0x0130,
        &[
            KeyItem {
                epc: 0x80,
                label: "Status",
            },
            KeyItem {
                epc: 0xB0,
                label: "Mode",
            },
            KeyItem {
                epc: 0xB3,
                label: "Set",
            },
            KeyItem {
                epc: 0xBB,
                label: "Room",
            },
        ],
    ),
    (
        0x026B,
        &[
            KeyItem {
                epc: 0xC3,
                label: "Hot water",
            },
            KeyItem {
                epc: 0xD1,
                label: "Supply",
            },
            KeyItem {
                epc: 0xD3,
                label: "Bath",
            },
            KeyItem {
                epc: 0xB1,
                label: "Auto",
            },
        ],
    ),
    (
        0x027D,
        &[KeyItem {
            epc: 0xE4,
            label: "Level",
        }],
    ),
];

struct ObjectRow {
    address: SocketAddr,
    eoj: Eoj,
    values: BTreeMap<u8, StoredValue>,
    last_seen: SystemTime,
}

impl ObjectRow {
    const fn new(
        address: SocketAddr,
        eoj: Eoj,
        at: SystemTime,
    ) -> Self {
        Self {
            address,
            eoj,
            values: BTreeMap::new(),
            last_seen: at,
        }
    }
}

struct StoredValue {
    raw: Vec<u8>,
    text: String,
}

#[derive(Clone, Copy)]
struct PowerSource<'a> {
    row: &'a ObjectRow,
    watts: i64,
}

struct Dashboard {
    config: RadarConfig,
    started: Instant,
    status: Option<(SystemTime, String)>,
    objects: BTreeMap<(IpAddr, u16, u8), ObjectRow>,
    samples: VecDeque<Sample>,
    scroll: usize,
    visible_rows: usize,
}

impl Dashboard {
    fn new(config: RadarConfig) -> Self {
        Self {
            config,
            started: Instant::now(),
            status: None,
            objects: BTreeMap::new(),
            samples: VecDeque::new(),
            scroll: 0,
            visible_rows: 0,
        }
    }

    fn apply(
        &mut self,
        event: RadarEvent,
    ) {
        match event {
            RadarEvent::Device(device) => {
                self.row_mut(device.source, device.eoj).last_seen = SystemTime::now();
            },
            RadarEvent::Change(change) => {
                let class_code = change.eoj.class_code();
                let text = cell_text(class_code, change.epc, &change.raw_edt);
                let row = self.row_mut(change.source, change.eoj);
                row.last_seen = change.at;
                row.values.insert(
                    change.epc,
                    StoredValue {
                        raw: change.raw_edt,
                        text,
                    },
                );
            },
            RadarEvent::Seen(seen) => {
                self.row_mut(seen.source, seen.eoj).last_seen = seen.at;
            },
            RadarEvent::Status(status) => {
                self.status = Some((SystemTime::now(), status));
            },
        }
    }

    fn row_mut(
        &mut self,
        address: SocketAddr,
        eoj: Eoj,
    ) -> &mut ObjectRow {
        self.objects
            .entry((address.ip(), eoj.class_code(), eoj.instance))
            .or_insert_with(|| ObjectRow::new(address, eoj, SystemTime::now()))
    }

    fn sample(
        &mut self,
        now: SystemTime,
    ) {
        let mut sample = Sample {
            watts: [None; SERIES.len()],
        };
        for series in SERIES {
            sample.watts[series.index()] = match series {
                Series::Consumption => self.consumption_watts(now),
                Series::Solar => self.solar_watts(now),
            };
        }
        if self.samples.len() == SAMPLE_CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    fn scroll_by(
        &mut self,
        delta: isize,
    ) {
        let max = self.objects.len().saturating_sub(self.visible_rows);
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    const fn stale_after(&self) -> Duration {
        self.config.update_interval.saturating_mul(STALE_POLLS)
    }

    fn is_fresh(
        &self,
        row: &ObjectRow,
        now: SystemTime,
    ) -> bool {
        now.duration_since(row.last_seen).unwrap_or(Duration::ZERO) <= self.stale_after()
    }

    /// The freshest metering object in [`POWER_PRIORITY`], with its reading.
    fn power_source(
        &self,
        now: SystemTime,
    ) -> Option<PowerSource<'_>> {
        POWER_PRIORITY.iter().find_map(|class_code| {
            self.objects
                .values()
                .filter(|row| row.eoj.class_code() == *class_code && self.is_fresh(row, now))
                .find_map(|row| power_reading(row, *class_code))
        })
    }

    /// The most recent reading from any metering object, fresh or not.
    fn last_power_source(&self) -> Option<PowerSource<'_>> {
        POWER_PRIORITY.iter().find_map(|class_code| {
            self.objects
                .values()
                .filter(|row| row.eoj.class_code() == *class_code)
                .find_map(|row| power_reading(row, *class_code))
        })
    }

    fn metering_object(&self) -> Option<&ObjectRow> {
        POWER_PRIORITY.iter().find_map(|class_code| {
            self.objects
                .values()
                .find(|row| row.eoj.class_code() == *class_code)
        })
    }

    fn pv_objects(&self) -> impl Iterator<Item = &ObjectRow> {
        self.objects
            .values()
            .filter(|row| row.eoj.class_code() == SOLAR_CLASS_CODE)
    }

    fn has_pv(&self) -> bool {
        self.pv_objects().next().is_some()
    }

    fn solar_units(&self) -> Vec<PowerSource<'_>> {
        self.pv_objects()
            .filter_map(|row| power_reading(row, SOLAR_CLASS_CODE))
            .collect()
    }

    fn metering_objects(&self) -> impl Iterator<Item = &ObjectRow> {
        self.objects.values().filter(|row| {
            let class_code = row.eoj.class_code();
            class_code == SOLAR_CLASS_CODE || POWER_PRIORITY.contains(&class_code)
        })
    }

    fn consumption_watts(
        &self,
        now: SystemTime,
    ) -> Option<i64> {
        self.power_source(now).map(|source| source.watts)
    }

    /// A stale unit invalidates the total: summing the rest would read as a
    /// real drop in generation.
    fn solar_watts(
        &self,
        now: SystemTime,
    ) -> Option<i64> {
        let units = self.solar_units();
        if units.is_empty() || units.iter().any(|unit| !self.is_fresh(unit.row, now)) {
            return None;
        }
        Some(total_watts(&units))
    }

    fn consumption_state(
        &self,
        now: SystemTime,
    ) -> SeriesState {
        if let Some(source) = self.power_source(now) {
            return SeriesState::Current {
                watts: source.watts,
                age: reading_age(now, source.row.last_seen),
                detail: Some(format_eoj(source.row.eoj)),
            };
        }
        if let Some(source) = self.last_power_source() {
            return SeriesState::Stale {
                last: source.watts,
                age: reading_age(now, source.row.last_seen),
                detail: None,
            };
        }
        let detail = self.metering_object().map_or_else(
            || String::from("no consumption meter"),
            |row| format!("{} · waiting for the first poll", format_eoj(row.eoj)),
        );
        SeriesState::Waiting { detail }
    }

    fn solar_state(
        &self,
        now: SystemTime,
    ) -> SeriesState {
        let pv_count = self.pv_objects().count();
        if pv_count == 0 {
            return SeriesState::Waiting {
                detail: String::from("no PV unit"),
            };
        }
        let units = self.solar_units();
        if units.is_empty() {
            let answered = self.pv_objects().any(|row| {
                power_epc(SOLAR_CLASS_CODE).is_some_and(|epc| row.values.contains_key(&epc))
            });
            return SeriesState::Waiting {
                detail: String::from(if answered {
                    "PV reports no value"
                } else {
                    "waiting for the first poll"
                }),
            };
        }
        let age = units
            .iter()
            .map(|unit| reading_age(now, unit.row.last_seen))
            .max()
            .unwrap_or(Duration::ZERO);
        let partial = (units.len() < pv_count).then(|| format!("{} of {pv_count} PV", units.len()));
        let stale = units
            .iter()
            .filter(|unit| !self.is_fresh(unit.row, now))
            .count();
        let stale_detail = partial.clone().or_else(|| {
            (stale > 0 && stale < units.len())
                .then(|| format!("{stale} of {} PV stale", units.len()))
        });
        self.solar_watts(now).map_or_else(
            || SeriesState::Stale {
                last: total_watts(&units),
                age,
                detail: stale_detail,
            },
            |watts| SeriesState::Current {
                watts,
                age,
                detail: partial,
            },
        )
    }

    fn series_state(
        &self,
        series: Series,
        now: SystemTime,
    ) -> SeriesState {
        match series {
            Series::Consumption => self.consumption_state(now),
            Series::Solar => self.solar_state(now),
        }
    }

    fn series_stats(
        &self,
        series: Series,
        seed: Option<i64>,
    ) -> Option<(i64, i64)> {
        let mut peak = seed?;
        let mut total = 0i64;
        let mut count = 0i64;
        for watts in self.samples.iter().filter_map(|sample| sample.get(series)) {
            peak = peak.max(watts);
            total = total.saturating_add(watts);
            count = count.saturating_add(1);
        }
        Some((peak, total.checked_div(count).unwrap_or(peak)))
    }

    fn series_line(
        &self,
        series: Series,
        state: SeriesState,
    ) -> Line<'static> {
        let mut spans = vec![Span::styled(
            format!(" {}", series.label()),
            Style::new().fg(series.color()),
        )];
        let seed = match &state {
            SeriesState::Waiting { .. } => None,
            SeriesState::Current { watts, .. } => Some(*watts),
            SeriesState::Stale { last, .. } => Some(*last),
        };
        // The detail plus the stats do not fit the 80-column layout this
        // panel targets.
        let show_stats = !matches!(
            &state,
            SeriesState::Stale {
                detail: Some(_),
                ..
            }
        );
        match state {
            SeriesState::Waiting { detail } => {
                spans.push(Span::styled(format!(" · {detail}"), dim()));
            },
            SeriesState::Current { watts, age, detail } => {
                spans.push(Span::styled(
                    format!(" {watts} W"),
                    Style::new().fg(VALUE).add_modifier(Modifier::BOLD),
                ));
                if let Some(detail) = detail {
                    spans.push(Span::styled(format!(" · {detail}"), dim()));
                }
                spans.push(Span::styled(
                    format!(" · {} ago", format_age(age)),
                    Style::new().fg(age_color(self.config.update_interval, age)),
                ));
            },
            SeriesState::Stale { last, age, detail } => {
                if let Some(detail) = detail {
                    spans.push(Span::styled(format!(" · {detail}"), dim()));
                }
                spans.push(Span::styled(
                    format!(" · no reading for {} · last {last} W", format_age(age)),
                    Style::new().fg(EXPIRED),
                ));
            },
        }
        if show_stats && let Some((peak, average)) = self.series_stats(series, seed) {
            spans.push(Span::styled(
                format!("   peak {peak} W   avg {average} W"),
                dim(),
            ));
        }
        Line::from(spans)
    }

    fn draw(
        &mut self,
        frame: &mut Frame,
    ) {
        let [header, power, devices, status, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(9),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.draw_header(frame, header);
        self.draw_power(frame, power);
        self.draw_devices(frame, devices);
        self.draw_status(frame, status);
        frame.render_widget(
            Paragraph::new(Line::from(" q quit · p poll now · j/k scroll").style(dim())),
            footer,
        );
    }

    fn draw_header(
        &self,
        frame: &mut Frame,
        area: Rect,
    ) {
        let devices = match self.objects.len() {
            1 => String::from("1 device"),
            count => format!("{count} devices"),
        };
        let text = format!(
            " echonet-radar-cli · {} · poll {}s · {devices} · up {}",
            self.config.interface,
            self.config.update_interval.as_secs(),
            format_uptime(self.started.elapsed()),
        );
        frame.render_widget(Paragraph::new(Line::from(text)), area);
    }

    fn draw_power(
        &self,
        frame: &mut Frame,
        area: Rect,
    ) {
        let now = SystemTime::now();
        let mut visible = vec![Series::Consumption];
        if self.has_pv() {
            visible.push(Series::Solar);
        }
        let states: Vec<SeriesState> = visible
            .iter()
            .map(|series| self.series_state(*series, now))
            .collect();
        if states
            .iter()
            .all(|state| matches!(state, SeriesState::Waiting { .. }))
        {
            let (title, body) = self.metering_objects().next().map_or_else(
                || {
                    (
                        String::from(" Power · no metering object"),
                        "waiting for a metering object",
                    )
                },
                |row| {
                    (
                        format!(" Power · {} · no reading yet", format_eoj(row.eoj)),
                        "waiting for the first poll",
                    )
                },
            );
            let block = panel(Line::from(Span::styled(title, accent())));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(Paragraph::new(Line::from(body).style(dim())), inner);
            return;
        }

        let block = panel(Line::from(Span::styled(" Power", accent())));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let summary_height = u16::try_from(visible.len()).unwrap_or(1);
        let [summary, chart_area] =
            Layout::vertical([Constraint::Length(summary_height), Constraint::Min(0)]).areas(inner);
        let lines: Vec<Line<'static>> = visible
            .iter()
            .zip(states)
            .map(|(series, state)| self.series_line(*series, state))
            .collect();
        frame.render_widget(Paragraph::new(lines), summary);
        self.draw_chart(frame, chart_area, &visible);
    }

    fn draw_chart(
        &self,
        frame: &mut Frame,
        area: Rect,
        visible: &[Series],
    ) {
        let runs = self.chart_runs(visible);
        let datasets: Vec<Dataset<'_>> = runs
            .iter()
            .map(|(series, run)| {
                Dataset::default()
                    .marker(Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::new().fg(series.color()))
                    .fill_to_y(0.0)
                    .data(run.as_slice())
            })
            .collect();
        let chart = Chart::new(datasets)
            .x_axis(Axis::default().bounds([
                0.0,
                chart_coordinate(i64::try_from(SAMPLE_CAPACITY - 1).unwrap_or(i64::MAX)),
            ]))
            .y_axis(Axis::default().bounds(self.chart_bounds()))
            .legend_position(None);
        frame.render_widget(chart, area);
    }

    fn chart_runs(
        &self,
        visible: &[Series],
    ) -> Vec<(Series, Vec<(f64, f64)>)> {
        let mut runs = Vec::new();
        let offset = SAMPLE_CAPACITY.saturating_sub(self.samples.len());
        for series in visible {
            let mut run = Vec::new();
            for (index, sample) in self.samples.iter().enumerate() {
                if let Some(watts) = sample.get(*series) {
                    let x = chart_coordinate(i64::try_from(offset + index).unwrap_or(i64::MAX));
                    run.push((x, chart_coordinate(watts)));
                } else if !run.is_empty() {
                    runs.push((*series, std::mem::take(&mut run)));
                }
            }
            if !run.is_empty() {
                runs.push((*series, run));
            }
        }
        runs
    }

    fn chart_bounds(&self) -> [f64; 2] {
        let mut min = 0i64;
        let mut max = 0i64;
        for sample in &self.samples {
            for series in SERIES {
                if let Some(watts) = sample.get(series) {
                    min = min.min(watts);
                    max = max.max(watts);
                }
            }
        }
        if min == 0 && max == 0 {
            return [0.0, 1.0];
        }
        let padding = (chart_coordinate(max.saturating_sub(min)) * 0.05).max(1.0);
        [
            chart_coordinate(min) - padding,
            chart_coordinate(max) + padding,
        ]
    }

    fn draw_devices(
        &mut self,
        frame: &mut Frame,
        area: Rect,
    ) {
        self.visible_rows = usize::from(area.height.saturating_sub(3));
        self.scroll = self
            .scroll
            .min(self.objects.len().saturating_sub(self.visible_rows));
        let title = format!(" Devices ({}) ", self.objects.len());
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(BORDER))
            .title(title);
        if self.objects.is_empty() {
            let waiting = format!(
                " waiting for discovery on {MULTICAST_GROUP}:{MULTICAST_PORT} via {}",
                self.config.interface
            );
            frame.render_widget(
                Paragraph::new(Line::from(waiting).style(dim())).block(block),
                area,
            );
            return;
        }
        let header = Row::new(["EOJ", "ADDRESS", "ENERGY", "KEY VALUES", "SEEN"])
            .style(Style::new().fg(LABEL));
        let rows: Vec<Row<'_>> = self
            .objects
            .values()
            .skip(self.scroll)
            .take(self.visible_rows)
            .map(|row| device_row(self.config.update_interval, row))
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(9),
                Constraint::Length(15),
                Constraint::Length(15),
                Constraint::Min(0),
                Constraint::Length(5),
            ],
        )
        .header(header)
        .column_spacing(1)
        .block(block);
        frame.render_widget(table, area);
    }

    fn draw_status(
        &self,
        frame: &mut Frame,
        area: Rect,
    ) {
        let text = self
            .status
            .as_ref()
            .map_or_else(String::new, |(at, message)| {
                let time = DateTime::<Utc>::from(*at).with_timezone(&Local);
                format!(" {} {message}", time.format("%H:%M:%S"))
            });
        frame.render_widget(Paragraph::new(Line::from(text).style(dim())), area);
    }
}

fn panel(title: Line<'static>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(title)
}

const fn accent() -> Style {
    Style::new().fg(ACCENT)
}

const fn dim() -> Style {
    Style::new().fg(LABEL)
}

fn device_row(
    update_interval: Duration,
    row: &ObjectRow,
) -> Row<'_> {
    let age = row.last_seen.elapsed().unwrap_or(Duration::ZERO);
    let seen = Line::from(format_age(age))
        .alignment(Alignment::Right)
        .style(Style::new().fg(age_color(update_interval, age)));
    Row::new(vec![
        Cell::from(format_eoj(row.eoj)),
        Cell::from(row.address.ip().to_string()),
        energy_cell(row),
        Cell::from(key_values(row)),
        Cell::from(seen),
    ])
}

// The counter, unit, and coefficient arrive as separate properties, so decode
// from the row's current values instead of caching at event time.
fn energy_text(row: &ObjectRow) -> Option<String> {
    format_cumulative_energy_kwh(row.eoj.class_code(), |epc| {
        row.values.get(&epc).map(|value| value.raw.as_slice())
    })
}

fn energy_cell(row: &ObjectRow) -> Cell<'static> {
    let line = energy_text(row).map_or_else(|| Line::from("--").style(dim()), Line::from);
    Cell::from(line.alignment(Alignment::Right))
}

fn key_items(class_code: u16) -> Vec<KeyItem> {
    let mut items = Vec::new();
    if let Some(epc) = power_epc(class_code) {
        items.push(KeyItem {
            epc,
            label: power_label(class_code),
        });
    }
    if let Some((_, class_items)) = KEY_ITEMS.iter().find(|(class, _)| *class == class_code) {
        items.extend_from_slice(class_items);
    }
    items
}

fn power_reading(
    row: &ObjectRow,
    class_code: u16,
) -> Option<PowerSource<'_>> {
    let epc = power_epc(class_code)?;
    let stored = row.values.get(&epc)?;
    let watts = instantaneous_watts(class_code, &stored.raw)?;
    Some(PowerSource { row, watts })
}

fn total_watts(units: &[PowerSource<'_>]) -> i64 {
    units
        .iter()
        .fold(0i64, |total, unit| total.saturating_add(unit.watts))
}

fn reading_age(
    now: SystemTime,
    last_seen: SystemTime,
) -> Duration {
    now.duration_since(last_seen).unwrap_or(Duration::ZERO)
}

fn cell_text(
    class_code: u16,
    epc: u8,
    raw: &[u8],
) -> String {
    format_value(class_code, epc, raw).replace(" Celsius", " C")
}

fn key_values(row: &ObjectRow) -> Line<'_> {
    let class_code = row.eoj.class_code();
    let power = power_epc(class_code);
    let mut spans = Vec::new();
    for item in key_items(class_code) {
        let Some(stored) = row.values.get(&item.epc) else {
            continue;
        };
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(item.label, Style::new().fg(LABEL)));
        spans.push(Span::raw(" "));
        if power == Some(item.epc)
            && let Some(watts) = instantaneous_watts(class_code, &stored.raw)
        {
            spans.push(Span::styled(format!("{watts} W"), Style::new().fg(VALUE)));
            continue;
        }
        spans.push(Span::styled(stored.text.as_str(), Style::new().fg(VALUE)));
    }
    Line::from(spans)
}

const fn power_label(class_code: u16) -> &'static str {
    match class_code {
        0x0279 => "Gen",
        0x027D => "Battery",
        _ => "Power",
    }
}

fn age_color(
    update_interval: Duration,
    age: Duration,
) -> Color {
    if age < update_interval.saturating_mul(2) {
        FRESH
    } else if age < update_interval.saturating_mul(5) {
        STALE
    } else {
        EXPIRED
    }
}

fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

fn format_uptime(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        total % 3600 / 60,
        total % 60
    )
}

/// Chart coordinates are floats; sample indices and watt values are far inside
/// the range `f64` represents exactly.
#[allow(clippy::cast_precision_loss)]
const fn chart_coordinate(value: i64) -> f64 {
    value as f64
}

/// Run the dashboard until the user quits, then return the service's result.
///
/// # Errors
///
/// Returns the bind or terminal I/O error, or the error the radar service
/// stopped with.
pub fn run(config: RadarConfig) -> io::Result<()> {
    let (event_sender, events) = mpsc::channel();
    let (commands, command_receiver) = tokio::sync::mpsc::channel(8);
    let (shutdown, shutdown_receiver) = tokio::sync::watch::channel(false);

    let mut terminal = ratatui::try_init()?;
    let guard = TerminalGuard::new(shutdown);

    // The socket must be created inside the runtime that drives it: Tokio
    // registers a socket's readiness with the reactor of the runtime that
    // created it, and a socket handed over from another runtime never becomes
    // writable here.
    let service = thread::Builder::new()
        .name(String::from("echonet-radar-dashboard"))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let socket = EchoNetSocket::bind_default_multicast(config.interface).await?;
                run_service(
                    socket,
                    config,
                    event_sender,
                    command_receiver,
                    shutdown_receiver,
                )
                .await
            })
        })?;

    let mut dashboard = Dashboard::new(config);
    let drawn = event_loop(&mut terminal, &mut dashboard, &events, &commands);
    drop(guard);

    let joined = service
        .join()
        .map_err(|_| io::Error::other("dashboard service thread panicked"))?;
    drawn?;
    joined
}

struct TerminalGuard {
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl TerminalGuard {
    const fn new(shutdown: tokio::sync::watch::Sender<bool>) -> Self {
        Self { shutdown }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
        let _ = self.shutdown.send(true);
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    dashboard: &mut Dashboard,
    events: &Receiver<RadarEvent>,
    commands: &tokio::sync::mpsc::Sender<Command>,
) -> io::Result<()> {
    let mut last_sample = Instant::now();
    loop {
        loop {
            match events.try_recv() {
                Ok(event) => dashboard.apply(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        if last_sample.elapsed() >= SAMPLE_PERIOD {
            dashboard.sample(SystemTime::now());
            last_sample = Instant::now();
        }
        terminal.draw(|frame| dashboard.draw(frame))?;
        if !event::poll(KEY_POLL)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Char('p') => {
                if commands.try_send(Command::PollNow).is_err() {
                    dashboard.status = Some((
                        SystemTime::now(),
                        String::from("poll request dropped: the service queue is full"),
                    ));
                }
            },
            KeyCode::Char('j') | KeyCode::Down => dashboard.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => dashboard.scroll_by(-1),
            _ => {},
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    use echonet_radar_core::{ChangeEvent, DeviceEvent, SeenEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell as BufferCell;

    const INTERFACE: Ipv4Addr = Ipv4Addr::new(192, 168, 100, 30);

    fn dashboard(update_seconds: u64) -> Dashboard {
        Dashboard::new(RadarConfig {
            interface: INTERFACE,
            discovery_interval: Duration::from_secs(60),
            update_interval: Duration::from_secs(update_seconds),
        })
    }

    fn source(ip: &str) -> SocketAddr {
        format!("{ip}:3610").parse().unwrap()
    }

    fn device(
        ip: &str,
        class_group: u8,
        class: u8,
        instance: u8,
    ) -> RadarEvent {
        RadarEvent::Device(DeviceEvent {
            source: source(ip),
            eoj: Eoj::new(class_group, class, instance),
        })
    }

    fn change(
        at: SystemTime,
        ip: &str,
        eoj: Eoj,
        epc: u8,
        raw: &[u8],
    ) -> RadarEvent {
        RadarEvent::Change(ChangeEvent {
            at,
            source: source(ip),
            eoj,
            epc,
            text: format_value(eoj.class_code(), epc, raw),
            raw_edt: raw.to_vec(),
        })
    }

    fn seen(
        at: SystemTime,
        ip: &str,
        eoj: Eoj,
    ) -> RadarEvent {
        RadarEvent::Seen(SeenEvent {
            at,
            source: source(ip),
            eoj,
        })
    }

    fn screen(
        dashboard: &mut Dashboard,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| dashboard.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(BufferCell::symbol)
            .collect()
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn sample(
        consumption: Option<i64>,
        solar: Option<i64>,
    ) -> Sample {
        let mut sample = Sample {
            watts: [None; SERIES.len()],
        };
        sample.watts[Series::Consumption.index()] = consumption;
        sample.watts[Series::Solar.index()] = solar;
        sample
    }

    #[test]
    fn key_items_put_the_power_property_first() {
        let heater = key_items(0x026B);
        assert_eq!(heater.first().map(|item| item.label), Some("Hot water"));

        let board = key_items(0x0287);
        assert_eq!(
            board.first().map(|item| (item.epc, item.label)),
            Some((0xC6, "Power"))
        );
        assert_eq!(board.len(), 1);

        let solar = key_items(0x0279);
        assert_eq!(
            solar.first().map(|item| (item.epc, item.label)),
            Some((0xE0, "Gen"))
        );
        assert_eq!(
            solar.len(),
            1,
            "the PV on this LAN omits its cumulative total from the Get map"
        );

        assert_eq!(
            key_items(0x027D).first().map(|item| item.label),
            Some("Battery")
        );
        assert_eq!(
            key_items(0x0288).len(),
            1,
            "the smart meter's cumulative energy needs its coefficient, so it is not a key item"
        );
        assert!(key_items(0xFFFF).is_empty());
    }

    fn stored(raw: &[u8]) -> StoredValue {
        StoredValue {
            raw: raw.to_vec(),
            text: String::new(),
        }
    }

    fn object_row(
        class_group: u8,
        class: u8,
    ) -> ObjectRow {
        ObjectRow::new(
            source("192.168.100.26"),
            Eoj::new(class_group, class, 0x01),
            SystemTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn energy_text_decodes_the_counter_with_its_unit() {
        let mut board = object_row(0x02, 0x87);
        board.values.insert(0xC0, stored(&[0x00, 0xDB, 0x1D, 0xAA]));
        board.values.insert(0xC2, stored(&[0x03]));
        assert_eq!(energy_text(&board).as_deref(), Some("14359.978 kWh"));

        let mut meter = object_row(0x02, 0x88);
        meter.values.insert(0xE0, stored(&[0x00, 0x00, 0x30, 0x39]));
        meter.values.insert(0xE1, stored(&[0x01]));
        meter.values.insert(0xD3, stored(&[0x00, 0x00, 0x00, 0x02]));
        assert_eq!(energy_text(&meter).as_deref(), Some("2469 kWh"));

        let mut malformed = object_row(0x02, 0x87);
        malformed.values.insert(0xC0, stored(&[0xDB, 0x1D, 0xAA]));
        malformed.values.insert(0xC2, stored(&[0x03]));
        assert_eq!(energy_text(&malformed), None);

        let heater = object_row(0x02, 0x6B);
        assert_eq!(energy_text(&heater), None);
    }

    #[test]
    fn the_devices_table_shows_each_energy_counter() {
        for (width, height) in [(100, 30), (80, 24)] {
            let mut dashboard = dashboard(10);
            let now = SystemTime::now();
            let board = Eoj::new(0x02, 0x87, 0x01);
            let solar = Eoj::new(0x02, 0x79, 0x01);
            let heater = Eoj::new(0x02, 0x6B, 0x01);

            dashboard.apply(change(
                now,
                "192.168.100.26",
                board,
                0xC0,
                &[0x00, 0xDB, 0x1D, 0xAA],
            ));
            dashboard.apply(change(now, "192.168.100.26", board, 0xC2, &[0x03]));
            dashboard.apply(change(
                now,
                "192.168.100.26",
                solar,
                0xE1,
                &[0x00, 0xE7, 0x6A, 0xCF],
            ));
            dashboard.apply(change(now, "192.168.100.26", heater, 0xC3, &[0x42]));
            dashboard.apply(change(now, "192.168.100.26", heater, 0xD1, &[0x28]));
            dashboard.apply(change(now, "192.168.100.26", heater, 0xD3, &[0x28]));
            dashboard.apply(change(now, "192.168.100.26", heater, 0xB1, &[0x41]));

            let screen = screen(&mut dashboard, width, height);
            for needle in ["ENERGY", "14359.978 kWh", "15166.159 kWh"] {
                assert!(
                    screen.contains(needle),
                    "{width}x{height} screen is missing {needle:?}: {screen}"
                );
            }
            let heater_row = screen
                .split("0x026B01")
                .nth(1)
                .and_then(|tail| tail.split("0x027901").next())
                .expect("the heater row is rendered");
            assert!(
                heater_row.contains("--"),
                "{width}x{height}: the heater's energy cell is missing: {screen}"
            );
        }
    }

    #[test]
    fn a_stale_board_keeps_showing_its_energy() {
        let mut dashboard = dashboard(10);
        let old = SystemTime::now() - Duration::from_secs(60);
        let board = Eoj::new(0x02, 0x87, 0x01);

        dashboard.apply(change(
            old,
            "192.168.100.26",
            board,
            0xC0,
            &[0x00, 0xDB, 0x1D, 0xAA],
        ));
        dashboard.apply(change(old, "192.168.100.26", board, 0xC2, &[0x03]));

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("14359.978 kWh"), "{screen}");
    }

    #[test]
    fn apply_stores_values_and_seen_updates_liveness() {
        let mut dashboard = dashboard(10);
        let eoj = Eoj::new(0x02, 0x87, 0x01);
        let start = SystemTime::UNIX_EPOCH;

        dashboard.apply(device("192.168.100.26", 0x02, 0x87, 0x01));
        dashboard.apply(change(start, "192.168.100.26", eoj, 0xC6, &[0, 0, 2, 0x44]));

        let row = dashboard
            .power_source(start + Duration::from_secs(5))
            .map(|source| source.row)
            .unwrap();
        let stored = row.values.get(&0xC6).unwrap();
        assert_eq!(stored.raw, vec![0, 0, 2, 0x44]);
        assert_eq!(stored.text, "580 W");
        let inserted = row.last_seen;

        let later = inserted + Duration::from_secs(60);
        dashboard.apply(seen(later, "192.168.100.26", eoj));
        let row = dashboard
            .power_source(later)
            .map(|source| source.row)
            .unwrap();
        assert!(row.last_seen > inserted);
        assert_eq!(row.values.len(), 1);
        assert_eq!(
            row.values.get(&0xC6).map(|value| value.text.as_str()),
            Some("580 W")
        );
    }

    #[test]
    fn the_meter_is_the_power_source_before_the_board() {
        let mut dashboard = dashboard(10);
        let start = SystemTime::UNIX_EPOCH;
        let solar = Eoj::new(0x02, 0x79, 0x01);
        let board = Eoj::new(0x02, 0x87, 0x01);
        let meter = Eoj::new(0x02, 0x88, 0x01);

        dashboard.apply(change(start, "192.168.100.26", solar, 0xE0, &[0x04, 0xD8]));
        assert!(
            dashboard.power_source(start).is_none(),
            "PV is not a consumption source"
        );

        dashboard.apply(change(
            start,
            "192.168.100.26",
            board,
            0xC6,
            &[0, 0, 2, 0x44],
        ));
        assert_eq!(
            dashboard.power_source(start).map(|source| source.watts),
            Some(580)
        );

        dashboard.apply(change(
            start,
            "192.168.100.26",
            meter,
            0xE7,
            &[0, 0, 0x02, 0xBC],
        ));
        assert_eq!(
            dashboard.power_source(start).map(|source| source.watts),
            Some(700)
        );
    }

    #[test]
    fn a_fresh_board_is_used_when_the_meter_goes_quiet() {
        let mut dashboard = dashboard(10);
        let start = SystemTime::UNIX_EPOCH;
        let meter = Eoj::new(0x02, 0x88, 0x01);
        let board = Eoj::new(0x02, 0x87, 0x01);

        dashboard.apply(change(
            start,
            "192.168.100.26",
            meter,
            0xE7,
            &[0, 0, 0x02, 0xBC],
        ));
        dashboard.apply(change(
            start + Duration::from_secs(40),
            "192.168.100.26",
            board,
            0xC6,
            &[0, 0, 2, 0x44],
        ));

        assert_eq!(
            dashboard
                .power_source(start + Duration::from_secs(40))
                .map(|source| source.watts),
            Some(580)
        );
    }

    #[test]
    fn a_stale_source_samples_nothing() {
        let mut dashboard = dashboard(10);
        let start = SystemTime::UNIX_EPOCH;
        let eoj = Eoj::new(0x02, 0x87, 0x01);

        dashboard.apply(change(start, "192.168.100.26", eoj, 0xC6, &[0, 0, 2, 0x44]));
        dashboard.apply(seen(start, "192.168.100.26", eoj));

        dashboard.sample(start + Duration::from_secs(29));
        assert_eq!(
            dashboard
                .samples
                .back()
                .and_then(|sample| sample.get(Series::Consumption)),
            Some(580)
        );

        dashboard.sample(start + Duration::from_secs(31));
        assert_eq!(
            dashboard
                .samples
                .back()
                .and_then(|sample| sample.get(Series::Consumption)),
            None
        );
    }

    #[test]
    fn a_silent_meter_keeps_its_last_reading_out_of_the_headline() {
        let mut dashboard = dashboard(10);
        let start = SystemTime::UNIX_EPOCH;
        let eoj = Eoj::new(0x02, 0x87, 0x01);

        dashboard.apply(change(start, "192.168.100.26", eoj, 0xC6, &[0, 0, 2, 0x44]));
        dashboard.apply(seen(start, "192.168.100.26", eoj));

        let now = start + Duration::from_secs(31);
        assert_eq!(dashboard.consumption_watts(now), None);
        let state = dashboard.series_state(Series::Consumption, now);
        let line = text(&dashboard.series_line(Series::Consumption, state));
        assert!(line.contains("no reading for 31s"), "{line}");
        assert!(line.contains("last 580 W"), "{line}");
    }

    #[test]
    fn one_tick_samples_both_series() {
        let mut dashboard = dashboard(10);
        let start = SystemTime::UNIX_EPOCH;
        let ip = "192.168.100.26";
        dashboard.apply(change(
            start,
            ip,
            Eoj::new(0x02, 0x87, 0x01),
            0xC6,
            &[0, 0, 2, 0x44],
        ));
        dashboard.apply(change(
            start,
            ip,
            Eoj::new(0x02, 0x79, 0x01),
            0xE0,
            &[0x04, 0xD8],
        ));
        dashboard.apply(change(
            start,
            ip,
            Eoj::new(0x02, 0x79, 0x02),
            0xE0,
            &[0x01, 0x2C],
        ));

        dashboard.sample(start);
        let sample = dashboard.samples.back().unwrap();
        assert_eq!(sample.get(Series::Consumption), Some(580));
        assert_eq!(sample.get(Series::Solar), Some(1540));

        let state = dashboard.series_state(Series::Consumption, start + Duration::from_secs(5));
        match state {
            SeriesState::Current { age, .. } => {
                assert_eq!(age, Duration::from_secs(5));
            },
            _ => panic!("expected a current consumption reading"),
        }
    }

    #[test]
    fn a_pv_unit_that_never_reported_leaves_a_partial_note() {
        let mut dashboard = dashboard(10);
        let start = SystemTime::UNIX_EPOCH;
        let ip = "192.168.100.26";
        dashboard.apply(device(ip, 0x02, 0x79, 0x01));
        dashboard.apply(device(ip, 0x02, 0x79, 0x02));
        dashboard.apply(change(
            start,
            ip,
            Eoj::new(0x02, 0x79, 0x01),
            0xE0,
            &[0x04, 0xD8],
        ));

        dashboard.sample(start);
        assert_eq!(
            dashboard
                .samples
                .back()
                .and_then(|sample| sample.get(Series::Solar)),
            Some(1240)
        );
        let state = dashboard.series_state(Series::Solar, start);
        let line = text(&dashboard.series_line(Series::Solar, state));
        assert!(line.contains("1 of 2 PV"), "{line}");
    }

    #[test]
    fn a_zero_pv_reading_is_a_value_not_a_gap() {
        let mut dashboard = dashboard(10);
        let start = SystemTime::UNIX_EPOCH;
        let ip = "192.168.100.26";
        dashboard.apply(change(
            start,
            ip,
            Eoj::new(0x02, 0x79, 0x01),
            0xE0,
            &[0x00, 0x00],
        ));
        dashboard.apply(change(
            start,
            ip,
            Eoj::new(0x02, 0x79, 0x02),
            0xE0,
            &[0x00, 0x00],
        ));

        dashboard.sample(start);
        assert_eq!(
            dashboard
                .samples
                .back()
                .and_then(|sample| sample.get(Series::Solar)),
            Some(0)
        );
        let state = dashboard.series_state(Series::Solar, start);
        let line = text(&dashboard.series_line(Series::Solar, state));
        assert!(line.contains("solar 0 W"), "{line}");
    }

    #[test]
    fn a_stale_pv_unit_withholds_the_solar_total() {
        let mut dashboard = dashboard(10);
        let start = SystemTime::UNIX_EPOCH;
        let ip = "192.168.100.26";
        let first = Eoj::new(0x02, 0x79, 0x01);
        dashboard.apply(change(start, ip, first, 0xE0, &[0x04, 0xD8]));
        dashboard.apply(change(
            start,
            ip,
            Eoj::new(0x02, 0x79, 0x02),
            0xE0,
            &[0x01, 0x2C],
        ));
        dashboard.apply(seen(start + Duration::from_secs(20), ip, first));

        assert_eq!(
            dashboard.solar_watts(start + Duration::from_secs(29)),
            Some(1540)
        );
        let stale_at = start + Duration::from_secs(31);
        assert_eq!(dashboard.solar_watts(stale_at), None);
        let state = dashboard.series_state(Series::Solar, stale_at);
        let line = text(&dashboard.series_line(Series::Solar, state));
        assert!(line.contains("1 of 2 PV stale"), "{line}");
        assert!(line.contains("no reading for 31s"), "{line}");
        assert!(line.contains("last 1540 W"), "{line}");
    }

    #[test]
    fn a_stale_partial_solar_series_keeps_its_note() {
        let mut dashboard = dashboard(10);
        let start = SystemTime::UNIX_EPOCH;
        let ip = "192.168.100.26";
        dashboard.apply(device(ip, 0x02, 0x79, 0x01));
        dashboard.apply(device(ip, 0x02, 0x79, 0x02));
        dashboard.apply(change(
            start,
            ip,
            Eoj::new(0x02, 0x79, 0x01),
            0xE0,
            &[0x04, 0xD8],
        ));

        let stale_at = start + Duration::from_secs(31);
        assert_eq!(dashboard.solar_watts(stale_at), None);
        let state = dashboard.series_state(Series::Solar, stale_at);
        let line = text(&dashboard.series_line(Series::Solar, state));
        assert!(line.contains("1 of 2 PV"), "{line}");
        assert!(line.contains("no reading for 31s"), "{line}");
        assert!(line.contains("last 1240 W"), "{line}");
    }

    #[test]
    fn a_current_consumption_and_stale_solar_draw_both_lines() {
        let mut dashboard = dashboard(10);
        let now = SystemTime::now();
        let stale_at = now.checked_sub(Duration::from_secs(40)).unwrap();
        let ip = "192.168.100.26";
        dashboard.apply(change(
            now,
            ip,
            Eoj::new(0x02, 0x87, 0x01),
            0xC6,
            &[0, 0, 2, 0x44],
        ));
        dashboard.apply(change(
            stale_at,
            ip,
            Eoj::new(0x02, 0x79, 0x01),
            0xE0,
            &[0x04, 0xD8],
        ));
        dashboard.apply(change(
            stale_at,
            ip,
            Eoj::new(0x02, 0x79, 0x02),
            0xE0,
            &[0x01, 0x2C],
        ));
        dashboard.apply(seen(now, ip, Eoj::new(0x02, 0x79, 0x01)));

        for (width, height) in [(100, 30), (80, 24)] {
            let screen = screen(&mut dashboard, width, height);
            assert!(screen.contains("consumption 580 W"), "{screen}");
            assert!(screen.contains("1 of 2 PV stale"), "{screen}");
            assert!(screen.contains("last 1540 W"), "{screen}");
        }
    }

    #[test]
    fn a_pv_unit_reporting_no_value_is_not_waiting_for_its_first_poll() {
        let mut dashboard = dashboard(10);
        let start = SystemTime::UNIX_EPOCH;
        dashboard.apply(change(
            start,
            "192.168.100.26",
            Eoj::new(0x02, 0x79, 0x01),
            0xE0,
            &[0xFF, 0xFF],
        ));

        assert_eq!(dashboard.solar_watts(start), None);
        let state = dashboard.series_state(Series::Solar, start);
        let line = text(&dashboard.series_line(Series::Solar, state));
        assert!(line.contains("PV reports no value"), "{line}");
    }

    #[test]
    fn a_site_without_pv_keeps_the_consumption_only_layout() {
        let mut dashboard = dashboard(10);
        let now = SystemTime::now();
        dashboard.apply(change(
            now,
            "192.168.100.26",
            Eoj::new(0x02, 0x87, 0x01),
            0xC6,
            &[0, 0, 2, 0x44],
        ));

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("consumption 580 W"), "{screen}");
        assert!(!screen.contains("solar"), "{screen}");
    }

    #[test]
    fn a_solar_only_site_draws_the_chart() {
        let mut dashboard = dashboard(10);
        let now = SystemTime::now();
        dashboard.apply(change(
            now,
            "192.168.100.26",
            Eoj::new(0x02, 0x79, 0x01),
            0xE0,
            &[0x04, 0xD8],
        ));

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("no consumption meter"), "{screen}");
        assert!(screen.contains("solar 1240 W"), "{screen}");
    }

    #[test]
    fn each_series_keeps_its_own_gaps() {
        let mut dashboard = dashboard(10);
        for tick in [
            sample(Some(100), None),
            sample(Some(100), Some(1000)),
            sample(None, Some(1000)),
            sample(None, None),
        ] {
            dashboard.samples.push_back(tick);
        }

        let runs = dashboard.chart_runs(&SERIES);
        assert_eq!(runs.len(), 2, "one run per series");
        let first_x = |series: Series| {
            let (_, run) = runs.iter().find(|(owner, _)| *owner == series).unwrap();
            assert_eq!(run.len(), 2, "{series:?} run");
            run[0].0
        };
        let offset = first_x(Series::Solar) - first_x(Series::Consumption);
        assert!((offset - 1.0).abs() < f64::EPSILON, "{offset}");
    }

    #[test]
    fn peak_and_average_are_per_series() {
        let mut dashboard = dashboard(10);
        dashboard.samples.push_back(sample(Some(100), Some(1000)));
        dashboard.samples.push_back(sample(Some(300), Some(0)));

        assert_eq!(
            dashboard.series_stats(Series::Consumption, Some(300)),
            Some((300, 200))
        );
        assert_eq!(
            dashboard.series_stats(Series::Solar, Some(1000)),
            Some((1000, 500))
        );
    }

    #[test]
    fn chart_bounds_span_both_series() {
        let mut dashboard = dashboard(10);
        dashboard.samples.push_back(sample(Some(100), Some(1000)));

        let bounds = dashboard.chart_bounds();
        assert!(bounds[0] < 0.0, "{bounds:?}");
        assert!(bounds[1] > 1000.0, "{bounds:?}");
    }

    #[test]
    fn series_index_matches_the_series_order() {
        assert!(
            SERIES
                .iter()
                .enumerate()
                .all(|(index, series)| series.index() == index)
        );
    }

    #[test]
    fn samples_are_capped() {
        let mut dashboard = dashboard(10);
        for _ in 0..(SAMPLE_CAPACITY + 5) {
            dashboard.sample(SystemTime::now());
        }
        assert_eq!(dashboard.samples.len(), SAMPLE_CAPACITY);
    }

    #[test]
    fn scrolling_stops_at_the_first_and_last_page() {
        let mut dashboard = dashboard(10);
        for instance in 1..=6 {
            dashboard.apply(device("192.168.100.26", 0x02, 0x87, instance));
        }
        dashboard.visible_rows = 4;

        dashboard.scroll_by(100);
        assert_eq!(dashboard.scroll, 2);
        dashboard.scroll_by(-100);
        assert_eq!(dashboard.scroll, 0);
    }

    #[test]
    fn a_discovered_pv_unit_without_a_reading_waits_in_the_placeholder() {
        let mut dashboard = dashboard(10);
        dashboard.apply(device("192.168.100.26", 0x02, 0x79, 0x01));

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("0x027901 · no reading yet"), "{screen}");
        assert!(screen.contains("waiting for the first poll"), "{screen}");
    }

    #[test]
    fn the_empty_dashboard_names_where_it_listens() {
        let mut dashboard = dashboard(10);

        let screen = screen(&mut dashboard, 100, 30);
        assert!(
            screen.contains("waiting for discovery on 224.0.23.0:3610"),
            "the empty state is clipped: {screen}"
        );
        assert!(screen.contains("no metering object"), "{screen}");
        assert!(!screen.contains("solar"), "{screen}");
    }

    #[test]
    fn the_dashboard_renders_the_power_and_devices() {
        for (width, height) in [(100, 30), (80, 24)] {
            let mut dashboard = dashboard(10);
            let now = SystemTime::now();
            let heater = Eoj::new(0x02, 0x6B, 0x01);
            let heater_changes: [(u8, &[u8]); 4] = [
                (0xC3, &[0x42]),
                (0xD1, &[0x28]),
                (0xD3, &[0x28]),
                (0xB1, &[0x41]),
            ];
            for (epc, raw) in heater_changes {
                dashboard.apply(change(now, "192.168.100.26", heater, epc, raw));
            }
            dashboard.apply(change(
                now,
                "192.168.100.26",
                Eoj::new(0x02, 0x87, 0x01),
                0xC6,
                &[0, 0, 2, 0x44],
            ));
            dashboard.apply(change(
                now,
                "192.168.100.26",
                Eoj::new(0x02, 0x87, 0x01),
                0xC0,
                &[0x00, 0xDB, 0x1D, 0xAA],
            ));
            dashboard.apply(change(
                now,
                "192.168.100.26",
                Eoj::new(0x02, 0x87, 0x01),
                0xC2,
                &[0x03],
            ));
            dashboard.apply(change(
                now,
                "192.168.100.26",
                Eoj::new(0x02, 0x79, 0x01),
                0xE0,
                &[0x04, 0xD8],
            ));
            dashboard.apply(change(
                now,
                "192.168.100.26",
                Eoj::new(0x02, 0x79, 0x01),
                0xE1,
                &[0x00, 0xE7, 0x6A, 0xCF],
            ));
            dashboard.apply(change(
                now,
                "192.168.100.26",
                Eoj::new(0x02, 0x79, 0x02),
                0xE0,
                &[0x01, 0x2C],
            ));
            dashboard.sample(now);

            let screen = screen(&mut dashboard, width, height);
            let mut needles = vec![
                "Power",
                "580 W",
                "0x028701",
                "Devices",
                "consumption 580 W",
                "solar 1540 W",
                "0x027901",
                "0x027902",
                "ENERGY",
                "14359.978 kWh",
                "15166.159 kWh",
            ];
            if width == 100 {
                // At 80 columns the 15-column ENERGY field takes the tail of
                // KEY VALUES, where the heater's mode sits.
                needles.push("Auto ON");
            }
            for needle in needles {
                assert!(
                    screen.contains(needle),
                    "{width}x{height} screen is missing {needle:?}: {screen}"
                );
            }
        }
    }
}
