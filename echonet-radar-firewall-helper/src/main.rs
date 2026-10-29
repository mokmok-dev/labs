mod exit;

#[cfg(any(target_os = "linux", windows))]
mod args;
#[cfg(any(target_os = "linux", windows))]
mod paths;
#[cfg(any(target_os = "linux", windows))]
mod runner;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(windows)]
mod windows;

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    ExitCode::from(run(&args))
}

#[cfg(target_os = "linux")]
fn run(args: &[OsString]) -> u8 {
    linux::run(args)
}

#[cfg(windows)]
fn run(args: &[OsString]) -> u8 {
    windows::run(args)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn run(_args: &[OsString]) -> u8 {
    eprintln!("error: the firewall helper runs on Linux and Windows only");
    exit::Exit::Failed.code()
}
