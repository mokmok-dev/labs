pub mod linux;
pub mod macos;
pub mod windows;

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use crate::command::{CommandOutput, CommandRunner};
use crate::{ApplyError, ApplyOutcome, Capability, Plan, PlanAction, Request, Scope, Verdict};

/// The host a plan is built for. Prod code reads the real one; tests pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Linux,
    Windows,
    MacOs,
}

impl HostOs {
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

/// The filesystem and environment probes, split from the commands so tests can
/// state them instead of reading the real machine.
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub flatpak: bool,
    pub snap: bool,
    pub nixos: bool,
    pub program_files: Option<PathBuf>,
}

impl HostFacts {
    pub fn system() -> Self {
        Self {
            flatpak: Path::new("/.flatpak-info").exists(),
            snap: std::env::var_os("SNAP").is_some(),
            nixos: Path::new("/etc/NIXOS").exists(),
            program_files: std::env::var_os("ProgramFiles").map(PathBuf::from),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Add,
    Remove,
}

impl Mode {
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Add => "--add",
            Self::Remove => "--remove",
        }
    }
}

pub const LINUX_HELPER: &str = "echonet-radar-firewall-helper";
pub const WINDOWS_HELPER: &str = "echonet-radar-firewall-helper.exe";

const TRUSTED_PREFIXES: [&str; 4] = ["/nix/store", "/usr", "/opt", "/run/current-system/sw"];

pub const fn capability(os: HostOs) -> Capability {
    match os {
        HostOs::Linux => Capability {
            scope: Scope::Port,
            can_add_rule: true,
            can_read_rule: false,
            needs_helper: true,
            notes: "Adds UDP 3610 for this machine, not for the app.",
        },
        HostOs::Windows => Capability {
            scope: Scope::ApplicationAndPort,
            can_add_rule: true,
            can_read_rule: true,
            needs_helper: true,
            notes: "Adds an app rule for UDP 3610 on the private and domain profiles.",
        },
        HostOs::MacOs => Capability {
            scope: Scope::Application,
            can_add_rule: false,
            can_read_rule: false,
            needs_helper: false,
            notes: "macOS asks per app; the system dialog is the approval.",
        },
    }
}

pub fn inspect(
    os: HostOs,
    request: &Request,
    runner: &dyn CommandRunner,
    facts: &HostFacts,
) -> Verdict {
    match os {
        HostOs::Linux => linux::detect(runner, facts).verdict(),
        HostOs::Windows => windows::inspect(runner, request),
        HostOs::MacOs => macos::inspect(),
    }
}

pub fn plan(
    os: HostOs,
    request: &Request,
    runner: &dyn CommandRunner,
    facts: &HostFacts,
    mode: Mode,
) -> Plan {
    match os {
        HostOs::Linux => linux::plan(linux::detect(runner, facts), request, mode),
        HostOs::Windows => windows::plan(request, facts, mode),
        HostOs::MacOs => macos::plan(mode),
    }
}

/// Windows does not honor a `requireAdministrator` manifest through
/// `CreateProcess`; only a `runas` shell launch raises the consent dialog. The
/// script stays a literal, and the helper and its argv arrive as separate
/// arguments in `$args`, so no path is interpolated into it.
const WINDOWS_ELEVATION: &str = "$ErrorActionPreference = 'Stop'\n\
try {\n\
  $helper = $args[0]\n\
  $rest = @()\n\
  if ($args.Length -gt 1) { $rest = $args[1..($args.Length - 1)] }\n\
  $process = Start-Process -FilePath $helper -ArgumentList $rest -Verb RunAs -Wait -PassThru\n\
  exit $process.ExitCode\n\
} catch {\n\
  $code = $_.Exception.NativeErrorCode\n\
  if (-not $code) { $code = 1 }\n\
  exit $code\n\
}";

pub fn apply(
    os: HostOs,
    plan: &Plan,
    runner: &dyn CommandRunner,
    trusted: fn(&Path) -> bool,
) -> Result<ApplyOutcome, ApplyError> {
    match &plan.action {
        PlanAction::Manual => Ok(ApplyOutcome::ManualSteps),
        PlanAction::OpenSettings { url } => {
            run(runner, OsStr::new("open"), &[OsString::from(url)], os)
        },
        PlanAction::RunHelper { helper, args } => match os {
            HostOs::MacOs => Ok(ApplyOutcome::ManualSteps),
            HostOs::Linux => {
                if !trusted(helper) {
                    return Err(ApplyError::UntrustedHelper);
                }
                let mut elevated = Vec::with_capacity(args.len() + 1);
                elevated.push(helper.as_os_str().to_owned());
                elevated.extend(args.iter().cloned());
                run(runner, OsStr::new("pkexec"), &elevated, os)
            },
            HostOs::Windows => {
                let mut elevated = vec![
                    OsString::from("-NoProfile"),
                    OsString::from("-NonInteractive"),
                    OsString::from("-ExecutionPolicy"),
                    OsString::from("Bypass"),
                    OsString::from("-Command"),
                    OsString::from(WINDOWS_ELEVATION),
                    helper.as_os_str().to_owned(),
                ];
                elevated.extend(args.iter().cloned());
                run(runner, OsStr::new("powershell"), &elevated, os)
            },
        },
    }
}

fn run(
    runner: &dyn CommandRunner,
    program: &OsStr,
    args: &[OsString],
    os: HostOs,
) -> Result<ApplyOutcome, ApplyError> {
    let output = match runner.run(program, args) {
        Ok(output) => output,
        Err(error) if os == HostOs::Windows && error.raw_os_error() == Some(1223) => {
            return Ok(ApplyOutcome::Cancelled);
        },
        Err(error) if os == HostOs::Windows && error.raw_os_error() == Some(740) => {
            return Err(ApplyError::NotAuthorized);
        },
        Err(error) => return Err(spawn_error(&error)),
    };
    outcome(os, &output)
}

fn outcome(
    os: HostOs,
    output: &CommandOutput,
) -> Result<ApplyOutcome, ApplyError> {
    let code = output.code;
    match (os, code) {
        // A cancelled UAC prompt is Windows 1223. pkexec reports its own 126
        // for a dismissed dialog and 127 for a refusal, so the helper never
        // emits those two.
        (HostOs::Windows, Some(1223)) | (HostOs::Linux, Some(126)) => Ok(ApplyOutcome::Cancelled),
        (HostOs::Windows, Some(740)) | (HostOs::Linux, Some(127)) => Err(ApplyError::NotAuthorized),
        (_, Some(0)) => Ok(ApplyOutcome::Applied),
        _ => Err(ApplyError::Failed(output.stderr.clone())),
    }
}

fn spawn_error(error: &io::Error) -> ApplyError {
    if error.kind() == io::ErrorKind::NotFound {
        ApplyError::NotFound
    } else {
        ApplyError::Failed(error.to_string())
    }
}

pub fn sibling(
    target: &Path,
    name: &str,
) -> PathBuf {
    target
        .parent()
        .map_or_else(|| PathBuf::from(name), |dir| dir.join(name))
}

/// Component-wise prefix test that ignores ASCII case, because Windows paths
/// are case-insensitive and `%ProgramFiles%` need not match the executable's
/// spelling.
pub fn is_under(
    target: &Path,
    root: &Path,
) -> bool {
    // A missing or bare-root prefix would trust every path, and the Windows
    // plan elevates whatever it trusts.
    let named_root = root.is_absolute()
        && root
            .components()
            .any(|part| matches!(part, std::path::Component::Normal(_)));
    if !named_root {
        return false;
    }
    let mut target = target.components();
    root.components().all(|root| {
        target.next().is_some_and(|part| {
            let part = part.as_os_str().to_string_lossy();
            let root = root.as_os_str().to_string_lossy();
            part.eq_ignore_ascii_case(&root)
        })
    })
}

/// The Linux helper check that `pkexec` does not do for us.
pub fn helper_trusted(helper: &Path) -> bool {
    let Ok(canonical) = std::fs::canonicalize(helper) else {
        return false;
    };
    if !TRUSTED_PREFIXES
        .iter()
        .any(|prefix| canonical.starts_with(prefix))
    {
        return false;
    }
    let Ok(metadata) = std::fs::metadata(&canonical) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.uid() == 0 && metadata.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        false
    }
}
