/// A run result, mapped to the process exit code the app reads back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Exit {
    Ok,
    Failed,
    Usage,
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
