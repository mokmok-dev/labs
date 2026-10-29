use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::args::{self, Action, Backend, Request};
use crate::exit::{Exit, Failure};
use crate::paths;
use crate::runner::{CommandRunner, Invocation, Output, SystemRunner};

pub const PORT_SPEC: &str = "3610/udp";

const FIREWALLD: &str = "firewall-cmd";
const UFW: &str = "ufw";
const SYSTEMCTL: &str = "systemctl";

/// firewalld's own status for "the daemon is not enabled", which a removal must
/// read as "nothing to remove".
const FIREWALLD_NOT_ENABLED: i32 = 12;
/// firewalld's status when polkit refused the request.
const FIREWALLD_NOT_AUTHORIZED: i32 = 253;
/// ufw's wording when the rule being deleted was never there.
const UFW_ABSENT_RULE: &str = "Could not delete non-existent rule";

/// Prefixes a root-owned, non-writable install may live under. pkexec validates
/// neither the helper path nor its arguments, so the helper checks itself.
const TRUSTED_PREFIXES: [&str; 4] = ["/nix/store", "/usr", "/opt", "/run/current-system/sw"];

pub fn run(args: &[OsString]) -> u8 {
    let request = match args::parse(args) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("{}", error.detail);
            return Exit::Usage.code();
        },
    };
    match dispatch(&SystemRunner, request) {
        Ok(()) => Exit::Ok.code(),
        Err(failure) => {
            eprintln!("error: {}", failure.detail);
            failure.exit.code()
        },
    }
}

fn dispatch(
    runner: &dyn CommandRunner,
    request: Request,
) -> Result<(), Failure> {
    let helper = std::env::current_exe().map_err(|error| {
        Failure::new(
            Exit::Failed,
            format!("could not locate the helper: {error}"),
        )
    })?;
    let target = paths::sibling_target(&helper).ok_or_else(|| untrusted(&helper, request))?;
    check_trusted(&helper, request)?;
    check_trusted(&target, request)?;
    apply(runner, request)
}

fn check_trusted(
    path: &Path,
    request: Request,
) -> Result<(), Failure> {
    if trusted(path) {
        Ok(())
    } else {
        Err(untrusted(path, request))
    }
}

fn untrusted(
    path: &Path,
    request: Request,
) -> Failure {
    Failure::new(
        Exit::Failed,
        format!(
            "{} is not a root-owned read-only path under a trusted prefix, so the helper will not elevate; run this by hand instead: {}",
            path.display(),
            manual_steps(request)
        ),
    )
}

fn trusted(path: &Path) -> bool {
    let Ok(canonical) = std::fs::canonicalize(path) else {
        return false;
    };
    trusted_prefix(&canonical)
        && std::fs::metadata(&canonical).is_ok_and(|meta| trusted_owner(meta.uid(), meta.mode()))
}

fn trusted_prefix(path: &Path) -> bool {
    TRUSTED_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

const fn trusted_owner(
    uid: u32,
    mode: u32,
) -> bool {
    uid == 0 && mode & 0o022 == 0
}

pub fn apply(
    runner: &dyn CommandRunner,
    request: Request,
) -> Result<(), Failure> {
    let steps = match request.backend {
        Backend::Firewalld => firewalld_steps(request.action),
        Backend::Ufw => ufw_steps(request.action),
        Backend::WindowsDefender => {
            return Err(Failure::new(
                Exit::Failed,
                "the Windows Defender backend has no Linux implementation",
            ));
        },
    };
    for step in steps {
        let invocation = step.invocation();
        let output = runner.run(&invocation).map_err(|error| {
            Failure::new(
                Exit::Failed,
                format!("could not run {}: {error}", invocation.program),
            )
        })?;
        let exit = step.classify(request.action, &output);
        if exit != Exit::Ok {
            return Err(step.failure(exit, request, &output));
        }
    }
    Ok(())
}

fn firewalld_steps(action: Action) -> Vec<Step> {
    let rule = match action {
        Action::Add => Step::FirewalldAdd,
        Action::Remove => Step::FirewalldRemove,
    };
    vec![rule, Step::FirewalldReload]
}

fn ufw_steps(action: Action) -> Vec<Step> {
    match action {
        Action::Add => vec![Step::RequireUfwActive, Step::UfwAllow],
        Action::Remove => vec![Step::UfwDelete],
    }
}

/// One tool call and the rule that reads its exit status. The argv lives here,
/// beside the status rule, so neither can drift from the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    RequireUfwActive,
    FirewalldAdd,
    FirewalldRemove,
    FirewalldReload,
    UfwAllow,
    UfwDelete,
}

impl Step {
    fn invocation(self) -> Invocation {
        match self {
            Self::RequireUfwActive => Invocation::new(SYSTEMCTL, &["is-active", "ufw"]),
            Self::FirewalldAdd => {
                Invocation::new(FIREWALLD, &["--permanent", "--add-port=3610/udp"])
            },
            Self::FirewalldRemove => {
                Invocation::new(FIREWALLD, &["--permanent", "--remove-port=3610/udp"])
            },
            Self::FirewalldReload => Invocation::new(FIREWALLD, &["--reload"]),
            Self::UfwAllow => Invocation::new(UFW, &["allow", "3610/udp"]),
            Self::UfwDelete => Invocation::new(UFW, &["delete", "allow", "3610/udp"]),
        }
    }

    const fn program(self) -> &'static str {
        match self {
            Self::RequireUfwActive => SYSTEMCTL,
            Self::FirewalldAdd | Self::FirewalldRemove | Self::FirewalldReload => FIREWALLD,
            Self::UfwAllow | Self::UfwDelete => UFW,
        }
    }

    fn classify(
        self,
        action: Action,
        output: &Output,
    ) -> Exit {
        match self {
            Self::RequireUfwActive | Self::UfwAllow => status_exit(output.status),
            Self::FirewalldAdd | Self::FirewalldRemove | Self::FirewalldReload => {
                firewalld_exit(action, output.status)
            },
            Self::UfwDelete => {
                if status_exit(output.status) == Exit::Ok || mentions_absent_rule(output) {
                    Exit::Ok
                } else {
                    Exit::Failed
                }
            },
        }
    }

    fn failure(
        self,
        exit: Exit,
        request: Request,
        output: &Output,
    ) -> Failure {
        let detail = match self {
            Self::RequireUfwActive => {
                "ufw is inactive, so the rule was not added; enable ufw and try again".to_owned()
            },
            _ if exit == Exit::Cancelled => format!(
                "the elevation request was not authorized; run this by hand instead: {}",
                manual_steps(request)
            ),
            _ => {
                let detail = output.detail();
                if detail.is_empty() {
                    format!("{} exited with status {}", self.program(), output.status)
                } else {
                    format!("{}: {detail}", self.program())
                }
            },
        };
        Failure::new(exit, detail)
    }
}

const fn status_exit(status: i32) -> Exit {
    if status == 0 { Exit::Ok } else { Exit::Failed }
}

const fn firewalld_exit(
    action: Action,
    status: i32,
) -> Exit {
    match status {
        0 => Exit::Ok,
        // A stopped daemon has no rule to remove. An add against it changes
        // nothing, so reporting success would claim a rule that does not exist.
        FIREWALLD_NOT_ENABLED if matches!(action, Action::Remove) => Exit::Ok,
        FIREWALLD_NOT_AUTHORIZED => Exit::Cancelled,
        _ => Exit::Failed,
    }
}

fn mentions_absent_rule(output: &Output) -> bool {
    output.stdout.contains(UFW_ABSENT_RULE) || output.stderr.contains(UFW_ABSENT_RULE)
}

fn manual_steps(request: Request) -> String {
    match (request.backend, request.action) {
        (Backend::Firewalld, Action::Add) => format!(
            "sudo {FIREWALLD} --permanent --add-port={PORT_SPEC} && sudo {FIREWALLD} --reload"
        ),
        (Backend::Firewalld, Action::Remove) => format!(
            "sudo {FIREWALLD} --permanent --remove-port={PORT_SPEC} && sudo {FIREWALLD} --reload"
        ),
        (Backend::Ufw, Action::Add) => format!("sudo {UFW} allow {PORT_SPEC}"),
        (Backend::Ufw, Action::Remove) => format!("sudo {UFW} delete allow {PORT_SPEC}"),
        (Backend::WindowsDefender, _) => {
            format!("sudo {FIREWALLD} --permanent --add-port={PORT_SPEC}")
        },
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io;

    use super::*;

    struct FakeRunner {
        scripted: RefCell<VecDeque<io::Result<Output>>>,
        seen: RefCell<Vec<Invocation>>,
    }

    impl FakeRunner {
        fn replying(outputs: Vec<io::Result<Output>>) -> Self {
            Self {
                scripted: RefCell::new(outputs.into()),
                seen: RefCell::new(Vec::new()),
            }
        }

        fn quiet(count: usize) -> Self {
            Self::replying((0..count).map(|_| Ok(output(0))).collect())
        }

        fn seen(&self) -> Vec<Invocation> {
            self.seen.borrow().clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            invocation: &Invocation,
        ) -> io::Result<Output> {
            self.seen.borrow_mut().push(invocation.clone());
            self.scripted
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(io::Error::other("unscripted invocation")))
        }
    }

    fn output(status: i32) -> Output {
        Output {
            status,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn output_with(
        status: i32,
        stderr: &str,
    ) -> Output {
        Output {
            status,
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    fn request(
        backend: Backend,
        action: Action,
    ) -> Request {
        Request { action, backend }
    }

    fn planned(request: Request) -> Vec<(String, Vec<String>)> {
        let steps = match request.backend {
            Backend::Ufw => ufw_steps(request.action),
            _ => firewalld_steps(request.action),
        };
        steps
            .into_iter()
            .map(|step| {
                let invocation = step.invocation();
                (
                    invocation.program.to_owned(),
                    invocation
                        .args
                        .iter()
                        .map(|arg| (*arg).to_owned())
                        .collect(),
                )
            })
            .collect()
    }

    fn pair(
        program: &str,
        args: &[&str],
    ) -> (String, Vec<String>) {
        (
            program.to_owned(),
            args.iter().map(|arg| (*arg).to_owned()).collect(),
        )
    }

    #[test]
    fn plans_the_firewalld_add_argv() {
        assert_eq!(
            planned(request(Backend::Firewalld, Action::Add)),
            vec![
                pair("firewall-cmd", &["--permanent", "--add-port=3610/udp"]),
                pair("firewall-cmd", &["--reload"]),
            ]
        );
    }

    #[test]
    fn plans_the_firewalld_remove_argv() {
        assert_eq!(
            planned(request(Backend::Firewalld, Action::Remove)),
            vec![
                pair("firewall-cmd", &["--permanent", "--remove-port=3610/udp"]),
                pair("firewall-cmd", &["--reload"]),
            ]
        );
    }

    #[test]
    fn plans_the_ufw_add_argv_behind_the_active_check() {
        assert_eq!(
            planned(request(Backend::Ufw, Action::Add)),
            vec![
                pair("systemctl", &["is-active", "ufw"]),
                pair("ufw", &["allow", "3610/udp"]),
            ]
        );
    }

    #[test]
    fn plans_the_ufw_remove_argv() {
        assert_eq!(
            planned(request(Backend::Ufw, Action::Remove)),
            vec![pair("ufw", &["delete", "allow", "3610/udp"])]
        );
    }

    #[test]
    fn the_planned_argv_spells_the_shared_port() {
        assert_eq!(format!("--add-port={PORT_SPEC}"), "--add-port=3610/udp");
        assert_eq!(format!("allow {PORT_SPEC}"), "allow 3610/udp");
    }

    #[test]
    fn maps_firewalld_statuses_to_exit_codes() {
        assert_eq!(firewalld_exit(Action::Add, 0).code(), 0);
        assert_eq!(
            firewalld_exit(Action::Remove, FIREWALLD_NOT_ENABLED).code(),
            0
        );
        assert_eq!(firewalld_exit(Action::Add, FIREWALLD_NOT_ENABLED).code(), 1);
        assert_eq!(
            firewalld_exit(Action::Add, FIREWALLD_NOT_AUTHORIZED).code(),
            126
        );
        assert_eq!(firewalld_exit(Action::Add, 1).code(), 1);
        assert_eq!(firewalld_exit(Action::Add, 2).code(), 1);
    }

    #[test]
    fn maps_the_usage_and_cancelled_codes() {
        assert_eq!(Exit::Usage.code(), 2);
        assert_eq!(Exit::Cancelled.code(), 126);
        assert_eq!(Exit::Failed.code(), 1);
        assert_eq!(Exit::Ok.code(), 0);
    }

    #[test]
    fn adding_twice_runs_the_same_argv_and_succeeds() {
        let runner = FakeRunner::quiet(4);
        let request = request(Backend::Firewalld, Action::Add);
        assert_eq!(apply(&runner, request), Ok(()));
        assert_eq!(apply(&runner, request), Ok(()));
        let seen = runner.seen();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[0], seen[2]);
        assert_eq!(seen[1], seen[3]);
    }

    #[test]
    fn removing_twice_runs_the_same_argv_and_succeeds() {
        let runner = FakeRunner::quiet(4);
        let request = request(Backend::Ufw, Action::Remove);
        assert_eq!(apply(&runner, request), Ok(()));
        assert_eq!(apply(&runner, request), Ok(()));
        assert_eq!(runner.seen()[0], runner.seen()[1]);
    }

    #[test]
    fn a_disabled_firewalld_is_a_successful_remove() {
        let runner = FakeRunner::replying(vec![Ok(output(FIREWALLD_NOT_ENABLED)), Ok(output(0))]);
        assert_eq!(
            apply(&runner, request(Backend::Firewalld, Action::Remove)),
            Ok(())
        );
    }

    #[test]
    fn a_disabled_firewalld_fails_an_add() {
        let runner = FakeRunner::replying(vec![Ok(output(FIREWALLD_NOT_ENABLED)), Ok(output(0))]);
        let failure = apply(&runner, request(Backend::Firewalld, Action::Add)).unwrap_err();
        assert_eq!(failure.exit.code(), 1);
    }

    #[test]
    fn an_absent_ufw_rule_is_a_successful_remove() {
        let runner = FakeRunner::replying(vec![Ok(output_with(
            1,
            "ERROR: Could not delete non-existent rule",
        ))]);
        assert_eq!(
            apply(&runner, request(Backend::Ufw, Action::Remove)),
            Ok(())
        );
    }

    #[test]
    fn a_refused_firewalld_request_is_cancelled() {
        let runner = FakeRunner::replying(vec![Ok(output(FIREWALLD_NOT_AUTHORIZED))]);
        let failure = apply(&runner, request(Backend::Firewalld, Action::Add)).unwrap_err();
        assert_eq!(failure.exit.code(), 126);
        assert!(
            failure.detail.contains("--add-port=3610/udp"),
            "{}",
            failure.detail
        );
    }

    #[test]
    fn a_tool_failure_carries_the_tool_stderr() {
        let runner = FakeRunner::replying(vec![Ok(output_with(1, "Error: INVALID_ZONE"))]);
        let failure = apply(&runner, request(Backend::Firewalld, Action::Add)).unwrap_err();
        assert_eq!(failure.exit.code(), 1);
        assert!(
            failure.detail.contains("INVALID_ZONE"),
            "{}",
            failure.detail
        );
    }

    #[test]
    fn an_inactive_ufw_is_refused_before_the_rule_is_touched() {
        let runner = FakeRunner::replying(vec![Ok(output(3))]);
        let failure = apply(&runner, request(Backend::Ufw, Action::Add)).unwrap_err();
        assert_eq!(failure.exit.code(), 1);
        assert!(
            failure.detail.contains("ufw is inactive"),
            "{}",
            failure.detail
        );
        assert_eq!(runner.seen().len(), 1);
    }

    #[test]
    fn a_missing_tool_is_a_failure_not_a_crash() {
        let runner = FakeRunner::replying(vec![Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no firewall-cmd",
        ))]);
        let failure = apply(&runner, request(Backend::Firewalld, Action::Add)).unwrap_err();
        assert_eq!(failure.exit.code(), 1);
        assert!(
            failure.detail.contains("firewall-cmd"),
            "{}",
            failure.detail
        );
    }

    #[test]
    fn the_windows_backend_is_refused_on_linux() {
        let runner = FakeRunner::quiet(0);
        let failure = apply(&runner, request(Backend::WindowsDefender, Action::Add)).unwrap_err();
        assert_eq!(failure.exit.code(), 1);
        assert_eq!(runner.seen().len(), 0);
    }

    #[test]
    fn accepts_the_trusted_prefixes() {
        for path in [
            "/nix/store/abc/bin/echonet-radar-firewall-helper",
            "/usr/bin/echonet-radar-firewall-helper",
            "/opt/echonet-radar/bin/echonet-radar-firewall-helper",
            "/run/current-system/sw/bin/echonet-radar-firewall-helper",
        ] {
            assert!(trusted_prefix(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn rejects_a_build_outside_the_trusted_prefixes() {
        for path in [
            "/home/user/target/debug/echonet-radar-firewall-helper",
            "/tmp/echonet-radar-firewall-helper",
            "/usrlocal/bin/echonet-radar-firewall-helper",
        ] {
            assert!(!trusted_prefix(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn requires_root_ownership_and_no_group_or_other_write() {
        assert!(trusted_owner(0, 0o755));
        assert!(!trusted_owner(1000, 0o755));
        assert!(!trusted_owner(0, 0o775));
        assert!(!trusted_owner(0, 0o757));
    }

    #[test]
    fn a_prefixed_path_that_does_not_exist_is_not_trusted() {
        assert!(!trusted(Path::new(
            "/usr/lib/echonet-radar/not-installed-here"
        )));
    }
}
