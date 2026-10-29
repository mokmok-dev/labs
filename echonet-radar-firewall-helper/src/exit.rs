//! The four outcomes the helper reports to its caller.

/// A run result, mapped to the process exit code the app reads back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Exit {
    /// The rule is in the requested state.
    Ok,
    /// The tool refused or failed, or the install path is not trusted.
    Failed,
    /// The command line matched no accepted shape.
    Usage,
    /// The user dismissed the elevation prompt, or polkit denied it.
    Cancelled,
}

impl Exit {
    pub const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Failed => 1,
            Self::Usage => 2,
            Self::Cancelled => 126,
        }
    }
}

/// A failed run and the single line the helper prints for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Failure {
    pub exit: Exit,
    pub detail: String,
}

impl Failure {
    pub fn new(
        exit: Exit,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            exit,
            detail: detail.into(),
        }
    }
}
