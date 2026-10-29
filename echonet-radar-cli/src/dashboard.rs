use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::format_eoj;
use chrono::{DateTime, Local, Utc};
use echonet_lite_udp::{MULTICAST_GROUP, MULTICAST_PORT};
use echonet_radar_core::{
    CommandError, Fault, NoopPulse, ObjectKey, ObjectSnapshot, PropertySnapshot, RadarHandle,
    RadarSession, RadarSnapshot, SessionConfig, SessionStatus, format_value, instantaneous_watts,
    power_epc,
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
    Grid,
    Solar,
}

const SERIES: [Series; 2] = [Series::Grid, Series::Solar];

impl Series {
    const fn index(self) -> usize {
        match self {
            Self::Grid => 0,
            Self::Solar => 1,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::Solar => "solar",
        }
    }

    const fn color(self) -> Color {
        match self {
            Self::Grid => VALUE,
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
struct PowerSource<'a> {
    object: &'a ObjectSnapshot,
    watts: i64,
}

struct Dashboard {
    config: SessionConfig,
    snapshot: Arc<RadarSnapshot>,
    started: Instant,
    status: Option<(SystemTime, String)>,
    samples: VecDeque<Sample>,
    selected: Option<ObjectKey>,
    first: usize,
    visible_rows: usize,
}

impl Dashboard {
    fn new(
        config: SessionConfig,
        snapshot: Arc<RadarSnapshot>,
    ) -> Self {
        Self {
            config,
            status: status_line(&snapshot),
            snapshot,
            started: Instant::now(),
            samples: VecDeque::new(),
            selected: None,
            first: 0,
            visible_rows: 0,
        }
    }

    fn apply(
        &mut self,
        snapshot: Arc<RadarSnapshot>,
    ) {
        let restarted = snapshot.session != self.snapshot.session;
        let vanished = self
            .selected
            .is_some_and(|key| !snapshot.objects.contains_key(&key));
        if restarted || vanished {
            self.selected = None;
        }
        self.status = status_line(&snapshot);
        self.snapshot = snapshot;
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
                Series::Grid => self.grid_watts(now),
                Series::Solar => self.solar_watts(now),
            };
        }
        if self.samples.len() == SAMPLE_CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    fn selected_key(&self) -> Option<ObjectKey> {
        self.selected
            .filter(|key| self.snapshot.objects.contains_key(key))
            .or_else(|| self.snapshot.objects.keys().next().copied())
    }

    fn selected_index(&self) -> Option<usize> {
        let key = self.selected_key()?;
        self.snapshot
            .objects
            .keys()
            .position(|candidate| *candidate == key)
    }

    fn select_by(
        &mut self,
        delta: isize,
    ) {
        let Some(index) = self.selected_index() else {
            return;
        };
        let last = self.snapshot.objects.len().saturating_sub(1);
        let next = index.saturating_add_signed(delta).min(last);
        self.selected = self.snapshot.objects.keys().nth(next).copied();
    }

    const fn stale_after(&self) -> Duration {
        self.config.update_interval.saturating_mul(STALE_POLLS)
    }

    fn is_fresh(
        &self,
        object: &ObjectSnapshot,
        now: SystemTime,
    ) -> bool {
        now.duration_since(object.last_seen)
            .unwrap_or(Duration::ZERO)
            <= self.stale_after()
    }

    /// The freshest metering object in [`POWER_PRIORITY`], with its reading.
    fn power_source(
        &self,
        now: SystemTime,
    ) -> Option<PowerSource<'_>> {
        POWER_PRIORITY.iter().find_map(|class_code| {
            self.snapshot
                .objects
                .values()
                .filter(|object| object.class_code == *class_code && self.is_fresh(object, now))
                .find_map(|object| power_reading(object, *class_code))
        })
    }

    /// The most recent reading from any metering object, fresh or not.
    fn last_power_source(&self) -> Option<PowerSource<'_>> {
        POWER_PRIORITY.iter().find_map(|class_code| {
            self.snapshot
                .objects
                .values()
                .filter(|object| object.class_code == *class_code)
                .find_map(|object| power_reading(object, *class_code))
        })
    }

    fn metering_object(&self) -> Option<&ObjectSnapshot> {
        POWER_PRIORITY.iter().find_map(|class_code| {
            self.snapshot
                .objects
                .values()
                .find(|object| object.class_code == *class_code)
        })
    }

    fn pv_objects(&self) -> impl Iterator<Item = &ObjectSnapshot> {
        self.snapshot
            .objects
            .values()
            .filter(|object| object.class_code == SOLAR_CLASS_CODE)
    }

    fn has_pv(&self) -> bool {
        self.pv_objects().next().is_some()
    }

    fn solar_units(&self) -> Vec<PowerSource<'_>> {
        self.pv_objects()
            .filter_map(|object| power_reading(object, SOLAR_CLASS_CODE))
            .collect()
    }

    fn metering_objects(&self) -> impl Iterator<Item = &ObjectSnapshot> {
        self.snapshot.objects.values().filter(|object| {
            let class_code = object.class_code;
            class_code == SOLAR_CLASS_CODE || POWER_PRIORITY.contains(&class_code)
        })
    }

    fn grid_watts(
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
        if units.is_empty() || units.iter().any(|unit| !self.is_fresh(unit.object, now)) {
            return None;
        }
        Some(total_watts(&units))
    }

    fn grid_state(
        &self,
        now: SystemTime,
    ) -> SeriesState {
        if let Some(source) = self.power_source(now) {
            return SeriesState::Current {
                watts: source.watts,
                age: reading_age(now, source.object.last_seen),
                detail: Some(format_eoj(source.object.key.eoj)),
            };
        }
        if let Some(source) = self.last_power_source() {
            return SeriesState::Stale {
                last: source.watts,
                age: reading_age(now, source.object.last_seen),
                detail: None,
            };
        }
        let detail = self.metering_object().map_or_else(
            || String::from("no consumption meter"),
            |object| {
                format!(
                    "{} · waiting for the first poll",
                    format_eoj(object.key.eoj)
                )
            },
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
            let answered = self.pv_objects().any(|object| {
                power_epc(SOLAR_CLASS_CODE).is_some_and(|epc| object.properties.contains_key(&epc))
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
            .map(|unit| reading_age(now, unit.object.last_seen))
            .max()
            .unwrap_or(Duration::ZERO);
        let partial = (units.len() < pv_count).then(|| format!("{} of {pv_count} PV", units.len()));
        let stale = units
            .iter()
            .filter(|unit| !self.is_fresh(unit.object, now))
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
            Series::Grid => self.grid_state(now),
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
            if watts.saturating_abs() > peak.saturating_abs() {
                peak = watts;
            }
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
                    format!(" {}", reading_text(series, watts)),
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
                    format!(
                        " · no reading for {} · last {}",
                        format_age(age),
                        reading_text(series, last)
                    ),
                    Style::new().fg(EXPIRED),
                ));
            },
        }
        if show_stats && let Some((peak, average)) = self.series_stats(series, seed) {
            let peak = match series {
                Series::Grid => peak.saturating_abs(),
                Series::Solar => peak,
            };
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
            Paragraph::new(Line::from(" q quit · p poll now · j/k select").style(dim())),
            footer,
        );
    }

    fn draw_header(
        &self,
        frame: &mut Frame,
        area: Rect,
    ) {
        let devices = match self.snapshot.objects.len() {
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
        let mut visible = vec![Series::Grid];
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
                |object| {
                    (
                        format!(" Power · {} · no reading yet", format_eoj(object.key.eoj)),
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

        let lines: Vec<Line<'static>> = visible
            .iter()
            .zip(states)
            .map(|(series, state)| self.series_line(*series, state))
            .collect();
        let plot = PowerPlot::build(&self.samples);
        if plot.runs.is_empty() {
            frame.render_widget(Paragraph::new(lines), inner);
            return;
        }

        let summary_height = u16::try_from(visible.len()).unwrap_or(1);
        let [summary, chart_area] =
            Layout::vertical([Constraint::Length(summary_height), Constraint::Min(0)]).areas(inner);
        frame.render_widget(Paragraph::new(lines), summary);
        plot.draw(frame, chart_area);
    }

    fn draw_devices(
        &mut self,
        frame: &mut Frame,
        area: Rect,
    ) {
        self.visible_rows = usize::from(area.height.saturating_sub(3));
        let title = format!(" Devices ({}) ", self.snapshot.objects.len());
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(BORDER))
            .title(title);
        if self.snapshot.objects.is_empty() {
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

        let [list_area, properties_area] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(area);
        let selected_index = self.selected_index().unwrap_or(0);
        // The window top is sticky: moving up from the bottom row must move the
        // highlight inside the window, not drag the whole list under a fixed one.
        self.first = self
            .first
            .min(selected_index)
            .max((selected_index + 1).saturating_sub(self.visible_rows))
            .min(
                self.snapshot
                    .objects
                    .len()
                    .saturating_sub(self.visible_rows),
            );
        let header = Row::new(vec![
            Cell::from("EOJ"),
            Cell::from(Line::from("POWER").alignment(Alignment::Right)),
            Cell::from("SEEN"),
        ])
        .style(Style::new().fg(LABEL));
        let rows: Vec<Row<'_>> = self
            .snapshot
            .objects
            .iter()
            .enumerate()
            .skip(self.first)
            .take(self.visible_rows)
            .map(|(index, (_, object))| {
                let row = device_row(self.config.update_interval, object);
                if index == selected_index {
                    row.style(Style::new().add_modifier(Modifier::REVERSED))
                } else {
                    row
                }
            })
            .collect();
        let table = Table::new(
            rows,
            [
                Constraint::Length(9),
                Constraint::Length(15),
                Constraint::Length(6),
            ],
        )
        .header(header)
        .column_spacing(1)
        .block(block);
        frame.render_widget(table, list_area);
        self.draw_properties(frame, properties_area);
    }

    fn draw_properties(
        &self,
        frame: &mut Frame,
        area: Rect,
    ) {
        let selected = self
            .selected_key()
            .and_then(|key| self.snapshot.objects.get(&key));
        let title = Line::from(Span::styled(" Properties ", accent()));
        let mut block = panel(title);
        if let Some(object) = selected {
            block = block.title_top(
                Line::from(format!(
                    " {} {} ",
                    format_eoj(object.key.eoj),
                    object.key.ip
                ))
                .alignment(Alignment::Right)
                .style(dim()),
            );
        }
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(object) = selected else {
            return;
        };
        if object.properties.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(" waiting for the first poll").style(dim())),
                inner,
            );
            return;
        }
        let header = Row::new(vec![
            Cell::from("EPC"),
            Cell::from("PROPERTY"),
            Cell::from("VALUE"),
        ])
        .style(Style::new().fg(LABEL));
        let data_rows = usize::from(inner.height.saturating_sub(1));
        let (shown, hidden) = if object.properties.len() > data_rows {
            (
                data_rows.saturating_sub(1),
                object.properties.len() + 1 - data_rows,
            )
        } else {
            (object.properties.len(), 0)
        };
        let rows: Vec<Row<'_>> = object
            .properties
            .iter()
            .take(shown)
            .map(|(epc, property)| property_row(object.class_code, *epc, property))
            .collect();
        let [table_area, more_area] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        let table = Table::new(
            rows,
            [
                Constraint::Length(4),
                Constraint::Fill(3),
                Constraint::Fill(2),
            ],
        )
        .header(header)
        .column_spacing(1);
        frame.render_widget(table, table_area);
        if hidden > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(format!(" … {hidden} more")).style(dim())),
                more_area,
            );
        }
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

/// Symmetric bounds keep the three evenly spaced labels truthful. Clamping the
/// range to `[0, limit]` for a buy-only site would move the middle `0` label off
/// the zero rule and leave a `sell` label for a range with no data.
#[derive(Clone, Copy)]
struct Scale {
    limit: i64,
}

impl Scale {
    const fn bounds(self) -> [f64; 2] {
        [chart_coordinate(-self.limit), chart_coordinate(self.limit)]
    }

    /// Bottom to top, the order [`Axis::labels`] draws.
    fn labels(self) -> [Line<'static>; 3] {
        let magnitude = format_limit(self.limit);
        [
            Line::from(format!("sell {magnitude}")).style(dim()),
            Line::from("0").style(dim()),
            Line::from(format!("buy {magnitude}")).style(dim()),
        ]
    }
}

/// The grid runs and the scale fitted to them in one walk. Ratatui drops a
/// point outside the bounds instead of clipping it, so the two must agree.
struct PowerPlot {
    runs: Vec<Vec<(f64, f64)>>,
    scale: Scale,
}

impl PowerPlot {
    fn build(samples: &VecDeque<Sample>) -> Self {
        let mut runs = Vec::new();
        let mut run = Vec::new();
        let mut peak = 0i64;
        let offset = SAMPLE_CAPACITY.saturating_sub(samples.len());
        for (index, sample) in samples.iter().enumerate() {
            if let Some(watts) = sample.get(Series::Grid) {
                let x = chart_coordinate(i64::try_from(offset + index).unwrap_or(i64::MAX));
                run.push((x, chart_coordinate(watts)));
                peak = peak.max(watts.saturating_abs());
            } else if !run.is_empty() {
                runs.push(std::mem::take(&mut run));
            }
        }
        if !run.is_empty() {
            runs.push(run);
        }
        Self {
            runs,
            scale: Scale {
                limit: nice_limit(peak),
            },
        }
    }

    fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
    ) {
        let x_end = chart_coordinate(i64::try_from(SAMPLE_CAPACITY - 1).unwrap_or(i64::MAX));
        let zero_rule = [(0.0, 0.0), (x_end, 0.0)];
        let mut datasets: Vec<Dataset<'_>> = vec![
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(LABEL))
                .data(&zero_rule),
        ];
        // Painted after the rule: a braille cell keeps the last colour painted
        // into it, so the data must win where the two meet.
        datasets.extend(self.runs.iter().map(|run| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(Series::Grid.color()))
                .data(run.as_slice())
        }));
        let chart = Chart::new(datasets)
            .x_axis(Axis::default().bounds([0.0, x_end]))
            .y_axis(
                Axis::default()
                    .bounds(self.scale.bounds())
                    .labels(self.scale.labels())
                    .labels_alignment(Alignment::Right)
                    .style(dim()),
            )
            .legend_position(None);
        frame.render_widget(chart, area);
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

const fn flow_word(watts: i64) -> &'static str {
    if watts > 0 {
        "buying"
    } else if watts < 0 {
        "selling"
    } else {
        "idle"
    }
}

fn reading_text(
    series: Series,
    watts: i64,
) -> String {
    match series {
        Series::Grid => format!("{} {} W", flow_word(watts), watts.saturating_abs()),
        Series::Solar => format!("{watts} W"),
    }
}

fn device_row(
    update_interval: Duration,
    object: &ObjectSnapshot,
) -> Row<'_> {
    let age = object.last_seen.elapsed().unwrap_or(Duration::ZERO);
    let seen = Line::from(format_age(age))
        .alignment(Alignment::Right)
        .style(Style::new().fg(age_color(update_interval, age)));
    Row::new(vec![
        Cell::from(format_eoj(object.key.eoj)),
        power_cell(object),
        Cell::from(seen),
    ])
}

fn property_row(
    class_code: u16,
    epc: u8,
    property: &PropertySnapshot,
) -> Row<'_> {
    let name = echonet_lite::ecodec::lookup(class_code, epc).map_or("(unknown)", |info| info.name);
    Row::new(vec![
        Cell::from(format!("0x{epc:02X}")),
        Cell::from(name).style(dim()),
        Cell::from(cell_text(class_code, epc, &property.raw_edt)).style(Style::new().fg(VALUE)),
    ])
}

// Do not render `PropertySnapshot::text` here: it prints the property name, while
// this column is a watt figure or `--`. Do not gate on liveness: SEEN already
// dates the reading, so blanking the last value only loses it.
fn power_text(object: &ObjectSnapshot) -> Option<String> {
    let class_code = object.class_code;
    power_reading(object, class_code)
        .map(|source| format!("{} {} W", power_label(class_code), source.watts))
}

fn power_cell(object: &ObjectSnapshot) -> Cell<'static> {
    let line = power_text(object).map_or_else(|| Line::from("--").style(dim()), Line::from);
    Cell::from(line.alignment(Alignment::Right))
}

fn power_reading(
    object: &ObjectSnapshot,
    class_code: u16,
) -> Option<PowerSource<'_>> {
    let epc = power_epc(class_code)?;
    let property = object.properties.get(&epc)?;
    let watts = instantaneous_watts(class_code, &property.raw_edt)?;
    Some(PowerSource { object, watts })
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

const AXIS_STEPS: [i64; 9] = [10, 15, 20, 25, 30, 40, 50, 60, 80];
const MIN_SCALE_LIMIT: i64 = 100;

fn nice_limit(peak: i64) -> i64 {
    let target = peak.saturating_add(peak / 10).max(MIN_SCALE_LIMIT);
    let mut decade = 1i64;
    while decade <= target / 10 {
        decade *= 10;
    }
    AXIS_STEPS
        .iter()
        .map(|step| decade / 10 * step)
        .find(|candidate| *candidate >= target)
        .unwrap_or_else(|| decade.saturating_mul(10))
}

fn format_limit(watts: i64) -> String {
    if watts < 1000 {
        format!("{watts} W")
    } else if watts % 1000 == 0 {
        format!("{} kW", watts / 1000)
    } else {
        format!("{}.{} kW", watts / 1000, watts % 1000 / 100)
    }
}

/// Run the dashboard until the user quits, then return the service's result.
///
/// # Errors
///
/// Returns the bind or terminal I/O error, or the error the radar service
/// stopped with.
pub fn run(config: SessionConfig) -> io::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let guard = TerminalGuard;

    let (session, mut handle) =
        RadarSession::start(config, Arc::new(NoopPulse)).map_err(|fault| fault_error(&fault))?;
    let mut dashboard = Dashboard::new(config, handle.snapshot());
    let drawn = event_loop(&mut terminal, &mut dashboard, &mut handle);
    drop(guard);

    let stopped = session.stop().map_err(|fault| fault_error(&fault));
    drawn?;
    stopped
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
    }
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    dashboard: &mut Dashboard,
    handle: &mut RadarHandle,
) -> io::Result<()> {
    let mut last_sample = Instant::now();
    loop {
        if handle.has_changed() {
            let published = handle.snapshot();
            dashboard.apply(published);
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
                if let Err(error) = handle.poll_now() {
                    dashboard.status = Some((SystemTime::now(), poll_error(error)));
                }
            },
            KeyCode::Char('j') | KeyCode::Down => dashboard.select_by(1),
            KeyCode::Char('k') | KeyCode::Up => dashboard.select_by(-1),
            _ => {},
        }
    }
    Ok(())
}

fn poll_error(error: CommandError) -> String {
    match error {
        CommandError::Busy => String::from("poll request dropped: the service queue is full"),
        CommandError::ServiceStopped => error.to_string(),
    }
}

/// A stopped service outranks its last notice: the display is frozen, while the
/// LAN may still be fine.
fn status_line(snapshot: &RadarSnapshot) -> Option<(SystemTime, String)> {
    match &snapshot.status {
        SessionStatus::Degraded { fault, .. } => Some((
            SystemTime::now(),
            format!("the radar service stopped: {}", fault_text(fault)),
        )),
        SessionStatus::Stopped => {
            Some((SystemTime::now(), String::from("the radar service stopped")))
        },
        SessionStatus::Binding | SessionStatus::Running { .. } => snapshot
            .notices
            .last()
            .map(|notice| (notice.at, notice.text.clone())),
    }
}

fn fault_text(fault: &Fault) -> String {
    match fault {
        Fault::Bind { interface, detail } => {
            format!("could not bind {interface}: {detail}")
        },
        Fault::Config(error) => error.to_string(),
        Fault::Service(detail) => detail.clone(),
    }
}

fn fault_error(fault: &Fault) -> io::Error {
    io::Error::other(fault_text(fault))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    use echonet_lite::frame::Eoj;
    use echonet_radar_core::{Observation, RadarModel, format_edt};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::{Buffer, Cell as BufferCell};

    const INTERFACE: Ipv4Addr = Ipv4Addr::new(192, 168, 100, 30);
    const IP: &str = "192.168.100.26";
    const OTHER: &str = "192.168.100.30";
    const BOARD: Eoj = Eoj::new(0x02, 0x87, 0x01);
    const METER: Eoj = Eoj::new(0x02, 0x88, 0x01);
    const PV: Eoj = Eoj::new(0x02, 0x79, 0x01);
    const PV_SECOND: Eoj = Eoj::new(0x02, 0x79, 0x02);
    const HEATER: Eoj = Eoj::new(0x02, 0x6B, 0x01);

    fn config(update_seconds: u64) -> SessionConfig {
        SessionConfig {
            interface: INTERFACE,
            update_interval: Duration::from_secs(update_seconds),
            ..SessionConfig::desktop()
        }
    }

    fn key(
        ip: &str,
        eoj: Eoj,
    ) -> ObjectKey {
        ObjectKey {
            ip: ip.parse().unwrap(),
            eoj,
        }
    }

    fn source(ip: &str) -> SocketAddr {
        format!("{ip}:3610").parse().unwrap()
    }

    fn session_snapshot(
        session: u64,
        observations: Vec<Observation>,
    ) -> Arc<RadarSnapshot> {
        let mut model = RadarModel::new(session);
        for observation in observations {
            model.observe(observation);
        }
        Arc::new(model.snapshot())
    }

    fn snapshot(observations: Vec<Observation>) -> Arc<RadarSnapshot> {
        session_snapshot(1, observations)
    }

    fn dashboard(
        update_seconds: u64,
        observations: Vec<Observation>,
    ) -> Dashboard {
        Dashboard::new(config(update_seconds), snapshot(observations))
    }

    fn device(
        ip: &str,
        eoj: Eoj,
    ) -> Observation {
        Observation::Device {
            key: key(ip, eoj),
            class_code: eoj.class_code(),
            source: source(ip),
        }
    }

    fn value(
        at: SystemTime,
        ip: &str,
        eoj: Eoj,
        epc: u8,
        raw: &[u8],
    ) -> Observation {
        Observation::Value {
            key: key(ip, eoj),
            epc,
            raw_edt: raw.to_vec(),
            text: format_edt(eoj.class_code(), epc, raw),
            at,
            source: source(ip),
        }
    }

    fn seen(
        at: SystemTime,
        ip: &str,
        eoj: Eoj,
    ) -> Observation {
        Observation::Seen {
            key: key(ip, eoj),
            at,
            source: source(ip),
        }
    }

    fn notice(text: &str) -> Observation {
        Observation::Status(String::from(text))
    }

    fn object(
        observations: Vec<Observation>,
        eoj: Eoj,
    ) -> ObjectSnapshot {
        snapshot(observations)
            .objects
            .values()
            .find(|object| object.key.eoj == eoj)
            .cloned()
            .unwrap()
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
            .chunks(usize::from(width))
            .map(|row| row.iter().map(BufferCell::symbol).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn buffer(
        dashboard: &mut Dashboard,
        width: u16,
        height: u16,
    ) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| dashboard.draw(frame)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn lists(
        screen: &str,
        eoj: &str,
    ) -> bool {
        screen.lines().any(|line| {
            line.strip_prefix('│')
                .is_some_and(|tail| tail.starts_with(eoj))
        })
    }

    fn pane_title(screen: &str) -> &str {
        screen
            .lines()
            .find(|line| line.contains("Properties"))
            .expect("the properties pane is rendered")
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn sample(
        grid: Option<i64>,
        solar: Option<i64>,
    ) -> Sample {
        let mut sample = Sample {
            watts: [None; SERIES.len()],
        };
        sample.watts[Series::Grid.index()] = grid;
        sample.watts[Series::Solar.index()] = solar;
        sample
    }

    #[test]
    fn power_text_labels_the_reading() {
        let board = object(
            vec![value(
                SystemTime::UNIX_EPOCH,
                IP,
                BOARD,
                0xC6,
                &[0, 0, 2, 0x44],
            )],
            BOARD,
        );
        assert_eq!(power_text(&board).as_deref(), Some("Power 580 W"));

        let solar = object(
            vec![value(SystemTime::UNIX_EPOCH, IP, PV, 0xE0, &[0x04, 0xD8])],
            PV,
        );
        assert_eq!(power_text(&solar).as_deref(), Some("Gen 1240 W"));

        let battery_eoj = Eoj::new(0x02, 0x7D, 0x01);
        let battery = object(
            vec![value(
                SystemTime::UNIX_EPOCH,
                IP,
                battery_eoj,
                0xD3,
                &[0xFF, 0xFF, 0xFF, 0x38],
            )],
            battery_eoj,
        );
        assert_eq!(power_text(&battery).as_deref(), Some("Battery -200 W"));

        let no_data = object(
            vec![value(
                SystemTime::UNIX_EPOCH,
                IP,
                BOARD,
                0xC6,
                &[0x7F, 0xFF, 0xFF, 0xFE],
            )],
            BOARD,
        );
        assert_eq!(power_text(&no_data), None);

        assert_eq!(power_text(&object(vec![device(IP, BOARD)], BOARD)), None);

        let heater = object(vec![device(IP, HEATER)], HEATER);
        assert_eq!(power_text(&heater), None);
    }

    #[test]
    fn a_stale_device_keeps_its_power_reading() {
        let old = SystemTime::now() - Duration::from_secs(3600);
        let mut dashboard = dashboard(10, vec![value(old, IP, BOARD, 0xC6, &[0, 0, 2, 0x44])]);

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("Power 580 W"), "{screen}");
    }

    #[test]
    fn a_seen_observation_keeps_the_value_and_stamps_liveness() {
        let start = SystemTime::UNIX_EPOCH;
        let later = start + Duration::from_secs(60);
        let dashboard = dashboard(
            10,
            vec![
                device(IP, BOARD),
                value(start, IP, BOARD, 0xC6, &[0, 0, 2, 0x44]),
                seen(later, IP, BOARD),
            ],
        );

        let object = dashboard
            .power_source(later)
            .map(|source| source.object)
            .unwrap();
        let property = object.properties.get(&0xC6).unwrap();
        assert_eq!(property.raw_edt, vec![0, 0, 2, 0x44]);
        assert_eq!(
            cell_text(BOARD.class_code(), 0xC6, &property.raw_edt),
            "580 W"
        );
        assert_eq!(object.properties.len(), 1);
        assert_eq!(object.last_seen, later);
        assert_eq!(property.at, start);
    }

    #[test]
    fn the_meter_is_the_power_source_before_the_board() {
        let start = SystemTime::UNIX_EPOCH;
        let pv_only = dashboard(10, vec![value(start, IP, PV, 0xE0, &[0x04, 0xD8])]);
        assert!(
            pv_only.power_source(start).is_none(),
            "PV is not a consumption source"
        );

        let board_only = dashboard(10, vec![value(start, IP, BOARD, 0xC6, &[0, 0, 2, 0x44])]);
        assert_eq!(
            board_only.power_source(start).map(|source| source.watts),
            Some(580)
        );

        let with_meter = dashboard(
            10,
            vec![
                value(start, IP, BOARD, 0xC6, &[0, 0, 2, 0x44]),
                value(start, IP, METER, 0xE7, &[0, 0, 0x02, 0xBC]),
            ],
        );
        assert_eq!(
            with_meter.power_source(start).map(|source| source.watts),
            Some(700)
        );
    }

    #[test]
    fn a_fresh_board_is_used_when_the_meter_goes_quiet() {
        let start = SystemTime::UNIX_EPOCH;
        let dashboard = dashboard(
            10,
            vec![
                value(start, IP, METER, 0xE7, &[0, 0, 0x02, 0xBC]),
                value(
                    start + Duration::from_secs(40),
                    IP,
                    BOARD,
                    0xC6,
                    &[0, 0, 2, 0x44],
                ),
            ],
        );

        assert_eq!(
            dashboard
                .power_source(start + Duration::from_secs(40))
                .map(|source| source.watts),
            Some(580)
        );
    }

    #[test]
    fn a_stale_source_samples_nothing() {
        let start = SystemTime::UNIX_EPOCH;
        let mut dashboard = dashboard(
            10,
            vec![
                value(start, IP, BOARD, 0xC6, &[0, 0, 2, 0x44]),
                seen(start, IP, BOARD),
            ],
        );

        dashboard.sample(start + Duration::from_secs(29));
        assert_eq!(
            dashboard
                .samples
                .back()
                .and_then(|sample| sample.get(Series::Grid)),
            Some(580)
        );

        dashboard.sample(start + Duration::from_secs(31));
        assert_eq!(
            dashboard
                .samples
                .back()
                .and_then(|sample| sample.get(Series::Grid)),
            None
        );
    }

    #[test]
    fn a_silent_meter_keeps_its_last_reading_out_of_the_headline() {
        let start = SystemTime::UNIX_EPOCH;
        let dashboard = dashboard(
            10,
            vec![
                value(start, IP, BOARD, 0xC6, &[0, 0, 2, 0x44]),
                seen(start, IP, BOARD),
            ],
        );

        let now = start + Duration::from_secs(31);
        assert_eq!(dashboard.grid_watts(now), None);
        let state = dashboard.series_state(Series::Grid, now);
        let line = text(&dashboard.series_line(Series::Grid, state));
        assert!(line.contains("no reading for 31s"), "{line}");
        assert!(line.contains("last buying 580 W"), "{line}");
    }

    #[test]
    fn one_tick_samples_both_series() {
        let start = SystemTime::UNIX_EPOCH;
        let mut dashboard = dashboard(
            10,
            vec![
                value(start, IP, BOARD, 0xC6, &[0, 0, 2, 0x44]),
                value(start, IP, PV, 0xE0, &[0x04, 0xD8]),
                value(start, IP, PV_SECOND, 0xE0, &[0x01, 0x2C]),
            ],
        );

        dashboard.sample(start);
        let sample = dashboard.samples.back().unwrap();
        assert_eq!(sample.get(Series::Grid), Some(580));
        assert_eq!(sample.get(Series::Solar), Some(1540));

        let state = dashboard.series_state(Series::Grid, start + Duration::from_secs(5));
        match state {
            SeriesState::Current { age, .. } => {
                assert_eq!(age, Duration::from_secs(5));
            },
            _ => panic!("expected a current grid reading"),
        }
    }

    #[test]
    fn a_pv_unit_that_never_reported_leaves_a_partial_note() {
        let start = SystemTime::UNIX_EPOCH;
        let mut dashboard = dashboard(
            10,
            vec![
                device(IP, PV),
                device(IP, PV_SECOND),
                value(start, IP, PV, 0xE0, &[0x04, 0xD8]),
            ],
        );

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
        let start = SystemTime::UNIX_EPOCH;
        let mut dashboard = dashboard(
            10,
            vec![
                value(start, IP, PV, 0xE0, &[0x00, 0x00]),
                value(start, IP, PV_SECOND, 0xE0, &[0x00, 0x00]),
            ],
        );

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
        let start = SystemTime::UNIX_EPOCH;
        let dashboard = dashboard(
            10,
            vec![
                value(start, IP, PV, 0xE0, &[0x04, 0xD8]),
                value(start, IP, PV_SECOND, 0xE0, &[0x01, 0x2C]),
                seen(start + Duration::from_secs(20), IP, PV),
            ],
        );

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
        let start = SystemTime::UNIX_EPOCH;
        let dashboard = dashboard(
            10,
            vec![
                device(IP, PV),
                device(IP, PV_SECOND),
                value(start, IP, PV, 0xE0, &[0x04, 0xD8]),
            ],
        );

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
        let now = SystemTime::now();
        let stale_at = now.checked_sub(Duration::from_secs(40)).unwrap();
        let mut dashboard = dashboard(
            10,
            vec![
                value(now, IP, BOARD, 0xC6, &[0, 0, 2, 0x44]),
                value(stale_at, IP, PV, 0xE0, &[0x04, 0xD8]),
                value(stale_at, IP, PV_SECOND, 0xE0, &[0x01, 0x2C]),
                seen(now, IP, PV),
            ],
        );

        for (width, height) in [(100, 30), (80, 24)] {
            let screen = screen(&mut dashboard, width, height);
            assert!(screen.contains("grid buying 580 W"), "{screen}");
            assert!(screen.contains("1 of 2 PV stale"), "{screen}");
            assert!(screen.contains("last 1540 W"), "{screen}");
        }
    }

    #[test]
    fn a_pv_unit_reporting_no_value_is_not_waiting_for_its_first_poll() {
        let start = SystemTime::UNIX_EPOCH;
        let dashboard = dashboard(10, vec![value(start, IP, PV, 0xE0, &[0xFF, 0xFF])]);

        assert_eq!(dashboard.solar_watts(start), None);
        let state = dashboard.series_state(Series::Solar, start);
        let line = text(&dashboard.series_line(Series::Solar, state));
        assert!(line.contains("PV reports no value"), "{line}");
    }

    #[test]
    fn a_site_without_pv_keeps_the_consumption_only_layout() {
        let now = SystemTime::now();
        let mut dashboard = dashboard(10, vec![value(now, IP, BOARD, 0xC6, &[0, 0, 2, 0x44])]);

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("grid buying 580 W"), "{screen}");
        assert!(!screen.contains("solar"), "{screen}");
    }

    #[test]
    fn a_solar_only_site_lists_solar_without_a_grid_chart() {
        let now = SystemTime::now();
        let mut dashboard = dashboard(10, vec![value(now, IP, PV, 0xE0, &[0x04, 0xD8])]);
        dashboard.sample(now);

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("no consumption meter"), "{screen}");
        assert!(screen.contains("solar 1240 W"), "{screen}");
        assert!(!screen.contains("sell "), "{screen}");
    }

    #[test]
    fn the_grid_chart_keeps_its_gaps() {
        let mut dashboard = dashboard(10, vec![]);
        for tick in [
            sample(Some(100), None),
            sample(Some(100), Some(1000)),
            sample(None, Some(1000)),
            sample(None, None),
        ] {
            dashboard.samples.push_back(tick);
        }

        let plot = PowerPlot::build(&dashboard.samples);
        assert_eq!(plot.runs.len(), 1, "solar creates no run");
        let run = &plot.runs[0];
        assert_eq!(run.len(), 2);
        assert!(
            run.iter()
                .all(|(_, watts)| (*watts - 100.0).abs() < f64::EPSILON),
            "{run:?}"
        );
    }

    #[test]
    fn peak_and_average_are_per_series() {
        let mut dashboard = dashboard(10, vec![]);
        dashboard.samples.push_back(sample(Some(100), Some(1000)));
        dashboard.samples.push_back(sample(Some(300), Some(0)));

        assert_eq!(
            dashboard.series_stats(Series::Grid, Some(300)),
            Some((300, 200))
        );
        assert_eq!(
            dashboard.series_stats(Series::Solar, Some(1000)),
            Some((1000, 500))
        );
    }

    #[test]
    fn a_selling_window_peaks_at_its_largest_export() {
        let mut dashboard = dashboard(10, vec![]);
        dashboard.samples.push_back(sample(Some(-300), None));
        dashboard.samples.push_back(sample(Some(-400), None));

        assert_eq!(
            dashboard.series_stats(Series::Grid, Some(-300)),
            Some((-400, -350))
        );
    }

    #[test]
    fn the_scale_is_symmetric_about_zero() {
        let mut dashboard = dashboard(10, vec![]);
        dashboard.samples.push_back(sample(Some(-373), Some(1240)));

        let bounds = PowerPlot::build(&dashboard.samples).scale.bounds();
        assert!((bounds[0] + 500.0).abs() < f64::EPSILON, "{bounds:?}");
        assert!((bounds[1] - 500.0).abs() < f64::EPSILON, "{bounds:?}");
        assert!((bounds[0] + bounds[1]).abs() < f64::EPSILON, "{bounds:?}");
    }

    #[test]
    fn nice_limit_rounds_to_readable_steps() {
        for (peak, limit) in [
            (0, 100),
            (41, 100),
            (100, 150),
            (373, 500),
            (580, 800),
            (2168, 2500),
            (2541, 3000),
            (8000, 10_000),
        ] {
            assert_eq!(nice_limit(peak), limit, "peak {peak}");
        }
    }

    #[test]
    fn the_axis_labels_say_buy_above_and_sell_below() {
        let scale = Scale { limit: 1500 };
        let labels: Vec<String> = scale.labels().iter().map(|line| text(line)).collect();
        assert_eq!(labels, ["sell 1.5 kW", "0", "buy 1.5 kW"]);

        let scale = Scale { limit: 800 };
        let labels: Vec<String> = scale.labels().iter().map(|line| text(line)).collect();
        assert_eq!(labels, ["sell 800 W", "0", "buy 800 W"]);
    }

    #[test]
    fn the_grid_reading_names_its_direction() {
        assert_eq!(reading_text(Series::Grid, 373), "buying 373 W");
        assert_eq!(reading_text(Series::Grid, -373), "selling 373 W");
        assert_eq!(reading_text(Series::Grid, 0), "idle 0 W");
        assert_eq!(reading_text(Series::Solar, 0), "0 W");
    }

    #[test]
    fn the_scale_holds_every_plotted_point() {
        for peak in [-5000, -373, -1, 0, 1, 373, 2168, 2541, 12_000] {
            let mut dashboard = dashboard(10, vec![]);
            dashboard
                .samples
                .push_back(sample(Some(peak), Some(peak.saturating_abs())));
            let plot = PowerPlot::build(&dashboard.samples);
            let [low, high] = plot.scale.bounds();
            assert!(
                plot.runs
                    .iter()
                    .flatten()
                    .all(|(_, watts)| *watts >= low && *watts <= high),
                "peak {peak} escapes {low}..{high}"
            );
        }
    }

    #[test]
    fn the_scale_ignores_solar() {
        let mut dashboard = dashboard(10, vec![]);
        dashboard.samples.push_back(sample(Some(100), Some(5000)));

        let plot = PowerPlot::build(&dashboard.samples);
        assert_eq!(plot.scale.limit, 150);
        assert_eq!(plot.runs.len(), 1);
        assert!(
            plot.runs
                .iter()
                .flatten()
                .all(|(_, watts)| (*watts - 100.0).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn a_missing_grid_reading_keeps_the_scale_at_its_floor() {
        let mut dashboard = dashboard(10, vec![]);
        for _ in 0..3 {
            dashboard.samples.push_back(sample(None, Some(1240)));
        }

        let plot = PowerPlot::build(&dashboard.samples);
        assert_eq!(plot.scale.limit, MIN_SCALE_LIMIT);
        assert_eq!(plot.runs.len(), 0, "a missing reading draws no run");
    }

    #[test]
    fn a_selling_reading_reads_as_selling() {
        let now = SystemTime::now();
        let mut dashboard = dashboard(
            10,
            vec![value(now, IP, BOARD, 0xC6, &[0xFF, 0xFF, 0xFE, 0x8B])],
        );
        dashboard.sample(now);

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("grid selling 373 W"), "{screen}");
        assert!(screen.contains("sell 500 W"), "{screen}");
        assert!(screen.contains("buy 500 W"), "{screen}");
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
        let mut dashboard = dashboard(10, vec![]);
        for _ in 0..(SAMPLE_CAPACITY + 5) {
            dashboard.sample(SystemTime::now());
        }
        assert_eq!(dashboard.samples.len(), SAMPLE_CAPACITY);
    }

    #[test]
    fn the_property_pane_lists_every_stored_value() {
        let now = SystemTime::now();
        let heater_changes: [(u8, &[u8]); 4] = [
            (0xC3, &[0x42]),
            (0xD1, &[0x28]),
            (0xD3, &[0x28]),
            (0xB1, &[0x41]),
        ];
        let observations = heater_changes
            .iter()
            .map(|(epc, raw)| value(now, IP, HEATER, *epc, raw))
            .collect();
        let mut dashboard = dashboard(10, observations);

        let screen = screen(&mut dashboard, 100, 30);
        let title = pane_title(&screen);
        assert!(title.contains("Properties"), "{screen}");
        assert!(title.contains("0x026B01 192.168.100.26"), "{screen}");
        for needle in [
            "PROPERTY",
            "VALUE",
            "Hot water supply status",
            "Temperature of supplied",
            "Bath water temperature",
            "Automatic water",
            "OFF",
            "40 C",
            "ON",
        ] {
            assert!(
                screen.contains(needle),
                "the pane is missing {needle:?}: {screen}"
            );
        }
        let order: Vec<usize> = ["0xB1", "0xC3", "0xD1", "0xD3"]
            .iter()
            .map(|epc| {
                screen
                    .find(epc)
                    .unwrap_or_else(|| panic!("{epc} is missing: {screen}"))
            })
            .collect();
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "the rows are out of EPC order: {order:?}"
        );
    }

    #[test]
    fn selecting_the_next_device_swaps_the_pane() {
        let now = SystemTime::now();
        let mut dashboard = dashboard(
            10,
            vec![
                value(now, IP, HEATER, 0xB1, &[0x41]),
                value(now, IP, METER, 0xE7, &[0, 0, 0x02, 0xBC]),
            ],
        );

        let before = screen(&mut dashboard, 100, 30);
        assert!(
            pane_title(&before).contains("0x026B01 192.168.100.26"),
            "{before}"
        );
        assert!(before.contains("Automatic water"), "{before}");

        dashboard.select_by(1);
        let after = screen(&mut dashboard, 100, 30);
        assert!(
            pane_title(&after).contains("0x028801 192.168.100.26"),
            "{after}"
        );
        assert!(after.contains("0xE7"), "{after}");
        assert!(
            !after.contains("Automatic water"),
            "the heater properties are still on screen: {after}"
        );
    }

    #[test]
    fn the_selection_clamps_and_scrolls_to_stay_visible() {
        let devices = (1..=6)
            .map(|instance| device(IP, Eoj::new(0x02, 0x87, instance)))
            .collect();
        let mut dashboard = dashboard(10, devices);

        dashboard.select_by(-1);
        let top = screen(&mut dashboard, 100, 20);
        assert!(lists(&top, "0x028701"), "{top}");

        dashboard.select_by(100);
        let bottom = screen(&mut dashboard, 100, 20);
        assert!(
            lists(&bottom, "0x028706"),
            "the last device is not on screen: {bottom}"
        );
        assert!(
            !lists(&bottom, "0x028701"),
            "the list did not scroll: {bottom}"
        );

        dashboard.select_by(-1);
        let one_up = screen(&mut dashboard, 100, 20);
        assert!(
            !lists(&one_up, "0x028701"),
            "moving up dragged the window instead of the highlight: {one_up}"
        );

        dashboard.select_by(-100);
        let again = screen(&mut dashboard, 100, 20);
        assert!(lists(&again, "0x028701"), "{again}");
    }

    #[test]
    fn a_pane_too_short_for_every_value_says_how_many_more() {
        let now = SystemTime::now();
        let observations = [
            (0xB1, &[0x41][..]),
            (0xC3, &[0x42]),
            (0xD1, &[0x28]),
            (0xD3, &[0x28]),
        ]
        .into_iter()
        .map(|(epc, raw)| value(now, IP, HEATER, epc, raw))
        .collect();
        let mut dashboard = dashboard(10, observations);

        let screen = screen(&mut dashboard, 100, 18);
        assert!(screen.contains("… 2 more"), "{screen}");
        assert!(screen.contains("0xC3"), "{screen}");
        assert!(!screen.contains("0xD3"), "{screen}");
    }

    #[test]
    fn a_device_discovered_later_does_not_move_the_selection() {
        let mut dashboard = dashboard(10, vec![device(OTHER, BOARD)]);
        dashboard.select_by(1);
        dashboard.apply(snapshot(vec![device(OTHER, BOARD), device(IP, BOARD)]));

        let screen = screen(&mut dashboard, 100, 30);
        let title = pane_title(&screen);
        assert!(
            title.contains("0x028701 192.168.100.30"),
            "the selection moved to the smaller IP: {screen}"
        );
    }

    #[test]
    fn an_epc_outside_the_mra_table_reads_as_unknown() {
        let mut dashboard = dashboard(
            10,
            vec![value(SystemTime::now(), IP, HEATER, 0xC5, &[0xFF])],
        );

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("0xC5"), "{screen}");
        assert!(screen.contains("(unknown)"), "{screen}");
    }

    #[test]
    fn the_selected_row_is_reversed() {
        for (width, height) in [(100, 30), (80, 24)] {
            let mut dashboard = dashboard(
                10,
                vec![
                    device(IP, Eoj::new(0x02, 0x87, 0x01)),
                    device(IP, Eoj::new(0x02, 0x87, 0x02)),
                ],
            );

            let buffer = buffer(&mut dashboard, width, height);
            let row = |eoj: &str| {
                buffer
                    .content
                    .chunks(usize::from(width))
                    .find(|cells| {
                        cells
                            .iter()
                            .map(BufferCell::symbol)
                            .collect::<String>()
                            .starts_with(&format!("│{eoj}"))
                    })
                    .unwrap_or_else(|| panic!("{width}x{height}: {eoj} is not rendered"))
            };
            let selected = row("0x028701");
            let text: String = selected.iter().map(BufferCell::symbol).collect();
            let border = text
                .chars()
                .skip(1)
                .position(|symbol| symbol == '│')
                .map_or_else(
                    || panic!("{width}x{height}: no pane border in {text}"),
                    |offset| offset + 1,
                );
            assert!(
                selected[1..border]
                    .iter()
                    .all(|cell| cell.modifier.contains(Modifier::REVERSED)),
                "{width}x{height}: the selected row is not reversed: {text}"
            );
            assert!(
                row("0x028702")[1..border]
                    .iter()
                    .all(|cell| !cell.modifier.contains(Modifier::REVERSED)),
                "{width}x{height}: an unselected row is reversed"
            );
        }
    }

    #[test]
    fn a_discovered_pv_unit_without_a_reading_waits_in_the_placeholder() {
        let mut dashboard = dashboard(10, vec![device(IP, PV)]);

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("0x027901 · no reading yet"), "{screen}");
        assert!(screen.contains("waiting for the first poll"), "{screen}");
    }

    #[test]
    fn the_empty_dashboard_names_where_it_listens() {
        let mut dashboard = dashboard(10, vec![]);

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
            let now = SystemTime::now();
            let heater_changes: [(u8, &[u8]); 4] = [
                (0xC3, &[0x42]),
                (0xD1, &[0x28]),
                (0xD3, &[0x28]),
                (0xB1, &[0x41]),
            ];
            let mut observations: Vec<Observation> = heater_changes
                .iter()
                .map(|(epc, raw)| value(now, IP, HEATER, *epc, raw))
                .collect();
            observations.push(value(now, IP, BOARD, 0xC6, &[0, 0, 2, 0x44]));
            observations.push(value(now, IP, PV, 0xE0, &[0x04, 0xD8]));
            observations.push(value(now, IP, PV_SECOND, 0xE0, &[0x01, 0x2C]));
            let mut dashboard = dashboard(10, observations);
            dashboard.sample(now);

            let screen = screen(&mut dashboard, width, height);
            let needles = vec![
                "0x028701",
                "Devices",
                "POWER",
                "Power 580 W",
                "Gen 1240 W",
                "Gen 300 W",
                "grid buying 580 W",
                "solar 1540 W",
                "0x027901",
                "0x027902",
            ];
            for needle in needles {
                assert!(
                    screen.contains(needle),
                    "{width}x{height} screen is missing {needle:?}: {screen}"
                );
            }
            assert!(
                screen
                    .lines()
                    .any(|line| line.contains("0x026B01") && line.contains("--")),
                "{width}x{height}: the heater's power cell is missing: {screen}"
            );
            let b1 = screen
                .lines()
                // At 80 columns the 19-cell name column clips this 43-character
                // name, so only its prefix is on screen.
                .find(|line| line.contains("0xB1") && line.contains("Automatic water"))
                .unwrap_or_else(|| panic!("{width}x{height}: the B1 row is missing: {screen}"));
            assert!(
                b1.contains("ON"),
                "{width}x{height}: the B1 value is missing: {screen}"
            );
        }
    }

    #[test]
    fn a_new_session_clears_the_selection() {
        let mut dashboard = dashboard(10, vec![device(IP, BOARD), device(OTHER, BOARD)]);
        dashboard.select_by(1);
        assert_eq!(dashboard.selected, Some(key(OTHER, BOARD)));

        dashboard.apply(session_snapshot(
            2,
            vec![device(IP, BOARD), device(OTHER, BOARD)],
        ));

        assert_eq!(dashboard.selected, None);
        let screen = screen(&mut dashboard, 100, 30);
        assert!(
            pane_title(&screen).contains("0x028701 192.168.100.26"),
            "{screen}"
        );
    }

    #[test]
    fn a_selected_key_that_disappears_is_cleared() {
        let mut dashboard = dashboard(10, vec![device(IP, BOARD), device(OTHER, BOARD)]);
        dashboard.select_by(0);
        assert_eq!(dashboard.selected, Some(key(IP, BOARD)));

        dashboard.apply(snapshot(vec![device(OTHER, BOARD)]));

        assert_eq!(dashboard.selected, None);
    }

    #[test]
    fn the_status_line_shows_the_latest_notice() {
        let mut dashboard = dashboard(
            10,
            vec![
                notice("discovery round sent"),
                notice("polling 2 device object(s)"),
            ],
        );
        let at = dashboard.snapshot.notices.last().unwrap().at;
        let clock = DateTime::<Utc>::from(at)
            .with_timezone(&Local)
            .format("%H:%M:%S")
            .to_string();

        let screen = screen(&mut dashboard, 100, 30);
        assert!(
            screen.contains(&format!(" {clock} polling 2 device object(s)")),
            "{screen}"
        );
        assert!(!screen.contains("discovery round sent"), "{screen}");
    }

    #[test]
    fn a_stopped_service_outranks_its_last_notice() {
        let mut model = RadarModel::new(1);
        model.observe(notice("polling 2 device object(s)"));
        model.set_status(SessionStatus::Stopped);
        let mut dashboard = Dashboard::new(config(10), Arc::new(model.snapshot()));

        let screen = screen(&mut dashboard, 100, 30);
        assert!(screen.contains("the radar service stopped"), "{screen}");
        assert!(!screen.contains("polling 2 device object(s)"), "{screen}");
    }

    #[test]
    fn a_dropped_poll_request_keeps_its_text() {
        assert_eq!(
            poll_error(CommandError::Busy),
            "poll request dropped: the service queue is full"
        );
        assert!(poll_error(CommandError::ServiceStopped).contains("not running"));
    }
}
