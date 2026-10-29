//! The discovery, polling and INF-watching state behind the service loop.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use echonet_lite::ecodec::{Access, lookup};
use echonet_lite::frame::{Eoj, FrameHeader, Property};
use echonet_lite_udp::EchoNetSocket;

use crate::format::{format_edt, power_epc};
use crate::model::{ObjectKey, Observation, RadarEvent, RadarModel, SessionStatus};
use crate::protocol::{
    CONTROLLER_CLASS_CODE, DISCOVERY_EPC, EMPTY_EDT, GET_PROPERTY_MAP_EPC, GET_RESPONSE_ESV_CODE,
    INF_ESV_CODE, NODE_PROFILE_CLASS_CODE, discovery_header, get_header, is_get_response_code,
    parse_instance_list, parse_property_map,
};
use crate::session::SessionConfig;

const VALUE_BATCH_SIZE: usize = 8;

/// A command sent from a shell to the radar service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Send value GETs to all known device objects immediately.
    PollNow,
    /// Send a discovery request without moving the scheduled round.
    Rediscover,
}

#[derive(Debug, Clone)]
enum PendingRequest {
    PropertyMap {
        key: ObjectKey,
        sent_at: Instant,
    },
    Values {
        key: ObjectKey,
        epcs: Vec<u8>,
        sent_at: Instant,
    },
}

impl PendingRequest {
    const fn key(&self) -> ObjectKey {
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

pub struct ServiceState {
    pub model: RadarModel,
    pub poll_epcs: HashMap<ObjectKey, Vec<u8>>,
    pub next_update: Instant,
    events: Option<Sender<RadarEvent>>,
    pending: HashMap<u16, PendingRequest>,
    next_tid: u16,
    first_refresh_done: bool,
    request_timeout: Duration,
    dirty: bool,
}

impl ServiceState {
    pub fn new(
        config: SessionConfig,
        events: Option<Sender<RadarEvent>>,
        session: u64,
    ) -> Self {
        let now = Instant::now();
        let mut model = RadarModel::new(session);
        model.set_status(SessionStatus::Running {
            since: SystemTime::now(),
        });
        Self {
            model,
            poll_epcs: HashMap::new(),
            next_update: now + config.update_interval,
            events,
            pending: HashMap::new(),
            next_tid: 0,
            first_refresh_done: false,
            request_timeout: config.request_timeout,
            dirty: false,
        }
    }

    pub fn notice(
        &mut self,
        message: String,
    ) {
        let projection = self.model.observe(Observation::Status(message));
        self.dirty = true;
        if let Some(event) = projection.event {
            self.send(event);
        }
    }

    /// Count a datagram the service accepted, before parsing it.
    pub const fn record_frame(
        &mut self,
        at: SystemTime,
    ) {
        self.model.record_frame(at);
        self.dirty = true;
    }

    /// Whether anything changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Expire unanswered requests, then send one discovery GET.
    pub async fn discover(
        &mut self,
        socket: &EchoNetSocket,
    ) {
        self.expire_pending(Instant::now());
        if let Err(error) = self.send_discovery(socket).await {
            self.notice(format!("discovery send failed: {error}"));
        }
    }

    fn send(
        &self,
        event: RadarEvent,
    ) {
        if let Some(events) = &self.events {
            let _ = events.send(event);
        }
    }

    fn fold(
        &mut self,
        observation: Observation,
    ) {
        let projection = self.model.observe(observation);
        self.dirty |= projection.dirty;
        if let Some(event) = projection.event {
            self.send(event);
        }
    }

    fn allocate_tid(&mut self) -> io::Result<u16> {
        for _ in 0..=u32::from(u16::MAX) {
            self.next_tid = self.next_tid.wrapping_add(1);
            if self.next_tid == 0 || self.pending.contains_key(&self.next_tid) {
                continue;
            }
            return Ok(self.next_tid);
        }
        Err(io::Error::other(
            "ECHONET Lite transaction ID space is exhausted",
        ))
    }

    pub fn expire_pending(
        &mut self,
        now: Instant,
    ) {
        self.pending.retain(|_, request| {
            now.saturating_duration_since(request.sent_at()) <= self.request_timeout
        });
    }

    async fn send_discovery(
        &mut self,
        socket: &EchoNetSocket,
    ) -> io::Result<()> {
        let tid = self.allocate_tid()?;
        let header = discovery_header(tid);
        let properties = [Property {
            epc: DISCOVERY_EPC,
            edt: EMPTY_EDT,
        }];
        socket.send_frame(header, &properties).await.map(|_| ())
    }

    async fn send_property_map(
        &mut self,
        socket: &EchoNetSocket,
        key: ObjectKey,
    ) -> io::Result<()> {
        let tid = self.allocate_tid()?;
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
            .send_frame_to(get_header(tid, key.eoj), &properties, key.listen_address())
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
        key: ObjectKey,
        epcs: Vec<u8>,
    ) -> io::Result<()> {
        let tid = self.allocate_tid()?;
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
            .send_frame_to(get_header(tid, key.eoj), &properties, key.listen_address())
            .await
        {
            self.pending.remove(&tid);
            return Err(error);
        }
        Ok(())
    }

    pub async fn refresh_values(
        &mut self,
        socket: &EchoNetSocket,
    ) -> io::Result<()> {
        let keys: Vec<ObjectKey> = self.poll_epcs.keys().copied().collect();
        // A transient send failure for one device must not skip the rest of
        // the round; report the first error and keep polling the others.
        let mut first_error = None;
        for key in keys {
            let Some(epcs) = self.poll_epcs.get(&key).cloned() else {
                continue;
            };
            for batch in epcs.chunks(VALUE_BATCH_SIZE) {
                if let Err(error) = self.send_value_batch(socket, key, batch.to_vec()).await {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    break;
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    pub async fn process_frame(
        &mut self,
        socket: &EchoNetSocket,
        header: FrameHeader,
        properties: Vec<(u8, Vec<u8>)>,
        source: SocketAddr,
    ) {
        if header.esv.code() == INF_ESV_CODE {
            self.process_inf(socket, header, properties, source).await;
            return;
        }
        if Self::is_discovery_response(header, &properties) {
            self.process_discovery(socket, properties, source).await;
            return;
        }

        let Some(pending) = self.pending.get(&header.tid).cloned() else {
            return;
        };
        let key = pending.key();
        if source.ip() != IpAddr::V4(key.ip) {
            return;
        }
        let Some(pending) = self.pending.remove(&header.tid) else {
            return;
        };
        if !is_get_response_code(header.esv.code()) {
            self.notice(format!("device rejected request from {}", source.ip()));
            return;
        }

        match pending {
            PendingRequest::PropertyMap { key, .. } => {
                self.process_property_map(key, &properties);
            },
            PendingRequest::Values { key, epcs, .. } => {
                self.process_values(key, &epcs, &properties, source);
            },
        }
    }

    /// Whether a frame is a node-profile instance list. Matching is by shape
    /// rather than transaction ID so responses that arrive after a newer
    /// discovery round (slow devices are common on the wire) still register.
    fn is_discovery_response(
        header: FrameHeader,
        properties: &[(u8, Vec<u8>)],
    ) -> bool {
        header.esv.code() == GET_RESPONSE_ESV_CODE
            && header.seoj.class_code() == NODE_PROFILE_CLASS_CODE
            && properties.iter().any(|(epc, _)| *epc == DISCOVERY_EPC)
    }

    /// Track a device object for polling if it is not known yet, announcing it
    /// as [`RadarEvent::Device`]. Returns whether the object was new.
    fn register_device(
        &mut self,
        source: SocketAddr,
        eoj: Eoj,
    ) -> bool {
        if !is_pollable_object(eoj) {
            return false;
        }
        let Some(key) = object_key(source, eoj) else {
            return false;
        };
        let is_new = match self.poll_epcs.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(fallback_poll_epcs(eoj.class_code()));
                true
            },
            std::collections::hash_map::Entry::Occupied(_) => false,
        };
        if is_new {
            self.fold(Observation::Device {
                key,
                class_code: eoj.class_code(),
                source,
            });
        }
        is_new
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
                self.notice(format!("invalid discovery response: {error}"));
                return;
            },
        };
        let pollable = instances
            .iter()
            .filter(|eoj| is_pollable_object(**eoj))
            .count();
        if pollable == 0 {
            // A node that announces only its node profile and controller
            // instances still proves it answered discovery: a controller-only
            // node, such as a controller under development, would otherwise
            // read as "no devices".
            self.notice(format!(
                "{} answered discovery with {} instance(s) and no device object",
                source.ip(),
                instances.len()
            ));
            return;
        }
        let mut count = 0;
        for eoj in instances {
            if !is_pollable_object(eoj) {
                continue;
            }
            if self.register_device(source, eoj) {
                count += 1;
            }
            let Some(key) = object_key(source, eoj) else {
                continue;
            };
            if !self.has_pending_map(key) {
                self.send_property_map(socket, key)
                    .await
                    .unwrap_or_else(|error| {
                        self.notice(format!("property-map request failed: {error}"));
                    });
            }
        }
        // On startup, Get values immediately after the first discovery finds
        // devices rather than waiting for the first 15s poll round.
        if count > 0 && !self.first_refresh_done {
            self.first_refresh_done = true;
            self.refresh_values(socket).await.unwrap_or_else(|error| {
                self.notice(format!("initial value update failed: {error}"));
            });
        }
        self.notice(format!("discovered {count} new device object(s)"));
    }

    fn has_pending_map(
        &self,
        key: ObjectKey,
    ) -> bool {
        self.pending.values().any(|request| {
            matches!(request, PendingRequest::PropertyMap { key: pending_key, .. }
                if *pending_key == key)
        })
    }

    fn process_property_map(
        &mut self,
        key: ObjectKey,
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
        let mut poll_epcs: Vec<u8> = parsed
            .into_iter()
            .filter(|epc| can_get(class_code, *epc))
            .collect();
        // The distribution board answers 0xC6 but omits it from its 0x9F map.
        if let Some(power) = power_epc(class_code)
            && can_get(class_code, power)
            && !poll_epcs.contains(&power)
        {
            poll_epcs.push(power);
        }
        if !poll_epcs.is_empty() {
            self.poll_epcs.insert(key, poll_epcs);
        }
    }

    fn process_values(
        &mut self,
        key: ObjectKey,
        requested_epcs: &[u8],
        properties: &[(u8, Vec<u8>)],
        source: SocketAddr,
    ) {
        self.send_seen(key, source);
        for (epc, edt) in properties {
            if !requested_epcs.contains(epc) || edt.is_empty() {
                // A requested property can come back without data (PDC 0) when
                // the device could not read it, e.g. in a GET response with
                // status. Keep the properties that did return data instead of
                // discarding the whole response.
                continue;
            }
            self.record_value(key, *epc, edt, source);
        }
    }

    async fn process_inf(
        &mut self,
        socket: &EchoNetSocket,
        header: FrameHeader,
        properties: Vec<(u8, Vec<u8>)>,
        source: SocketAddr,
    ) {
        if !is_pollable_object(header.seoj) {
            return;
        }
        let Some(key) = object_key(source, header.seoj) else {
            return;
        };
        // Track INF-sourced devices so later polling keeps watching them, and
        // request their property map so polling uses the real Get-able set
        // rather than the fallback list.
        let is_new = self.register_device(source, header.seoj);
        if is_new
            && !self.has_pending_map(key)
            && let Err(error) = self.send_property_map(socket, key).await
        {
            self.notice(format!("property-map request failed: {error}"));
        }
        let class_code = header.seoj.class_code();
        for (epc, edt) in properties {
            // Draw a state change only for values the class may announce via INF.
            if can_inf(class_code, epc) && !edt.is_empty() {
                self.record_value(key, epc, &edt, source);
            }
        }
        self.send_seen(key, source);
    }

    /// Record a property EDT, emitting a [`RadarEvent::Change`] when it differs
    /// from the last known value for the device object.
    fn record_value(
        &mut self,
        key: ObjectKey,
        epc: u8,
        edt: &[u8],
        source: SocketAddr,
    ) {
        self.fold(Observation::Value {
            key,
            epc,
            raw_edt: edt.to_vec(),
            text: format_edt(key.eoj.class_code(), epc, edt),
            at: SystemTime::now(),
            source,
        });
    }

    fn send_seen(
        &mut self,
        key: ObjectKey,
        source: SocketAddr,
    ) {
        self.fold(Observation::Seen {
            key,
            at: SystemTime::now(),
            source,
        });
    }
}

/// Whether an EOJ is an object this radar polls.
///
/// A node announces its node profile, and a node that drives other objects
/// announces a controller instance beside the appliances it controls. A
/// controller's own properties are its identity and the count and index of the
/// devices it drives, so they are not appliance state this radar watches; on
/// this LAN the controller instances of two different controllers came back in
/// the discovery list. Controllers also send from the same EOJ this radar uses
/// ([`CONTROLLER_EOJ`]), which makes one of them easy to mistake for our own
/// traffic in a passive log.
const fn is_pollable_object(eoj: Eoj) -> bool {
    !matches!(
        eoj.class_code(),
        NODE_PROFILE_CLASS_CODE | CONTROLLER_CLASS_CODE
    )
}

const fn object_key(
    source: SocketAddr,
    eoj: Eoj,
) -> Option<ObjectKey> {
    match source.ip() {
        IpAddr::V4(ip) => Some(ObjectKey { ip, eoj }),
        IpAddr::V6(_) => None,
    }
}

fn fallback_poll_epcs(class_code: u16) -> Vec<u8> {
    let mut candidates = vec![0x80, 0xE0, 0xE1, 0xE7, 0xBB, 0xB0, 0xBE, 0xBA, 0xBF];
    candidates.extend(power_epc(class_code));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{CONTROLLER_EOJ, DISCOVERY_NODE_PROFILE_EOJ};
    use echonet_lite::frame::Esv;
    use std::net::Ipv4Addr;
    use std::sync::mpsc::{self, TryRecvError};

    fn key(
        ip: &str,
        class_group: u8,
        class: u8,
        instance: u8,
    ) -> ObjectKey {
        ObjectKey {
            ip: ip.parse().unwrap(),
            eoj: Eoj::new(class_group, class, instance),
        }
    }

    fn object(
        source: SocketAddr,
        eoj: Eoj,
    ) -> ObjectKey {
        let IpAddr::V4(ip) = source.ip() else {
            panic!("expected an IPv4 source");
        };
        ObjectKey { ip, eoj }
    }

    fn air_conditioner(source: SocketAddr) -> ObjectKey {
        object(source, Eoj::new(0x01, 0x30, 0x01))
    }

    fn service() -> (ServiceState, mpsc::Receiver<RadarEvent>) {
        let (sender, receiver) = mpsc::channel();
        let service = ServiceState::new(SessionConfig::desktop(), Some(sender), 1);
        (service, receiver)
    }

    const fn is_change(event: &RadarEvent) -> bool {
        matches!(event, RadarEvent::Change(_))
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
                    .checked_sub(SessionConfig::desktop().request_timeout + Duration::from_secs(1))
                    .unwrap_or_else(Instant::now),
            },
        );
        service.expire_pending(Instant::now());
        assert!(service.pending.is_empty());
    }

    #[test]
    fn record_value_emits_change_only_when_edt_differs() {
        let (mut service, receiver) = service();
        let key = air_conditioner("127.0.0.1:3610".parse().unwrap());

        // First observation is a change.
        service.record_value(key, 0x80, &[0x30], key.listen_address());
        let RadarEvent::Change(first) = receiver.recv().unwrap() else {
            panic!("expected a change event");
        };
        assert_eq!(first.epc, 0x80);
        assert_eq!(first.text, "Operation status ON");

        // An identical value is not reported again.
        service.record_value(key, 0x80, &[0x30], key.listen_address());
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

        // A different value is reported.
        service.record_value(key, 0x80, &[0x31], key.listen_address());
        let RadarEvent::Change(second) = receiver.recv().unwrap() else {
            panic!("expected a change event");
        };
        assert_eq!(second.text, "Operation status OFF");
    }

    #[test]
    fn record_value_is_per_epc() {
        let (mut service, receiver) = service();
        let key = air_conditioner("127.0.0.1:3610".parse().unwrap());

        service.record_value(key, 0x80, &[0x30], key.listen_address());
        service.record_value(key, 0xBB, &[0x01, 0x1E], key.listen_address());
        assert!(is_change(&receiver.recv().unwrap()));
        assert!(is_change(&receiver.recv().unwrap()));

        // Changing one EPC does not re-report the other.
        service.record_value(key, 0x80, &[0x31], key.listen_address());
        let RadarEvent::Change(changed) = receiver.recv().unwrap() else {
            panic!("expected a change event");
        };
        assert_eq!(changed.epc, 0x80);
    }

    #[test]
    fn change_event_carries_the_raw_edt() {
        let (mut service, receiver) = service();
        let key = air_conditioner("127.0.0.1:3610".parse().unwrap());

        service.record_value(key, 0x80, &[0x30], key.listen_address());
        let RadarEvent::Change(change) = receiver.recv().unwrap() else {
            panic!("expected a change event");
        };
        assert_eq!(change.raw_edt, vec![0x30]);
        assert_eq!(change.text, "Operation status ON");
    }

    #[test]
    fn seen_is_emitted_for_an_unchanged_value() {
        let (mut service, receiver) = service();
        let key = air_conditioner("127.0.0.1:3610".parse().unwrap());

        service.process_values(key, &[0x80], &[(0x80, vec![0x30])], key.listen_address());
        assert!(matches!(receiver.recv().unwrap(), RadarEvent::Seen(_)));
        assert!(is_change(&receiver.recv().unwrap()));

        service.process_values(key, &[0x80], &[(0x80, vec![0x30])], key.listen_address());
        assert!(matches!(receiver.recv().unwrap(), RadarEvent::Seen(_)));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
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

        let RadarEvent::Device(device) = receiver.recv().unwrap() else {
            panic!("expected a device event");
        };
        assert_eq!(device.source.ip(), source.ip());
        assert_eq!(device.eoj, Eoj::new(0x01, 0x30, 0x01));
        let RadarEvent::Change(change) = receiver.recv().unwrap() else {
            panic!("expected a change event");
        };
        assert_eq!(change.source.ip(), source.ip());
        assert_eq!(change.eoj, Eoj::new(0x01, 0x30, 0x01));
        assert_eq!(change.epc, 0x80);
        assert_eq!(change.text, "Operation status ON");
        // INF-sourced devices become polling targets.
        assert!(service.poll_epcs.contains_key(&air_conditioner(source)));
    }

    #[tokio::test]
    async fn poll_response_records_changes_per_device() {
        let socket = test_socket().await;
        let (mut service, receiver) = service();
        let source: SocketAddr = "127.0.0.1:3610".parse().unwrap();
        let key = air_conditioner(source);

        // Discover the device object.
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
        let source: SocketAddr = "127.0.0.1:3610".parse().unwrap();

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

    #[tokio::test]
    async fn inf_from_new_device_requests_property_map() {
        let socket = test_socket().await;
        let (mut service, _receiver) = service();
        let source: SocketAddr = "127.0.0.1:3610".parse().unwrap();

        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: 1,
                    seoj: Eoj::new(0x01, 0x30, 0x01),
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(INF_ESV_CODE),
                },
                vec![(0x80, vec![0x30])],
                source,
            )
            .await;

        // Otherwise polling keeps the fallback list rather than the device's
        // Get-able set.
        assert!(
            service
                .pending
                .values()
                .any(|request| matches!(request, PendingRequest::PropertyMap { .. }))
        );
    }

    #[test]
    fn property_map_is_filtered_to_get_able_values() {
        let (mut service, _receiver) = service();
        let key = air_conditioner("127.0.0.1:3610".parse().unwrap());
        // 0x80 is Get-able; 0xD0 (Buzzer) is Set-only and not readable.
        service.process_property_map(key, &[(0x9F, vec![2, 0x80, 0xD0])]);
        assert_eq!(
            service.poll_epcs.get(&key).map(Vec::as_slice),
            Some(&[0x80][..])
        );
    }

    #[test]
    fn unlisted_power_epc_is_polled() {
        let (mut service, _receiver) = service();
        let key = key("127.0.0.1", 0x02, 0x87, 0x01);
        assert!(fallback_poll_epcs(0x0287).contains(&0xC6));
        service.process_property_map(key, &[(0x9F, vec![2, 0xB0, 0xB1])]);
        assert!(
            service
                .poll_epcs
                .get(&key)
                .is_some_and(|epcs| epcs.contains(&0xC6))
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
        assert!(matches!(receiver.recv().unwrap(), RadarEvent::Device(_)));
        assert!(matches!(receiver.recv().unwrap(), RadarEvent::Seen(_)));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn requests_go_to_the_standard_port_not_the_sender_port() {
        // The air conditioner on this LAN answers from an ephemeral port; it
        // listens on the standard one, where requests must go.
        let key = object(
            "192.0.2.1:49153".parse().unwrap(),
            Eoj::new(0x01, 0x30, 0x01),
        );
        assert_eq!(key.listen_address(), "192.0.2.1:3610".parse().unwrap());

        let standard = object(
            "192.0.2.2:3610".parse().unwrap(),
            Eoj::new(0x02, 0x6B, 0x01),
        );
        assert_eq!(standard.listen_address(), "192.0.2.2:3610".parse().unwrap());
    }

    #[tokio::test]
    async fn a_controller_only_discovery_answer_is_reported() {
        let socket = test_socket().await;
        let (mut service, receiver) = service();
        let source: SocketAddr = "192.0.2.13:3610".parse().unwrap();

        // The Remo E and the controller under development both announce a
        // single controller instance and no appliance.
        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: 1,
                    seoj: Eoj::new(0x0E, 0xF0, 0x00),
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(GET_RESPONSE_ESV_CODE),
                },
                vec![(DISCOVERY_EPC, vec![1, 0x05, 0xFF, 0x01])],
                source,
            )
            .await;

        let statuses: Vec<String> = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter_map(|event| match event {
                RadarEvent::Status(status) => Some(status),
                _ => None,
            })
            .collect();
        assert_eq!(
            statuses,
            vec![String::from(
                "192.0.2.13 answered discovery with 1 instance(s) and no device object"
            )]
        );
        assert!(service.poll_epcs.is_empty());
    }

    #[tokio::test]
    async fn late_discovery_response_still_registers_devices() {
        let socket = test_socket().await;
        let (mut service, receiver) = service();
        let source: SocketAddr = "192.0.2.9:3610".parse().unwrap();

        // A reply shaped like an instance list but with a TID this service
        // never allocated (a slow reply to a long-forgotten round, or another
        // controller's discovery) must still register the device.
        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: 0xBEEF,
                    seoj: Eoj::new(0x0E, 0xF0, 0x00),
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(GET_RESPONSE_ESV_CODE),
                },
                vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])],
                source,
            )
            .await;

        let RadarEvent::Device(device) = receiver.recv().unwrap() else {
            panic!("expected a device event");
        };
        assert_eq!(device.source.ip(), source.ip());
        assert_eq!(device.eoj, Eoj::new(0x01, 0x30, 0x01));
        // Exactly one device announcement; the rest are status updates.
        let extra_devices = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter(|event| matches!(event, RadarEvent::Device(_)))
            .count();
        assert_eq!(extra_devices, 0);
        assert!(service.poll_epcs.contains_key(&air_conditioner(source)));
    }

    #[tokio::test]
    async fn controller_and_node_profile_instances_are_not_devices() {
        // A controller node announces its node profile and its controller
        // instance beside the appliances it drives; only the appliances hold
        // state this radar polls.
        let socket = test_socket().await;
        let (mut service, receiver) = service();
        let source: SocketAddr = "192.0.2.11:3610".parse().unwrap();

        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: 0xF00D,
                    seoj: DISCOVERY_NODE_PROFILE_EOJ,
                    deoj: CONTROLLER_EOJ,
                    esv: Esv::from_code(GET_RESPONSE_ESV_CODE),
                },
                vec![(
                    DISCOVERY_EPC,
                    vec![
                        4, 0x0E, 0xF0, 0x01, 0x05, 0xFF, 0x01, 0x02, 0x6B, 0x01, 0x01, 0x30, 0x01,
                    ],
                )],
                source,
            )
            .await;

        let announced: Vec<Eoj> = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter_map(|event| match event {
                RadarEvent::Device(device) => Some(device.eoj),
                _ => None,
            })
            .collect();
        assert_eq!(
            announced,
            vec![Eoj::new(0x02, 0x6B, 0x01), Eoj::new(0x01, 0x30, 0x01)]
        );
        assert!(
            !service
                .poll_epcs
                .contains_key(&object(source, Eoj::new(0x05, 0xFF, 0x01)))
        );
        assert_eq!(service.poll_epcs.len(), 2);
    }

    #[tokio::test]
    async fn controller_notification_does_not_register_a_device() {
        let socket = test_socket().await;
        let (mut service, receiver) = service();
        let source: SocketAddr = "192.0.2.12:3610".parse().unwrap();

        service
            .process_frame(
                &socket,
                FrameHeader {
                    tid: 7,
                    seoj: CONTROLLER_EOJ,
                    deoj: Eoj::new(0x02, 0x6B, 0x01),
                    esv: Esv::PropertyNotification,
                },
                vec![(0x80, vec![0x30])],
                source,
            )
            .await;

        assert!(service.poll_epcs.is_empty());
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn rediscovered_devices_are_not_announced_twice() {
        let socket = test_socket().await;
        let (mut service, receiver) = service();
        let source: SocketAddr = "192.0.2.10:3610".parse().unwrap();
        let instance_list = vec![(DISCOVERY_EPC, vec![1, 0x01, 0x30, 0x01])];

        for tid in [1, 2] {
            service
                .process_frame(
                    &socket,
                    FrameHeader {
                        tid,
                        seoj: Eoj::new(0x0E, 0xF0, 0x00),
                        deoj: CONTROLLER_EOJ,
                        esv: Esv::from_code(GET_RESPONSE_ESV_CODE),
                    },
                    instance_list.clone(),
                    source,
                )
                .await;
        }

        let device_events = std::iter::from_fn(|| receiver.try_recv().ok())
            .filter(|event| matches!(event, RadarEvent::Device(_)))
            .count();
        assert_eq!(device_events, 1);
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
