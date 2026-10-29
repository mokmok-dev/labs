//! macOS. The Application Firewall verdict is not readable, so the app opens
//! the pane and lets the system dialog be the approval.

use crate::platform::Mode;
use crate::{BackendState, MACOS_FIREWALL_SETTINGS_URL, Plan, PlanAction, Scope, Verdict};

const OPEN_STEPS: &str = "Open System Settings > Network > Firewall and allow incoming \
                          connections for ECHONET Radar. macOS 15 or later may also ask for \
                          local network access.";
const REMOVE_STEPS: &str = "Turn ECHONET Radar off in System Settings > Network > Firewall.";

pub const fn inspect() -> Verdict {
    Verdict::NotObservable {
        backend: BackendState::NotObservable,
    }
}

pub fn plan(mode: Mode) -> Plan {
    match mode {
        Mode::Add => Plan {
            action: PlanAction::OpenSettings {
                url: MACOS_FIREWALL_SETTINGS_URL.to_owned(),
            },
            scope: Scope::Application,
            manual: OPEN_STEPS.to_owned(),
        },
        Mode::Remove => Plan {
            action: PlanAction::Manual,
            scope: Scope::Application,
            manual: REMOVE_STEPS.to_owned(),
        },
    }
}
