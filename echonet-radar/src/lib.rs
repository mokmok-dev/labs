//! ECHONET Lite state-change discovery and logging for `echonet-radar`.
//!
//! The service keeps the transport and terminal rendering separate: this module
//! owns protocol state and emits [`RadarEvent`]s, while the binary renders the
//! resulting time-series feed with ratatui.
//!
//! The radar performs periodic discovery (every minute) to learn which device
//! objects exist, and periodic value polling (every 15 seconds) to detect state
//! changes. Any property whose raw EDT differs from the last known value is
//! reported as a [`RadarEvent::Change`]. When a device pushes an INF telegram,
//! its properties are processed immediately so the change is rendered without
//! waiting for the next poll round.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use echonet_lite::ecodec::{Access, EdtValue, decode, lookup};
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
/// The ECHONET Lite INF service code: a device pushes a property notification.
pub const INF_ESV_CODE: u8 = 0x63;
/// The standard ECHONET Lite get-property-map EPC.
pub const GET_PROPERTY_MAP_EPC: u8 = 0x9F;
/// The default discovery interval.
pub const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
/// The default interval for value polling.
pub const DEFAULT_UPDATE_INTERVAL: Duration = Duration::from_secs(15);

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
    /// Interval between value polling rounds.
    pub update_interval: Duration,
}

impl Default for RadarConfig {
    fn default() -> Self {
        Self {
            interface: Ipv4Addr::UNSPECIFIED,
            discovery_interval: DEFAULT_DISCOVERY_INTERVAL,
            update_interval: DEFAULT_UPDATE_INTERVAL,
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

/// A discovered device object, identified by its source address and EOJ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceKey {
    /// Address from which the device responded.
    pub address: SocketAddr,
    /// Device object EOJ.
    pub eoj: Eoj,
}

impl DeviceKey {
    /// The identity used to track a device object: source IP and EOJ.
    ///
    /// The port is deliberately excluded so that a device answering from
    /// several ports is treated as a single object.
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

/// A single observed state change on a device object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeEvent {
    /// Wall-clock time at which the change was observed.
    pub at: SystemTime,
    /// Address from which the change was observed.
    pub source: SocketAddr,
    /// The device object that changed.
    pub eoj: Eoj,
    /// The EPC whose EDT changed.
    pub epc: u8,
    /// Human-readable English description of the EDT.
    pub edt: String,
}

/// An event emitted by the radar service for the terminal to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadarEvent {
    /// A property value changed on a device object.
    Change(ChangeEvent),
    /// A status message for the header (discovery, errors, poll rounds).
    Status(String),
}

/// A command sent from the terminal to the radar service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Send value GETs to all known device objects immediately.
    PollNow,
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

/// Build a human-readable English description of a property's EDT, e.g.
/// `"Operation status ON"` or `"Room temperature 26 Celsius"`.
#[must_use]
pub fn format_edt(
    class_code: u16,
    epc: u8,
    edt: &[u8],
) -> String {
    let name = lookup(class_code, epc).map_or_else(
        || format!("EPC 0x{epc:02X}"),
        |info| String::from(info.name),
    );
    format!(
        "{name} {}",
        humanize_value(format_value(class_code, epc, edt))
    )
}

/// Render a boolean state value as ON/OFF so it reads naturally in a log line.
fn humanize_value(value: String) -> String {
    match value.as_str() {
        "true" => String::from("ON"),
        "false" => String::from("OFF"),
        _ => value,
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
    events: Sender<RadarEvent>,
    poll_epcs: HashMap<DeviceId, Vec<u8>>,
    values: HashMap<DeviceId, BTreeMap<u8, Vec<u8>>>,
    source_ports: HashMap<DeviceId, u16>,
    pending: HashMap<u16, PendingRequest>,
    discovery_tid: Option<u16>,
    next_tid: u16,
    first_refresh_done: bool,
    status: String,
    next_update: Instant,
}

impl ServiceState {
    fn new(
        config: RadarConfig,
        events: Sender<RadarEvent>,
    ) -> Self {
        let now = Instant::now();
        Self {
            events,
            poll_epcs: HashMap::new(),
            values: HashMap::new(),
            source_ports: HashMap::new(),
            pending: HashMap::new(),
            discovery_tid: None,
            next_tid: 0,
            first_refresh_done: false,
            status: String::from("waiting for discovery"),
            next_update: now + config.update_interval,
        }
    }

    fn send_status(
        &self,
        message: String,
    ) {
        let _ = self.events.send(RadarEvent::Status(message));
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
        // Refresh each known device object using its freshest source port.
        let keys: Vec<DeviceKey> = self
            .poll_epcs
            .keys()
            .filter_map(|id| {
                self.source_address(*id).map(|address| DeviceKey {
                    address,
                    eoj: id.eoj,
                })
            })
            .collect();
        for key in keys {
            let Some(epcs) = self.poll_epcs.get(&key.id()).cloned() else {
                continue;
            };
            for batch in epcs.chunks(VALUE_BATCH_SIZE) {
                self.send_value_batch(socket, key, batch.to_vec()).await?;
            }
        }
        Ok(())
    }

    /// The freshest known source address for a device object.
    fn source_address(
        &self,
        id: DeviceId,
    ) -> Option<SocketAddr> {
        self.source_ports
            .get(&id)
            .map(|port| SocketAddr::from((id.ip, *port)))
    }

    async fn process_frame(
        &mut self,
        socket: &EchoNetSocket,
        header: FrameHeader,
        properties: Vec<(u8, Vec<u8>)>,
        source: SocketAddr,
    ) {
        if header.esv.code() == INF_ESV_CODE {
            self.process_inf(header, properties, source);
            return;
        }
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
            self.send_status(self.status.clone());
            return;
        }

        let key = pending.key();
        // The answer proves the device is reachable at this source; remember
        // the freshest contact port for subsequent unicast requests.
        self.remember_source(key.address.ip(), key.eoj, source.port());

        match pending {
            PendingRequest::PropertyMap { key, .. } => {
                self.process_property_map(key, &properties);
            },
            PendingRequest::Values { key, epcs, .. } => {
                self.process_values(key, &epcs, &properties);
            },
        }
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
                self.send_status(self.status.clone());
                return;
            },
        };
        let mut count = 0;
        for eoj in instances {
            if eoj.class_code() == NODE_PROFILE_CLASS_CODE {
                continue;
            }
            let id = DeviceId {
                ip: source.ip(),
                eoj,
            };
            self.poll_epcs.entry(id).or_insert_with(|| {
                count += 1;
                fallback_poll_epcs(eoj.class_code())
            });
            self.remember_source(source.ip(), eoj, source.port());
            if !self.has_pending_map(DeviceKey {
                address: source,
                eoj,
            }) {
                self.send_property_map(
                    socket,
                    DeviceKey {
                        address: source,
                        eoj,
                    },
                )
                .await
                .unwrap_or_else(|error| {
                    self.status = format!("property-map request failed: {error}");
                });
            }
        }
        // On startup, Get values immediately after the first discovery finds
        // devices rather than waiting for the first 15s poll round.
        if count > 0 && !self.first_refresh_done {
            self.first_refresh_done = true;
            self.refresh_values(socket).await.unwrap_or_else(|error| {
                self.status = format!("initial value update failed: {error}");
            });
        }
        self.status = format!("discovered {count} new device object(s)");
        self.send_status(self.status.clone());
    }

    fn remember_source(
        &mut self,
        ip: IpAddr,
        eoj: Eoj,
        port: u16,
    ) {
        let id = DeviceId { ip, eoj };
        let entry = self.source_ports.entry(id).or_insert(port);
        *entry = port;
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
        let Some((_, edt)) = properties
            .iter()
            .find(|(epc, _)| *epc == GET_PROPERTY_MAP_EPC)
        else {
            return;
        };
        let Ok(parsed) = parse_property_map(edt) else {
            return;
        };
        // Poll only the properties that can actually be read with a Get.
        let class_code = key.eoj.class_code();
        let poll_epcs: Vec<u8> = parsed
            .into_iter()
            .filter(|epc| can_get(class_code, *epc))
            .collect();
        if !poll_epcs.is_empty() {
            self.poll_epcs.insert(key.id(), poll_epcs);
        }
    }

    fn process_values(
        &mut self,
        key: DeviceKey,
        requested_epcs: &[u8],
        properties: &[(u8, Vec<u8>)],
    ) {
        for (epc, edt) in properties {
            if !requested_epcs.contains(epc) || edt.is_empty() {
                // A requested property can come back without data (PDC 0) when
                // the device could not read it, e.g. in a GET response with
                // status. Keep the properties that did return data instead of
                // discarding the whole response.
                continue;
            }
            self.record_value(key, *epc, edt);
        }
    }

    fn process_inf(
        &mut self,
        header: FrameHeader,
        properties: Vec<(u8, Vec<u8>)>,
        source: SocketAddr,
    ) {
        let key = DeviceKey {
            address: source,
            eoj: header.seoj,
        };
        let id = key.id();
        // Track INF-sourced devices so later polling keeps watching them.
        self.poll_epcs
            .entry(id)
            .or_insert_with(|| fallback_poll_epcs(header.seoj.class_code()));
        self.remember_source(source.ip(), header.seoj, source.port());
        let class_code = header.seoj.class_code();
        for (epc, edt) in properties {
            // Draw a state change only for values the class may announce via INF.
            if can_inf(class_code, epc) && !edt.is_empty() {
                self.record_value(key, epc, &edt);
            }
        }
    }

    /// Record a property EDT, emitting a [`RadarEvent::Change`] when it differs
    /// from the last known value for the device object.
    fn record_value(
        &mut self,
        key: DeviceKey,
        epc: u8,
        edt: &[u8],
    ) {
        let id = key.id();
        let last = self.values.entry(id).or_default().insert(epc, edt.to_vec());
        if last.as_deref() == Some(edt) {
            return;
        }
        let change = ChangeEvent {
            at: SystemTime::now(),
            source: key.address,
            eoj: key.eoj,
            epc,
            edt: format_edt(key.eoj.class_code(), epc, edt),
        };
        let _ = self.events.send(RadarEvent::Change(change));
    }
}

fn fallback_poll_epcs(class_code: u16) -> Vec<u8> {
    let candidates = [0x80, 0xE0, 0xE1, 0xE7, 0xBB, 0xB0, 0xBE, 0xBA, 0xBF];
    candidates
        .into_iter()
        .filter(|epc| can_get(class_code, *epc))
        .collect()
}

/// Whether a property can be read with a Get request per the appendix.
fn can_get(
    class_code: u16,
    epc: u8,
) -> bool {
    lookup(class_code, epc)
        .is_some_and(|info| matches!(info.get, Access::Required | Access::Optional))
}

/// Whether a device may push an INF telegram for a property per the appendix.
fn can_inf(
    class_code: u16,
    epc: u8,
) -> bool {
    lookup(class_code, epc)
        .is_some_and(|info| matches!(info.inf, Access::Required | Access::Optional))
}

/// Run the asynchronous discovery, polling, and INF-watching service until
/// shutdown.
///
/// The receiver is a Tokio watch channel so a terminal thread can request a
/// clean stop without interrupting a pending UDP receive.
///
/// # Errors
///
/// Returns an I/O error for invalid configuration or receive failures.
/// Individual request-send failures are reported as status events and the
/// service continues polling.
pub async fn run_service(
    socket: EchoNetSocket,
    config: RadarConfig,
    events: Sender<RadarEvent>,
    mut commands: tokio::sync::mpsc::Receiver<Command>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> io::Result<()> {
    config
        .validate()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;

    let mut service = ServiceState::new(config, events);
    service.send_status(service.status.clone());

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
                    service.process_frame(&socket, header, properties, source).await;
                }
            }
            _ = discovery_timer.tick() => {
                let now = Instant::now();
                service.expire_pending(now);
                if let Err(error) = service.send_discovery(&socket).await {
                    service.status = format!("discovery send failed: {error}");
                }
                service.send_status(service.status.clone());
            }
            () = &mut update_sleep => {
                let now = Instant::now();
                service.expire_pending(now);
                service.status = format!(
                    "polling {} device object(s)",
                    service.poll_epcs.len()
                );
                if let Err(error) = service.refresh_values(&socket).await {
                    service.status = format!("value update failed: {error}");
                }
                service.next_update = now + config.update_interval;
                service.send_status(service.status.clone());
                update_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::from_std(service.next_update));
            }
            command = commands.recv() => {
                match command {
                    Some(Command::PollNow) => {
                        // A manual poll does not shift the scheduled cadence;
                        // the next automatic round still fires on its timer.
                        let now = Instant::now();
                        service.expire_pending(now);
                        service.status = format!(
                            "manual poll of {} device object(s)",
                            service.poll_epcs.len()
                        );
                        if let Err(error) = service.refresh_values(&socket).await {
                            service.status = format!("manual value update failed: {error}");
                        }
                        service.send_status(service.status.clone());
                    }
                    None => break,
                }
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
    use std::sync::mpsc::{self, TryRecvError};

    fn key(
        ip: &str,
        class_group: u8,
        class: u8,
        instance: u8,
    ) -> DeviceKey {
        DeviceKey {
            address: format!("{ip}:3610").parse().unwrap(),
            eoj: Eoj::new(class_group, class, instance),
        }
    }

    fn air_conditioner(source: SocketAddr) -> DeviceKey {
        DeviceKey {
            address: source,
            eoj: Eoj::new(0x01, 0x30, 0x01),
        }
    }

    fn service() -> (ServiceState, mpsc::Receiver<RadarEvent>) {
        let (sender, receiver) = mpsc::channel();
        let service = ServiceState::new(RadarConfig::default(), sender);
        (service, receiver)
    }

    const fn is_change(event: &RadarEvent) -> bool {
        matches!(event, RadarEvent::Change(_))
    }

    #[test]
    fn default_schedule_matches_radar_defaults() {
        let config = RadarConfig::default();
        assert_eq!(config.discovery_interval, Duration::from_secs(60));
        assert_eq!(config.update_interval, Duration::from_secs(15));
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
        let (mut service, _receiver) = service();
        let key = key("127.0.0.1", 0x00, 0x11, 0x01);
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

    #[test]
    fn format_edt_prepends_the_property_name() {
        // 0x80 on an air conditioner is "Operation status" with state ON/OFF.
        assert_eq!(format_edt(0x0130, 0x80, &[0x30]), "Operation status ON");
        assert_eq!(
            format_edt(0x0130, 0xBB, &[0x01, 0x1E]),
            "Measured value of room temperature 01 1E"
        );
        assert_eq!(format_edt(0xFFFF, 0x80, &[0x01]), "EPC 0x80 01");
    }

    #[test]
    fn record_value_emits_change_only_when_edt_differs() {
        let (mut service, receiver) = service();
        let key = air_conditioner("127.0.0.1:3610".parse().unwrap());

        // First observation is a change.
        service.record_value(key, 0x80, &[0x30]);
        let RadarEvent::Change(first) = receiver.recv().unwrap() else {
            panic!("expected a change event");
        };
        assert_eq!(first.epc, 0x80);
        assert_eq!(first.edt, "Operation status ON");

        // An identical value is not reported again.
        service.record_value(key, 0x80, &[0x30]);
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

        // A different value is reported.
        service.record_value(key, 0x80, &[0x31]);
        let RadarEvent::Change(second) = receiver.recv().unwrap() else {
            panic!("expected a change event");
        };
        assert_eq!(second.edt, "Operation status OFF");
    }

    #[test]
    fn record_value_is_per_epc() {
        let (mut service, receiver) = service();
        let key = air_conditioner("127.0.0.1:3610".parse().unwrap());

        service.record_value(key, 0x80, &[0x30]);
        service.record_value(key, 0xBB, &[0x01, 0x1E]);
        assert!(is_change(&receiver.recv().unwrap()));
        assert!(is_change(&receiver.recv().unwrap()));

        // Changing one EPC does not re-report the other.
        service.record_value(key, 0x80, &[0x31]);
        let RadarEvent::Change(changed) = receiver.recv().unwrap() else {
            panic!("expected a change event");
        };
        assert_eq!(changed.epc, 0x80);
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

    #[tokio::test]
    async fn inf_telegram_emits_change_immediately() {
        let socket = test_socket().await;
        let (mut service, receiver) = service();
        let source: SocketAddr = "192.0.2.7:3610".parse().unwrap();

        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: 7,
                    seoj: Eoj::new(0x01, 0x30, 0x01),
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(INF_ESV_CODE),
                },
                vec![(0x80, vec![0x30])],
                source,
            )
            .await;

        let RadarEvent::Change(change) = receiver.recv().unwrap() else {
            panic!("expected a change event");
        };
        assert_eq!(change.source.ip(), source.ip());
        assert_eq!(change.eoj, Eoj::new(0x01, 0x30, 0x01));
        assert_eq!(change.epc, 0x80);
        assert_eq!(change.edt, "Operation status ON");
        // INF-sourced devices become polling targets.
        let id = DeviceId {
            ip: source.ip(),
            eoj: Eoj::new(0x01, 0x30, 0x01),
        };
        assert!(service.poll_epcs.contains_key(&id));
    }

    #[tokio::test]
    async fn poll_response_records_changes_per_device() {
        let socket = test_socket().await;
        let (mut service, receiver) = service();
        let source: SocketAddr = "127.0.0.1:3610".parse().unwrap();
        let key = air_conditioner(source);

        // Discover the device object.
        service.discovery_tid = Some(1);
        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: 1,
                    seoj: Eoj::new(0x0E, 0xF0, 0x00),
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(GET_RESPONSE_ESV_CODE),
                },
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])],
                source,
            )
            .await;
        // Answer the pending property-map request.
        let map_tid = service
            .pending
            .iter()
            .find_map(|(tid, request)| match request {
                PendingRequest::PropertyMap { .. } => Some(*tid),
                PendingRequest::Values { .. } => None,
            })
            .unwrap();
        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: map_tid,
                    seoj: key.eoj,
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(GET_RESPONSE_ESV_CODE),
                },
                vec![(0x9F, vec![3, 0x80, 0xBB, 0xB0])],
                source,
            )
            .await;
        // The first discovery triggers an immediate value poll; answer it.
        let value_tid = service
            .pending
            .iter()
            .find_map(|(tid, request)| match request {
                PendingRequest::Values { .. } => Some(*tid),
                PendingRequest::PropertyMap { .. } => None,
            })
            .unwrap();
        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: value_tid,
                    seoj: key.eoj,
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(GET_RESPONSE_ESV_CODE),
                },
                vec![(0x80, vec![0x30]), (0xB0, vec![0x41])],
                source,
            )
            .await;

        let mut changes = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let RadarEvent::Change(change) = event {
                changes.push(change.epc);
            }
        }
        assert!(changes.contains(&0x80));
        assert!(changes.contains(&0xB0));
    }

    #[tokio::test]
    async fn first_discovery_polls_values_immediately() {
        let socket = test_socket().await;
        let (mut service, _receiver) = service();
        let source: SocketAddr = "192.0.2.9:3610".parse().unwrap();

        service.discovery_tid = Some(1);
        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: 1,
                    seoj: Eoj::new(0x0E, 0xF0, 0x00),
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(GET_RESPONSE_ESV_CODE),
                },
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])],
                source,
            )
            .await;

        assert!(service.first_refresh_done);
        let sent_value_gets = service
            .pending
            .values()
            .any(|request| matches!(request, PendingRequest::Values { .. }));
        assert!(sent_value_gets);
    }

    #[test]
    fn property_map_is_filtered_to_get_able_values() {
        let (mut service, _receiver) = service();
        let key = air_conditioner("127.0.0.1:3610".parse().unwrap());
        // 0x80 is Get-able; 0xD0 (Buzzer) is Set-only and not readable.
        service.process_property_map(key, &[(0x9F, vec![2, 0x80, 0xD0])]);
        assert_eq!(
            service.poll_epcs.get(&key.id()).map(Vec::as_slice),
            Some(&[0x80][..])
        );
    }

    #[tokio::test]
    async fn inf_for_non_infable_value_is_not_drawn() {
        let socket = test_socket().await;
        let (mut service, receiver) = service();
        let source: SocketAddr = "192.0.2.11:3610".parse().unwrap();
        // 0xD0 (Buzzer) is not announced via INF on an air conditioner.
        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: 1,
                    seoj: Eoj::new(0x01, 0x30, 0x01),
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(INF_ESV_CODE),
                },
                vec![(0xD0, vec![0x41])],
                source,
            )
            .await;
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    async fn test_socket() -> EchoNetSocket {
        EchoNetSocket::bind_multicast(
            SocketAddr::from((echonet_lite_udp::MULTICAST_GROUP, 0)),
            Ipv4Addr::LOCALHOST,
        )
        .await
        .unwrap()
    }
}
