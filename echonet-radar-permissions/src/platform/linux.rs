//! Linux backends: sandbox, NixOS, firewalld and ufw.
//!
//! The stock backends expose no unprivileged rule query, so a running backend
//! is reported as not observable and traffic is the only proof.

use std::ffi::{OsStr, OsString};
use std::io;

use crate::command::{CommandOutput, CommandRunner};
use crate::platform::{HostFacts, LINUX_HELPER, Mode, sibling};
use crate::{BackendState, ManualCase, Plan, PlanAction, Request, Scope, Verdict};

const FLATPAK_REASON: &str =
    "Flatpak grants network access per app; add --share=network to the manifest and rebuild.";
const SNAP_REASON: &str =
    "Snap grants network access through an interface, not through a firewall rule.";
const NIXOS_STEPS: &str =
    "Declare the port: networking.firewall.allowedUDPPorts = [ 3610 ]; then nixos-rebuild switch.";
const UFW_INACTIVE_STEPS: &str = "ufw is installed but inactive. Enable it and allow the port: \
                                   sudo ufw enable && sudo ufw allow 3610/udp";
const NO_BACKEND_STEPS: &str = "No firewalld or ufw was found. Open UDP 3610 for inbound traffic \
                                in whatever firewall this machine runs (nftables, iptables, ...).";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    Flatpak,
    Snap,
    NixOs,
    Firewalld,
    Ufw,
    UfwInactive,
    None,
}

impl Detection {
    pub const fn verdict(self) -> Verdict {
        match self {
            Self::Flatpak => Verdict::Unsupported {
                reason: FLATPAK_REASON,
            },
            Self::Snap => Verdict::Unsupported {
                reason: SNAP_REASON,
            },
            Self::NixOs => Verdict::ManualSteps {
                case: ManualCase::NixOs,
                steps: NIXOS_STEPS,
            },
            Self::Firewalld | Self::Ufw => Verdict::NotObservable {
                backend: BackendState::Running { permanent: None },
            },
            Self::UfwInactive => Verdict::ManualSteps {
                case: ManualCase::UfwInactive,
                steps: UFW_INACTIVE_STEPS,
            },
            Self::None => Verdict::ManualSteps {
                case: ManualCase::NoSupportedBackend,
                steps: NO_BACKEND_STEPS,
            },
        }
    }
}

enum UfwState {
    Active,
    Inactive,
    Absent,
}

pub fn detect(
    runner: &dyn CommandRunner,
    facts: &HostFacts,
) -> Detection {
    if facts.flatpak {
        return Detection::Flatpak;
    }
    if facts.snap {
        return Detection::Snap;
    }
    if facts.nixos {
        return Detection::NixOs;
    }
    if is_exit_zero(runner.run(OsStr::new("firewall-cmd"), &[OsString::from("--state")])) {
        return Detection::Firewalld;
    }
    match ufw_state(runner) {
        UfwState::Active => Detection::Ufw,
        UfwState::Inactive => Detection::UfwInactive,
        UfwState::Absent => Detection::None,
    }
}

fn is_exit_zero(result: io::Result<CommandOutput>) -> bool {
    matches!(result, Ok(output) if output.code == Some(0))
}

fn ufw_state(runner: &dyn CommandRunner) -> UfwState {
    // `command -v` is a shell builtin and there is no binary to spawn. The
    // script is a fixed literal, so no caller value reaches the shell.
    let present = is_exit_zero(runner.run(
        OsStr::new("sh"),
        &[OsString::from("-c"), OsString::from("command -v ufw")],
    ));
    if !present {
        return UfwState::Absent;
    }
    if is_exit_zero(runner.run(
        OsStr::new("systemctl"),
        &[OsString::from("is-active"), OsString::from("ufw")],
    )) {
        UfwState::Active
    } else {
        UfwState::Inactive
    }
}

pub fn plan(
    detection: Detection,
    request: &Request,
    mode: Mode,
) -> Plan {
    let port = request.port;
    match detection {
        Detection::Firewalld => helper_plan(
            request,
            mode,
            "firewalld",
            &format!(
                "sudo firewall-cmd --permanent {} --reload",
                port_flag(mode, port)
            ),
        ),
        Detection::Ufw => helper_plan(request, mode, "ufw", &ufw_command(mode, port)),
        other => Plan {
            action: PlanAction::Manual,
            scope: Scope::Port,
            manual: manual_text(other),
        },
    }
}

fn helper_plan(
    request: &Request,
    mode: Mode,
    backend: &str,
    manual: &str,
) -> Plan {
    Plan {
        action: PlanAction::RunHelper {
            helper: sibling(&request.target, LINUX_HELPER),
            args: vec![
                OsString::from("--backend"),
                OsString::from(backend),
                OsString::from(mode.flag()),
                OsString::from("--port"),
                OsString::from(request.port.to_string()),
            ],
        },
        scope: Scope::Port,
        manual: manual.to_owned(),
    }
}

fn port_flag(
    mode: Mode,
    port: u16,
) -> String {
    let flag = match mode {
        Mode::Add => "--add-port",
        Mode::Remove => "--remove-port",
    };
    format!("{flag}={port}/udp")
}

fn ufw_command(
    mode: Mode,
    port: u16,
) -> String {
    match mode {
        Mode::Add => format!("sudo ufw allow {port}/udp"),
        Mode::Remove => format!("sudo ufw delete allow {port}/udp"),
    }
}

fn manual_text(detection: Detection) -> String {
    let steps = match detection {
        Detection::Flatpak => FLATPAK_REASON,
        Detection::Snap => SNAP_REASON,
        Detection::NixOs => NIXOS_STEPS,
        Detection::UfwInactive => UFW_INACTIVE_STEPS,
        Detection::Ufw | Detection::Firewalld | Detection::None => NO_BACKEND_STEPS,
    };
    steps.to_owned()
}
