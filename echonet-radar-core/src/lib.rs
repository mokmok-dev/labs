//! ECHONET Lite state-change discovery and logging.
//!
//! This crate is the headless core behind the `echonet-radar-cli` terminal
//! monitor. The service keeps the transport and rendering separate: it owns
//! protocol state and emits [`RadarEvent`]s, leaving the caller to render them.
//!
//! The radar performs periodic discovery (every minute) to learn which device
//! objects exist, and periodic value polling (every 15 seconds) to detect state
//! changes. Devices are announced as [`RadarEvent::Device`] as soon as they are
//! tracked, so they can be listed before any value is observed. Any property
//! whose raw EDT differs from the last known value is reported as a
//! [`RadarEvent::Change`]. Every answer from a tracked object is also reported
//! as a [`RadarEvent::Seen`], so a renderer can tell a quiet device from a dead
//! one. When a device pushes an INF telegram, its properties are processed
//! immediately so the change is rendered without waiting for the next poll
//! round.

mod format;
mod model;
mod protocol;
mod service;
mod session;

pub use echonet_lite::frame::Eoj;
pub use echonet_lite_udp::{MULTICAST_GROUP, MULTICAST_PORT};
pub use format::{format_edt, format_value, instantaneous_watts, power_epc};
pub use model::{
    ChangeEvent, ChangeRecord, ConfigError, DeviceEvent, Fault, Notice, ObjectKey, ObjectSnapshot,
    Observation, Projection, PropertySnapshot, RadarEvent, RadarModel, RadarSnapshot, SeenEvent,
    SessionStatus,
};
pub use protocol::{
    CONTROLLER_CLASS_CODE, CONTROLLER_EOJ, DEFAULT_DISCOVERY_INTERVAL, DEFAULT_UPDATE_INTERVAL,
    DISCOVERY_EPC, DISCOVERY_NODE_PROFILE_EOJ, GET_ESV_CODE, GET_PROPERTY_MAP_EPC,
    GET_RESPONSE_ESV_CODE, GET_RESPONSE_WITH_STATUS_ESV_CODE, INF_ESV_CODE,
    NODE_PROFILE_CLASS_CODE, ProtocolError, discovery_header, get_header, parse_instance_list,
    parse_property_map,
};
pub use service::Command;
pub use session::{
    CommandError, NoopPulse, Pulse, RadarHandle, RadarSession, ServiceIo, SessionConfig,
    run_service,
};
