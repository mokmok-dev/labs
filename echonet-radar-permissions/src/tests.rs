use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::json;

use crate::command::{CommandOutput, CommandRunner};
use crate::platform::{self, HostFacts, HostOs, Mode};
use crate::{
    ApplyError, ApplyOutcome, BackendState, ECHONET_PORT, MACOS_FIREWALL_SETTINGS_URL, ManualCase,
    Method, Plan, PlanAction, Profiles, Report, Request, Scope, Verdict,
};

const HOST_HELPER: &str = "/usr/bin/echonet-radar-firewall-helper";

#[derive(Default)]
struct FakeRunner {
    replies: RefCell<Vec<(String, CommandOutput)>>,
    calls: RefCell<Vec<String>>,
}

impl FakeRunner {
    fn new() -> Self {
        Self::default()
    }

    fn reply(
        self,
        program: &str,
        args: &[&str],
        output: CommandOutput,
    ) -> Self {
        self.replies.borrow_mut().push((key(program, args), output));
        self
    }

    fn exit(
        self,
        program: &str,
        args: &[&str],
        code: i32,
    ) -> Self {
        self.reply(program, args, output(code, "", ""))
    }

    fn reply_key(
        self,
        rendered: &str,
        output: CommandOutput,
    ) -> Self {
        self.replies
            .borrow_mut()
            .push((rendered.to_owned(), output));
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

struct FailingRunner(i32);

impl CommandRunner for FailingRunner {
    fn run(
        &self,
        _program: &OsStr,
        _args: &[OsString],
    ) -> io::Result<CommandOutput> {
        Err(io::Error::from_raw_os_error(self.0))
    }
}

impl CommandRunner for FakeRunner {
    fn run(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> io::Result<CommandOutput> {
        let rendered = key_os(program, args);
        self.calls.borrow_mut().push(rendered.clone());
        self.replies
            .borrow()
            .iter()
            .find(|(expected, _)| *expected == rendered)
            .map_or_else(
                || Err(io::Error::from(io::ErrorKind::NotFound)),
                |(_, output)| Ok(output.clone()),
            )
    }
}

fn key(
    program: &str,
    args: &[&str],
) -> String {
    let mut key = program.to_owned();
    for arg in args {
        key.push('\0');
        key.push_str(arg);
    }
    key
}

fn key_os(
    program: &OsStr,
    args: &[OsString],
) -> String {
    let mut key = program.to_string_lossy().into_owned();
    for arg in args {
        key.push('\0');
        key.push_str(&arg.to_string_lossy());
    }
    key
}

fn output(
    code: i32,
    stdout: &str,
    stderr: &str,
) -> CommandOutput {
    CommandOutput {
        code: Some(code),
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    }
}

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

fn request(target: &str) -> Request {
    Request {
        target: PathBuf::from(target),
        port: ECHONET_PORT,
        profiles: Profiles::default(),
    }
}

fn linux_request() -> Request {
    request("/usr/bin/echonet-radar")
}

fn windows_request() -> Request {
    Request {
        target: PathBuf::from("/Program Files/ECHONET Radar/echonet-radar.exe"),
        ..linux_request()
    }
}

fn program_files() -> HostFacts {
    HostFacts {
        program_files: Some(PathBuf::from("/Program Files")),
        ..HostFacts::default()
    }
}

fn firewalld_runner() -> FakeRunner {
    FakeRunner::new().exit("firewall-cmd", &["--state"], 0)
}

fn ufw_runner() -> FakeRunner {
    FakeRunner::new()
        .exit("sh", &["-c", "command -v ufw"], 0)
        .exit("systemctl", &["is-active", "ufw"], 0)
}

fn linux_plan(
    runner: &FakeRunner,
    request: &Request,
    mode: Mode,
) -> Plan {
    platform::plan(HostOs::Linux, request, runner, &HostFacts::default(), mode)
}

fn linux_verdict(
    runner: &FakeRunner,
    facts: &HostFacts,
) -> Verdict {
    platform::inspect(HostOs::Linux, &linux_request(), runner, facts)
}

fn firewalld_apply(exit: i32) -> Result<ApplyOutcome, ApplyError> {
    let request = linux_request();
    let plan = linux_plan(&firewalld_runner(), &request, Mode::Add);
    let runner = FakeRunner::new().reply(
        "pkexec",
        &[
            HOST_HELPER,
            "--backend",
            "firewalld",
            "--add",
            "--port",
            "3610",
        ],
        output(exit, "", "firewalld said no"),
    );
    platform::apply(HostOs::Linux, &plan, &runner, |_| true)
}

#[test]
fn capability_matches_each_host() {
    let linux = platform::capability(HostOs::Linux);
    assert_eq!(linux.scope, Scope::Port);
    assert!(linux.can_add_rule);
    assert!(!linux.can_read_rule);
    assert!(linux.needs_helper);

    let windows = platform::capability(HostOs::Windows);
    assert_eq!(windows.scope, Scope::ApplicationAndPort);
    assert!(windows.can_add_rule);
    assert!(windows.can_read_rule);

    let macos = platform::capability(HostOs::MacOs);
    assert_eq!(macos.scope, Scope::Application);
    assert!(!macos.can_add_rule);
    assert!(!macos.can_read_rule);
    assert!(!macos.needs_helper);
}

#[test]
fn nixos_is_manual_with_the_declaration_snippet() {
    let runner = FakeRunner::new();
    let facts = HostFacts {
        nixos: true,
        ..HostFacts::default()
    };
    let verdict = linux_verdict(&runner, &facts);
    assert!(matches!(
        verdict,
        Verdict::ManualSteps {
            case: ManualCase::NixOs,
            steps,
        } if steps.contains("allowedUDPPorts")
    ));
    assert_eq!(
        runner.calls(),
        Vec::<String>::new(),
        "the sandbox probe needs no command"
    );
}

#[test]
fn flatpak_is_unsupported() {
    let runner = FakeRunner::new();
    let facts = HostFacts {
        flatpak: true,
        ..HostFacts::default()
    };
    assert!(matches!(
        linux_verdict(&runner, &facts),
        Verdict::Unsupported { reason } if reason.contains("Flatpak")
    ));
    assert_eq!(runner.calls(), Vec::<String>::new());
}

#[test]
fn snap_is_unsupported() {
    let runner = FakeRunner::new();
    let facts = HostFacts {
        snap: true,
        ..HostFacts::default()
    };
    assert!(matches!(
        linux_verdict(&runner, &facts),
        Verdict::Unsupported { reason } if reason.contains("Snap")
    ));
    assert_eq!(runner.calls(), Vec::<String>::new());
}

#[test]
fn a_sandbox_wins_over_a_running_firewalld() {
    let facts = HostFacts {
        snap: true,
        ..HostFacts::default()
    };
    assert!(matches!(
        linux_verdict(&firewalld_runner(), &facts),
        Verdict::Unsupported { .. }
    ));
}

#[test]
fn running_firewalld_is_not_observable_and_never_queries_the_rule() {
    let runner = firewalld_runner();
    assert_eq!(
        linux_verdict(&runner, &HostFacts::default()),
        Verdict::NotObservable {
            backend: BackendState::Running { permanent: None }
        }
    );
    assert_eq!(
        runner.calls(),
        vec![key("firewall-cmd", &["--state"])],
        "only --state is unprivileged"
    );
}

#[test]
fn running_ufw_is_not_observable_and_never_queries_the_rule() {
    let runner = ufw_runner();
    assert_eq!(
        linux_verdict(&runner, &HostFacts::default()),
        Verdict::NotObservable {
            backend: BackendState::Running { permanent: None }
        }
    );
    assert_eq!(
        runner.calls(),
        vec![
            key("firewall-cmd", &["--state"]),
            key("sh", &["-c", "command -v ufw"]),
            key("systemctl", &["is-active", "ufw"]),
        ],
        "ufw status needs root, so it is never called"
    );
}

#[test]
fn inactive_ufw_is_manual() {
    let runner = FakeRunner::new()
        .exit("sh", &["-c", "command -v ufw"], 0)
        .exit("systemctl", &["is-active", "ufw"], 3);
    assert!(matches!(
        linux_verdict(&runner, &HostFacts::default()),
        Verdict::ManualSteps {
            case: ManualCase::UfwInactive,
            ..
        }
    ));
}

#[test]
fn no_backend_is_manual_and_never_no_firewall() {
    let runner = FakeRunner::new().exit("firewall-cmd", &["--state"], 252);
    assert!(matches!(
        linux_verdict(&runner, &HostFacts::default()),
        Verdict::ManualSteps {
            case: ManualCase::NoSupportedBackend,
            ..
        }
    ));
}

#[test]
fn firewalld_add_plan_runs_the_sibling_helper() {
    let plan = linux_plan(&firewalld_runner(), &linux_request(), Mode::Add);
    assert_eq!(
        plan.action,
        PlanAction::RunHelper {
            helper: PathBuf::from(HOST_HELPER),
            args: args(&["--backend", "firewalld", "--add", "--port", "3610"]),
        }
    );
    assert_eq!(plan.scope, Scope::Port);
    assert!(plan.manual.contains("firewall-cmd"));
}

#[test]
fn firewalld_remove_plan_differs_only_in_the_action() {
    let add = linux_plan(&firewalld_runner(), &linux_request(), Mode::Add);
    let remove = linux_plan(&firewalld_runner(), &linux_request(), Mode::Remove);
    assert_eq!(
        remove.action,
        PlanAction::RunHelper {
            helper: PathBuf::from(HOST_HELPER),
            args: args(&["--backend", "firewalld", "--remove", "--port", "3610"]),
        }
    );
    assert_eq!(remove.scope, add.scope);
    assert!(remove.manual.contains("--remove-port"));
}

#[test]
fn ufw_plan_names_the_ufw_backend() {
    let plan = linux_plan(&ufw_runner(), &linux_request(), Mode::Add);
    assert_eq!(
        plan.action,
        PlanAction::RunHelper {
            helper: PathBuf::from(HOST_HELPER),
            args: args(&["--backend", "ufw", "--add", "--port", "3610"]),
        }
    );
    assert!(plan.manual.contains("ufw allow"));
}

#[test]
fn every_unusable_backend_plans_manual() {
    let inactive_ufw = FakeRunner::new()
        .exit("sh", &["-c", "command -v ufw"], 0)
        .exit("systemctl", &["is-active", "ufw"], 3);
    let cases = [
        (
            HostFacts {
                nixos: true,
                ..HostFacts::default()
            },
            FakeRunner::new(),
        ),
        (
            HostFacts {
                flatpak: true,
                ..HostFacts::default()
            },
            FakeRunner::new(),
        ),
        (
            HostFacts {
                snap: true,
                ..HostFacts::default()
            },
            FakeRunner::new(),
        ),
        (HostFacts::default(), FakeRunner::new()),
        (HostFacts::default(), inactive_ufw),
    ];
    for (facts, runner) in cases {
        let plan = platform::plan(HostOs::Linux, &linux_request(), &runner, &facts, Mode::Add);
        assert_eq!(plan.action, PlanAction::Manual);
        assert_eq!(plan.scope, Scope::Port);
        assert_ne!(plan.manual, "");
    }
}

#[test]
fn helper_path_with_a_space_and_a_quote_stays_one_argv_element() {
    let request = request("/opt/it's ECHONET Radar/echonet-radar");
    let plan = linux_plan(&firewalld_runner(), &request, Mode::Add);
    assert_eq!(
        plan.action,
        PlanAction::RunHelper {
            helper: PathBuf::from("/opt/it's ECHONET Radar/echonet-radar-firewall-helper"),
            args: args(&["--backend", "firewalld", "--add", "--port", "3610"]),
        }
    );
}

#[test]
fn the_helper_is_the_targets_sibling() {
    assert_eq!(
        platform::sibling(Path::new("/usr/bin/echonet-radar"), "helper"),
        PathBuf::from("/usr/bin/helper")
    );
    assert_eq!(
        platform::sibling(Path::new("echonet-radar"), "helper"),
        PathBuf::from("helper")
    );
}

#[test]
fn program_files_membership_is_component_wise() {
    let root = Path::new("/Program Files");
    assert!(platform::is_under(
        Path::new("/program files/ECHONET Radar/echonet-radar.exe"),
        root
    ));
    assert!(!platform::is_under(
        Path::new("/Program Filesevil/echonet-radar.exe"),
        root
    ));
    assert!(
        platform::is_under(Path::new("/Program Files"), root),
        "the root is under itself"
    );
}

#[test]
fn an_empty_program_files_trusts_nothing() {
    assert!(!platform::is_under(
        Path::new("/Program Files/ECHONET Radar/echonet-radar.exe"),
        Path::new("")
    ));
    assert!(!platform::is_under(
        Path::new("/Program Files/ECHONET Radar/echonet-radar.exe"),
        Path::new("/")
    ));
    assert!(!platform::is_under(
        Path::new("/Program Files/ECHONET Radar/echonet-radar.exe"),
        Path::new("Program Files")
    ));
}

#[test]
fn windows_plan_under_program_files_runs_the_helper() {
    let add = platform::plan(
        HostOs::Windows,
        &windows_request(),
        &FakeRunner::new(),
        &program_files(),
        Mode::Add,
    );
    assert_eq!(
        add.action,
        PlanAction::RunHelper {
            helper: PathBuf::from("/Program Files/ECHONET Radar/echonet-radar-firewall-helper.exe"),
            args: args(&["--add", "--port", "3610"]),
        }
    );
    assert_eq!(add.scope, Scope::ApplicationAndPort);

    let remove = platform::plan(
        HostOs::Windows,
        &windows_request(),
        &FakeRunner::new(),
        &program_files(),
        Mode::Remove,
    );
    assert_eq!(
        remove.action,
        PlanAction::RunHelper {
            helper: PathBuf::from("/Program Files/ECHONET Radar/echonet-radar-firewall-helper.exe"),
            args: args(&["--remove", "--port", "3610"]),
        }
    );
}

#[test]
fn windows_plan_outside_program_files_is_manual() {
    let plan = platform::plan(
        HostOs::Windows,
        &request("/home/user/echonet-radar.exe"),
        &FakeRunner::new(),
        &program_files(),
        Mode::Add,
    );
    assert_eq!(plan.action, PlanAction::Manual);
    assert!(plan.manual.contains("New-NetFirewallRule"));
    assert!(plan.manual.contains("echonet-radar.exe"));

    let remove = platform::plan(
        HostOs::Windows,
        &request("/home/user/echonet-radar.exe"),
        &FakeRunner::new(),
        &program_files(),
        Mode::Remove,
    );
    assert_eq!(remove.action, PlanAction::Manual);
    assert!(remove.manual.contains("Remove-NetFirewallRule"));
}

#[test]
fn windows_plan_without_program_files_is_manual() {
    let plan = platform::plan(
        HostOs::Windows,
        &windows_request(),
        &FakeRunner::new(),
        &HostFacts::default(),
        Mode::Add,
    );
    assert_eq!(plan.action, PlanAction::Manual);
}

#[test]
fn macos_add_plan_opens_the_firewall_pane() {
    let runner = FakeRunner::new();
    let plan = platform::plan(
        HostOs::MacOs,
        &linux_request(),
        &runner,
        &HostFacts::default(),
        Mode::Add,
    );
    assert_eq!(
        plan.action,
        PlanAction::OpenSettings {
            url: MACOS_FIREWALL_SETTINGS_URL.to_owned()
        }
    );
    assert_eq!(plan.scope, Scope::Application);
    assert_eq!(runner.calls(), Vec::<String>::new());
}

#[test]
fn macos_remove_is_manual_steps() {
    let plan = platform::plan(
        HostOs::MacOs,
        &linux_request(),
        &FakeRunner::new(),
        &HostFacts::default(),
        Mode::Remove,
    );
    assert_eq!(plan.action, PlanAction::Manual);
    assert_eq!(
        platform::apply(HostOs::MacOs, &plan, &FakeRunner::new(), |_| true),
        Ok(ApplyOutcome::ManualSteps)
    );
}

#[test]
fn macos_inspect_is_not_observable_and_runs_nothing() {
    let runner = FakeRunner::new();
    assert_eq!(
        platform::inspect(
            HostOs::MacOs,
            &linux_request(),
            &runner,
            &HostFacts::default()
        ),
        Verdict::NotObservable {
            backend: BackendState::NotObservable
        }
    );
    assert_eq!(
        runner.calls(),
        Vec::<String>::new(),
        "socketfilterfw is never queried"
    );
}

fn rule(
    name: &str,
    profile: &str,
    program: &str,
) -> serde_json::Value {
    json!({
        "name": name,
        "enabled": true,
        "direction": "Inbound",
        "action": "Allow",
        "protocol": "UDP",
        "local_port": "3610",
        "program": program,
        "profiles": [profile],
    })
}

fn readback(
    rules: &[serde_json::Value],
    enabled: &[bool],
) -> String {
    let profiles: Vec<_> = enabled
        .iter()
        .map(|enabled| json!({ "name": "Private", "firewall_enabled": enabled }))
        .collect();
    json!({ "rules": rules, "profiles": profiles }).to_string()
}

fn windows_program() -> &'static str {
    "/Program Files/ECHONET Radar/echonet-radar.exe"
}

fn matching_rules() -> Vec<serde_json::Value> {
    vec![
        rule("EchonetRadarUdp3610Private", "Private", windows_program()),
        rule("EchonetRadarUdp3610Domain", "Domain", windows_program()),
    ]
}

fn windows_verdict(
    code: i32,
    stdout: &str,
) -> Verdict {
    platform::windows::verdict(&output(code, stdout, ""), &windows_request())
}

fn present() -> Verdict {
    Verdict::RulePresent {
        method: Method::WindowsDefender,
        scope: Scope::ApplicationAndPort,
    }
}

fn not_observable() -> Verdict {
    Verdict::NotObservable {
        backend: BackendState::NotObservable,
    }
}

#[test]
fn windows_exit_zero_with_both_rules_is_present() {
    assert_eq!(
        windows_verdict(0, &readback(&matching_rules(), &[true])),
        present()
    );
}

#[test]
fn windows_exit_one_without_rules_is_absent() {
    assert_eq!(
        windows_verdict(1, &readback(&[], &[true])),
        Verdict::RuleAbsent
    );
}

#[test]
fn windows_exit_two_is_not_observable() {
    assert_eq!(windows_verdict(2, ""), not_observable());
}

#[test]
fn windows_wrong_port_is_absent() {
    let mut rules = matching_rules();
    rules[0]["local_port"] = json!("3611");
    assert_eq!(
        windows_verdict(0, &readback(&rules, &[true])),
        Verdict::RuleAbsent
    );
}

#[test]
fn windows_wrong_program_is_absent() {
    let mut rules = matching_rules();
    rules[1]["program"] = json!("/Users/someone/echonet-radar.exe");
    assert_eq!(
        windows_verdict(0, &readback(&rules, &[true])),
        Verdict::RuleAbsent
    );
}

#[test]
fn windows_wrong_profile_is_absent() {
    let mut rules = matching_rules();
    rules[1]["profiles"] = json!(["Public"]);
    assert_eq!(
        windows_verdict(0, &readback(&rules, &[true])),
        Verdict::RuleAbsent
    );
}

#[test]
fn windows_wrong_action_is_absent() {
    let mut rules = matching_rules();
    rules[0]["action"] = json!("Block");
    assert_eq!(
        windows_verdict(0, &readback(&rules, &[true])),
        Verdict::RuleAbsent
    );
}

#[test]
fn windows_disabled_rule_is_absent() {
    let mut rules = matching_rules();
    rules[0]["enabled"] = json!(false);
    assert_eq!(
        windows_verdict(0, &readback(&rules, &[true])),
        Verdict::RuleAbsent
    );
}

#[test]
fn windows_one_rule_is_absent() {
    let rules = vec![rule(
        "EchonetRadarUdp3610Private",
        "Private",
        windows_program(),
    )];
    assert_eq!(
        windows_verdict(1, &readback(&rules, &[true])),
        Verdict::RuleAbsent
    );
}

#[test]
fn windows_single_object_shape_parses() {
    let stdout = json!({
        "rules": rule("EchonetRadarUdp3610Private", "Private", windows_program()),
        "profiles": { "name": "Private", "firewall_enabled": true },
    })
    .to_string();
    assert_eq!(windows_verdict(1, &stdout), Verdict::RuleAbsent);
}

#[test]
fn windows_broken_json_is_not_observable() {
    assert_eq!(windows_verdict(0, "not json"), not_observable());
}

#[test]
fn windows_every_profile_disabled_is_no_firewall() {
    assert_eq!(
        windows_verdict(1, &readback(&[], &[false, false, false])),
        Verdict::NoFirewall
    );
}

#[test]
fn windows_one_profile_enabled_is_not_no_firewall() {
    assert_eq!(
        windows_verdict(1, &readback(&[], &[false, true, false])),
        Verdict::RuleAbsent
    );
}

#[test]
fn windows_without_a_profile_report_is_not_no_firewall() {
    assert_eq!(windows_verdict(1, &readback(&[], &[])), Verdict::RuleAbsent);
}

#[test]
fn windows_missing_powershell_is_not_observable() {
    assert_eq!(
        platform::windows::inspect(&FakeRunner::new(), &windows_request()),
        not_observable()
    );
}

#[test]
fn untrusted_helper_is_rejected_before_any_command() {
    let plan = Plan {
        action: PlanAction::RunHelper {
            helper: PathBuf::from("/tmp/echonet-radar-firewall-helper"),
            args: args(&["--backend", "firewalld", "--add", "--port", "3610"]),
        },
        scope: Scope::Port,
        manual: String::new(),
    };
    let runner = FakeRunner::new();
    assert_eq!(
        platform::apply(HostOs::Linux, &plan, &runner, platform::helper_trusted),
        Err(ApplyError::UntrustedHelper)
    );
    assert_eq!(runner.calls(), Vec::<String>::new());
}

#[test]
fn trust_needs_a_trusted_prefix_and_a_regular_file() {
    assert!(!platform::helper_trusted(Path::new(
        "/tmp/echonet-radar-firewall-helper"
    )));
    assert!(!platform::helper_trusted(Path::new(
        "/usr/lib/echonet-radar-untrusted-helper"
    )));
    assert!(
        !platform::helper_trusted(Path::new("/usr")),
        "a directory is not a helper"
    );
}

#[test]
fn firewalld_success_is_applied() {
    assert_eq!(firewalld_apply(0), Ok(ApplyOutcome::Applied));
}

#[test]
fn a_stopped_firewalld_is_not_a_successful_add() {
    assert_eq!(
        firewalld_apply(12),
        Err(ApplyError::Failed("firewalld said no".to_owned()))
    );
}

#[test]
fn firewalld_other_exit_carries_stderr() {
    assert_eq!(
        firewalld_apply(1),
        Err(ApplyError::Failed("firewalld said no".to_owned()))
    );
}

#[test]
fn pkexec_codes_map_to_cancelled_and_not_authorized() {
    assert_eq!(firewalld_apply(126), Ok(ApplyOutcome::Cancelled));
    assert_eq!(firewalld_apply(127), Err(ApplyError::NotAuthorized));
}

fn windows_plan(mode: Mode) -> Plan {
    platform::plan(
        HostOs::Windows,
        &windows_request(),
        &FakeRunner::new(),
        &program_files(),
        mode,
    )
}

fn elevated_windows_call(plan: &Plan) -> String {
    let probe = FakeRunner::new();
    let _ = platform::apply(HostOs::Windows, plan, &probe, |_| true);
    let mut calls = probe.calls();
    calls.remove(0)
}

#[test]
fn windows_apply_raises_uac_through_powershell() {
    let plan = windows_plan(Mode::Add);
    let call = elevated_windows_call(&plan);
    let parts: Vec<&str> = call.split('\0').collect();

    assert_eq!(parts[0], "powershell");
    assert_eq!(parts[5], "-Command");
    let script = parts[6];
    assert!(script.contains("Start-Process"));
    assert!(script.contains("-Verb RunAs"));
    assert!(
        !script.contains("Program Files"),
        "the helper path must not be interpolated into the script"
    );
    assert_eq!(
        &parts[7..],
        &[
            "/Program Files/ECHONET Radar/echonet-radar-firewall-helper.exe",
            "--add",
            "--port",
            "3610",
        ]
    );
}

#[test]
fn windows_cancelled_uac_is_cancelled() {
    let plan = windows_plan(Mode::Add);
    let key = elevated_windows_call(&plan);
    let runner = FakeRunner::new().reply_key(&key, output(1223, "", "cancelled"));

    assert_eq!(
        platform::apply(HostOs::Windows, &plan, &runner, |_| true),
        Ok(ApplyOutcome::Cancelled)
    );
}

#[test]
fn windows_other_exit_is_failed() {
    let plan = windows_plan(Mode::Add);
    let key = elevated_windows_call(&plan);
    let runner = FakeRunner::new().reply_key(&key, output(5, "", "access denied"));

    assert_eq!(
        platform::apply(HostOs::Windows, &plan, &runner, |_| true),
        Err(ApplyError::Failed("access denied".to_owned()))
    );
}

#[test]
fn windows_spawn_errors_map_to_outcomes() {
    let plan = windows_plan(Mode::Add);

    assert_eq!(
        platform::apply(HostOs::Windows, &plan, &FailingRunner(1223), |_| true),
        Ok(ApplyOutcome::Cancelled)
    );
    assert_eq!(
        platform::apply(HostOs::Windows, &plan, &FailingRunner(740), |_| true),
        Err(ApplyError::NotAuthorized)
    );
}

#[test]
fn add_and_remove_plans_are_idempotent() {
    let request = linux_request();
    let add = linux_plan(&firewalld_runner(), &request, Mode::Add);
    assert_eq!(add, linux_plan(&firewalld_runner(), &request, Mode::Add));
    let remove = linux_plan(&firewalld_runner(), &request, Mode::Remove);
    assert_eq!(
        remove,
        linux_plan(&firewalld_runner(), &request, Mode::Remove)
    );
    assert_ne!(add, remove);
}

#[test]
fn report_and_plan_serialize() {
    let report = Report {
        verdict: Verdict::RuleAbsent,
        checked_at: SystemTime::UNIX_EPOCH,
    };
    let value = serde_json::to_value(&report).unwrap_or_default();
    assert_eq!(value["verdict"], "RuleAbsent");

    let plan = linux_plan(&firewalld_runner(), &linux_request(), Mode::Add);
    let value = serde_json::to_value(&plan).unwrap_or_default();
    assert_eq!(value["scope"], "Port");
    assert_eq!(value["action"]["RunHelper"]["args"][0], "--backend");
}
