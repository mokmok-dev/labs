//! The status bar: what the session is doing and the three commands.

use std::time::SystemTime;

use echonet_radar_core::RadarSnapshot;

use crate::app::AppActions;
use crate::readiness::Readiness;
use crate::views::format_age;

/// Draw the session summary and record button clicks in `actions`.
pub fn status_bar(
    ui: &mut egui::Ui,
    readiness: &Readiness,
    snapshot: &RadarSnapshot,
    actions: &mut AppActions,
    interface: &str,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(format!("Interface {interface}"));
        ui.separator();
        ui.label(readiness.describe());
        ui.separator();
        ui.label(format!(
            "Last frame {}",
            describe_last_frame(readiness, snapshot)
        ));
        ui.separator();
        ui.label(format!("{} devices", snapshot.objects.len()));
        ui.separator();
        let bound = matches!(
            readiness,
            Readiness::Binding | Readiness::Unverified { .. } | Readiness::Listening { .. }
        );
        if ui.add_enabled(bound, egui::Button::new("Poll")).clicked() {
            actions.poll = true;
        }
        if ui
            .add_enabled(bound, egui::Button::new("Rediscover"))
            .clicked()
        {
            actions.rediscover = true;
        }
        if matches!(readiness, Readiness::Degraded { .. }) && ui.button("Restart").clicked() {
            actions.restart = true;
        }
    });
}

fn describe_last_frame(
    readiness: &Readiness,
    snapshot: &RadarSnapshot,
) -> String {
    readiness.last_frame().or(snapshot.last_frame).map_or_else(
        || String::from("none yet"),
        |last| format_age(SystemTime::now(), last),
    )
}
