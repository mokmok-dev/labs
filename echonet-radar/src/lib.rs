//! ECHONET Lite discovery and value polling for `echonet-radar`.
//!
//! The service deliberately keeps the transport and terminal rendering separate:
//! this module owns protocol state, while the binary renders [`RadarSnapshot`]
//! values with ratatui.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::Sender;
use std::time::Instant;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use echonet_lite::ecodec::{Access, DataKind, EdtValue, decode, lookup};
use echonet_lite::frame::{Eoj, Esv, FrameHeader, Property, parse};
use echonet_lite_udp::EchoNetSocket;

/// The controller EOJ used as the source of requests.
pub const CONTROLLER_EOJ: Eoj = Eoj::new(0x05, 0xFF, 0x01);
/// The wildcard node-profile EOJ used for discovery requests.
pub const DISCOVERY_NODE_PROFILE_EOJ: Eoj = Eoj::new(0x0E, 0xF0, 0x00);
/// The node-profile class code.
pub const NODE_PROFILE_CLASS_CODE: u16 = 0x0EF0;
/// The D6 self-node instance-list EPC.
pub const DISCOVERY_EPC: u8 = 0xD6;
/// The standard ECHONET Lite GET service code.
pub const GET_ESV_CODE: u8 = 0x62;
/// The standard ECHONET Lite GET response service code.
pub const GET_RESPONSE_ESV_CODE: u8 = 0x72;
/// The ECHONET Lite GET response service code used when at least one requested
/// property could not be read; readable properties still carry their EDT.
pub const GET_RESPONSE_WITH_STATUS_ESV_CODE: u8 = 0x52;
/// The standard ECHONET Lite get-property-map EPC.
pub const GET_PROPERTY_MAP_EPC: u8 = 0x9F;
/// The default discovery interval.
pub const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
/// The default base interval for value polling.
pub const DEFAULT_UPDATE_INTERVAL: Duration = Duration::from_secs(15);
/// The default maximum positive jitter added to value polling.
pub const DEFAULT_UPDATE_JITTER: Duration = Duration::from_secs(5);

const EMPTY_EDT: &[u8] = &[];
const VALUE_BATCH_SIZE: usize = 8;
const MAX_INSTANCE_LIST_ITEMS: usize = 84;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether a response service code answers a GET request.
///
/// `0x52` (GET response with status) is used when at least one requested
/// property could not be read; the properties that could be read still carry
/// their EDT, so the frame must not be discarded as a whole.
const fn is_get_response_code(code: u8) -> bool {
    code == GET_RESPONSE_ESV_CODE || code == GET_RESPONSE_WITH_STATUS_ESV_CODE
}

/// Runtime configuration for the radar service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadarConfig {
    /// IPv4 interface used to join the ECHONET Lite multicast group.
    pub interface: Ipv4Addr,
    /// Interval between D6 discovery requests.
    pub discovery_interval: Duration,
    /// Base interval between value polling rounds.
    pub update_interval: Duration,
    /// Maximum positive jitter added to each value polling round.
    pub update_jitter: Duration,
}

impl Default for RadarConfig {
    fn default() -> Self {
        Self {
            interface: Ipv4Addr::UNSPECIFIED,
            discovery_interval: DEFAULT_DISCOVERY_INTERVAL,
            update_interval: DEFAULT_UPDATE_INTERVAL,
            update_jitter: DEFAULT_UPDATE_JITTER,
        }
    }
}

impl RadarConfig {
    /// Validate intervals before constructing Tokio timers.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ZeroInterval`] when discovery or value polling
    /// would otherwise create a zero-duration timer.
    pub const fn validate(self) -> Result<(), ConfigError> {
        if self.discovery_interval.is_zero() {
            return Err(ConfigError::ZeroInterval("discovery_interval"));
        }
        if self.update_interval.is_zero() {
            return Err(ConfigError::ZeroInterval("update_interval"));
        }
        Ok(())
    }

    /// Produce the next value-poll delay using the supplied jitter source.
    #[must_use]
    pub fn next_update_delay(
        self,
        jitter: &mut JitterSource,
    ) -> Duration {
        self.update_interval
            .saturating_add(jitter.sample(self.update_jitter))
    }
}

/// Configuration validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// A required interval was zero.
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

/// Small non-cryptographic source used only to desynchronise polling rounds.
#[derive(Debug, Clone, Copy)]
pub struct JitterSource {
    state: u64,
}

impl JitterSource {
    /// Create a deterministic jitter source, useful for tests.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    /// Create a process-local source seeded from wall-clock time and process id.
    #[must_use]
    pub fn seeded() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
            });
        Self::new(nanos ^ u64::from(std::process::id()))
    }

    /// Sample a duration in the inclusive range `0..=maximum`.
    #[must_use]
    pub fn sample(
        &mut self,
        maximum: Duration,
    ) -> Duration {
        if maximum.is_zero() {
            return Duration::ZERO;
        }

        // Xorshift is sufficient here: jitter is not an authentication or
        // secrecy mechanism, it only prevents synchronized request bursts.
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;

        let maximum_millis = u64::try_from(maximum.as_millis()).unwrap_or(u64::MAX);
        let range = maximum_millis.saturating_add(1);
        Duration::from_millis(self.state % range)
    }
}

/// A discovered device object, identified by its source address and EOJ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceKey {
    /// Address from which the device responded.
    pub address: SocketAddr,
    /// Device object EOJ.
    pub eoj: Eoj,
}

impl DeviceKey {
    /// The identity used to deduplicate device rows: source IP and EOJ.
    ///
    /// The port is deliberately excluded so that a device answering from
    /// several ports is shown as a single row and contacted on its latest
    /// port.
    const fn id(&self) -> DeviceId {
        DeviceId {
            ip: self.address.ip(),
            eoj: self.eoj,
        }
    }
}

/// Identity of a device object, excluding the source port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DeviceId {
    ip: IpAddr,
    eoj: Eoj,
}

/// A decoded value displayed for one device object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueSnapshot {
    /// EPC of the value.
    pub epc: u8,
    /// MRA property name, or an EPC fallback for unknown properties.
    pub name: String,
    /// Human-readable decoded EDT.
    pub value: String,
    /// Raw EDT bytes as received, for detail views.
    pub edt: Vec<u8>,
    /// Time at which this value was last received.
    pub updated_at: Instant,
}

/// A device row in the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSnapshot {
    /// Device source and EOJ.
    pub key: DeviceKey,
    /// Values most recently received from the device.
    pub values: Vec<ValueSnapshot>,
    /// Time at which the device was last seen.
    pub last_seen: Instant,
    /// Time at which a value was last received.
    pub last_update: Option<Instant>,
}

/// Complete state sent from the network service to the terminal.
#[derive(Debug, Clone)]
pub struct RadarSnapshot {
    /// Discovered device rows.
    pub devices: Vec<DeviceSnapshot>,
    /// Current service status for the header.
    pub status: String,
    /// Most recent discovery request/response activity.
    pub last_discovery: Option<Instant>,
    /// Estimated time of the next discovery request.
    pub next_discovery: Instant,
    /// Most recent value polling round.
    pub last_update: Option<Instant>,
    /// Estimated time of the next value polling round.
    pub next_update: Instant,
}

impl RadarSnapshot {
    /// Create an empty snapshot with a startup status.
    #[must_use]
    pub fn empty() -> Self {
        let now = Instant::now();
        Self {
            devices: Vec::new(),
            status: String::from("starting"),
            last_discovery: None,
            next_discovery: now,
            last_update: None,
            next_update: now,
        }
    }

    /// Create an empty snapshot carrying a service error or status message.
    #[must_use]
    pub fn with_status(status: String) -> Self {
        let mut snapshot = Self::empty();
        snapshot.status = status;
        snapshot
    }
}

/// Errors found in variable-length ECHONET Lite property data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    /// The EDT did not contain its leading count byte.
    MissingCount,
    /// The number of EOJs exceeded the MRA limit.
    TooManyInstances,
    /// The EDT length did not match its leading count.
    InvalidInstanceListLength,
    /// The property-map EDT length was not valid for its encoding.
    InvalidPropertyMapLength,
    /// The property-map count did not match its bitmap contents.
    InvalidPropertyMapCount,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        let message = match self {
            Self::MissingCount => "property data is missing its count",
            Self::TooManyInstances => "instance list exceeds the MRA limit",
            Self::InvalidInstanceListLength => "instance list length does not match its count",
            Self::InvalidPropertyMapLength => "property map length is invalid",
            Self::InvalidPropertyMapCount => "property map count does not match its bitmap",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ProtocolError {}

/// Build the standard D6 discovery GET request header.
#[must_use]
pub const fn discovery_header(tid: u16) -> FrameHeader {
    FrameHeader {
        tid,
        seoj: CONTROLLER_EOJ,
        deoj: DISCOVERY_NODE_PROFILE_EOJ,
        // The existing low-level enum predates the standard ESV names. Keep
        // the application wire-compatible without changing that public API.
        esv: Esv::Unknown(GET_ESV_CODE),
    }
}

/// Build a standard GET request header for a device object.
#[must_use]
pub const fn get_header(
    tid: u16,
    deoj: Eoj,
) -> FrameHeader {
    FrameHeader {
        tid,
        seoj: CONTROLLER_EOJ,
        deoj,
        esv: Esv::Unknown(GET_ESV_CODE),
    }
}

/// Decode a D6 self-node instance-list EDT into device EOJs.
///
/// # Errors
///
/// Returns a [`ProtocolError`] when the count or byte length is inconsistent.
pub fn parse_instance_list(edt: &[u8]) -> Result<Vec<Eoj>, ProtocolError> {
    let count = edt.first().copied().ok_or(ProtocolError::MissingCount)?;
    let count = usize::from(count);
    if count > MAX_INSTANCE_LIST_ITEMS {
        return Err(ProtocolError::TooManyInstances);
    }
    let expected_len = 1 + count * 3;
    if edt.len() != expected_len {
        return Err(ProtocolError::InvalidInstanceListLength);
    }

    let (instances, _) = edt[1..].as_chunks::<3>();
    Ok(instances
        .iter()
        .map(|bytes| Eoj::new(bytes[0], bytes[1], bytes[2]))
        .collect())
}

/// Decode an ECHONET Lite property map into EPC bytes.
///
/// A count of 15 or less uses the compact EPC-list representation; larger
/// counts use the 16-byte bitmap covering EPC `0x80..=0xFF`.
///
/// # Errors
///
/// Returns a [`ProtocolError`] when the representation is malformed.
pub fn parse_property_map(edt: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    let count = edt.first().copied().ok_or(ProtocolError::MissingCount)?;
    let count = usize::from(count);
    if count <= 15 {
        if edt.len() != count + 1 {
            return Err(ProtocolError::InvalidPropertyMapLength);
        }
        return Ok(edt[1..].to_vec());
    }

    if edt.len() != 17 {
        return Err(ProtocolError::InvalidPropertyMapLength);
    }

    let mut epcs = Vec::with_capacity(count);
    for (byte_index, byte) in edt[1..].iter().copied().enumerate() {
        let byte_index =
            u16::try_from(byte_index).map_err(|_| ProtocolError::InvalidPropertyMapCount)?;
        for bit in 0..8u16 {
            if byte & (0x80 >> bit) != 0 {
                let epc = 0x80u16 + byte_index * 8 + bit;
                let epc = u8::try_from(epc).map_err(|_| ProtocolError::InvalidPropertyMapCount)?;
                epcs.push(epc);
            }
        }
    }
    if epcs.len() != count {
        return Err(ProtocolError::InvalidPropertyMapCount);
    }
    Ok(epcs)
}

/// Format an EDT using the generated MRA codec when the property is known.
#[must_use]
pub fn format_value(
    class_code: u16,
    epc: u8,
    edt: &[u8],
) -> String {
    match decode(class_code, epc, edt).map(|decoded| decoded.value) {
        Ok(EdtValue::Number {
            raw,
            scale_num,
            scale_den,
            unit,
        }) => format_number(raw, scale_num, scale_den, unit),
        Ok(EdtValue::State(name)) => String::from(name),
        Ok(EdtValue::Raw(bytes)) => format_bytes(bytes),
        Err(_) => format_bytes(edt),
    }
}

fn format_number(
    raw: i64,
    scale_num: u32,
    scale_den: u32,
    unit: &str,
) -> String {
    if scale_den == 0 {
        return format_with_unit(raw.to_string(), unit);
    }

    let scaled = i128::from(raw) * i128::from(scale_num);
    let negative = scaled.is_negative();
    let magnitude = scaled.abs();
    let denominator = i128::from(scale_den);
    let whole = magnitude / denominator;
    let mut result = whole.to_string();
    let mut remainder = magnitude % denominator;

    if remainder != 0 {
        let mut fraction = String::new();
        for _ in 0..6 {
            remainder *= 10;
            let digit = remainder / denominator;
            remainder %= denominator;
            if let Ok(digit) = u8::try_from(digit) {
                fraction.push(char::from(b'0' + digit));
            }
            if remainder == 0 {
                break;
            }
        }
        while fraction.ends_with('0') {
            fraction.pop();
        }
        if !fraction.is_empty() {
            result.push('.');
            result.push_str(&fraction);
        }
    }

    if negative && scaled != 0 {
        result.insert(0, '-');
    }
    format_with_unit(result, unit)
}

fn format_with_unit(
    mut value: String,
    unit: &str,
) -> String {
    if !unit.is_empty() {
        value.push(' ');
        value.push_str(unit);
    }
    value
}

fn format_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::from("(empty)");
    }
    let mut value = String::with_capacity(bytes.len() * 3 - 1);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            value.push(' ');
        }
        let _ = write!(value, "{byte:02X}");
    }
    value
}

#[derive(Debug, Clone)]
struct DeviceState {
    key: DeviceKey,
    poll_epcs: Vec<u8>,
    values: BTreeMap<u8, ValueSnapshot>,
    last_seen: Instant,
    last_update: Option<Instant>,
}

impl DeviceState {
    fn new(
        key: DeviceKey,
        now: Instant,
    ) -> Self {
        Self {
            poll_epcs: fallback_poll_epcs(key.eoj.class_code()),
            key,
            values: BTreeMap::new(),
            last_seen: now,
            last_update: None,
        }
    }

    fn snapshot(&self) -> DeviceSnapshot {
        DeviceSnapshot {
            key: self.key,
            values: self.values.values().cloned().collect(),
            last_seen: self.last_seen,
            last_update: self.last_update,
        }
    }
}

#[derive(Debug, Clone)]
enum PendingRequest {
    PropertyMap {
        key: DeviceKey,
        sent_at: Instant,
    },
    Values {
        key: DeviceKey,
        epcs: Vec<u8>,
        sent_at: Instant,
    },
}

impl PendingRequest {
    const fn key(&self) -> DeviceKey {
        match self {
            Self::PropertyMap { key, .. } | Self::Values { key, .. } => *key,
        }
    }

    const fn sent_at(&self) -> Instant {
        match self {
            Self::PropertyMap { sent_at, .. } | Self::Values { sent_at, .. } => *sent_at,
        }
    }
}

struct ServiceState {
    snapshots: Sender<RadarSnapshot>,
    devices: HashMap<DeviceId, DeviceState>,
    pending: HashMap<u16, PendingRequest>,
    discovery_tid: Option<u16>,
    next_tid: u16,
    status: String,
    last_discovery: Option<Instant>,
    last_update: Option<Instant>,
    next_discovery: Instant,
    next_update: Instant,
    jitter: JitterSource,
}

impl ServiceState {
    fn new(
        config: RadarConfig,
        snapshots: Sender<RadarSnapshot>,
    ) -> Self {
        let now = Instant::now();
        let mut jitter = JitterSource::seeded();
        let next_update = now + config.next_update_delay(&mut jitter);
        Self {
            snapshots,
            devices: HashMap::new(),
            pending: HashMap::new(),
            discovery_tid: None,
            next_tid: 0,
            status: String::from("waiting for discovery"),
            last_discovery: None,
            last_update: None,
            next_discovery: now,
            next_update,
            jitter,
        }
    }

    fn publish(&self) {
        let mut devices: Vec<_> = self.devices.values().map(DeviceState::snapshot).collect();
        devices.sort_by(|left, right| {
            left.key
                .address
                .cmp(&right.key.address)
                .then_with(|| left.key.eoj.class_code().cmp(&right.key.eoj.class_code()))
                .then_with(|| left.key.eoj.instance.cmp(&right.key.eoj.instance))
        });
        let snapshot = RadarSnapshot {
            devices,
            status: self.status.clone(),
            last_discovery: self.last_discovery,
            next_discovery: self.next_discovery,
            last_update: self.last_update,
            next_update: self.next_update,
        };
        let _ = self.snapshots.send(snapshot);
    }

    fn allocate_tid(
        &mut self,
        replace_discovery: bool,
    ) -> io::Result<u16> {
        for _ in 0..=u32::from(u16::MAX) {
            self.next_tid = self.next_tid.wrapping_add(1);
            if self.next_tid == 0 || self.pending.contains_key(&self.next_tid) {
                continue;
            }
            if !replace_discovery && self.discovery_tid == Some(self.next_tid) {
                continue;
            }
            return Ok(self.next_tid);
        }
        Err(io::Error::other(
            "ECHONET Lite transaction ID space is exhausted",
        ))
    }

    fn expire_pending(
        &mut self,
        now: Instant,
    ) {
        self.pending.retain(|_, request| {
            now.saturating_duration_since(request.sent_at()) <= REQUEST_TIMEOUT
        });
    }

    async fn send_discovery(
        &mut self,
        socket: &EchoNetSocket,
    ) -> io::Result<()> {
        let tid = self.allocate_tid(true)?;
        let header = discovery_header(tid);
        let properties = [Property {
            epc: DISCOVERY_EPC,
            edt: EMPTY_EDT,
        }];
        socket.send_frame(header, &properties).await.map(|_| ())?;
        self.discovery_tid = Some(tid);
        self.status = format!("discovery sent (TID 0x{tid:04X})");
        Ok(())
    }

    async fn send_property_map(
        &mut self,
        socket: &EchoNetSocket,
        key: DeviceKey,
    ) -> io::Result<()> {
        let tid = self.allocate_tid(false)?;
        self.pending.insert(
            tid,
            PendingRequest::PropertyMap {
                key,
                sent_at: Instant::now(),
            },
        );
        let properties = [
            Property {
                epc: 0x9D,
                edt: EMPTY_EDT,
            },
            Property {
                epc: 0x9E,
                edt: EMPTY_EDT,
            },
            Property {
                epc: GET_PROPERTY_MAP_EPC,
                edt: EMPTY_EDT,
            },
        ];
        if let Err(error) = socket
            .send_frame_to(get_header(tid, key.eoj), &properties, key.address)
            .await
        {
            self.pending.remove(&tid);
            return Err(error);
        }
        Ok(())
    }

    async fn send_value_batch(
        &mut self,
        socket: &EchoNetSocket,
        key: DeviceKey,
        epcs: Vec<u8>,
    ) -> io::Result<()> {
        let tid = self.allocate_tid(false)?;
        let properties: Vec<_> = epcs
            .iter()
            .copied()
            .map(|epc| Property {
                epc,
                edt: EMPTY_EDT,
            })
            .collect();
        self.pending.insert(
            tid,
            PendingRequest::Values {
                key,
                epcs,
                sent_at: Instant::now(),
            },
        );
        if let Err(error) = socket
            .send_frame_to(get_header(tid, key.eoj), &properties, key.address)
            .await
        {
            self.pending.remove(&tid);
            return Err(error);
        }
        Ok(())
    }

    async fn refresh_values(
        &mut self,
        socket: &EchoNetSocket,
    ) -> io::Result<()> {
        let keys: Vec<_> = self.devices.values().map(|device| device.key).collect();
        for key in keys {
            let Some(device) = self.devices.get(&key.id()) else {
                continue;
            };
            let epcs = device.poll_epcs.clone();
            for batch in epcs.chunks(VALUE_BATCH_SIZE) {
                self.send_value_batch(socket, key, batch.to_vec()).await?;
            }
        }
        Ok(())
    }

    async fn process_properties(
        &mut self,
        socket: &EchoNetSocket,
        header: FrameHeader,
        properties: Vec<(u8, Vec<u8>)>,
        source: SocketAddr,
    ) {
        if self.is_discovery_response(header, &properties) {
            self.process_discovery(socket, properties, source).await;
            return;
        }

        let Some(pending) = self.pending.get(&header.tid).cloned() else {
            return;
        };
        let key = pending.key();
        if source.ip() != key.address.ip() {
            return;
        }
        let Some(pending) = self.pending.remove(&header.tid) else {
            return;
        };
        if !is_get_response_code(header.esv.code()) {
            self.status = format!("device rejected request from {}", source.ip());
            self.publish();
            return;
        }

        let key = pending.key();
        if let Some(device) = self.devices.get_mut(&key.id()) {
            // The answer proves the device is reachable at this source; keep
            // the freshest contact port for subsequent unicast requests.
            device.key = DeviceKey {
                address: source,
                eoj: key.eoj,
            };
        }

        match pending {
            PendingRequest::PropertyMap { key, .. } => {
                self.process_property_map(key, &properties);
            },
            PendingRequest::Values { key, epcs, .. } => {
                self.process_values(key, &epcs, &properties);
            },
        }
        self.publish();
    }

    fn is_discovery_response(
        &self,
        header: FrameHeader,
        properties: &[(u8, Vec<u8>)],
    ) -> bool {
        self.discovery_tid == Some(header.tid)
            && header.esv.code() == GET_RESPONSE_ESV_CODE
            && header.seoj.class_code() == NODE_PROFILE_CLASS_CODE
            && properties.iter().any(|(epc, _)| *epc == DISCOVERY_EPC)
    }

    async fn process_discovery(
        &mut self,
        socket: &EchoNetSocket,
        properties: Vec<(u8, Vec<u8>)>,
        source: SocketAddr,
    ) {
        let Some((_, edt)) = properties.iter().find(|(epc, _)| *epc == DISCOVERY_EPC) else {
            return;
        };
        let instances = match parse_instance_list(edt) {
            Ok(instances) => instances,
            Err(error) => {
                self.status = format!("invalid discovery response: {error}");
                self.publish();
                return;
            },
        };
        let now = Instant::now();
        let mut keys = Vec::with_capacity(instances.len());
        for eoj in instances {
            if eoj.class_code() == NODE_PROFILE_CLASS_CODE {
                continue;
            }
            let id = DeviceId {
                ip: source.ip(),
                eoj,
            };
            self.devices
                .entry(id)
                .and_modify(|device| {
                    device.last_seen = now;
                    device.key = DeviceKey {
                        address: source,
                        eoj,
                    };
                })
                .or_insert_with(|| {
                    DeviceState::new(
                        DeviceKey {
                            address: source,
                            eoj,
                        },
                        now,
                    )
                });
            keys.push(eoj);
        }
        self.last_discovery = Some(now);
        self.status = format!("discovered {} device object(s)", keys.len());
        for eoj in keys {
            let id = DeviceId {
                ip: source.ip(),
                eoj,
            };
            let Some(key) = self.devices.get(&id).map(|device| device.key) else {
                continue;
            };
            if !self.has_pending_map(key)
                && let Err(error) = self.send_property_map(socket, key).await
            {
                self.status = format!("property-map request failed: {error}");
            }
        }
        self.publish();
    }

    fn has_pending_map(
        &self,
        key: DeviceKey,
    ) -> bool {
        self.pending.values().any(|request| {
            matches!(request, PendingRequest::PropertyMap { key: pending_key, .. }
                if pending_key.address.ip() == key.address.ip() && pending_key.eoj == key.eoj)
        })
    }

    fn process_property_map(
        &mut self,
        key: DeviceKey,
        properties: &[(u8, Vec<u8>)],
    ) {
        let Some(device) = self.devices.get_mut(&key.id()) else {
            return;
        };
        device.last_seen = Instant::now();
        let Some((_, edt)) = properties
            .iter()
            .find(|(epc, _)| *epc == GET_PROPERTY_MAP_EPC)
        else {
            return;
        };
        let Ok(poll_epcs) = parse_property_map(edt) else {
            return;
        };
        if !poll_epcs.is_empty() {
            device.poll_epcs = poll_epcs;
        }
    }

    fn process_values(
        &mut self,
        key: DeviceKey,
        requested_epcs: &[u8],
        properties: &[(u8, Vec<u8>)],
    ) {
        let Some(device) = self.devices.get_mut(&key.id()) else {
            return;
        };
        let now = Instant::now();
        let class_code = key.eoj.class_code();
        let mut received = false;
        for (epc, edt) in properties {
            if !requested_epcs.contains(epc) || edt.is_empty() {
                // A requested property can come back without data (PDC 0) when
                // the device could not read it, e.g. in a GET response with
                // status. Keep the properties that did return data instead of
                // discarding the whole response.
                continue;
            }
            let name = lookup(class_code, *epc).map_or_else(
                || format!("EPC 0x{epc:02X}"),
                |info| String::from(info.name),
            );
            device.values.insert(
                *epc,
                ValueSnapshot {
                    epc: *epc,
                    name,
                    value: format_value(class_code, *epc, edt),
                    edt: edt.clone(),
                    updated_at: now,
                },
            );
            received = true;
        }
        device.last_seen = now;
        if received {
            device.last_update = Some(now);
        }
    }
}

fn fallback_poll_epcs(class_code: u16) -> Vec<u8> {
    let candidates = [0x80, 0xE0, 0xE1, 0xE7, 0xBB, 0xB0, 0xBE, 0xBA, 0xBF];
    candidates
        .into_iter()
        .filter(|epc| is_pollable(class_code, *epc))
        .collect()
}

fn is_pollable(
    class_code: u16,
    epc: u8,
) -> bool {
    lookup(class_code, epc).is_some_and(|info| {
        matches!(info.get, Access::Required | Access::Optional)
            && matches!(
                info.kind,
                DataKind::Number | DataKind::State | DataKind::Level
            )
    })
}

/// Run the asynchronous discovery and polling service until shutdown.
///
/// The receiver is a Tokio watch channel so a terminal thread can request a
/// clean stop without interrupting a pending UDP receive.
///
/// # Errors
///
/// Returns an I/O error for invalid configuration or receive failures.
/// Individual request-send failures are reported in the snapshot status and
/// the service continues polling.
pub async fn run_service(
    socket: EchoNetSocket,
    config: RadarConfig,
    snapshots: Sender<RadarSnapshot>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    config
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;

    let mut service = ServiceState::new(config, snapshots);
    service.publish();

    let mut discovery_timer = tokio::time::interval(config.discovery_interval);
    discovery_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut update_sleep = Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
        service.next_update,
    )));
    let mut receive_buffer = [0u8; 512];

    loop {
        tokio::select! {
            result = socket.recv(&mut receive_buffer) => {
                let (length, source) = result?;
                if let Ok(frame) = parse(&receive_buffer[..length]) {
                    let header = frame.header();
                    let properties = frame
                        .properties()
                        .map(|property| (property.epc, property.edt.to_vec()))
                        .collect();
                    service.process_properties(&socket, header, properties, source).await;
                }
            }
            _ = discovery_timer.tick() => {
                let now = Instant::now();
                service.expire_pending(now);
                service.next_discovery = now + config.discovery_interval;
                if let Err(error) = service.send_discovery(&socket).await {
                    service.status = format!("discovery send failed: {error}");
                }
                service.publish();
            }
            () = &mut update_sleep => {
                let now = Instant::now();
                service.expire_pending(now);
                service.last_update = Some(now);
                let delay = config.next_update_delay(&mut service.jitter);
                service.next_update = now + delay;
                service.status = format!("polling {} device object(s)", service.devices.len());
                if let Err(error) = service.refresh_values(&socket).await {
                    service.status = format!("value update failed: {error}");
                }
                service.publish();
                update_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::from_std(service.next_update));
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use echonet_lite::frame::write;

    #[test]
    fn default_schedule_matches_radar_defaults() {
        let config = RadarConfig::default();
        assert_eq!(config.discovery_interval, Duration::from_secs(60));
        assert_eq!(config.update_interval, Duration::from_secs(15));
        assert_eq!(config.update_jitter, Duration::from_secs(5));
    }

    #[test]
    fn zero_poll_interval_is_rejected() {
        let config = RadarConfig {
            update_interval: Duration::ZERO,
            ..RadarConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err(ConfigError::ZeroInterval("update_interval"))
        );
    }

    #[test]
    fn expired_requests_do_not_block_future_polls() {
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut service = ServiceState::new(RadarConfig::default(), sender);
        let key = DeviceKey {
            address: "127.0.0.1:3610".parse().unwrap(),
            eoj: Eoj::new(0x00, 0x11, 0x01),
        };
        service.pending.insert(
            1,
            PendingRequest::PropertyMap {
                key,
                sent_at: Instant::now()
                    .checked_sub(REQUEST_TIMEOUT + Duration::from_secs(1))
                    .unwrap_or_else(Instant::now),
            },
        );
        service.expire_pending(Instant::now());
        assert!(service.pending.is_empty());
    }

    #[test]
    fn jitter_is_bounded_and_added_to_base_interval() {
        let config = RadarConfig::default();
        let mut jitter = JitterSource::new(42);
        let delay = config.next_update_delay(&mut jitter);
        assert!(delay >= Duration::from_secs(15));
        assert!(delay <= Duration::from_secs(20));
    }

    #[test]
    fn discovery_header_uses_standard_get_wire_code() {
        let header = discovery_header(0x1234);
        assert_eq!(header.tid, 0x1234);
        assert_eq!(header.seoj, CONTROLLER_EOJ);
        assert_eq!(header.deoj, DISCOVERY_NODE_PROFILE_EOJ);
        assert_eq!(header.esv.code(), GET_ESV_CODE);
    }

    #[test]
    fn discovery_frame_has_standard_wire_layout() {
        let mut buffer = [0u8; 32];
        let length = write(
            discovery_header(1),
            &[Property {
                epc: DISCOVERY_EPC,
                edt: EMPTY_EDT,
            }],
            &mut buffer,
        )
        .unwrap();
        assert_eq!(
            &buffer[..length],
            &[
                0x10, 0x81, 0x00, 0x01, 0x05, 0xFF, 0x01, 0x0E, 0xF0, 0x00, 0x62, 0x01, 0xD6, 0x00
            ]
        );
    }

    #[test]
    fn instance_list_round_trip() {
        let instances = parse_instance_list(&[2, 0x00, 0x11, 0x01, 0x01, 0x30, 0x02]).unwrap();
        assert_eq!(
            instances,
            vec![Eoj::new(0x00, 0x11, 0x01), Eoj::new(0x01, 0x30, 0x02)]
        );
    }

    #[test]
    fn instance_list_rejects_inconsistent_length() {
        assert_eq!(
            parse_instance_list(&[2, 0x00, 0x11, 0x01]),
            Err(ProtocolError::InvalidInstanceListLength)
        );
    }

    #[test]
    fn property_map_supports_compact_and_bitmap_forms() {
        assert_eq!(
            parse_property_map(&[2, 0x80, 0xE0]).unwrap(),
            vec![0x80, 0xE0]
        );

        let mut bitmap = vec![16];
        bitmap.extend_from_slice(&[0xFF, 0xFF]);
        bitmap.extend(std::iter::repeat_n(0, 14));
        assert_eq!(
            parse_property_map(&bitmap).unwrap(),
            (0x80..=0x8F).collect::<Vec<_>>()
        );
    }

    #[test]
    fn property_map_rejects_count_mismatch() {
        assert_eq!(
            parse_property_map(&[2, 0x80]),
            Err(ProtocolError::InvalidPropertyMapLength)
        );
        let mut invalid_bitmap = vec![16];
        invalid_bitmap.extend(std::iter::repeat_n(0, 16));
        assert_eq!(
            parse_property_map(&invalid_bitmap),
            Err(ProtocolError::InvalidPropertyMapCount)
        );
    }

    #[test]
    fn numeric_values_are_decoded_with_units() {
        assert_eq!(format_value(0x0011, 0xE0, &[0x01, 0x04]), "26 Celsius");
        assert_eq!(format_value(0x0012, 0xE0, &[55]), "55 %");
    }

    #[test]
    fn unknown_values_are_kept_as_hex() {
        assert_eq!(format_value(0xFFFF, 0x80, &[0x01, 0xAF]), "01 AF");
    }

    #[tokio::test]
    async fn unknown_class_map_epcs_become_poll_targets() {
        let socket = test_socket().await;
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut service = ServiceState::new(RadarConfig::default(), sender);
        let source: SocketAddr = "127.0.0.1:3610".parse().unwrap();
        // 0x013D (dehumidifier) is not in the generated appendix tables.
        let key = DeviceKey {
            address: source,
            eoj: Eoj::new(0x01, 0x3D, 0x01),
        };

        service.discovery_tid = Some(1);
        service
            .process_discovery(
                &socket,
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x3D, 0x01])],
                source,
            )
            .await;
        let (header, properties) = map_response(
            *service.pending.keys().next().unwrap(),
            key,
            GET_RESPONSE_ESV_CODE,
            vec![3, 0x80, 0xC0, 0xE1],
        );
        service
            .process_properties(&socket, header, properties, source)
            .await;

        let device = service.devices.get(&key.id()).unwrap();
        assert_eq!(device.poll_epcs, [0x80, 0xC0, 0xE1]);
    }

    #[test]
    fn is_get_response_code_accepts_normal_and_status_responses() {
        assert!(is_get_response_code(GET_RESPONSE_ESV_CODE));
        assert!(is_get_response_code(GET_RESPONSE_WITH_STATUS_ESV_CODE));
        for code in [0x50, 0x51, 0x53, 0x71, 0x73] {
            assert!(
                !is_get_response_code(code),
                "unexpected acceptance of 0x{code:02X}"
            );
        }
    }

    async fn test_socket() -> EchoNetSocket {
        EchoNetSocket::bind_multicast(
            SocketAddr::from((echonet_lite_udp::MULTICAST_GROUP, 0)),
            Ipv4Addr::LOCALHOST,
        )
        .await
        .unwrap()
    }

    fn air_conditioner_key(source: SocketAddr) -> DeviceKey {
        DeviceKey {
            address: source,
            eoj: Eoj::new(0x01, 0x30, 0x01),
        }
    }

    fn map_response(
        tid: u16,
        key: DeviceKey,
        esv: u8,
        property_map: Vec<u8>,
    ) -> (FrameHeader, Vec<(u8, Vec<u8>)>) {
        (
            FrameHeader {
                tid,
                seoj: key.eoj,
                deoj: CONTROLLER_EOJ,
                esv: Esv::from_code(esv),
            },
            vec![
                (0x9D, property_map.clone()),
                (0x9E, property_map.clone()),
                (0x9F, property_map),
            ],
        )
    }

    #[tokio::test]
    async fn devices_deduplicate_across_source_ports() {
        let socket = test_socket().await;
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut service = ServiceState::new(RadarConfig::default(), sender);
        let instance_list = vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])];

        // The same device object answering the same discovery from two ports
        // must collapse into one row.
        let first: SocketAddr = "192.0.2.7:3610".parse().unwrap();
        let second: SocketAddr = "192.0.2.7:40000".parse().unwrap();
        service.discovery_tid = Some(1);
        service
            .process_discovery(&socket, instance_list.clone(), first)
            .await;
        service.discovery_tid = Some(2);
        service
            .process_discovery(&socket, instance_list, second)
            .await;

        assert_eq!(service.devices.len(), 1);
        let device = service.devices.values().next().unwrap();
        assert_eq!(device.key.address.ip(), first.ip());
        // The freshest source port is kept for unicast requests.
        assert_eq!(device.key.address.port(), 40000);
    }

    #[tokio::test]
    async fn devices_with_distinct_ip_or_eoj_stay_separate() {
        let socket = test_socket().await;
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut service = ServiceState::new(RadarConfig::default(), sender);
        service.discovery_tid = Some(1);

        service
            .process_discovery(
                &socket,
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])],
                "192.0.2.7:3610".parse().unwrap(),
            )
            .await;
        service
            .process_discovery(
                &socket,
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])],
                "192.0.2.8:3610".parse().unwrap(),
            )
            .await;
        service
            .process_discovery(
                &socket,
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x33, 0x01])],
                "192.0.2.7:3610".parse().unwrap(),
            )
            .await;

        assert_eq!(service.devices.len(), 3);
    }

    #[tokio::test]
    async fn response_from_new_port_updates_contact_address() {
        let socket = test_socket().await;
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut service = ServiceState::new(RadarConfig::default(), sender);
        let source: SocketAddr = "127.0.0.1:3610".parse().unwrap();
        let key = air_conditioner_key(source);

        service.discovery_tid = Some(1);
        service
            .process_discovery(
                &socket,
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])],
                source,
            )
            .await;
        let tid = *service.pending.keys().next().unwrap();
        let (header, properties) =
            map_response(tid, key, GET_RESPONSE_ESV_CODE, vec![2, 0x80, 0xB0]);

        // The map answer comes back from a different source port.
        let answer_from: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        service
            .process_properties(&socket, header, properties, answer_from)
            .await;

        let device = service.devices.get(&key.id()).unwrap();
        assert_eq!(device.key.address.port(), 40000);
    }

    #[tokio::test]
    async fn property_map_with_status_is_parsed() {
        let socket = test_socket().await;
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut service = ServiceState::new(RadarConfig::default(), sender);
        let source: SocketAddr = "127.0.0.1:3610".parse().unwrap();
        let key = air_conditioner_key(source);

        service.discovery_tid = Some(1);
        service
            .process_discovery(
                &socket,
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])],
                source,
            )
            .await;
        let tid = *service.pending.keys().next().unwrap();

        // The device answers with GET response with status (0x52): the 0x9D and
        // 0x9E map EPCs come back empty, but 0x9F still carries the map.
        let (header, properties) = map_response(
            tid,
            key,
            GET_RESPONSE_WITH_STATUS_ESV_CODE,
            vec![6, 0x80, 0xBB, 0xB0, 0xBE, 0xBA, 0xBF],
        );
        service
            .process_properties(&socket, header, properties, source)
            .await;

        let device = service.devices.get(&key.id()).unwrap();
        assert_eq!(device.poll_epcs, [0x80, 0xBB, 0xB0, 0xBE, 0xBA, 0xBF]);
        assert!(service.pending.is_empty());
    }

    #[tokio::test]
    async fn normal_response_stores_all_requested_values() {
        let socket = test_socket().await;
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut service = ServiceState::new(RadarConfig::default(), sender);
        let source: SocketAddr = "127.0.0.1:3610".parse().unwrap();
        let key = air_conditioner_key(source);

        service.discovery_tid = Some(1);
        service
            .process_discovery(
                &socket,
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])],
                source,
            )
            .await;
        let (header, properties) = map_response(
            *service.pending.keys().next().unwrap(),
            key,
            GET_RESPONSE_ESV_CODE,
            vec![3, 0x80, 0xBB, 0xB0],
        );
        service
            .process_properties(&socket, header, properties, source)
            .await;

        service.refresh_values(&socket).await.unwrap();
        let tid = *service.pending.keys().next().unwrap();
        service
            .process_properties(
                &socket,
                FrameHeader {
                    tid,
                    seoj: key.eoj,
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(GET_RESPONSE_ESV_CODE),
                },
                vec![
                    (0x80, vec![0x30]),
                    (0xBB, vec![0x01, 0x1E]),
                    (0xB0, vec![0x41]),
                ],
                source,
            )
            .await;

        let device = service.devices.get(&key.id()).unwrap();
        assert_eq!(device.values.len(), 3);
        assert_eq!(device.values.get(&0x80).unwrap().value, "true");
        assert_eq!(device.values.get(&0xB0).unwrap().value, "auto");
        // 0xBB is declared Raw in the property tables, so it is kept as hex.
        assert_eq!(device.values.get(&0xBB).unwrap().value, "01 1E");
        assert!(device.last_update.is_some());
    }

    #[tokio::test]
    async fn response_with_status_keeps_readable_values() {
        let socket = test_socket().await;
        let (sender, _receiver) = std::sync::mpsc::channel();
        let mut service = ServiceState::new(RadarConfig::default(), sender);
        let source: SocketAddr = "127.0.0.1:3610".parse().unwrap();
        let key = air_conditioner_key(source);

        service.discovery_tid = Some(1);
        service
            .process_discovery(
                &socket,
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])],
                source,
            )
            .await;
        let (header, properties) = map_response(
            *service.pending.keys().next().unwrap(),
            key,
            GET_RESPONSE_ESV_CODE,
            vec![6, 0x80, 0xBB, 0xB0, 0xBE, 0xBA, 0xBF],
        );
        service
            .process_properties(&socket, header, properties, source)
            .await;

        service.refresh_values(&socket).await.unwrap();
        let tid = *service.pending.keys().next().unwrap();

        // One property is readable, the rest fail; the readable value must not
        // be discarded with the failures.
        service
            .process_properties(
                &socket,
                FrameHeader {
                    tid,
                    seoj: key.eoj,
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(GET_RESPONSE_WITH_STATUS_ESV_CODE),
                },
                vec![
                    (0x80, vec![0x30]),
                    (0xBB, Vec::new()),
                    (0xB0, Vec::new()),
                    (0xBE, Vec::new()),
                    (0xBA, Vec::new()),
                    (0xBF, Vec::new()),
                ],
                source,
            )
            .await;

        let device = service.devices.get(&key.id()).unwrap();
        assert_eq!(device.values.len(), 1);
        assert_eq!(device.values.get(&0x80).unwrap().value, "true");
        assert!(service.pending.is_empty());
    }
}
