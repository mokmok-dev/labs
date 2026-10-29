//! The device table: one row per object, sorted by class name then address.

use std::time::{Duration, SystemTime};

use echonet_radar_core::{ObjectKey, ObjectSnapshot, PropertySnapshot, RadarSnapshot, power_epc};
use egui_extras::{Column, TableBuilder};

use crate::readiness::freshness;
use crate::views::{aged_text, format_age, format_eoj};

/// Height the device table may take before it scrolls, so the pane below stays visible.
const TABLE_MAX_HEIGHT: f32 = 240.0;

/// Row order shared by the table and the arrow keys.
#[must_use]
pub fn ordered_keys(snapshot: &RadarSnapshot) -> Vec<ObjectKey> {
    let mut keys: Vec<ObjectKey> = snapshot.objects.keys().copied().collect();
    keys.sort_by(|left, right| {
        let left_name = snapshot
            .objects
            .get(left)
            .map_or("", |object| object.class_name);
        let right_name = snapshot
            .objects
            .get(right)
            .map_or("", |object| object.class_name);
        left_name
            .cmp(right_name)
            .then(left.ip.cmp(&right.ip))
            .then(left.eoj.cmp(&right.eoj))
    });
    keys
}

/// Draw the table and write a clicked row into `selection`.
pub fn device_list(
    ui: &mut egui::Ui,
    snapshot: &RadarSnapshot,
    selection: &mut Option<ObjectKey>,
    update_interval: Duration,
) {
    let keys = ordered_keys(snapshot);
    let now = SystemTime::now();
    let mut clicked = None;
    TableBuilder::new(ui)
        .striped(true)
        .min_scrolled_height(0.0)
        .max_scroll_height(TABLE_MAX_HEIGHT)
        .column(Column::remainder().at_least(140.0))
        .column(Column::auto())
        .column(Column::auto())
        .column(Column::remainder().at_least(80.0))
        .column(Column::auto())
        .header(18.0, |mut header| {
            for title in ["Class", "EOJ", "Address", "Value", "Last seen"] {
                header.col(|ui| {
                    ui.strong(title);
                });
            }
        })
        .body(|body| {
            body.rows(18.0, keys.len(), |mut row| {
                let Some(key) = keys.get(row.index()).copied() else {
                    return;
                };
                let Some(object) = snapshot.objects.get(&key) else {
                    return;
                };
                let state = freshness(object.last_seen, now, update_interval);
                row.col(|ui| {
                    let text = aged_text(ui, object.class_name, state);
                    if ui.selectable_label(*selection == Some(key), text).clicked() {
                        clicked = Some(key);
                    }
                });
                row.col(|ui| {
                    ui.label(aged_text(ui, &format_eoj(key), state));
                });
                row.col(|ui| {
                    ui.label(aged_text(ui, &object.key.ip.to_string(), state));
                });
                row.col(|ui| {
                    ui.label(aged_text(ui, key_value(object), state));
                });
                row.col(|ui| {
                    ui.label(aged_text(ui, &format_age(now, object.last_seen), state));
                });
            });
        });
    if clicked.is_some() {
        *selection = clicked;
    }
}

fn key_value(object: &ObjectSnapshot) -> &str {
    let power = power_epc(object.class_code).and_then(|epc| object.properties.get(&epc));
    let property: Option<&PropertySnapshot> = power.or_else(|| object.properties.values().next());
    property.map_or("", |property| property.text.as_str())
}
