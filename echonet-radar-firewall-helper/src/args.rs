use std::ffi::{OsStr, OsString};

/// The helper carries no backend flag on Windows, where the Defender rules are
/// the only backend, so the accepted shape depends on the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Host {
    Linux,
    Windows,
}

impl Host {
    const fn usage(self) -> &'static str {
        match self {
            Self::Linux => "--backend <firewalld|ufw> --add|--remove --port 3610",
            Self::Windows => "--add|--remove --port 3610",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Add,
    Remove,
}

impl Action {
    pub const fn word(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    Firewalld,
    Ufw,
    WindowsDefender,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub action: Action,
    pub backend: Backend,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsageError {
    pub detail: String,
}

impl UsageError {
    fn new(
        host: Host,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            detail: format!("error: {}; usage: {}", detail.into(), host.usage()),
        }
    }
}

pub const PORT: &str = "3610";

pub fn parse(
    host: Host,
    args: &[OsString],
) -> Result<Request, UsageError> {
    let mut action = None;
    let mut backend = None;
    let mut saw_port = false;
    let mut remaining = args.iter();

    while let Some(argument) = remaining.next() {
        let Some(flag) = argument.to_str() else {
            return Err(UsageError::new(
                host,
                format!(
                    "argument \"{}\" is not valid UTF-8",
                    argument.to_string_lossy()
                ),
            ));
        };
        match flag {
            "--add" => set_action(host, &mut action, Action::Add)?,
            "--remove" => set_action(host, &mut action, Action::Remove)?,
            "--backend" => {
                if host == Host::Windows {
                    return Err(UsageError::new(
                        host,
                        "the backend is fixed on this platform",
                    ));
                }
                if backend.is_some() {
                    return Err(UsageError::new(host, "--backend is given twice"));
                }
                let value = remaining
                    .next()
                    .ok_or_else(|| UsageError::new(host, "--backend needs a value"))?;
                backend = Some(backend_named(host, value)?);
            },
            "--port" => {
                if saw_port {
                    return Err(UsageError::new(host, "--port is given twice"));
                }
                let value = remaining
                    .next()
                    .ok_or_else(|| UsageError::new(host, "--port needs a value"))?;
                if value.to_str() != Some(PORT) {
                    return Err(UsageError::new(host, format!("the port must be {PORT}")));
                }
                saw_port = true;
            },
            other => {
                return Err(UsageError::new(
                    host,
                    format!("unknown argument \"{other}\""),
                ));
            },
        }
    }

    let action = action.ok_or_else(|| UsageError::new(host, "missing --add or --remove"))?;
    if !saw_port {
        return Err(UsageError::new(host, "missing --port"));
    }
    let backend = match host {
        Host::Linux => backend.ok_or_else(|| UsageError::new(host, "missing --backend"))?,
        Host::Windows => Backend::WindowsDefender,
    };
    Ok(Request { action, backend })
}

fn set_action(
    host: Host,
    slot: &mut Option<Action>,
    action: Action,
) -> Result<(), UsageError> {
    match slot {
        Some(current) if *current == action => Err(UsageError::new(
            host,
            format!("--{} is given twice", action.word()),
        )),
        Some(_) => Err(UsageError::new(host, "--add and --remove are exclusive")),
        None => {
            *slot = Some(action);
            Ok(())
        },
    }
}

fn backend_named(
    host: Host,
    value: &OsStr,
) -> Result<Backend, UsageError> {
    match value.to_str() {
        Some("firewalld") => Ok(Backend::Firewalld),
        Some("ufw") => Ok(Backend::Ufw),
        _ => Err(UsageError::new(
            host,
            format!("unknown backend \"{}\"", value.to_string_lossy()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn linux(values: &[&str]) -> Result<Request, UsageError> {
        parse(Host::Linux, &args(values))
    }

    fn windows(values: &[&str]) -> Result<Request, UsageError> {
        parse(Host::Windows, &args(values))
    }

    fn rejected(result: Result<Request, UsageError>) -> String {
        result.unwrap_err().detail
    }

    #[test]
    fn accepts_the_linux_add_shapes() {
        for backend in ["firewalld", "ufw"] {
            let request = linux(&["--backend", backend, "--add", "--port", "3610"]).unwrap();
            assert_eq!(request.action, Action::Add);
        }
        assert_eq!(
            linux(&["--backend", "firewalld", "--add", "--port", "3610"])
                .unwrap()
                .backend,
            Backend::Firewalld
        );
        assert_eq!(
            linux(&["--backend", "ufw", "--add", "--port", "3610"])
                .unwrap()
                .backend,
            Backend::Ufw
        );
    }

    #[test]
    fn accepts_the_remove_shape_and_any_flag_order() {
        assert_eq!(
            linux(&["--port", "3610", "--remove", "--backend", "ufw"]).unwrap(),
            Request {
                action: Action::Remove,
                backend: Backend::Ufw,
            }
        );
    }

    #[test]
    fn accepts_the_windows_shapes_only_without_a_backend() {
        assert_eq!(
            windows(&["--add", "--port", "3610"]).unwrap(),
            Request {
                action: Action::Add,
                backend: Backend::WindowsDefender,
            }
        );
        assert_eq!(
            windows(&["--remove", "--port", "3610"]).unwrap().action,
            Action::Remove
        );
    }

    #[test]
    fn rejects_a_windows_backend_flag() {
        let detail = rejected(windows(&[
            "--backend",
            "firewalld",
            "--add",
            "--port",
            "3610",
        ]));
        assert!(detail.contains("backend is fixed"), "{detail}");
    }

    #[test]
    fn rejects_a_missing_backend_on_linux() {
        assert!(rejected(linux(&["--add", "--port", "3610"])).contains("missing --backend"));
    }

    #[test]
    fn rejects_a_missing_action() {
        assert!(
            rejected(linux(&["--backend", "ufw", "--port", "3610"]))
                .contains("missing --add or --remove")
        );
    }

    #[test]
    fn rejects_a_wrong_port_and_a_missing_port_value() {
        assert!(
            rejected(linux(&["--backend", "ufw", "--add", "--port", "3611"]))
                .contains("the port must be 3610")
        );
        assert!(
            rejected(linux(&["--backend", "ufw", "--add", "--port"]))
                .contains("--port needs a value")
        );
    }

    #[test]
    fn rejects_a_missing_port() {
        assert!(rejected(linux(&["--backend", "ufw", "--add"])).contains("missing --port"));
        assert!(rejected(windows(&["--remove"])).contains("missing --port"));
    }

    #[test]
    fn rejects_a_duplicate_action() {
        let detail = rejected(linux(&[
            "--backend",
            "ufw",
            "--add",
            "--remove",
            "--port",
            "3610",
        ]));
        assert!(detail.contains("exclusive"), "{detail}");
        let detail = rejected(linux(&[
            "--backend",
            "ufw",
            "--add",
            "--add",
            "--port",
            "3610",
        ]));
        assert!(detail.contains("--add is given twice"), "{detail}");
    }

    #[test]
    fn rejects_duplicate_flags() {
        assert!(
            rejected(linux(&[
                "--backend",
                "ufw",
                "--backend",
                "ufw",
                "--add",
                "--port",
                "3610",
            ]))
            .contains("--backend is given twice")
        );
        assert!(
            rejected(linux(&[
                "--backend",
                "ufw",
                "--add",
                "--port",
                "3610",
                "--port",
                "3610",
            ]))
            .contains("--port is given twice")
        );
    }

    #[test]
    fn rejects_an_unknown_flag_and_an_extra_argument() {
        assert!(
            rejected(linux(&[
                "--backend",
                "ufw",
                "--add",
                "--port",
                "3610",
                "--force",
            ]))
            .contains("unknown argument \"--force\"")
        );
        assert!(
            rejected(linux(&[
                "--backend",
                "ufw",
                "--add",
                "--port",
                "3610",
                "extra"
            ]))
            .contains("unknown argument \"extra\"")
        );
    }

    #[test]
    fn rejects_an_unknown_backend() {
        assert!(
            rejected(linux(
                &["--backend", "iptables", "--add", "--port", "3610",]
            ))
            .contains("unknown backend")
        );
    }

    #[test]
    fn rejects_an_empty_command_line() {
        assert!(rejected(linux(&[])).contains("missing --add or --remove"));
        assert!(rejected(windows(&[])).contains("missing --add or --remove"));
    }
}
