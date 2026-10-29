//! The one place that spawns a process, so tests can replace it.

use std::ffi::{OsStr, OsString};
use std::io;
use std::process::Command;

/// One finished command, with both streams captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// `None` when a signal terminated the process.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a program from a fixed argv.
///
/// Every caller passes the program and each argument separately, so no path or
/// port is ever assembled into a shell command line.
pub trait CommandRunner {
    /// # Errors
    ///
    /// Returns the spawn or wait failure; a nonzero exit is a successful call.
    fn run(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> io::Result<CommandOutput>;
}

/// The runner the app uses.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdCommandRunner;

impl CommandRunner for StdCommandRunner {
    fn run(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> io::Result<CommandOutput> {
        let output = Command::new(program).args(args).output()?;
        Ok(CommandOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
