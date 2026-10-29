//! `egui_plot` pairs with egui 0.36 or 0.38, never with the egui 0.37 of
//! `eframe` 0.37, so the chart is drawn with egui primitives.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use echonet_radar_core::{RadarSnapshot, instantaneous_watts, power_epc};

use crate::readiness::{Freshness, freshness};

pub const CHART_CAPACITY: usize = 120;

const METER_CLASS: u16 = 0x0288;
const BOARD_CLASS: u16 = 0x0287;

/// A meter that has missed three polls contributes a gap instead of a flat line
/// at its last value, so a dead device cannot look like a steady reading.
pub fn push_sample(
    series: &mut VecDeque<Option<i64>>,
    snapshot: &RadarSnapshot,
    now: SystemTime,
    update_interval: Duration,
) {
    series.push_back(sample(snapshot, now, update_interval));
    while series.len() > CHART_CAPACITY {
        series.pop_front();
    }
}

pub fn draw(
    ui: &mut egui::Ui,
    series: &VecDeque<Option<i64>>,
) {
    let (response, painter) =
        ui.allocate_painter(egui::vec2(ui.available_width(), 90.0), egui::Sense::hover());
    let rect = response.rect;
    let visuals = ui.visuals();
    let zero = rect.center().y;
    painter.rect_filled(rect, 2.0, visuals.extreme_bg_color);
    painter.line_segment(
        [
            egui::pos2(rect.left(), zero),
            egui::pos2(rect.right(), zero),
        ],
        egui::Stroke::new(1.0, visuals.weak_text_color()),
    );
    painter.text(
        egui::pos2(rect.right() - 4.0, zero - 4.0),
        egui::Align2::RIGHT_BOTTOM,
        "Buy",
        egui::FontId::proportional(10.0),
        visuals.weak_text_color(),
    );
    painter.text(
        egui::pos2(rect.right() - 4.0, zero + 4.0),
        egui::Align2::RIGHT_TOP,
        "Sell",
        egui::FontId::proportional(10.0),
        visuals.weak_text_color(),
    );

    let peak = series
        .iter()
        .flatten()
        .map(|watts| watts.saturating_abs())
        .max();
    let Some(peak) = peak else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No power reading yet",
            egui::FontId::proportional(12.0),
            visuals.weak_text_color(),
        );
        return;
    };
    let peak = peak.max(1);
    let half = (rect.height() / 2.0 - 10.0).max(1.0);
    let stroke = egui::Stroke::new(1.5, visuals.text_color());
    let mut run: Vec<egui::Pos2> = Vec::new();
    for (index, sample) in series.iter().enumerate() {
        let Some(watts) = sample else {
            flush(&painter, &mut run, stroke);
            continue;
        };
        let x = rect.width().mul_add(fraction(index), rect.left());
        let y = half.mul_add(-scale(*watts, peak), zero);
        run.push(egui::pos2(x, y));
    }
    flush(&painter, &mut run, stroke);
    painter.text(
        egui::pos2(rect.left() + 4.0, rect.top() + 2.0),
        egui::Align2::LEFT_TOP,
        format!("{peak} W"),
        egui::FontId::proportional(10.0),
        visuals.weak_text_color(),
    );
}

fn flush(
    painter: &egui::Painter,
    run: &mut Vec<egui::Pos2>,
    stroke: egui::Stroke,
) {
    if run.len() > 1 {
        painter.add(egui::Shape::line(std::mem::take(run), stroke));
    } else if let Some(point) = run.pop() {
        painter.circle_filled(point, 1.5, stroke.color);
    }
    run.clear();
}

/// Sample indices are small and watt values are far inside the range `f32`
/// represents without visible loss.
#[allow(clippy::cast_precision_loss)]
fn fraction(index: usize) -> f32 {
    index as f32 / (CHART_CAPACITY - 1) as f32
}

#[allow(clippy::cast_precision_loss)]
fn scale(
    watts: i64,
    peak: i64,
) -> f32 {
    watts as f32 / peak as f32
}

fn sample(
    snapshot: &RadarSnapshot,
    now: SystemTime,
    update_interval: Duration,
) -> Option<i64> {
    let object = [METER_CLASS, BOARD_CLASS].into_iter().find_map(|class| {
        snapshot
            .objects
            .values()
            .find(|object| object.class_code == class)
    })?;
    if freshness(object.last_seen, now, update_interval) != Freshness::Fresh {
        return None;
    }
    let epc = power_epc(object.class_code)?;
    let property = object.properties.get(&epc)?;
    instantaneous_watts(object.class_code, &property.raw_edt)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::Ipv4Addr;

    use echonet_radar_core::ObjectSnapshot;

    use super::*;
    use crate::test_support::{object, property, snapshot};

    fn meter(
        ip: u8,
        class_code: u16,
        name: &'static str,
        edt: &[u8],
        last_seen: SystemTime,
    ) -> ObjectSnapshot {
        let epc = power_epc(class_code).expect("class has a power property");
        object(
            Ipv4Addr::new(192, 168, 1, ip),
            class_code,
            name,
            BTreeMap::from([(epc, property(edt, last_seen))]),
            last_seen,
        )
    }

    fn with(
        objects: Vec<ObjectSnapshot>,
        now: SystemTime,
    ) -> RadarSnapshot {
        let mut snap = snapshot(now);
        snap.objects = objects
            .into_iter()
            .map(|object| (object.key, object))
            .collect();
        snap
    }

    #[test]
    fn the_series_stops_at_the_capacity() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let snap = with(
            vec![meter(
                20,
                METER_CLASS,
                "Low-voltage smart electric energy meter",
                &[0, 0, 2, 0x44],
                now,
            )],
            now,
        );
        let mut series = VecDeque::new();
        for _ in 0..CHART_CAPACITY + 10 {
            push_sample(&mut series, &snap, now, Duration::from_secs(15));
        }
        assert_eq!(series.len(), CHART_CAPACITY);
        assert!(series.iter().all(Option::is_some));
    }

    #[test]
    fn a_smart_meter_wins_over_a_distribution_board() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let snap = with(
            vec![
                meter(
                    10,
                    METER_CLASS,
                    "Low-voltage smart electric energy meter",
                    &[0, 0, 2, 0x44],
                    now,
                ),
                meter(
                    20,
                    BOARD_CLASS,
                    "Power distribution board metering",
                    &[0, 0, 0, 100],
                    now,
                ),
            ],
            now,
        );
        let mut series = VecDeque::new();
        push_sample(&mut series, &snap, now, Duration::from_secs(15));
        assert_eq!(series.back(), Some(&Some(580)));
    }

    #[test]
    fn the_distribution_board_is_the_fallback() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let snap = with(
            vec![meter(
                20,
                BOARD_CLASS,
                "Power distribution board metering",
                &[0, 0, 0, 100],
                now,
            )],
            now,
        );
        let mut series = VecDeque::new();
        push_sample(&mut series, &snap, now, Duration::from_secs(15));
        assert_eq!(series.back(), Some(&Some(100)));
    }

    #[test]
    fn a_sample_is_a_gap_when_the_meter_is_stale() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let seen = now - Duration::from_secs(60);
        let snap = with(
            vec![meter(
                20,
                METER_CLASS,
                "Low-voltage smart electric energy meter",
                &[0, 0, 2, 0x44],
                seen,
            )],
            now,
        );
        let mut series = VecDeque::new();
        push_sample(&mut series, &snap, now, Duration::from_secs(15));
        assert_eq!(series.back(), Some(&None));
    }

    #[test]
    fn an_empty_snapshot_is_a_gap() {
        let now = SystemTime::UNIX_EPOCH;
        let mut series = VecDeque::from([Some(1)]);
        push_sample(&mut series, &snapshot(now), now, Duration::from_secs(15));
        assert_eq!(series.back(), Some(&None));
    }

    #[test]
    fn a_non_power_class_is_ignored() {
        let now = SystemTime::UNIX_EPOCH;
        let air_conditioner = object(
            Ipv4Addr::new(192, 168, 1, 30),
            0x0130,
            "Home air conditioner",
            BTreeMap::from([(0x80, property(&[0x30], now))]),
            now,
        );
        let snap = with(vec![air_conditioner], now);
        let mut series = VecDeque::new();
        push_sample(&mut series, &snap, now, Duration::from_secs(15));
        assert_eq!(series.back(), Some(&None));
    }
}
