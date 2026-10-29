//! Inbound firewall capability, inspection and elevation plans.
//!
//! Three questions get three answers and never collapse into one boolean: what
//! the host allows in principle ([`capability`]), what it reports right now
//! ([`inspect`]), and what to run to change it ([`plan`], [`apply`],
//! [`remove`]). A readable rule is not proof of the LAN path; only inbound
//! traffic is.

mod command;
mod platform;

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::Serialize;

pub use command::{CommandOutput, CommandRunner, StdCommandRunner};

use platform::{HostFacts, HostOs, Mode};

pub const ECHONET_PORT: u16 = 3610;

pub const MACOS_FIREWALL_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Firewall";

pub const MACOS_LOCAL_NETWORK_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Scope {
    Application,
    ApplicationAndPort,
    Port,
}

/// What this host can do, independent of its current rule state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Capability {
    pub scope: Scope,
    pub can_add_rule: bool,
    pub can_read_rule: bool,
    pub needs_helper: bool,
    pub notes: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Request {
    /// The app path, used for the Windows read-back only.
    pub target: PathBuf,
    pub port: u16,
    pub profiles: Profiles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Profiles {
    pub private: bool,
    pub domain: bool,
}

impl Default for Profiles {
    fn default() -> Self {
        Self {
            private: true,
            domain: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Verdict {
    NotInspected,
    /// Windows only, when every profile reports the firewall disabled.
    NoFirewall,
    RulePresent {
        method: Method,
        scope: Scope,
    },
    RuleAbsent,
    NotObservable {
        backend: BackendState,
    },
    ManualSteps {
        case: ManualCase,
        steps: &'static str,
    },
    Unsupported {
        reason: &'static str,
    },
}

/// The only readable rule source in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Method {
    WindowsDefender,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BackendState {
    Running { permanent: Option<bool> },
    Inactive,
    Absent,
    NotObservable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ManualCase {
    NixOs,
    Flatpak,
    Snap,
    NoSupportedBackend,
    UfwInactive,
    UntrustedHelper,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub verdict: Verdict,
    pub checked_at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ApplyOutcome {
    Applied,
    Cancelled,
    ManualSteps,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ApplyError {
    UntrustedHelper,
    NotFound,
    NotAuthorized,
    Failed(String),
}

impl fmt::Display for ApplyError {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        match self {
            Self::UntrustedHelper => f.write_str("the helper path is not trusted"),
            Self::NotFound => f.write_str("the helper or the elevation tool was not found"),
            Self::NotAuthorized => f.write_str("authorization was refused"),
            Self::Failed(detail) => write!(f, "the helper failed: {detail}"),
        }
    }
}

impl std::error::Error for ApplyError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PlanAction {
    RunHelper {
        helper: PathBuf,
        /// Serialized as text because `OsString`'s own form is a platform byte
        /// blob, and the diagnostics view shows this argv.
        #[serde(serialize_with = "lossy_args")]
        args: Vec<OsString>,
    },
    OpenSettings {
        url: String,
    },
    Manual,
}

fn lossy_args<S>(
    args: &[OsString],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.collect_seq(args.iter().map(|arg| arg.to_string_lossy()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub action: PlanAction,
    pub scope: Scope,
    pub manual: String,
}

#[must_use]
pub const fn capability() -> Capability {
    platform::capability(HostOs::current())
}

/// Reads the current rule state. Shells out; never call it on the UI thread.
#[must_use]
pub fn inspect(request: &Request) -> Report {
    Report {
        verdict: platform::inspect(
            HostOs::current(),
            request,
            &StdCommandRunner,
            &HostFacts::system(),
        ),
        checked_at: SystemTime::now(),
    }
}

/// Builds the add plan for this host. Shells out to detect the backend.
#[must_use]
pub fn plan(request: &Request) -> Plan {
    platform::plan(
        HostOs::current(),
        request,
        &StdCommandRunner,
        &HostFacts::system(),
        Mode::Add,
    )
}

/// Builds the remove plan for this host without running it.
///
/// A caller that must show the exact action before it runs uses this and then
/// [`apply`], because [`remove`] builds and runs the plan in one step.
#[must_use]
pub fn plan_remove(request: &Request) -> Plan {
    platform::plan(
        HostOs::current(),
        request,
        &StdCommandRunner,
        &HostFacts::system(),
        Mode::Remove,
    )
}

/// Whether a helper path passes the trusted-prefix and ownership checks.
///
/// The helper repeats this check before acting. A shell that only decides
/// whether to offer an elevate button may call it without running anything.
#[must_use]
pub fn helper_trusted(path: &std::path::Path) -> bool {
    platform::helper_trusted(path)
}

/// # Errors
///
/// [`ApplyError::UntrustedHelper`] when a Linux helper fails the trusted-prefix,
/// owner or write-permission checks, [`ApplyError::NotFound`] when the helper or
/// the elevation tool is missing, [`ApplyError::NotAuthorized`] when polkit
/// refuses, and [`ApplyError::Failed`] for every other nonzero exit.
pub fn apply(plan: &Plan) -> Result<ApplyOutcome, ApplyError> {
    platform::apply(
        HostOs::current(),
        plan,
        &StdCommandRunner,
        platform::helper_trusted,
    )
}

pub fn remove(request: &Request) -> Result<ApplyOutcome, ApplyError> {
    let plan = plan_remove(request);
    apply(&plan)
}
