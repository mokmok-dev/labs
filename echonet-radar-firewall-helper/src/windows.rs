use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::args::{self, Action, Host};
use crate::exit::{Exit, Failure};
use crate::paths;
use crate::runner::{CommandRunner, Invocation, Output, SystemRunner};

const SCRIPT: &str = include_str!("../scripts/rule.ps1");
const ACTION_ENV: &str = "ECHONET_RADAR_FIREWALL_ACTION";
const TARGET_ENV: &str = "ECHONET_RADAR_FIREWALL_TARGET";

/// Windows' own status when the user dismisses the UAC prompt.
const UAC_CANCELLED: i32 = 1223;

pub fn run(args: &[OsString]) -> u8 {
    let request = match args::parse(Host::Windows, args) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("{}", error.detail);
            return Exit::Usage.code();
        },
    };
    match dispatch(&SystemRunner, request.action) {
        Ok(()) => Exit::Ok.code(),
        Err(failure) => {
            eprintln!("error: {}", failure.detail);
            failure.exit.code()
        },
    }
}

fn dispatch(
    runner: &dyn CommandRunner,
    action: Action,
) -> Result<(), Failure> {
    let helper = std::env::current_exe().map_err(|error| {
        Failure::new(
            Exit::Failed,
            format!("could not locate the helper: {error}"),
        )
    })?;
    let target = paths::sibling_target(&helper).ok_or_else(manual)?;
    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .ok_or_else(manual)?;
    if !installed(&helper, &program_files) || !installed(&target, &program_files) {
        return Err(manual());
    }

    let output = runner.run(&invocation(action, &target)).map_err(|error| {
        Failure::new(Exit::Failed, format!("could not run PowerShell: {error}"))
    })?;
    match powershell_exit(output.status) {
        Exit::Ok => Ok(()),
        exit @ Exit::Cancelled => Err(Failure::new(
            exit,
            "the elevation prompt was dismissed; add the rules by hand in an administrator PowerShell",
        )),
        exit => Err(Failure::new(exit, describe(&output))),
    }
}

/// Symlink metadata, so a reparse point beside the helper is refused rather
/// than followed.
fn installed(
    path: &Path,
    program_files: &Path,
) -> bool {
    under(path, program_files)
        && std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

fn under(
    path: &Path,
    root: &Path,
) -> bool {
    let root = root.to_string_lossy().to_lowercase();
    let path = path.to_string_lossy().to_lowercase();
    path.strip_prefix(&root)
        .is_some_and(|rest| rest.starts_with('\\'))
}

fn invocation(
    action: Action,
    target: &Path,
) -> Invocation {
    Invocation::new(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", SCRIPT],
    )
    .with_env(ACTION_ENV, OsString::from(action.word()))
    .with_env(TARGET_ENV, target.as_os_str().to_owned())
}

const fn powershell_exit(status: i32) -> Exit {
    match status {
        0 => Exit::Ok,
        UAC_CANCELLED => Exit::Cancelled,
        _ => Exit::Failed,
    }
}

fn describe(output: &Output) -> String {
    let detail = output.detail();
    if detail.is_empty() {
        format!("PowerShell exited with status {}", output.status)
    } else {
        detail.to_owned()
    }
}

fn manual() -> Failure {
    Failure::new(
        Exit::Failed,
        "the helper and the app must both sit under %ProgramFiles% and exist as regular files; add the UDP 3610 inbound rules for the private and domain profiles by hand in an administrator PowerShell",
    )
}
