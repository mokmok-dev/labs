//! The settings file: interface choice, intervals and the selected device.
//!
//! eframe persists window geometry in its own store. This file owns everything
//! else and is written before the window opens, because the interface choice is
//! needed to build the session.

use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use directories::ProjectDirs;
use echonet_radar_core::{DEFAULT_DISCOVERY_INTERVAL, DEFAULT_UPDATE_INTERVAL, Eoj, ObjectKey};
use serde::{Deserialize, Serialize};

/// The schema this build reads and writes.
pub const SETTINGS_SCHEMA: u32 = 1;

/// Shortest discovery interval a hand-edited file may request.
pub const MIN_DISCOVERY: Duration = Duration::from_secs(10);

/// Longest discovery interval a hand-edited file may request.
pub const MAX_DISCOVERY: Duration = Duration::from_secs(3600);

/// Shortest value-poll interval a hand-edited file may request.
pub const MIN_UPDATE: Duration = Duration::from_secs(5);

/// Longest value-poll interval a hand-edited file may request.
pub const MAX_UPDATE: Duration = Duration::from_secs(600);

/// Everything the app persists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// File schema version.
    pub schema: u32,
    /// Which interface the next session joins the multicast group on.
    pub interface: InterfaceChoice,
    /// Interval between discovery requests.
    pub discovery_interval: Duration,
    /// Interval between value-polling rounds.
    pub update_interval: Duration,
    /// The device selected when the app last closed.
    pub selected: Option<ObjectKey>,
}

/// A stored interface name, not an address, so a DHCP renewal does not persist
/// a stale lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterfaceChoice {
    /// Join the multicast group on every interface the host offers.
    Auto,
    /// Join on the named interface.
    Named(String),
}

impl InterfaceChoice {
    /// The text a picker shows for this choice.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Auto => String::from("Automatic"),
            Self::Named(name) => name.clone(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema: SETTINGS_SCHEMA,
            interface: InterfaceChoice::Auto,
            discovery_interval: DEFAULT_DISCOVERY_INTERVAL,
            update_interval: DEFAULT_UPDATE_INTERVAL,
            selected: None,
        }
    }
}

/// The settings file next to the app's data directory.
///
/// `None` when the host has no home directory; the app then keeps settings in
/// memory for the run.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "mokmok", "echonet-radar")
        .map(|dirs| dirs.data_local_dir().join("settings.json"))
}

/// Load the settings from the default path.
///
/// Never fails: a missing, unreadable, corrupt or newer file yields defaults and
/// a notice the app shows.
#[must_use]
pub fn load() -> (Settings, Option<String>) {
    default_path().map_or_else(
        || {
            (
                Settings::default(),
                Some(String::from(
                    "No data directory is available; settings stay in memory for this run.",
                )),
            )
        },
        |path| load_from(&path),
    )
}

/// Load the settings from `path`.
///
/// Never fails, for the same reasons as [`load`].
#[must_use]
pub fn load_from(path: &Path) -> (Settings, Option<String>) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (
                Settings::default(),
                Some(String::from("No settings file yet; using defaults.")),
            );
        },
        Err(error) => {
            return (
                Settings::default(),
                Some(format!(
                    "Could not read {}: {error}; using defaults.",
                    path.display()
                )),
            );
        },
    };
    let disk = match serde_json::from_str::<DiskSettings>(&text) {
        Ok(disk) => disk,
        Err(error) => {
            return (
                Settings::default(),
                Some(format!(
                    "{} is not readable settings JSON ({error}); using defaults.",
                    path.display()
                )),
            );
        },
    };
    if disk.schema > SETTINGS_SCHEMA {
        return (
            Settings::default(),
            Some(format!(
                "{} uses schema {}; this build reads schema {SETTINGS_SCHEMA}, so defaults are in use.",
                path.display(),
                disk.schema
            )),
        );
    }
    let mut settings = disk.into_settings();
    let mut clamped = Vec::new();
    let discovery = settings
        .discovery_interval
        .clamp(MIN_DISCOVERY, MAX_DISCOVERY);
    if discovery != settings.discovery_interval {
        clamped.push(format!(
            "discovery interval {} s clamped to {} s",
            settings.discovery_interval.as_secs(),
            discovery.as_secs()
        ));
        settings.discovery_interval = discovery;
    }
    let update = settings.update_interval.clamp(MIN_UPDATE, MAX_UPDATE);
    if update != settings.update_interval {
        clamped.push(format!(
            "poll interval {} s clamped to {} s",
            settings.update_interval.as_secs(),
            update.as_secs()
        ));
        settings.update_interval = update;
    }
    let notice = (!clamped.is_empty())
        .then(|| format!("{} changed; using clamped values.", clamped.join(", ")));
    (settings, notice)
}

/// Write the settings through a temporary file in the same directory.
///
/// # Errors
///
/// Returns the directory, write or rename error. A rename inside one directory
/// is atomic, so a crash leaves either the old file or the new one.
pub fn save_to(
    path: &Path,
    settings: &Settings,
) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(&DiskSettings::from_settings(settings))
        .map_err(io::Error::other)?;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, json)?;
    fs::rename(&temporary, path)
}

#[derive(Debug, Serialize, Deserialize)]
struct DiskSettings {
    schema: u32,
    interface: DiskInterface,
    discovery_interval: u64,
    update_interval: u64,
    selected: Option<DiskSelected>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DiskInterface {
    Auto,
    Named(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct DiskSelected {
    ip: String,
    eoj: [u8; 3],
}

impl DiskSettings {
    fn from_settings(settings: &Settings) -> Self {
        Self {
            schema: SETTINGS_SCHEMA,
            interface: match &settings.interface {
                InterfaceChoice::Auto => DiskInterface::Auto,
                InterfaceChoice::Named(name) => DiskInterface::Named(name.clone()),
            },
            discovery_interval: settings.discovery_interval.as_secs(),
            update_interval: settings.update_interval.as_secs(),
            selected: settings.selected.map(|key| DiskSelected {
                ip: key.ip.to_string(),
                eoj: [key.eoj.class_group, key.eoj.class, key.eoj.instance],
            }),
        }
    }

    fn into_settings(self) -> Settings {
        Settings {
            schema: self.schema,
            interface: match self.interface {
                DiskInterface::Auto => InterfaceChoice::Auto,
                DiskInterface::Named(name) => InterfaceChoice::Named(name),
            },
            discovery_interval: Duration::from_secs(self.discovery_interval),
            update_interval: Duration::from_secs(self.update_interval),
            selected: self.selected.as_ref().and_then(selected_from_disk),
        }
    }
}

fn selected_from_disk(disk: &DiskSelected) -> Option<ObjectKey> {
    let ip = disk.ip.parse::<Ipv4Addr>().ok()?;
    let eoj = Eoj::new(disk.eoj[0], disk.eoj[1], disk.eoj[2]);
    Some(ObjectKey { ip, eoj })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "echonet-radar-settings-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("temporary directory");
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("settings.json")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn selected() -> ObjectKey {
        let ip = Ipv4Addr::new(192, 168, 1, 20);
        ObjectKey {
            ip,
            eoj: Eoj::new(0x02, 0x87, 0x01),
        }
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = TempDir::new();
        let settings = Settings {
            schema: SETTINGS_SCHEMA,
            interface: InterfaceChoice::Named(String::from("br0")),
            discovery_interval: Duration::from_secs(30),
            update_interval: Duration::from_secs(7),
            selected: Some(selected()),
        };
        save_to(&dir.file(), &settings).expect("save");
        let (loaded, notice) = load_from(&dir.file());
        assert_eq!(loaded, settings);
        assert!(notice.is_none(), "unexpected notice: {notice:?}");
        assert!(
            !dir.file().with_file_name("settings.json.tmp").exists(),
            "the temporary file must be renamed away"
        );
    }

    #[test]
    fn round_trips_the_automatic_choice() {
        let dir = TempDir::new();
        let settings = Settings::default();
        save_to(&dir.file(), &settings).expect("save");
        let (loaded, notice) = load_from(&dir.file());
        assert_eq!(loaded, settings);
        assert!(notice.is_none());
    }

    #[test]
    fn a_missing_file_yields_defaults_and_a_notice() {
        let dir = TempDir::new();
        let (settings, notice) = load_from(&dir.file());
        assert_eq!(settings, Settings::default());
        assert!(notice.is_some());
    }

    #[test]
    fn a_corrupt_file_yields_defaults_and_a_notice() {
        let dir = TempDir::new();
        fs::write(dir.file(), b"{ not json").expect("write");
        let (settings, notice) = load_from(&dir.file());
        assert_eq!(settings, Settings::default());
        assert!(notice.is_some_and(|notice| notice.contains("not readable")));
    }

    #[test]
    fn a_newer_schema_yields_defaults_and_a_notice() {
        let dir = TempDir::new();
        let json = serde_json::json!({
            "schema": SETTINGS_SCHEMA + 1,
            "interface": "auto",
            "discovery_interval": 60,
            "update_interval": 15,
            "selected": null,
        });
        fs::write(dir.file(), json.to_string()).expect("write");
        let (settings, notice) = load_from(&dir.file());
        assert_eq!(settings, Settings::default());
        assert!(notice.is_some_and(|notice| notice.contains("schema")));
    }

    #[test]
    fn intervals_are_clamped_to_the_allowed_range() {
        let dir = TempDir::new();
        let json = serde_json::json!({
            "schema": SETTINGS_SCHEMA,
            "interface": "auto",
            "discovery_interval": 1,
            "update_interval": 99_999,
            "selected": null,
        });
        fs::write(dir.file(), json.to_string()).expect("write");
        let (settings, notice) = load_from(&dir.file());
        assert_eq!(settings.discovery_interval, MIN_DISCOVERY);
        assert_eq!(settings.update_interval, MAX_UPDATE);
        assert!(notice.is_some_and(|notice| notice.contains("clamped")));

        let json = serde_json::json!({
            "schema": SETTINGS_SCHEMA,
            "interface": "auto",
            "discovery_interval": 99_999,
            "update_interval": 1,
            "selected": null,
        });
        fs::write(dir.file(), json.to_string()).expect("write");
        let (settings, _notice) = load_from(&dir.file());
        assert_eq!(settings.discovery_interval, MAX_DISCOVERY);
        assert_eq!(settings.update_interval, MIN_UPDATE);
    }

    #[test]
    fn an_unknown_interface_name_is_kept_for_resolution_to_report() {
        let dir = TempDir::new();
        let json = serde_json::json!({
            "schema": SETTINGS_SCHEMA,
            "interface": { "named": "vanish0" },
            "discovery_interval": 60,
            "update_interval": 15,
            "selected": null,
        });
        fs::write(dir.file(), json.to_string()).expect("write");
        let (settings, _notice) = load_from(&dir.file());
        assert_eq!(
            settings.interface,
            InterfaceChoice::Named(String::from("vanish0"))
        );
    }

    #[test]
    fn a_malformed_selected_device_is_dropped() {
        let dir = TempDir::new();
        let json = serde_json::json!({
            "schema": SETTINGS_SCHEMA,
            "interface": "auto",
            "discovery_interval": 60,
            "update_interval": 15,
            "selected": { "ip": "not an address", "eoj": [2, 136, 1] },
        });
        fs::write(dir.file(), json.to_string()).expect("write");
        let (settings, _notice) = load_from(&dir.file());
        assert_eq!(settings.selected, None);
    }

    #[test]
    fn interface_labels_read_as_the_picker_shows_them() {
        assert_eq!(InterfaceChoice::Auto.label(), "Automatic");
        assert_eq!(InterfaceChoice::Named(String::from("eth0")).label(), "eth0");
    }

    #[test]
    fn defaults_carry_the_schema_and_desktop_intervals() {
        let settings = Settings::default();
        assert_eq!(settings.schema, SETTINGS_SCHEMA);
        assert_eq!(settings.discovery_interval, Duration::from_secs(60));
        assert_eq!(settings.update_interval, Duration::from_secs(15));
        assert_eq!(settings.selected, None);
    }
}
