//! Paths the helper derives rather than receives.

use std::path::{Path, PathBuf};

/// The GUI binary beside the helper. The app path never crosses the elevation
/// boundary as an argument on either platform.
pub const TARGET_NAME: &str = if cfg!(windows) {
    "echonet-radar.exe"
} else {
    "echonet-radar"
};

pub fn sibling_target(helper: &Path) -> Option<PathBuf> {
    helper
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join(TARGET_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_target_beside_the_helper() {
        let helper = Path::new("/nix/store/0123-echonet-radar/bin/echonet-radar-firewall-helper");
        assert_eq!(
            sibling_target(helper).unwrap(),
            Path::new("/nix/store/0123-echonet-radar/bin/echonet-radar")
        );
    }

    #[test]
    fn keeps_a_directory_with_a_space_and_a_quote() {
        let helper = Path::new("/nix/store/x y'z/bin/echonet-radar-firewall-helper");
        assert_eq!(
            sibling_target(helper).unwrap(),
            Path::new("/nix/store/x y'z/bin/echonet-radar")
        );
    }

    #[test]
    fn has_no_target_for_a_path_without_a_directory() {
        assert_eq!(
            sibling_target(Path::new("echonet-radar-firewall-helper")),
            None
        );
    }
}
