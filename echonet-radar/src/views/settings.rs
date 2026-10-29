//! Settings and diagnostics.

use std::time::Duration;

use echonet_radar_core::{MULTICAST_GROUP, MULTICAST_PORT};

use crate::net::{self, Interface};
use crate::settings::{MAX_DISCOVERY, MAX_UPDATE, MIN_DISCOVERY, MIN_UPDATE, Settings};
use crate::views::interface_picker;

/// Draw the interface picker, the two intervals and the bind diagnostics.
pub fn settings(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    interfaces: &[Interface],
) {
    egui::Grid::new("settings-grid")
        .num_columns(2)
        .show(ui, |ui| {
            let label = ui.label("Interface");
            interface_picker(ui, &mut settings.interface, interfaces).labelled_by(label.id);
            ui.end_row();

            let label = ui.label("Discovery interval (s)");
            seconds_field(
                ui,
                &mut settings.discovery_interval,
                MIN_DISCOVERY,
                MAX_DISCOVERY,
            )
            .labelled_by(label.id);
            ui.end_row();

            let label = ui.label("Poll interval (s)");
            seconds_field(ui, &mut settings.update_interval, MIN_UPDATE, MAX_UPDATE)
                .labelled_by(label.id);
            ui.end_row();
        });

    ui.separator();
    ui.label(format!(
        "Multicast group {MULTICAST_GROUP}:{MULTICAST_PORT}"
    ));
    ui.label(format!("Bound addresses 0.0.0.0:{MULTICAST_PORT}"));
    match net::resolve_in(&settings.interface, interfaces) {
        Ok(address) if address.is_unspecified() => {
            ui.label(String::from("Resolved interface: every interface"));
        },
        Ok(address) => {
            ui.label(format!("Resolved interface {address}:{MULTICAST_PORT}"));
        },
        Err(name) => {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Interface {name} is not present; Automatic runs instead."),
            );
        },
    }
}

fn seconds_field(
    ui: &mut egui::Ui,
    interval: &mut Duration,
    min: Duration,
    max: Duration,
) -> egui::Response {
    let mut seconds = interval.as_secs();
    let range = min.as_secs()..=max.as_secs();
    let response = ui.add(egui::DragValue::new(&mut seconds).range(range).speed(1.0));
    if response.changed() {
        *interval = Duration::from_secs(seconds);
    }
    response
}
