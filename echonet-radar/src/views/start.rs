//! The first-run card: one interface choice and one button.

use crate::app::{StartAction, StartFailure};
use crate::net::Interface;
use crate::readiness::describe_fault;
use crate::settings::InterfaceChoice;
use crate::views::interface_picker;

/// Draw the card, recording a click in `action`.
pub fn start_card(
    ui: &mut egui::Ui,
    interface: &mut InterfaceChoice,
    interfaces: &[Interface],
    action: &mut StartAction,
    failure: Option<&StartFailure>,
) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(72.0);
            ui.heading("ECHONET Radar");
            ui.label(
                "Finds ECHONET Lite appliances on your local network and shows their live values.",
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let label = ui.label("Interface");
                interface_picker(ui, interface, interfaces).labelled_by(label.id);
            });
            if interfaces.is_empty() {
                ui.label(
                    "No IPv4 interface was found. Automatic joins every interface the host offers.",
                );
            }
            ui.add_space(12.0);
            if ui.button("Start listening").clicked() {
                *action = StartAction::Start;
            }
            ui.label("The service binds UDP 3610 and sends one discovery request.");
            if let Some(failure) = failure {
                ui.colored_label(ui.visuals().error_fg_color, describe_failure(failure));
            }
        });
    });
}

fn describe_failure(failure: &StartFailure) -> String {
    match failure {
        StartFailure::InterfaceMissing(name) => {
            format!("Interface {name} is not present; Automatic is selected instead.")
        },
        StartFailure::Fault(fault) => describe_fault(fault),
    }
}
