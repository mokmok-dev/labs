use std::time::Duration;

use echonet_radar_core::{MULTICAST_GROUP, MULTICAST_PORT};
use echonet_radar_permissions::PlanAction;

use crate::net::{self, Interface};
use crate::permission::Host;
use crate::settings::{MAX_DISCOVERY, MAX_UPDATE, MIN_DISCOVERY, MIN_UPDATE, Settings};
use crate::views::interface_picker;
use crate::views::permission::{Card, describe_action, outcome_text, scope_text, verdict_label};

#[derive(Debug, Clone, Copy, Default)]
pub struct SettingsActions {
    pub add_rule: bool,
    pub remove_rule: bool,
}

pub fn settings(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    interfaces: &[Interface],
    card: &Card<'_>,
    actions: &mut SettingsActions,
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

    ui.separator();
    firewall(ui, card, actions);
}

fn firewall(
    ui: &mut egui::Ui,
    card: &Card<'_>,
    actions: &mut SettingsActions,
) {
    ui.strong("Firewall");
    ui.label(format!(
        "Report: {}",
        card.report
            .map_or("not checked yet", |report| verdict_label(report.verdict))
    ));
    if let Some(plan) = card.plan {
        ui.label(format!("Plan: {}", scope_text(plan.scope)));
        ui.label(format!("Command: {}", describe_action(plan)));
    }
    let addable = card.capability.can_add_rule
        && card
            .plan
            .is_some_and(|plan| !matches!(plan.action, PlanAction::Manual));
    ui.horizontal(|ui| {
        if ui
            .add_enabled(addable, egui::Button::new("Add rule"))
            .clicked()
        {
            actions.add_rule = true;
        }
        if card.host != Host::MacOs && ui.button("Remove rule").clicked() {
            actions.remove_rule = true;
        }
    });
    if let Some(error) = card.error {
        ui.colored_label(ui.visuals().error_fg_color, format!("Last error: {error}"));
    } else if let Some(outcome) = card.outcome {
        ui.label(format!("Last outcome: {}", outcome_text(outcome)));
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

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::time::SystemTime;

    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable as _;

    use echonet_radar_permissions::{Capability, Plan, Report, Scope, Verdict};

    use crate::test_support::drawn_text;
    use crate::views::permission::Card;

    use super::*;

    struct Panel {
        report: Report,
        plan: Plan,
        settings: Settings,
        actions: SettingsActions,
    }

    impl Panel {
        fn draw(
            &mut self,
            ui: &mut egui::Ui,
        ) {
            let card = Card {
                host: Host::Linux,
                capability: Capability {
                    scope: Scope::Port,
                    can_add_rule: true,
                    can_read_rule: false,
                    needs_helper: true,
                    notes: "",
                },
                report: Some(&self.report),
                plan: Some(&self.plan),
                outcome: None,
                error: None,
                trusted: true,
                closable: false,
            };
            settings(ui, &mut self.settings, &[], &card, &mut self.actions);
        }
    }

    fn panel() -> Panel {
        Panel {
            report: Report {
                verdict: Verdict::RuleAbsent,
                checked_at: SystemTime::UNIX_EPOCH,
            },
            plan: Plan {
                action: PlanAction::RunHelper {
                    helper: PathBuf::from("/usr/libexec/echonet-radar-firewall-helper"),
                    args: ["--backend", "ufw", "--add", "--port", "3610"]
                        .into_iter()
                        .map(OsString::from)
                        .collect(),
                },
                scope: Scope::Port,
                manual: String::from("sudo ufw allow 3610/udp"),
            },
            settings: Settings::default(),
            actions: SettingsActions::default(),
        }
    }

    #[test]
    fn the_firewall_section_names_the_report_plan_and_command() {
        let mut state = panel();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| state.draw(ui));
        let drawn = drawn_text(&mut output);
        for needle in [
            "Report: no matching rule",
            "Command: /usr/libexec/echonet-radar-firewall-helper --backend ufw --add --port 3610",
            "Add rule",
            "Remove rule",
        ] {
            assert!(
                drawn.iter().any(|text| text.contains(needle)),
                "{needle} missing from {drawn:?}"
            );
        }
    }

    #[test]
    fn the_add_rule_button_asks_the_card_for_its_confirmation() {
        let mut harness = Harness::new_ui_state(|ui, panel: &mut Panel| panel.draw(ui), panel());
        harness.get_by_label("Add rule").click();
        harness.step();
        assert!(harness.state().actions.add_rule);
        assert!(!harness.state().actions.remove_rule);
    }
}
