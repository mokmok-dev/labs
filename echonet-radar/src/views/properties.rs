use std::time::SystemTime;

use echonet_radar_core::{ObjectKey, RadarSnapshot};

use crate::views::{format_age, format_eoj};

pub fn property_pane(
    ui: &mut egui::Ui,
    snapshot: &RadarSnapshot,
    key: ObjectKey,
) {
    let Some(object) = snapshot.objects.get(&key) else {
        return;
    };
    ui.separator();
    ui.heading(object.class_name);
    ui.label(format!("{} at {}", format_eoj(key), key.ip));
    let now = SystemTime::now();
    egui::ScrollArea::vertical()
        .max_height(220.0)
        .show(ui, |ui| {
            egui::Grid::new("property-rows")
                .num_columns(3)
                .striped(true)
                .show(ui, |ui| {
                    for (epc, property) in &object.properties {
                        ui.monospace(format!("0x{epc:02X}"));
                        ui.label(&property.text);
                        ui.label(format_age(now, property.at));
                        ui.end_row();
                    }
                });
        });
}
