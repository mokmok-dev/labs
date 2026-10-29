//! Windows Defender: one fixed read-back script and the elevated helper plan.
//!
//! The script carries no caller input. It returns the fields of both rules and
//! every profile's firewall state, and the comparisons happen in Rust so the
//! exit code alone can never claim a rule matches.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use serde::Deserialize;

use crate::command::{CommandOutput, CommandRunner};
use crate::platform::{HostFacts, Mode, WINDOWS_HELPER, is_under, sibling};
use crate::{BackendState, Method, Plan, PlanAction, Request, Scope, Verdict};

/// The rule names the MSI and the helper write.
pub const PRIVATE_RULE: &str = "EchonetRadarUdp3610Private";
pub const DOMAIN_RULE: &str = "EchonetRadarUdp3610Domain";

/// Emits the two rules and the firewall state of every profile as JSON. Exit 0
/// means both names were found, 1 means at least one was not, and 2 means the
/// read itself failed.
pub const SCRIPT: &str = r"
$ErrorActionPreference = 'Stop'
try {
  $names = @('EchonetRadarUdp3610Private', 'EchonetRadarUdp3610Domain')
  $rules = @()
  foreach ($name in $names) {
    $rule = Get-NetFirewallRule -Name $name -PolicyStore PersistentStore -ErrorAction SilentlyContinue
    if ($null -eq $rule) { continue }
    $port = $rule | Get-NetFirewallPortFilter
    $app = $rule | Get-NetFirewallApplicationFilter
    $rules += [pscustomobject]@{
      name       = [string]$rule.Name
      enabled    = ([string]$rule.Enabled -eq 'True')
      direction  = [string]$rule.Direction
      action     = [string]$rule.Action
      protocol   = [string]$port.Protocol
      local_port = (@($port.LocalPort) -join ',')
      program    = [string]$app.Program
      profiles   = @([string]$rule.Profile -split ',')
    }
  }
  $profiles = @(Get-NetFirewallProfile | ForEach-Object {
    [pscustomobject]@{ name = [string]$_.Name; firewall_enabled = [bool]$_.Enabled }
  })
  [pscustomobject]@{ rules = @($rules); profiles = @($profiles) } | ConvertTo-Json -Depth 4 -Compress
  if ($rules.Count -eq 2) { exit 0 } else { exit 1 }
} catch {
  Write-Error $_
  exit 2
}
";

/// The rule state the app can read.
pub fn inspect(
    runner: &dyn CommandRunner,
    request: &Request,
) -> Verdict {
    runner
        .run(OsStr::new("powershell"), &script_args())
        .map_or_else(|_| not_observable(), |output| verdict(&output, request))
}

pub fn verdict(
    output: &CommandOutput,
    request: &Request,
) -> Verdict {
    match output.code {
        Some(0 | 1) => from_json(&output.stdout, request),
        _ => not_observable(),
    }
}

const fn not_observable() -> Verdict {
    Verdict::NotObservable {
        backend: BackendState::NotObservable,
    }
}

fn from_json(
    json: &str,
    request: &Request,
) -> Verdict {
    let Ok(readback) = serde_json::from_str::<Readback>(json) else {
        return not_observable();
    };
    let rules = readback.rules.into_vec();
    let profiles = readback.profiles.into_vec();
    let present = required_rules(request).iter().all(|(name, profile)| {
        rules
            .iter()
            .any(|rule| rule.matches(name, profile, request))
    });
    if present {
        return Verdict::RulePresent {
            method: Method::WindowsDefender,
            scope: Scope::ApplicationAndPort,
        };
    }
    if !profiles.is_empty() && profiles.iter().all(|profile| !profile.firewall_enabled) {
        return Verdict::NoFirewall;
    }
    Verdict::RuleAbsent
}

fn required_rules(request: &Request) -> Vec<(&'static str, &'static str)> {
    match (request.profiles.private, request.profiles.domain) {
        (true, false) => vec![(PRIVATE_RULE, "Private")],
        (false, true) => vec![(DOMAIN_RULE, "Domain")],
        _ => vec![(PRIVATE_RULE, "Private"), (DOMAIN_RULE, "Domain")],
    }
}

pub fn plan(
    request: &Request,
    facts: &HostFacts,
    mode: Mode,
) -> Plan {
    let in_program_files = facts
        .program_files
        .as_deref()
        .is_some_and(|root| is_under(&request.target, root));
    let action = if in_program_files {
        PlanAction::RunHelper {
            helper: sibling(&request.target, WINDOWS_HELPER),
            args: vec![
                OsString::from(mode.flag()),
                OsString::from("--port"),
                OsString::from(request.port.to_string()),
            ],
        }
    } else {
        PlanAction::Manual
    };
    Plan {
        action,
        scope: Scope::ApplicationAndPort,
        manual: manual(request, mode),
    }
}

fn manual(
    request: &Request,
    mode: Mode,
) -> String {
    match mode {
        Mode::Add => {
            let program = request.target.display();
            let port = request.port;
            let rule = |name: &str, profile: &str| {
                format!(
                    "New-NetFirewallRule -DisplayName {name} -Direction Inbound -Action Allow \
                     -Protocol UDP -LocalPort {port} -Program \"{program}\" -Profile {profile}"
                )
            };
            format!(
                "{}\n{}",
                rule(PRIVATE_RULE, "Private"),
                rule(DOMAIN_RULE, "Domain")
            )
        },
        Mode::Remove => format!("Remove-NetFirewallRule -Name {PRIVATE_RULE},{DOMAIN_RULE}"),
    }
}

fn script_args() -> Vec<OsString> {
    ["-NoProfile", "-NonInteractive", "-Command", SCRIPT]
        .into_iter()
        .map(OsString::from)
        .collect()
}

fn program_matches(
    program: &str,
    target: &Path,
) -> bool {
    let program = program.trim().trim_end_matches(['\\', '/']);
    let target = target.to_string_lossy();
    program.eq_ignore_ascii_case(target.trim_end_matches(['\\', '/']))
}

#[derive(Deserialize)]
struct Readback {
    rules: List<Rule>,
    profiles: List<ProfileState>,
}

/// PowerShell renders a one-element array as a bare object, so both shapes
/// have to parse.
#[derive(Deserialize)]
#[serde(untagged)]
enum List<T> {
    One(Box<T>),
    Many(Vec<T>),
}

impl<T> List<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            Self::One(one) => vec![*one],
            Self::Many(items) => items,
        }
    }

    fn as_slice(&self) -> &[T] {
        match self {
            Self::One(one) => std::slice::from_ref(&**one),
            Self::Many(items) => items,
        }
    }
}

#[derive(Deserialize)]
struct Rule {
    name: String,
    enabled: bool,
    direction: String,
    action: String,
    protocol: String,
    local_port: String,
    program: String,
    profiles: List<String>,
}

impl Rule {
    fn matches(
        &self,
        name: &str,
        profile: &str,
        request: &Request,
    ) -> bool {
        self.name.eq_ignore_ascii_case(name)
            && self.enabled
            && self.direction.eq_ignore_ascii_case("Inbound")
            && self.action.eq_ignore_ascii_case("Allow")
            && self.protocol.eq_ignore_ascii_case("UDP")
            && self.local_port.trim() == request.port.to_string()
            && program_matches(&self.program, &request.target)
            && matches!(self.profiles.as_slice(), [only] if only.eq_ignore_ascii_case(profile))
    }
}

#[derive(Deserialize)]
struct ProfileState {
    firewall_enabled: bool,
}
