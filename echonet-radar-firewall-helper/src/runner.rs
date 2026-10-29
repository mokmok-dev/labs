use std::ffi::OsString;
use std::io;
use std::process::Command;

/// One process invocation. Arguments stay separate argv entries, so no path or
/// value is ever parsed by a shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub program: &'static str,
    pub args: Vec<&'static str>,
    pub env: Vec<(&'static str, OsString)>,
}

impl Invocation {
    pub fn new(
        program: &'static str,
        args: &[&'static str],
    ) -> Self {
        Self {
            program,
            args: args.to_vec(),
            env: Vec::new(),
        }
    }

    #[cfg(windows)]
    pub fn with_env(
        mut self,
        key: &'static str,
        value: OsString,
    ) -> Self {
        self.env.push((key, value));
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn detail(&self) -> &str {
        let stderr = self.stderr.trim();
        if stderr.is_empty() {
            self.stdout.trim()
        } else {
            stderr
        }
    }
}

pub trait CommandRunner {
    fn run(
        &self,
        invocation: &Invocation,
    ) -> io::Result<Output>;
}

pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(
        &self,
        invocation: &Invocation,
    ) -> io::Result<Output> {
        let mut command = Command::new(invocation.program);
        command.args(&invocation.args);
        for (key, value) in &invocation.env {
            command.env(key, value);
        }
        let output = command.output()?;
        Ok(Output {
            // A signal-terminated child has no code.
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
