//! One worker owns every permission call that can block.
//!
//! UAC and polkit hold a prompt open for as long as the user takes to answer,
//! and inspection shells out, so the egui thread only sends commands and folds
//! the results back each frame.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use echonet_radar_permissions::{
    ApplyError, ApplyOutcome, ECHONET_PORT, MACOS_FIREWALL_SETTINGS_URL,
    MACOS_LOCAL_NETWORK_SETTINGS_URL, Plan, PlanAction, Profiles, Report, Request, Scope,
};

const RECHECK_COUNT: u32 = 5;
const RECHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    MacOs,
    Windows,
    Linux,
}

impl Host {
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

#[must_use]
pub fn request() -> Request {
    Request {
        target: current_exe(),
        port: ECHONET_PORT,
        profiles: Profiles::default(),
    }
}

fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from(EXE_NAME))
}

#[cfg(windows)]
const EXE_NAME: &str = "echonet-radar.exe";
#[cfg(not(windows))]
const EXE_NAME: &str = "echonet-radar";

/// The host calls the worker makes. Tests state the answers instead.
trait System: Send + Sync {
    fn inspect(
        &self,
        request: &Request,
    ) -> Report;
    fn plan(
        &self,
        request: &Request,
    ) -> Plan;
    fn plan_remove(
        &self,
        request: &Request,
    ) -> Plan {
        self.plan(request)
    }
    fn helper_trusted(
        &self,
        plan: &Plan,
    ) -> bool;
    fn apply(
        &self,
        plan: &Plan,
    ) -> Result<ApplyOutcome, ApplyError>;
    fn remove(
        &self,
        request: &Request,
    ) -> Result<ApplyOutcome, ApplyError>;
}

struct HostSystem;

impl System for HostSystem {
    fn inspect(
        &self,
        request: &Request,
    ) -> Report {
        echonet_radar_permissions::inspect(request)
    }

    fn plan(
        &self,
        request: &Request,
    ) -> Plan {
        echonet_radar_permissions::plan(request)
    }

    fn plan_remove(
        &self,
        request: &Request,
    ) -> Plan {
        echonet_radar_permissions::plan_remove(request)
    }

    fn helper_trusted(
        &self,
        plan: &Plan,
    ) -> bool {
        match &plan.action {
            PlanAction::RunHelper { helper, .. } => {
                echonet_radar_permissions::helper_trusted(helper)
            },
            PlanAction::OpenSettings { .. } | PlanAction::Manual => false,
        }
    }

    fn apply(
        &self,
        plan: &Plan,
    ) -> Result<ApplyOutcome, ApplyError> {
        echonet_radar_permissions::apply(plan)
    }

    fn remove(
        &self,
        request: &Request,
    ) -> Result<ApplyOutcome, ApplyError> {
        echonet_radar_permissions::remove(request)
    }
}

enum Command {
    Check,
    Add,
    PlanRemove,
    Remove,
    OpenSettings(&'static str),
}

enum Update {
    Report(Report),
    Plan(Plan, bool),
    RemovePlan(Plan, bool),
    Outcome(Result<ApplyOutcome, ApplyError>),
}

pub struct PermissionController {
    commands: Option<Sender<Command>>,
    updates: Option<Receiver<Update>>,
    report: Option<Report>,
    plan: Option<Plan>,
    remove_plan: Option<Plan>,
    outcome: Option<ApplyOutcome>,
    error: Option<ApplyError>,
    trusted: bool,
    remove_trusted: bool,
}

impl PermissionController {
    #[must_use]
    pub fn start() -> Self {
        Self::with_system(Arc::new(HostSystem))
    }

    fn with_system(system: Arc<dyn System>) -> Self {
        let request = request();
        let (commands, command_rx) = mpsc::channel();
        let (update_tx, updates) = mpsc::channel();
        let worker = thread::Builder::new()
            .name(String::from("permission"))
            .spawn(move || work(&*system, &request, &command_rx, &update_tx));
        match worker {
            Ok(_detached) => {
                let controller = Self {
                    commands: Some(commands),
                    updates: Some(updates),
                    ..Self::empty()
                };
                controller.check_again();
                controller
            },
            Err(error) => {
                tracing::warn!(%error, "the permission worker did not start");
                Self::empty()
            },
        }
    }

    const fn empty() -> Self {
        Self {
            commands: None,
            updates: None,
            report: None,
            plan: None,
            remove_plan: None,
            outcome: None,
            error: None,
            trusted: false,
            remove_trusted: false,
        }
    }

    #[cfg(test)]
    pub fn stub(report: Report) -> Self {
        Self {
            report: Some(report),
            ..Self::empty()
        }
    }

    pub fn poll(&mut self) {
        let Some(updates) = &self.updates else {
            return;
        };
        while let Ok(update) = updates.try_recv() {
            match update {
                Update::Report(report) => self.report = Some(report),
                Update::Plan(plan, trusted) => {
                    self.plan = Some(plan);
                    self.trusted = trusted;
                },
                Update::RemovePlan(plan, trusted) => {
                    self.remove_plan = Some(plan);
                    self.remove_trusted = trusted;
                },
                Update::Outcome(Ok(outcome)) => {
                    self.outcome = Some(outcome);
                    self.error = None;
                },
                Update::Outcome(Err(error)) => {
                    self.error = Some(error);
                    self.outcome = None;
                },
            }
        }
    }

    #[must_use]
    pub fn report(&self) -> Option<Report> {
        self.report.clone()
    }

    #[must_use]
    pub fn last_plan(&self) -> Option<Plan> {
        self.plan.clone()
    }

    #[must_use]
    pub const fn last_outcome(&self) -> Option<ApplyOutcome> {
        self.outcome
    }

    #[must_use]
    pub fn last_error(&self) -> Option<ApplyError> {
        self.error.clone()
    }

    #[must_use]
    pub const fn helper_trusted(&self) -> bool {
        self.trusted
    }

    pub fn check_again(&self) {
        self.send(Command::Check);
    }

    pub fn add_rule(&self) {
        self.send(Command::Add);
    }

    pub fn remove_rule(&self) {
        self.send(Command::Remove);
    }

    /// Ask the worker for the remove plan, so the confirmation can name the
    /// exact action without spawning a process on the frame thread.
    pub fn request_remove_plan(&self) {
        self.send(Command::PlanRemove);
    }

    #[must_use]
    pub fn last_remove_plan(&self) -> Option<Plan> {
        self.remove_plan.clone()
    }

    #[must_use]
    pub const fn remove_trusted(&self) -> bool {
        self.remove_trusted
    }

    pub fn open_firewall_settings(&self) {
        self.send(Command::OpenSettings(MACOS_FIREWALL_SETTINGS_URL));
    }

    pub fn open_local_network_settings(&self) {
        self.send(Command::OpenSettings(MACOS_LOCAL_NETWORK_SETTINGS_URL));
    }

    fn send(
        &self,
        command: Command,
    ) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }
}

fn work(
    system: &dyn System,
    request: &Request,
    commands: &Receiver<Command>,
    updates: &Sender<Update>,
) {
    let mut rechecks: u32 = 0;
    let mut next: Option<Instant> = None;
    loop {
        let command = if let Some(deadline) = next {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match commands.recv_timeout(remaining) {
                Ok(command) => command,
                Err(RecvTimeoutError::Timeout) => {
                    inspect_and_plan(system, request, updates);
                    rechecks = rechecks.saturating_sub(1);
                    next = (rechecks > 0).then(|| Instant::now() + RECHECK_INTERVAL);
                    continue;
                },
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            let Ok(command) = commands.recv() else {
                return;
            };
            command
        };
        match command {
            Command::Check => inspect_and_plan(system, request, updates),
            Command::Add => {
                let plan = system.plan(request);
                let _ = updates.send(Update::Plan(plan.clone(), system.helper_trusted(&plan)));
                let result = system.apply(&plan);
                if matches!(result, Ok(ApplyOutcome::Applied)) {
                    rechecks = RECHECK_COUNT;
                    next = Some(Instant::now() + RECHECK_INTERVAL);
                }
                let _ = updates.send(Update::Outcome(result));
            },
            Command::PlanRemove => {
                let plan = system.plan_remove(request);
                let _ = updates.send(Update::RemovePlan(
                    plan.clone(),
                    system.helper_trusted(&plan),
                ));
            },
            Command::Remove => {
                let _ = updates.send(Update::Outcome(system.remove(request)));
            },
            Command::OpenSettings(url) => {
                let plan = Plan {
                    action: PlanAction::OpenSettings {
                        url: url.to_owned(),
                    },
                    scope: Scope::Application,
                    manual: String::new(),
                };
                // Opening a pane changes no rule, so only a failure is worth a line.
                if let Err(error) = system.apply(&plan) {
                    let _ = updates.send(Update::Outcome(Err(error)));
                }
            },
        }
    }
}

fn inspect_and_plan(
    system: &dyn System,
    request: &Request,
    updates: &Sender<Update>,
) {
    let _ = updates.send(Update::Report(system.inspect(request)));
    let plan = system.plan(request);
    let _ = updates.send(Update::Plan(plan.clone(), system.helper_trusted(&plan)));
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::SystemTime;

    use echonet_radar_permissions::{Method, Verdict};

    use super::*;

    struct FakeSystem {
        verdict: Verdict,
        applied: Mutex<Vec<Plan>>,
        result: Result<ApplyOutcome, ApplyError>,
    }

    impl FakeSystem {
        fn new(result: Result<ApplyOutcome, ApplyError>) -> Self {
            Self {
                verdict: Verdict::NotInspected,
                applied: Mutex::new(Vec::new()),
                result,
            }
        }

        fn applied(&self) -> Vec<Plan> {
            self.applied
                .lock()
                .map(|applied| applied.clone())
                .unwrap_or_default()
        }
    }

    impl System for FakeSystem {
        fn inspect(
            &self,
            _request: &Request,
        ) -> Report {
            Report {
                verdict: self.verdict,
                checked_at: SystemTime::UNIX_EPOCH,
            }
        }

        fn plan(
            &self,
            _request: &Request,
        ) -> Plan {
            Plan {
                action: PlanAction::RunHelper {
                    helper: PathBuf::from("/usr/libexec/echonet-radar-firewall-helper"),
                    args: vec![
                        std::ffi::OsString::from("--backend"),
                        std::ffi::OsString::from("ufw"),
                        std::ffi::OsString::from("--add"),
                        std::ffi::OsString::from("--port"),
                        std::ffi::OsString::from("3610"),
                    ],
                },
                scope: Scope::Port,
                manual: String::from("sudo ufw allow 3610/udp"),
            }
        }

        fn helper_trusted(
            &self,
            _plan: &Plan,
        ) -> bool {
            true
        }

        fn apply(
            &self,
            plan: &Plan,
        ) -> Result<ApplyOutcome, ApplyError> {
            if let Ok(mut applied) = self.applied.lock() {
                applied.push(plan.clone());
            }
            self.result.clone()
        }

        fn remove(
            &self,
            _request: &Request,
        ) -> Result<ApplyOutcome, ApplyError> {
            self.result.clone()
        }
    }

    fn poll_until(
        controller: &mut PermissionController,
        mut ready: impl FnMut(&PermissionController) -> bool,
    ) {
        for _ in 0..200 {
            controller.poll();
            if ready(controller) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_request_names_the_echonet_port_and_this_executable() {
        let request = request();
        assert_eq!(request.port, ECHONET_PORT);
        assert_eq!(request.profiles, Profiles::default());
        assert!(!request.target.as_os_str().is_empty());
    }

    #[test]
    fn a_worker_reports_an_inspection_started_from_the_ui_thread() {
        let mut system = FakeSystem::new(Ok(ApplyOutcome::Applied));
        system.verdict = Verdict::RulePresent {
            method: Method::WindowsDefender,
            scope: Scope::ApplicationAndPort,
        };
        let mut controller = PermissionController::with_system(Arc::new(system));
        controller.check_again();
        poll_until(&mut controller, |controller| {
            controller.report().is_some() && controller.last_plan().is_some()
        });
        assert_eq!(
            controller.report().map(|report| report.verdict),
            Some(Verdict::RulePresent {
                method: Method::WindowsDefender,
                scope: Scope::ApplicationAndPort,
            })
        );
        assert!(controller.helper_trusted());
    }

    #[test]
    fn a_cancelled_prompt_is_an_outcome_not_an_error() {
        let system = FakeSystem::new(Ok(ApplyOutcome::Cancelled));
        let mut controller = PermissionController::with_system(Arc::new(system));
        controller.add_rule();
        poll_until(&mut controller, |controller| {
            controller.last_outcome().is_some()
        });
        assert_eq!(controller.last_outcome(), Some(ApplyOutcome::Cancelled));
        assert!(controller.last_error().is_none());
    }

    #[test]
    fn a_failed_apply_keeps_its_error() {
        let system = FakeSystem::new(Err(ApplyError::Failed(String::from("pkexec exited 1"))));
        let mut controller = PermissionController::with_system(Arc::new(system));
        controller.add_rule();
        poll_until(&mut controller, |controller| {
            controller.last_error().is_some()
        });
        assert!(controller.last_outcome().is_none());
        assert_eq!(
            controller.last_error(),
            Some(ApplyError::Failed(String::from("pkexec exited 1")))
        );
    }

    #[test]
    fn the_firewall_opener_applies_an_open_settings_plan() {
        let fake = Arc::new(FakeSystem::new(Ok(ApplyOutcome::Applied)));
        let system: Arc<dyn System> = Arc::clone(&fake) as Arc<dyn System>;
        let mut controller = PermissionController::with_system(system);
        controller.open_local_network_settings();
        poll_until(&mut controller, |_controller| !fake.applied().is_empty());
        assert!(controller.last_outcome().is_none());
        assert_eq!(
            fake.applied().first().map(|plan| plan.action.clone()),
            Some(PlanAction::OpenSettings {
                url: String::from(MACOS_LOCAL_NETWORK_SETTINGS_URL),
            })
        );
    }

    #[test]
    fn a_stub_answers_without_a_worker() {
        let controller = PermissionController::stub(Report {
            verdict: Verdict::RuleAbsent,
            checked_at: SystemTime::UNIX_EPOCH,
        });
        controller.check_again();
        controller.add_rule();
        assert_eq!(
            controller.report().map(|report| report.verdict),
            Some(Verdict::RuleAbsent)
        );
        assert!(controller.last_outcome().is_none());
    }
}
