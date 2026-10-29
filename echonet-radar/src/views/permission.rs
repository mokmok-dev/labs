//! The network-access card: one verdict, one action, and the proof traffic gives.

use echonet_radar_permissions::{
    ApplyError, ApplyOutcome, BackendState, Capability, ECHONET_PORT, Plan, PlanAction, Report,
    Scope, Verdict,
};

use crate::permission::Host;

/// The apply the confirmation is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    AddRule,
}

/// A click the card reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CardAction {
    /// Nothing was clicked.
    #[default]
    None,
    /// Inspect the host again.
    CheckAgain,
    /// The confirmation was accepted, so the rule may be added.
    AddRule,
    /// Open the macOS Firewall pane.
    OpenFirewallSettings,
    /// Open the macOS Local Network pane.
    OpenLocalNetworkSettings,
    /// Hide a card the status bar opened.
    Close,
}

/// The card's own click state, owned by the app.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CardState {
    /// The apply waiting for its second click.
    pub confirming: Option<Confirm>,
}

/// Everything the card draws.
pub struct Card<'a> {
    pub host: Host,
    pub capability: Capability,
    pub report: Option<&'a Report>,
    pub plan: Option<&'a Plan>,
    pub outcome: Option<ApplyOutcome>,
    pub error: Option<&'a ApplyError>,
    /// Whether the last plan names a helper the app is willing to elevate.
    pub trusted: bool,
    /// Whether the card was opened by hand and may be closed.
    pub closable: bool,
}

/// Draw the card and report the one click it received.
pub fn permission_card(
    ui: &mut egui::Ui,
    card: &Card<'_>,
    state: &mut CardState,
) -> CardAction {
    let verdict = card.report.map(|report| report.verdict);
    let mut action = CardAction::None;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.heading("Firewall and network access");
        explain(ui, card, verdict);
        result_line(ui, card);
        ui.horizontal_wrapped(|ui| {
            row(ui, card, verdict, state, &mut action);
        });
        if state.confirming.is_some() {
            confirmation(ui, card, state, &mut action);
        }
    });
    action
}

/// The scope in the words the confirmation and the settings panel use.
#[must_use]
pub fn scope_text(scope: Scope) -> String {
    match scope {
        Scope::Application => String::from("this app"),
        Scope::ApplicationAndPort => format!("this app and UDP {ECHONET_PORT}"),
        Scope::Port => format!("port {ECHONET_PORT} for this machine"),
    }
}

/// The exact argv an action would run.
#[must_use]
pub fn describe_action(plan: &Plan) -> String {
    match &plan.action {
        PlanAction::RunHelper { helper, args } => {
            let mut parts = vec![helper.display().to_string()];
            parts.extend(args.iter().map(|arg| arg.to_string_lossy().into_owned()));
            parts.join(" ")
        },
        PlanAction::OpenSettings { url } => format!("open {url}"),
        PlanAction::Manual => String::from("nothing; the steps are manual"),
    }
}

/// A verdict in the one sentence the settings panel shows.
#[must_use]
pub const fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::NotInspected => "not checked yet",
        Verdict::NoFirewall => "the firewall is off on every profile",
        Verdict::RulePresent { .. } => "a matching rule is present",
        Verdict::RuleAbsent => "no matching rule",
        Verdict::NotObservable { .. } => "not readable on this host",
        Verdict::ManualSteps { .. } => "manual steps required",
        Verdict::Unsupported { .. } => "unsupported on this host",
    }
}

/// What an apply or remove achieved, in one sentence.
#[must_use]
pub const fn outcome_text(outcome: ApplyOutcome) -> &'static str {
    match outcome {
        ApplyOutcome::Applied => "The helper reported success.",
        ApplyOutcome::Cancelled => {
            "The prompt was cancelled. Nothing changed, so the steps still apply."
        },
        ApplyOutcome::ManualSteps => "Nothing ran. Follow the steps above.",
    }
}

fn explain(
    ui: &mut egui::Ui,
    card: &Card<'_>,
    verdict: Option<Verdict>,
) {
    match verdict {
        None | Some(Verdict::NotInspected) => {
            ui.label("The firewall has not been checked yet.");
        },
        Some(Verdict::Unsupported { reason }) => {
            ui.label("This copy of ECHONET Radar cannot open a firewall port.");
            ui.monospace(reason);
        },
        Some(Verdict::ManualSteps { steps, .. }) => {
            ui.label("The app cannot change the firewall on this host. Do this instead:");
            ui.monospace(steps);
        },
        Some(Verdict::RulePresent { .. }) => rule_present(ui),
        Some(Verdict::NoFirewall) => {
            ui.label(
                "Windows reports the firewall disabled on every profile, so no rule is needed.",
            );
        },
        Some(Verdict::RuleAbsent | Verdict::NotObservable { .. }) => match card.host {
            Host::MacOs => macos_access(ui),
            Host::Windows => windows_access(ui, card, verdict),
            Host::Linux => linux_access(ui, card, verdict),
        },
    }
}

fn rule_present(ui: &mut egui::Ui) {
    ui.label("A matching firewall rule is present.");
    ui.label("Only incoming traffic proves the path, so the status stays unverified until a datagram arrives.");
}

fn macos_access(ui: &mut egui::Ui) {
    ui.label(
        "macOS asks per app. The system dialog is the approval, and the app cannot read the answer back.",
    );
    ui.label(format!(
        "macOS cannot scope the rule to port {ECHONET_PORT}, so only incoming traffic proves the path. \
         On macOS 15 or later the first multicast access may also ask for local network access, and an \
         unsigned or ad-hoc rebuild can count as a new app."
    ));
}

fn windows_access(
    ui: &mut egui::Ui,
    card: &Card<'_>,
    verdict: Option<Verdict>,
) {
    if matches!(verdict, Some(Verdict::RuleAbsent)) {
        ui.label(format!(
            "Windows has no inbound firewall rule for ECHONET Radar on UDP {ECHONET_PORT}."
        ));
    } else {
        ui.label("Windows would not report the firewall rule to this account.");
    }
    ui.label(format!(
        "The rule is inbound, UDP {ECHONET_PORT}, for this program, on the private and domain profiles. \
         A Public network profile does not match it."
    ));
    if !add_offered(card, verdict) {
        manual_fallback(
            ui,
            card,
            "The app will not elevate a helper from this location. Add the rule by hand:",
        );
    }
}

fn linux_access(
    ui: &mut egui::Ui,
    card: &Card<'_>,
    verdict: Option<Verdict>,
) {
    let running = matches!(
        verdict,
        Some(Verdict::NotObservable {
            backend: BackendState::Running { .. }
        })
    );
    if running {
        ui.label(format!(
            "The rule opens port {ECHONET_PORT} for this machine, not for this app."
        ));
        ui.label(
            "A running firewalld or ufw exposes no readable verdict, so only incoming traffic proves the path.",
        );
    } else {
        ui.label(
            "The app cannot read this host's firewall, so it cannot tell whether the port is open.",
        );
    }
    if !allow_offered(card, verdict) {
        manual_fallback(
            ui,
            card,
            "The helper is not under a trusted path, so the app will not run it. Add the rule by hand:",
        );
    }
}

fn manual_fallback(
    ui: &mut egui::Ui,
    card: &Card<'_>,
    lead: &str,
) {
    ui.label(lead);
    if let Some(plan) = card.plan {
        ui.monospace(&plan.manual);
    }
}

fn result_line(
    ui: &mut egui::Ui,
    card: &Card<'_>,
) {
    if let Some(error) = card.error {
        ui.colored_label(
            ui.visuals().error_fg_color,
            format!("The last attempt failed: {error}"),
        );
    } else if let Some(outcome) = card.outcome {
        ui.label(outcome_text(outcome));
    }
}

fn add_offered(
    card: &Card<'_>,
    verdict: Option<Verdict>,
) -> bool {
    card.capability.can_add_rule
        && matches!(
            verdict,
            Some(Verdict::RuleAbsent | Verdict::NotObservable { .. })
        )
        && plans_helper(card)
}

fn allow_offered(
    card: &Card<'_>,
    verdict: Option<Verdict>,
) -> bool {
    card.trusted
        && matches!(
            verdict,
            Some(Verdict::NotObservable {
                backend: BackendState::Running { .. }
            })
        )
        && plans_helper(card)
}

fn plans_helper(card: &Card<'_>) -> bool {
    matches!(
        card.plan.map(|plan| &plan.action),
        Some(PlanAction::RunHelper { .. })
    )
}

fn row(
    ui: &mut egui::Ui,
    card: &Card<'_>,
    verdict: Option<Verdict>,
    state: &mut CardState,
    action: &mut CardAction,
) {
    match card.host {
        Host::MacOs => {
            if ui.button("Open Firewall settings").clicked() {
                *action = CardAction::OpenFirewallSettings;
            }
            if ui.button("Open Local Network settings").clicked() {
                *action = CardAction::OpenLocalNetworkSettings;
            }
        },
        Host::Windows => {
            if add_offered(card, verdict) && ui.button("Add firewall rule").clicked() {
                state.confirming = Some(Confirm::AddRule);
            }
        },
        Host::Linux => {
            if allow_offered(card, verdict)
                && ui.button(format!("Allow UDP {ECHONET_PORT}")).clicked()
            {
                state.confirming = Some(Confirm::AddRule);
            }
        },
    }
    if ui.button("Check again").clicked() {
        *action = CardAction::CheckAgain;
    }
    if card.closable && ui.button("Close").clicked() {
        *action = CardAction::Close;
    }
}

fn confirmation(
    ui: &mut egui::Ui,
    card: &Card<'_>,
    state: &mut CardState,
    action: &mut CardAction,
) {
    ui.separator();
    ui.strong("Add the firewall rule?");
    if let Some(plan) = card.plan {
        ui.label(format!("Scope: {}", scope_text(plan.scope)));
        ui.label(format!("Runs: {}", describe_action(plan)));
    }
    ui.horizontal(|ui| {
        if ui.button("Confirm").clicked() {
            state.confirming = None;
            *action = CardAction::AddRule;
        }
        if ui.button("Cancel").clicked() {
            state.confirming = None;
        }
    });
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::time::SystemTime;

    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable as _;

    use echonet_radar_permissions::{MACOS_FIREWALL_SETTINGS_URL, Method};

    use crate::test_support::drawn_text;

    use super::*;

    const HELPER: &str = "/usr/libexec/echonet-radar-firewall-helper";
    const WINDOWS_RULE: &str =
        "New-NetFirewallRule -DisplayName EchonetRadarUdp3610Private -Protocol UDP -LocalPort 3610";

    fn report(verdict: Verdict) -> Report {
        Report {
            verdict,
            checked_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn helper_plan(scope: Scope) -> Plan {
        Plan {
            action: PlanAction::RunHelper {
                helper: PathBuf::from(HELPER),
                args: ["--backend", "ufw", "--add", "--port", "3610"]
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
            },
            scope,
            manual: String::from("sudo ufw allow 3610/udp"),
        }
    }

    fn manual_plan() -> Plan {
        Plan {
            action: PlanAction::Manual,
            scope: Scope::ApplicationAndPort,
            manual: String::from(WINDOWS_RULE),
        }
    }

    fn capability(
        scope: Scope,
        can_add_rule: bool,
    ) -> Capability {
        Capability {
            scope,
            can_add_rule,
            can_read_rule: false,
            needs_helper: true,
            notes: "",
        }
    }

    struct Fixture {
        host: Host,
        capability: Capability,
        report: Report,
        plan: Plan,
        outcome: Option<ApplyOutcome>,
        error: Option<ApplyError>,
        trusted: bool,
        state: CardState,
        action: CardAction,
    }

    impl Fixture {
        fn new(
            host: Host,
            verdict: Verdict,
            plan: Plan,
        ) -> Self {
            let scope = plan.scope;
            Self {
                host,
                capability: capability(scope, true),
                report: report(verdict),
                plan,
                outcome: None,
                error: None,
                trusted: true,
                state: CardState::default(),
                action: CardAction::None,
            }
        }

        fn draw(
            &mut self,
            ui: &mut egui::Ui,
        ) {
            let card = Card {
                host: self.host,
                capability: self.capability,
                report: Some(&self.report),
                plan: Some(&self.plan),
                outcome: self.outcome,
                error: self.error.as_ref(),
                trusted: self.trusted,
                closable: false,
            };
            self.action = permission_card(ui, &card, &mut self.state);
        }
    }

    fn draw(fixture: &mut Fixture) -> Vec<String> {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| fixture.draw(ui));
        drawn_text(&mut output)
    }

    fn shows(
        texts: &[String],
        needle: &str,
    ) -> bool {
        texts.iter().any(|text| text.contains(needle))
    }

    #[test]
    fn an_unchecked_card_says_so_and_offers_no_apply() {
        let mut fixture =
            Fixture::new(Host::Linux, Verdict::NotInspected, helper_plan(Scope::Port));
        let texts = draw(&mut fixture);
        assert!(shows(&texts, "has not been checked yet"));
        assert!(shows(&texts, "Check again"));
        assert!(!shows(&texts, "Allow UDP 3610"));
    }

    #[test]
    fn a_present_rule_still_says_only_traffic_proves_the_path() {
        let mut fixture = Fixture::new(
            Host::Windows,
            Verdict::RulePresent {
                method: Method::WindowsDefender,
                scope: Scope::ApplicationAndPort,
            },
            helper_plan(Scope::ApplicationAndPort),
        );
        let texts = draw(&mut fixture);
        assert!(shows(&texts, "A matching firewall rule is present"));
        assert!(shows(&texts, "Only incoming traffic proves the path"));
        assert!(!shows(&texts, "Add firewall rule"));
    }

    #[test]
    fn windows_offers_the_add_button_for_a_runnable_helper() {
        let mut fixture = Fixture::new(
            Host::Windows,
            Verdict::RuleAbsent,
            helper_plan(Scope::ApplicationAndPort),
        );
        let texts = draw(&mut fixture);
        assert!(shows(&texts, "Windows has no inbound firewall rule"));
        assert!(shows(&texts, "Add firewall rule"));
        assert!(!shows(&texts, "New-NetFirewallRule"));
    }

    #[test]
    fn a_windows_build_outside_program_files_shows_the_command() {
        let mut fixture = Fixture::new(Host::Windows, Verdict::RuleAbsent, manual_plan());
        let texts = draw(&mut fixture);
        assert!(!shows(&texts, "Add firewall rule"));
        assert!(shows(
            &texts,
            "will not elevate a helper from this location"
        ));
        assert!(shows(&texts, "New-NetFirewallRule"));
    }

    #[test]
    fn a_windows_capability_without_add_shows_the_command() {
        let mut fixture = Fixture::new(
            Host::Windows,
            Verdict::RuleAbsent,
            helper_plan(Scope::ApplicationAndPort),
        );
        fixture.capability = capability(Scope::ApplicationAndPort, false);
        let texts = draw(&mut fixture);
        assert!(!shows(&texts, "Add firewall rule"));
        assert!(shows(&texts, "sudo ufw allow 3610/udp"));
    }

    #[test]
    fn windows_without_a_firewall_needs_no_rule() {
        let mut fixture = Fixture::new(
            Host::Windows,
            Verdict::NoFirewall,
            helper_plan(Scope::ApplicationAndPort),
        );
        let texts = draw(&mut fixture);
        assert!(shows(&texts, "the firewall disabled on every profile"));
        assert!(!shows(&texts, "Add firewall rule"));
    }

    #[test]
    fn macos_offers_both_panes_and_no_helper_action() {
        let mut fixture = Fixture::new(
            Host::MacOs,
            Verdict::NotObservable {
                backend: BackendState::NotObservable,
            },
            Plan {
                action: PlanAction::OpenSettings {
                    url: String::from(MACOS_FIREWALL_SETTINGS_URL),
                },
                scope: Scope::Application,
                manual: String::new(),
            },
        );
        let texts = draw(&mut fixture);
        assert!(shows(&texts, "The system dialog is the approval"));
        assert!(shows(&texts, "Open Firewall settings"));
        assert!(shows(&texts, "Open Local Network settings"));
        assert!(!shows(&texts, "Add firewall rule"));
        assert!(!shows(&texts, "Allow UDP 3610"));
    }

    #[test]
    fn linux_offers_the_port_button_and_never_calls_the_rule_app_specific() {
        let mut fixture = Fixture::new(
            Host::Linux,
            Verdict::NotObservable {
                backend: BackendState::Running { permanent: None },
            },
            helper_plan(Scope::Port),
        );
        let texts = draw(&mut fixture);
        assert!(shows(&texts, "port 3610 for this machine"));
        assert!(shows(&texts, "exposes no readable verdict"));
        assert!(shows(&texts, "Allow UDP 3610"));
        assert!(!shows(&texts, "Add firewall rule"));
    }

    #[test]
    fn an_untrusted_linux_helper_shows_the_command_instead() {
        let mut fixture = Fixture::new(
            Host::Linux,
            Verdict::NotObservable {
                backend: BackendState::Running { permanent: None },
            },
            helper_plan(Scope::Port),
        );
        fixture.trusted = false;
        let texts = draw(&mut fixture);
        assert!(!shows(&texts, "Allow UDP 3610"));
        assert!(shows(&texts, "not under a trusted path"));
        assert!(shows(&texts, "sudo ufw allow 3610/udp"));
    }

    #[test]
    fn an_inactive_backend_shows_the_steps_and_no_button() {
        let mut fixture = Fixture::new(
            Host::Linux,
            Verdict::ManualSteps {
                case: echonet_radar_permissions::ManualCase::UfwInactive,
                steps: "ufw is installed but inactive.",
            },
            helper_plan(Scope::Port),
        );
        let texts = draw(&mut fixture);
        assert!(shows(&texts, "ufw is installed but inactive."));
        assert!(!shows(&texts, "Allow UDP 3610"));
    }

    #[test]
    fn unsupported_names_its_reason() {
        let mut fixture = Fixture::new(
            Host::Linux,
            Verdict::Unsupported {
                reason: "Flatpak grants network access per app.",
            },
            helper_plan(Scope::Port),
        );
        let texts = draw(&mut fixture);
        assert!(shows(&texts, "cannot open a firewall port"));
        assert!(shows(&texts, "Flatpak grants network access per app."));
    }

    #[test]
    fn a_cancelled_prompt_is_not_drawn_as_a_failure() {
        let mut fixture = Fixture::new(
            Host::Linux,
            Verdict::NotObservable {
                backend: BackendState::Running { permanent: None },
            },
            helper_plan(Scope::Port),
        );
        fixture.outcome = Some(ApplyOutcome::Cancelled);
        let texts = draw(&mut fixture);
        assert!(shows(&texts, "The prompt was cancelled"));
        assert!(!shows(&texts, "failed"));
    }

    #[test]
    fn an_apply_error_is_drawn_as_a_failure() {
        let mut fixture = Fixture::new(
            Host::Linux,
            Verdict::NotObservable {
                backend: BackendState::Running { permanent: None },
            },
            helper_plan(Scope::Port),
        );
        fixture.error = Some(ApplyError::NotAuthorized);
        let texts = draw(&mut fixture);
        assert!(shows(
            &texts,
            "The last attempt failed: authorization was refused"
        ));
    }

    #[test]
    fn the_first_click_opens_the_confirmation_and_the_second_applies() {
        let mut harness = Harness::new_ui_state(
            |ui, fixture: &mut Fixture| fixture.draw(ui),
            Fixture::new(
                Host::Linux,
                Verdict::NotObservable {
                    backend: BackendState::Running { permanent: None },
                },
                helper_plan(Scope::Port),
            ),
        );
        harness.get_by_label("Allow UDP 3610").click();
        harness.step();
        assert_eq!(harness.state().state.confirming, Some(Confirm::AddRule));
        assert_eq!(harness.state().action, CardAction::None);
        harness.get_by_label("Confirm").click();
        harness.step();
        assert_eq!(harness.state().action, CardAction::AddRule);
        assert_eq!(harness.state().state.confirming, None);
    }

    #[test]
    fn the_confirmation_names_the_scope_and_the_exact_argv() {
        let mut fixture = Fixture::new(
            Host::Linux,
            Verdict::NotObservable {
                backend: BackendState::Running { permanent: None },
            },
            helper_plan(Scope::Port),
        );
        fixture.state.confirming = Some(Confirm::AddRule);
        let texts = draw(&mut fixture);
        assert!(shows(&texts, "Scope: port 3610 for this machine"));
        assert!(shows(
            &texts,
            &format!("Runs: {HELPER} --backend ufw --add --port 3610")
        ));
    }

    #[test]
    fn cancelling_the_confirmation_emits_nothing() {
        let mut harness = Harness::new_ui_state(
            |ui, fixture: &mut Fixture| fixture.draw(ui),
            Fixture::new(
                Host::Windows,
                Verdict::RuleAbsent,
                helper_plan(Scope::ApplicationAndPort),
            ),
        );
        harness.get_by_label("Add firewall rule").click();
        harness.step();
        harness.get_by_label("Cancel").click();
        harness.step();
        assert_eq!(harness.state().state.confirming, None);
        assert_eq!(harness.state().action, CardAction::None);
    }

    #[test]
    fn the_scope_and_command_come_from_the_plan() {
        let plan = helper_plan(Scope::ApplicationAndPort);
        assert_eq!(scope_text(plan.scope), "this app and UDP 3610");
        assert_eq!(
            describe_action(&plan),
            format!("{HELPER} --backend ufw --add --port 3610")
        );
        assert_eq!(scope_text(Scope::Port), "port 3610 for this machine");
    }
}
