use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use echonet_lite::ecodec::class_name;
use echonet_lite::frame::Eoj;

const CHANGE_RING_CAPACITY: usize = 500;
const NOTICE_RING_CAPACITY: usize = 20;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

pub fn next_session_id() -> u64 {
    NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
}

/// Device identity. The port is excluded so a device that answers from an
/// ephemeral port stays one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey {
    pub ip: Ipv4Addr,
    pub eoj: Eoj,
}

impl ObjectKey {
    /// The address to send a request to.
    ///
    /// Devices listen on the standard ECHONET Lite port but differ in the port
    /// they send *from*: the air conditioner on this LAN answers from an
    /// ephemeral port, so a request sent to the port a frame arrived from never
    /// reaches it.
    #[must_use]
    pub fn listen_address(self) -> SocketAddr {
        SocketAddr::from((self.ip, echonet_lite_udp::MULTICAST_PORT))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadarSnapshot {
    pub revision: u64,
    pub session: u64,
    pub status: SessionStatus,
    pub objects: BTreeMap<ObjectKey, ObjectSnapshot>,
    pub recent: Vec<ChangeRecord>,
    pub notices: Vec<Notice>,
    /// Every datagram this session accepted. Readiness and the status bar
    /// read these fields; the capped change ring cannot stand in for them.
    pub frames_seen: u64,
    pub last_frame: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSnapshot {
    pub key: ObjectKey,
    pub source: SocketAddr,
    pub class_code: u16,
    pub class_name: &'static str,
    pub last_seen: SystemTime,
    pub properties: BTreeMap<u8, PropertySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertySnapshot {
    pub raw_edt: Vec<u8>,
    /// The value rendered by [`crate::format_edt`], derived from `raw_edt`.
    pub text: String,
    pub at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRecord {
    pub at: SystemTime,
    pub key: ObjectKey,
    pub epc: u8,
    /// The value rendered by [`crate::format_edt`], derived from `raw_edt`.
    pub text: String,
    pub raw_edt: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub at: SystemTime,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Binding,
    Running {
        since: SystemTime,
    },
    Degraded {
        fault: Fault,
        last_frame: Option<SystemTime>,
    },
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    Bind { interface: Ipv4Addr, detail: String },
    Config(ConfigError),
    Service(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    ZeroInterval(&'static str),
}

impl std::fmt::Display for ConfigError {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self {
            Self::ZeroInterval(name) => write!(f, "{name} must be greater than zero"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEvent {
    pub at: SystemTime,
    pub source: SocketAddr,
    pub eoj: Eoj,
    pub epc: u8,
    /// The property value rendered by [`crate::format_edt`], derived from `raw_edt`.
    pub text: String,
    pub raw_edt: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEvent {
    pub source: SocketAddr,
    pub eoj: Eoj,
}

/// A tracked device object answered a telegram.
///
/// One event is emitted per answered telegram, including answers that also
/// carry [`RadarEvent::Change`] values, so a poll batched over several
/// telegrams emits several.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenEvent {
    pub at: SystemTime,
    pub source: SocketAddr,
    pub eoj: Eoj,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadarEvent {
    Change(ChangeEvent),
    Device(DeviceEvent),
    Seen(SeenEvent),
    Status(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    Device {
        key: ObjectKey,
        class_code: u16,
        source: SocketAddr,
    },
    Value {
        key: ObjectKey,
        epc: u8,
        raw_edt: Vec<u8>,
        /// The value rendered by [`crate::format_edt`], derived from `raw_edt`.
        text: String,
        at: SystemTime,
        source: SocketAddr,
    },
    Seen {
        key: ObjectKey,
        at: SystemTime,
        source: SocketAddr,
    },
    Status(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    pub event: Option<RadarEvent>,
    pub dirty: bool,
}

#[derive(Debug, Clone)]
pub struct RadarModel {
    session: u64,
    revision: u64,
    status: SessionStatus,
    objects: BTreeMap<ObjectKey, ObjectSnapshot>,
    recent: Vec<ChangeRecord>,
    notices: Vec<Notice>,
    frames_seen: u64,
    last_frame: Option<SystemTime>,
}

impl RadarModel {
    #[must_use]
    pub const fn new(session: u64) -> Self {
        Self {
            session,
            revision: 0,
            status: SessionStatus::Binding,
            objects: BTreeMap::new(),
            recent: Vec::new(),
            notices: Vec::new(),
            frames_seen: 0,
            last_frame: None,
        }
    }

    pub fn set_status(
        &mut self,
        status: SessionStatus,
    ) {
        self.status = status;
    }

    pub fn degrade(
        &mut self,
        fault: Fault,
    ) {
        self.status = SessionStatus::Degraded {
            fault,
            last_frame: self.last_frame,
        };
    }

    pub const fn record_frame(
        &mut self,
        at: SystemTime,
    ) {
        self.frames_seen += 1;
        self.last_frame = Some(at);
    }

    /// The event is `None` when a renderer would draw nothing new, as with a
    /// repeated value; the snapshot still carries the observation's timestamp.
    pub fn observe(
        &mut self,
        observation: Observation,
    ) -> Projection {
        match observation {
            Observation::Device {
                key,
                class_code,
                source,
            } => {
                let is_new = !self.objects.contains_key(&key);
                let object = self.object_mut(key, SystemTime::now());
                object.class_code = class_code;
                object.class_name = class_name(class_code);
                object.source = source;
                Projection {
                    event: is_new.then_some(RadarEvent::Device(DeviceEvent {
                        source,
                        eoj: key.eoj,
                    })),
                    dirty: true,
                }
            },
            Observation::Value {
                key,
                epc,
                raw_edt,
                text,
                at,
                source,
            } => {
                let object = self.object_mut(key, at);
                object.last_seen = at;
                object.source = source;
                let previous = object.properties.insert(
                    epc,
                    PropertySnapshot {
                        raw_edt: raw_edt.clone(),
                        text: text.clone(),
                        at,
                    },
                );
                if previous.is_some_and(|previous| previous.raw_edt == raw_edt) {
                    return Projection {
                        event: None,
                        dirty: true,
                    };
                }
                push_capped(
                    &mut self.recent,
                    ChangeRecord {
                        at,
                        key,
                        epc,
                        text: text.clone(),
                        raw_edt: raw_edt.clone(),
                    },
                    CHANGE_RING_CAPACITY,
                );
                Projection {
                    event: Some(RadarEvent::Change(ChangeEvent {
                        at,
                        source,
                        eoj: key.eoj,
                        epc,
                        text,
                        raw_edt,
                    })),
                    dirty: true,
                }
            },
            Observation::Seen { key, at, source } => {
                let object = self.object_mut(key, at);
                object.last_seen = at;
                object.source = source;
                Projection {
                    event: Some(RadarEvent::Seen(SeenEvent {
                        at,
                        source,
                        eoj: key.eoj,
                    })),
                    dirty: true,
                }
            },
            Observation::Status(text) => {
                push_capped(
                    &mut self.notices,
                    Notice {
                        at: SystemTime::now(),
                        text: text.clone(),
                    },
                    NOTICE_RING_CAPACITY,
                );
                Projection {
                    event: Some(RadarEvent::Status(text)),
                    dirty: true,
                }
            },
        }
    }

    pub fn snapshot(&mut self) -> RadarSnapshot {
        self.revision += 1;
        self.snapshot_at(self.revision)
    }

    /// Revision zero never collides with a publish, so a reader can tell a
    /// first publish from the seed.
    #[must_use]
    pub fn initial_snapshot(&self) -> RadarSnapshot {
        self.snapshot_at(self.revision)
    }

    fn snapshot_at(
        &self,
        revision: u64,
    ) -> RadarSnapshot {
        RadarSnapshot {
            revision,
            session: self.session,
            status: self.status.clone(),
            objects: self.objects.clone(),
            recent: self.recent.clone(),
            notices: self.notices.clone(),
            frames_seen: self.frames_seen,
            last_frame: self.last_frame,
        }
    }

    fn object_mut(
        &mut self,
        key: ObjectKey,
        at: SystemTime,
    ) -> &mut ObjectSnapshot {
        self.objects.entry(key).or_insert_with(|| {
            let class_code = key.eoj.class_code();
            ObjectSnapshot {
                key,
                source: key.listen_address(),
                class_code,
                class_name: class_name(class_code),
                last_seen: at,
                properties: BTreeMap::new(),
            }
        })
    }
}

fn push_capped<T>(
    ring: &mut Vec<T>,
    item: T,
    capacity: usize,
) {
    if ring.len() >= capacity {
        ring.remove(0);
    }
    ring.push(item);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    fn key(class_code: u16) -> ObjectKey {
        ObjectKey {
            ip: Ipv4Addr::new(192, 0, 2, 7),
            eoj: Eoj::from_class_code(class_code, 0x01),
        }
    }

    fn source() -> SocketAddr {
        "192.0.2.7:49152".parse().unwrap()
    }

    fn value(
        key: ObjectKey,
        epc: u8,
        raw_edt: Vec<u8>,
        text: &str,
    ) -> Observation {
        Observation::Value {
            key,
            epc,
            raw_edt,
            text: String::from(text),
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            source: source(),
        }
    }

    #[test]
    fn device_observation_tracks_the_object() {
        let mut model = RadarModel::new(1);
        let key = key(0x0130);

        let projection = model.observe(Observation::Device {
            key,
            class_code: 0x0130,
            source: source(),
        });

        let Some(RadarEvent::Device(device)) = projection.event else {
            panic!("expected a device event");
        };
        assert_eq!(device.source, source());
        assert_eq!(device.eoj, key.eoj);
        assert!(projection.dirty);
        let snapshot = model.snapshot();
        assert_eq!(
            snapshot.objects.get(&key).map(|object| object.source),
            Some(source())
        );
    }

    #[test]
    fn value_observation_records_the_property() {
        let mut model = RadarModel::new(1);
        let key = key(0x0130);

        let projection = model.observe(value(key, 0x80, vec![0x30], "Operation status ON"));

        let Some(RadarEvent::Change(change)) = projection.event else {
            panic!("expected a change event");
        };
        assert_eq!(change.epc, 0x80);
        let snapshot = model.snapshot();
        let property = snapshot.objects[&key].properties.get(&0x80);
        assert_eq!(
            property.map(|property| (property.raw_edt.as_slice(), property.text.as_str())),
            Some((&[0x30][..], "Operation status ON"))
        );
    }

    #[test]
    fn seen_observation_stamps_the_object() {
        let mut model = RadarModel::new(1);
        let key = key(0x0130);
        model.observe(Observation::Device {
            key,
            class_code: 0x0130,
            source: source(),
        });
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(5);

        let projection = model.observe(Observation::Seen {
            key,
            at,
            source: source(),
        });

        let Some(RadarEvent::Seen(seen)) = projection.event else {
            panic!("expected a seen event");
        };
        assert_eq!(seen.at, at);
        assert_eq!(model.snapshot().objects[&key].last_seen, at);
    }

    #[test]
    fn status_observation_appends_a_notice() {
        let mut model = RadarModel::new(1);

        let projection = model.observe(Observation::Status(String::from("discovered 1")));

        assert_eq!(
            projection.event,
            Some(RadarEvent::Status(String::from("discovered 1")))
        );
        let snapshot = model.snapshot();
        assert_eq!(snapshot.notices.len(), 1);
        assert_eq!(snapshot.notices[0].text, "discovered 1");
    }

    #[test]
    fn class_name_is_present_on_an_object() {
        let mut model = RadarModel::new(1);
        let key = key(0x0130);

        model.observe(Observation::Device {
            key,
            class_code: 0x0130,
            source: source(),
        });

        let object = &model.snapshot().objects[&key];
        assert_eq!(object.class_code, 0x0130);
        assert_eq!(object.class_name, "Home air conditioner");
    }

    #[test]
    fn a_value_change_reaches_the_snapshot() {
        let mut model = RadarModel::new(1);
        let key = key(0x0130);
        model.observe(value(key, 0x80, vec![0x30], "Operation status ON"));

        let projection = model.observe(value(key, 0x80, vec![0x31], "Operation status OFF"));

        let Some(RadarEvent::Change(change)) = projection.event else {
            panic!("expected a change event");
        };
        assert_eq!(change.text, "Operation status OFF");
        let snapshot = model.snapshot();
        assert_eq!(
            snapshot.objects[&key].properties[&0x80].text,
            "Operation status OFF"
        );
        assert_eq!(snapshot.recent.len(), 2);
        assert_eq!(snapshot.recent[1].text, "Operation status OFF");
    }

    #[test]
    fn a_repeated_value_reports_no_change() {
        let mut model = RadarModel::new(1);
        let key = key(0x0130);
        model.observe(value(key, 0x80, vec![0x30], "Operation status ON"));

        let projection = model.observe(value(key, 0x80, vec![0x30], "Operation status ON"));

        assert_eq!(projection.event, None);
        assert!(projection.dirty);
        assert_eq!(model.snapshot().recent.len(), 1);
    }

    #[test]
    fn change_ring_keeps_the_newest_entries() {
        let mut model = RadarModel::new(1);
        let key = key(0x0130);
        for index in 0..=CHANGE_RING_CAPACITY {
            let raw_edt = vec![u8::try_from(index % 251).unwrap()];
            model.observe(value(key, 0x80, raw_edt, "value"));
        }

        let snapshot = model.snapshot();
        assert_eq!(snapshot.recent.len(), CHANGE_RING_CAPACITY);
        assert_eq!(snapshot.recent[0].raw_edt, vec![1]);
        assert_eq!(
            snapshot.recent[CHANGE_RING_CAPACITY - 1].raw_edt,
            vec![u8::try_from(CHANGE_RING_CAPACITY % 251).unwrap()]
        );
    }

    #[test]
    fn notice_ring_keeps_the_newest_entries() {
        let mut model = RadarModel::new(1);
        for index in 0..=NOTICE_RING_CAPACITY {
            model.observe(Observation::Status(index.to_string()));
        }

        let snapshot = model.snapshot();
        assert_eq!(snapshot.notices.len(), NOTICE_RING_CAPACITY);
        assert_eq!(snapshot.notices[0].text, "1");
        assert_eq!(
            snapshot.notices[NOTICE_RING_CAPACITY - 1].text,
            NOTICE_RING_CAPACITY.to_string()
        );
    }

    #[test]
    fn revision_increases_on_every_snapshot() {
        let mut model = RadarModel::new(4);

        let first = model.snapshot();
        let second = model.snapshot();

        assert_eq!(first.revision, 1);
        assert_eq!(second.revision, 2);
        assert_eq!(first.session, 4);
        assert_eq!(second.session, 4);
    }

    #[test]
    fn frame_counters_track_every_datagram() {
        let mut model = RadarModel::new(1);
        let first = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        let second = SystemTime::UNIX_EPOCH + Duration::from_secs(2);

        model.record_frame(first);
        model.record_frame(second);

        let snapshot = model.snapshot();
        assert_eq!(snapshot.frames_seen, 2);
        assert_eq!(snapshot.last_frame, Some(second));
    }
}
