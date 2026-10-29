//! Fixtures shared by the app's tests.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::SystemTime;

use echonet_radar_core::{
    ObjectKey, ObjectSnapshot, PropertySnapshot, RadarSnapshot, SessionStatus,
};

/// Build an `ObjectKey` without naming the `echonet-lite` EOJ type, which the
/// app is not allowed to depend on. The instance list starts with its count.
pub fn key(
    ip: Ipv4Addr,
    class_code: u16,
) -> ObjectKey {
    let mut eojs = echonet_radar_core::parse_instance_list(&[
        1,
        u8::try_from(class_code >> 8).expect("class group"),
        u8::try_from(class_code & 0x00FF).expect("class code"),
        1,
    ])
    .expect("a one-item instance list parses");
    ObjectKey {
        ip,
        eoj: eojs.remove(0),
    }
}

pub fn property(
    edt: &[u8],
    at: SystemTime,
) -> PropertySnapshot {
    PropertySnapshot {
        raw_edt: edt.to_vec(),
        text: String::from("value"),
        at,
    }
}

/// A snapshot from a running session that has seen one datagram.
pub fn snapshot(now: SystemTime) -> RadarSnapshot {
    RadarSnapshot {
        revision: 1,
        session: 1,
        status: SessionStatus::Running { since: now },
        objects: BTreeMap::new(),
        recent: Vec::new(),
        notices: Vec::new(),
        frames_seen: 1,
        last_frame: Some(now),
    }
}

pub fn object(
    ip: Ipv4Addr,
    class_code: u16,
    class_name: &'static str,
    properties: BTreeMap<u8, PropertySnapshot>,
    last_seen: SystemTime,
) -> ObjectSnapshot {
    ObjectSnapshot {
        key: key(ip, class_code),
        source: SocketAddr::from((ip, 3610)),
        class_code,
        class_name,
        last_seen,
        properties,
    }
}
