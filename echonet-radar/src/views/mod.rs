pub mod devices;
pub mod permission;
pub mod properties;
pub mod settings;
pub mod start;
pub mod status;

use std::time::SystemTime;

use echonet_radar_core::ObjectKey;

use crate::net::Interface;
use crate::readiness::Freshness;
use crate::settings::InterfaceChoice;

pub fn interface_picker(
    ui: &mut egui::Ui,
    choice: &mut InterfaceChoice,
    interfaces: &[Interface],
) -> egui::Response {
    egui::ComboBox::from_id_salt("interface-picker")
        .selected_text(choice.label())
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(*choice == InterfaceChoice::Auto, "Automatic")
                .clicked()
            {
                *choice = InterfaceChoice::Auto;
            }
            for interface in interfaces {
                let named = InterfaceChoice::Named(interface.name.clone());
                let text = format!("{} ({})", interface.name, interface.addr);
                if ui.selectable_label(*choice == named, text).clicked() {
                    *choice = named;
                }
            }
        })
        .response
}

#[must_use]
pub fn format_eoj(key: ObjectKey) -> String {
    format!(
        "0x{:02X}{:02X}{:02X}",
        key.eoj.class_group, key.eoj.class, key.eoj.instance
    )
}

#[must_use]
pub fn format_age(
    now: SystemTime,
    at: SystemTime,
) -> String {
    let age = now.duration_since(at).unwrap_or_default();
    if age.as_secs() < 60 {
        format!("{} s ago", age.as_secs())
    } else if age.as_secs() < 3600 {
        format!("{} min ago", age.as_secs() / 60)
    } else {
        format!("{} h ago", age.as_secs() / 3600)
    }
}

#[must_use]
pub fn aged_text(
    ui: &egui::Ui,
    text: &str,
    freshness: Freshness,
) -> egui::RichText {
    let color = if freshness == Freshness::Fresh {
        ui.visuals().text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    egui::RichText::new(text).color(color)
}
