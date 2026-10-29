use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self as std_mpsc, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use echonet_lite::frame::parse;
use echonet_lite_udp::EchoNetSocket;
use tokio::sync::{mpsc, watch};

use crate::model::{
    ConfigError, Fault, RadarEvent, RadarModel, RadarSnapshot, SessionStatus, next_session_id,
};
use crate::protocol::{DEFAULT_DISCOVERY_INTERVAL, DEFAULT_UPDATE_INTERVAL};
use crate::service::{Command, ServiceState};

const COMMAND_CAPACITY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfig {
    pub interface: Ipv4Addr,
    pub discovery_interval: Duration,
    pub update_interval: Duration,
    pub request_timeout: Duration,
    pub snapshot_interval: Duration,
}

impl SessionConfig {
    #[must_use]
    pub const fn desktop() -> Self {
        Self {
            interface: Ipv4Addr::UNSPECIFIED,
            discovery_interval: DEFAULT_DISCOVERY_INTERVAL,
            update_interval: DEFAULT_UPDATE_INTERVAL,
            request_timeout: Duration::from_secs(5),
            snapshot_interval: Duration::from_millis(100),
        }
    }

    /// Validate intervals before constructing Tokio timers.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::ZeroInterval`] for an interval that a timer or the
    /// publish deadline would otherwise schedule at zero.
    pub const fn validate(self) -> Result<(), ConfigError> {
        if self.discovery_interval.is_zero() {
            return Err(ConfigError::ZeroInterval("discovery_interval"));
        }
        if self.update_interval.is_zero() {
            return Err(ConfigError::ZeroInterval("update_interval"));
        }
        if self.snapshot_interval.is_zero() {
            return Err(ConfigError::ZeroInterval("snapshot_interval"));
        }
        Ok(())
    }
}

/// The wake-up a publisher signals after a new snapshot.
///
/// The trait keeps egui out of the core: the app implements it over a repaint
/// request and the terminal uses [`NoopPulse`].
pub trait Pulse: Send + Sync {
    fn pulse(&self);
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopPulse;

impl Pulse for NoopPulse {
    fn pulse(&self) {}
}

pub struct ServiceIo {
    pub events: Option<Sender<RadarEvent>>,
    /// Published snapshots. `None` is for event-only renderers; `send_replace`
    /// makes a receiver-less channel harmless.
    pub snapshots: Option<watch::Sender<Arc<RadarSnapshot>>>,
    pub pulse: Arc<dyn Pulse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    ServiceStopped,
    Busy,
}

impl std::fmt::Display for CommandError {
    fn fmt(
        &self,
        f: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self {
            Self::ServiceStopped => f.write_str("the radar service is not running"),
            Self::Busy => f.write_str("the radar service is still working on the last command"),
        }
    }
}

impl std::error::Error for CommandError {}

/// A running radar session.
///
/// Dropping the session signals shutdown and returns; only [`Self::stop`] waits
/// for the service thread, so a panic path cannot deadlock.
pub struct RadarSession {
    thread: Option<JoinHandle<()>>,
    shutdown: watch::Sender<bool>,
}

impl RadarSession {
    /// Start a session that publishes snapshots only.
    ///
    /// # Errors
    ///
    /// Returns [`Fault::Config`] for invalid intervals, [`Fault::Bind`] when the
    /// multicast socket cannot be bound on the interface, and [`Fault::Service`]
    /// when the service thread or its runtime cannot start.
    pub fn start(
        config: SessionConfig,
        pulse: Arc<dyn Pulse>,
    ) -> Result<(Self, RadarHandle), Fault> {
        Self::spawn(config, None, pulse)
    }

    /// Start a session that also projects events for line-oriented renderers.
    ///
    /// # Errors
    ///
    /// Same as [`Self::start`].
    pub fn start_with_events(
        config: SessionConfig,
        events: Sender<RadarEvent>,
        pulse: Arc<dyn Pulse>,
    ) -> Result<(Self, RadarHandle), Fault> {
        Self::spawn(config, Some(events), pulse)
    }

    #[must_use]
    pub fn alive(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }

    /// Signal shutdown and wait for the service thread to end.
    ///
    /// # Errors
    ///
    /// Returns [`Fault::Service`] when the service thread panicked.
    pub fn stop(mut self) -> Result<(), Fault> {
        self.shutdown.send_replace(true);
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_| Fault::Service(String::from("the service thread panicked")))
    }

    fn spawn(
        config: SessionConfig,
        events: Option<Sender<RadarEvent>>,
        pulse: Arc<dyn Pulse>,
    ) -> Result<(Self, RadarHandle), Fault> {
        let (snapshots, snapshot_rx) = snapshot_channel(next_session_id());
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (ready, ready_rx) = std_mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name(String::from("echonet-radar"))
            .spawn(move || {
                run_session(
                    config,
                    events,
                    pulse,
                    snapshots,
                    command_rx,
                    shutdown_rx,
                    ready,
                );
            })
            .map_err(|error| {
                Fault::Service(format!("could not spawn the service thread: {error}"))
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => {},
            Ok(Err(fault)) => {
                let _ = thread.join();
                return Err(fault);
            },
            Err(_) => {
                let _ = thread.join();
                return Err(Fault::Service(String::from(
                    "the service thread stopped before binding its socket",
                )));
            },
        }
        Ok((
            Self {
                thread: Some(thread),
                shutdown,
            },
            RadarHandle {
                snapshots: snapshot_rx,
                commands,
                seen: AtomicU64::new(0),
            },
        ))
    }
}

impl Drop for RadarSession {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

pub struct RadarHandle {
    snapshots: watch::Receiver<Arc<RadarSnapshot>>,
    commands: mpsc::Sender<Command>,
    seen: AtomicU64,
}

impl RadarHandle {
    /// Whether a snapshot was published since the last [`Self::snapshot`].
    ///
    /// Compares revisions instead of the watch channel's own change flag,
    /// because the channel reports an error as soon as the service drops its
    /// sender. A final publish, such as the `Degraded` or `Stopped` status,
    /// would otherwise be invisible.
    #[must_use]
    pub fn has_changed(&self) -> bool {
        self.snapshots.borrow().revision != self.seen.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn snapshot(&mut self) -> Arc<RadarSnapshot> {
        let snapshot = self.snapshots.borrow_and_update().clone();
        self.seen.store(snapshot.revision, Ordering::Relaxed);
        snapshot
    }

    /// Ask the service to poll values without moving the scheduled round.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError::Busy`] when the queue is full and
    /// [`CommandError::ServiceStopped`] when the service has ended.
    pub fn poll_now(&self) -> Result<(), CommandError> {
        self.send(Command::PollNow)
    }

    /// Ask the service to send a discovery request without moving the scheduled
    /// round.
    ///
    /// # Errors
    ///
    /// Same as [`Self::poll_now`].
    pub fn rediscover(&self) -> Result<(), CommandError> {
        self.send(Command::Rediscover)
    }

    fn send(
        &self,
        command: Command,
    ) -> Result<(), CommandError> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => CommandError::Busy,
                mpsc::error::TrySendError::Closed(_) => CommandError::ServiceStopped,
            })
    }
}

/// Run the asynchronous discovery, polling and INF-watching service until
/// shutdown.
///
/// `io.events` is drained by the caller. The service sends on it inline, so a
/// consumer that cannot keep up must move the receiver to a blocking thread.
///
/// `commands` asks for an immediate value poll or discovery request without
/// moving the scheduled rounds.
///
/// The session id is the one on the snapshot already in `io.snapshots`, which a
/// session publishes before entering this loop.
///
/// The service stops when `shutdown` changes to `true` or when its sender is
/// dropped.
///
/// # Errors
///
/// Returns an I/O error for invalid configuration or receive failures.
/// Individual request-send failures are reported as notices and the service
/// continues polling.
pub async fn run_service(
    socket: EchoNetSocket,
    config: SessionConfig,
    io: ServiceIo,
    mut commands: mpsc::Receiver<Command>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    config.validate().map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string())
    })?;

    let ServiceIo {
        events,
        snapshots,
        pulse,
    } = io;
    let session = snapshots
        .as_ref()
        .map_or_else(next_session_id, |channel| channel.borrow().session);
    let publisher = Publisher::new(snapshots, pulse);
    let mut service = ServiceState::new(config, events, session);
    service.notice(String::from("waiting for discovery"));
    publisher.publish(&mut service.model);

    let mut discovery_timer = tokio::time::interval(config.discovery_interval);
    discovery_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut update_sleep = Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
        service.next_update,
    )));
    let mut publish_tick = tokio::time::interval(config.snapshot_interval);
    publish_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut receive_buffer = [0u8; 512];

    loop {
        tokio::select! {
            result = socket.recv(&mut receive_buffer) => {
                let (length, source) = match result {
                    Ok(received) => received,
                    Err(error) => {
                        service
                            .model
                            .degrade(Fault::Service(format!("receive failed: {error}")));
                        publisher.publish(&mut service.model);
                        return Err(error);
                    },
                };
                service.record_frame(SystemTime::now());
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
                service.discover(&socket).await;
            }
            () = &mut update_sleep => {
                let now = std::time::Instant::now();
                service.expire_pending(now);
                let message = match service.refresh_values(&socket).await {
                    Ok(()) => format!(
                        "polling {} device object(s)",
                        service.poll_epcs.len()
                    ),
                    Err(error) => format!("value update failed: {error}"),
                };
                service.notice(message);
                service.next_update = now + config.update_interval;
                update_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::from_std(service.next_update));
            }
            command = commands.recv() => {
                match command {
                    Some(Command::PollNow) => {
                        // A manual poll does not shift the scheduled cadence;
                        // the next automatic round still fires on its timer.
                        let now = std::time::Instant::now();
                        service.expire_pending(now);
                        let message = match service.refresh_values(&socket).await {
                            Ok(()) => format!(
                                "manual poll of {} device object(s)",
                                service.poll_epcs.len()
                            ),
                            Err(error) => format!("manual value update failed: {error}"),
                        };
                        service.notice(message);
                    }
                    Some(Command::Rediscover) => service.discover(&socket).await,
                    None => break,
                }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = publish_tick.tick() => {
                if service.take_dirty() {
                    publisher.publish(&mut service.model);
                }
            }
        }
    }
    service.model.set_status(SessionStatus::Stopped);
    publisher.publish(&mut service.model);
    Ok(())
}

fn snapshot_channel(
    session: u64
) -> (
    watch::Sender<Arc<RadarSnapshot>>,
    watch::Receiver<Arc<RadarSnapshot>>,
) {
    watch::channel(Arc::new(RadarModel::new(session).initial_snapshot()))
}

fn run_session(
    config: SessionConfig,
    events: Option<Sender<RadarEvent>>,
    pulse: Arc<dyn Pulse>,
    snapshots: watch::Sender<Arc<RadarSnapshot>>,
    commands: mpsc::Receiver<Command>,
    shutdown: watch::Receiver<bool>,
    ready: std_mpsc::SyncSender<Result<(), Fault>>,
) {
    if let Err(error) = config.validate() {
        let _ = ready.send(Err(Fault::Config(error)));
        return;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(Fault::Service(format!(
                "could not start the service runtime: {error}"
            ))));
            return;
        },
    };
    runtime.block_on(async move {
        let socket = match EchoNetSocket::bind_default_multicast(config.interface).await {
            Ok(socket) => socket,
            Err(error) => {
                let _ = ready.send(Err(Fault::Bind {
                    interface: config.interface,
                    detail: error.to_string(),
                }));
                return;
            },
        };
        if ready.send(Ok(())).is_err() {
            return;
        }
        let io = ServiceIo {
            events,
            snapshots: Some(snapshots),
            pulse,
        };
        let _ = run_service(socket, config, io, commands, shutdown).await;
    });
}

/// Publishes the model behind an interval tick, so a burst reaches the reader
/// within one interval.
struct Publisher {
    snapshots: Option<watch::Sender<Arc<RadarSnapshot>>>,
    pulse: Arc<dyn Pulse>,
}

impl Publisher {
    const fn new(
        snapshots: Option<watch::Sender<Arc<RadarSnapshot>>>,
        pulse: Arc<dyn Pulse>,
    ) -> Self {
        Self { snapshots, pulse }
    }

    fn publish(
        &self,
        model: &mut RadarModel,
    ) {
        let Some(snapshots) = &self.snapshots else {
            return;
        };
        snapshots.send_replace(Arc::new(model.snapshot()));
        self.pulse.pulse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DISCOVERY_EPC;
    use echonet_lite_udp::{MULTICAST_GROUP, MULTICAST_PORT};
    use std::net::SocketAddr;
    use std::time::Instant;

    /// Every session test binds UDP 3610, and `SO_REUSEPORT` splits datagrams
    /// by 4-tuple hash, so a parallel test could consume another test's frame.
    static SOCKET_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn socket_lock() -> std::sync::MutexGuard<'static, ()> {
        SOCKET_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn config() -> SessionConfig {
        SessionConfig {
            interface: Ipv4Addr::LOCALHOST,
            ..SessionConfig::desktop()
        }
    }

    fn start_session() -> (RadarSession, RadarHandle) {
        RadarSession::start(config(), Arc::new(NoopPulse)).unwrap()
    }

    fn wait_for_running(handle: &mut RadarHandle) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(handle.snapshot().status, SessionStatus::Running { .. }) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn desktop_schedule_matches_the_design() {
        let config = SessionConfig::desktop();
        assert_eq!(config.discovery_interval, Duration::from_secs(60));
        assert_eq!(config.update_interval, Duration::from_secs(15));
        assert_eq!(config.request_timeout, Duration::from_secs(5));
        assert_eq!(config.snapshot_interval, Duration::from_millis(100));
    }

    #[test]
    fn zero_intervals_are_rejected() {
        assert_eq!(
            SessionConfig {
                discovery_interval: Duration::ZERO,
                ..SessionConfig::desktop()
            }
            .validate(),
            Err(ConfigError::ZeroInterval("discovery_interval"))
        );
        assert_eq!(
            SessionConfig {
                update_interval: Duration::ZERO,
                ..SessionConfig::desktop()
            }
            .validate(),
            Err(ConfigError::ZeroInterval("update_interval"))
        );
        assert_eq!(
            SessionConfig {
                snapshot_interval: Duration::ZERO,
                ..SessionConfig::desktop()
            }
            .validate(),
            Err(ConfigError::ZeroInterval("snapshot_interval"))
        );
    }

    #[test]
    fn the_initial_snapshot_is_binding() {
        let (_sender, receiver) = snapshot_channel(3);

        let snapshot = receiver.borrow();

        assert_eq!(snapshot.status, SessionStatus::Binding);
        assert_eq!(snapshot.session, 3);
        assert_eq!(snapshot.revision, 0);
    }

    #[test]
    fn a_publish_just_before_the_sender_drops_is_visible() {
        let (snapshots, receiver) = snapshot_channel(1);
        let (commands, _command_rx) = mpsc::channel(1);
        let mut handle = RadarHandle {
            snapshots: receiver,
            commands,
            seen: AtomicU64::new(0),
        };

        snapshots.send_replace(Arc::new(RadarModel::new(1).snapshot()));
        drop(snapshots);

        assert!(handle.has_changed());
        assert_eq!(handle.snapshot().revision, 1);
        assert!(!handle.has_changed());
    }

    #[tokio::test]
    async fn start_records_running_after_the_socket_binds() {
        let _guard = socket_lock();
        let (session, mut handle) = start_session();

        assert!(
            wait_for_running(&mut handle),
            "the service never reported Running"
        );
        session.stop().unwrap();
    }

    #[tokio::test]
    async fn a_snapshot_is_readable_from_the_handle() {
        let _guard = socket_lock();
        let (session, mut handle) = start_session();
        assert!(wait_for_running(&mut handle));

        let snapshot = handle.snapshot();

        assert!(snapshot.session >= 1);
        assert!(snapshot.revision >= 1);
        assert!(snapshot.objects.is_empty());
        assert!(!handle.has_changed());
        session.stop().unwrap();
    }

    #[tokio::test]
    async fn stop_joins_the_service_thread() {
        let _guard = socket_lock();
        let (session, mut handle) = start_session();
        let started = Instant::now();
        assert!(session.alive());

        session.stop().unwrap();

        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(handle.snapshot().status, SessionStatus::Stopped);
        assert_eq!(handle.poll_now(), Err(CommandError::ServiceStopped));
    }

    #[tokio::test]
    async fn dropping_a_session_does_not_wait_for_the_thread() {
        let _guard = socket_lock();
        let (session, handle) = start_session();
        let started = Instant::now();

        drop(session);

        assert!(started.elapsed() < Duration::from_secs(1));
        drop(handle);
    }

    #[tokio::test]
    async fn an_unbindable_interface_is_a_fault() {
        let _guard = socket_lock();
        let interface = Ipv4Addr::new(192, 0, 2, 1);
        let result = RadarSession::start(
            SessionConfig {
                interface,
                ..SessionConfig::desktop()
            },
            Arc::new(NoopPulse),
        );

        let Err(Fault::Bind {
            interface: faulted, ..
        }) = result
        else {
            panic!("expected a bind fault");
        };
        assert_eq!(faulted, interface);
    }

    #[test]
    fn a_received_datagram_is_published() {
        let _guard = socket_lock();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let sender = EchoNetSocket::bind_multicast(
                SocketAddr::from((MULTICAST_GROUP, MULTICAST_PORT)),
                Ipv4Addr::LOCALHOST,
            )
            .await
            .unwrap();
            let (session, mut handle) = start_session();
            assert!(wait_for_running(&mut handle));

            sender.send(&[0x10, 0x81, 0x00, 0x01]).await.unwrap();

            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while tokio::time::Instant::now() < deadline {
                if handle.snapshot().frames_seen > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let snapshot = handle.snapshot();
            assert!(snapshot.frames_seen > 0, "no datagram was counted");
            assert!(snapshot.last_frame.is_some());
            session.stop().unwrap();
        });
    }

    #[test]
    fn rediscover_sends_a_discovery_frame() {
        let _guard = socket_lock();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = EchoNetSocket::bind_multicast(
                SocketAddr::from((MULTICAST_GROUP, MULTICAST_PORT)),
                Ipv4Addr::LOCALHOST,
            )
            .await
            .unwrap();
            let (session, handle) = start_session();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let mut buffer = [0u8; 512];
            loop {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no discovery frame arrived"
                );
                handle.rediscover().unwrap();
                let received = tokio::time::timeout(
                    Duration::from_millis(200),
                    listener.recv_frame(&mut buffer),
                )
                .await;
                if let Ok(Ok((frame, _))) = received
                    && frame
                        .properties()
                        .any(|property| property.epc == DISCOVERY_EPC)
                {
                    break;
                }
            }
            session.stop().unwrap();
        });
    }
}
