//! End-to-end tests of the shipped `echonet-radar-cli` binary.
//!
//! Both tests spawn the real executable (`CARGO_BIN_EXE_echonet-radar-cli`) and
//! drive it over loopback unicast to port 3610, which is the transport path the
//! crate's own tests use because multicast delivery is environment dependent.
//! One test injects telegrams at the listener; the other stands in for a device
//! and answers the write the binary sends. They share port 3610, so they take
//! turns (see [`PORT_LOCK`]).

use std::error::Error;
use std::io::{BufRead, BufReader, Read};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use chrono::DateTime;
use echonet_lite::frame::{Eoj, Esv, FrameHeader, Property, parse, write};

/// The ECHONET Lite port the tool always listens on.
const PORT: u16 = 3610;
/// Time allowed for the child to report that it bound the socket.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Time allowed for the child to log one attempt's telegrams.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);
/// Time a stand-in device waits for a request.
const DEVICE_TIMEOUT: Duration = Duration::from_secs(10);
/// Attempts allowed to get the frames into the child's socket.
///
/// Port 3610 is shared with any other ECHONET Lite listener running on the host:
/// [`EchoNetSocket`] sets `SO_REUSEPORT`, and the kernel picks the receiving
/// socket by hashing the datagram's 4-tuple, so a peer with a fresh source port
/// is a fresh chance to be hashed to this child.
///
/// [`EchoNetSocket`]: echonet_lite_udp::EchoNetSocket
const DELIVERY_ATTEMPTS: usize = 3;

/// Held for the duration of each test: both tests bind port 3610, and a second
/// socket on that port would take datagrams by 4-tuple hash.
static PORT_LOCK: Mutex<()> = Mutex::new(());

/// Serialize the tests that share port 3610.
fn exclusive_port() -> MutexGuard<'static, ()> {
    PORT_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The shipped binary under test.
const BINARY: &str = env!("CARGO_BIN_EXE_echonet-radar-cli");

/// One telegram to inject, with the line it must produce.
///
/// The expected line holds everything after the timestamp and the sender, which
/// the test can only know once its socket is bound.
struct Injection {
    bytes: Vec<u8>,
    expected: &'static str,
}

/// A running `echonet-radar-cli` child with its output streamed line by line.
struct Cli {
    child: Child,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
}

impl Cli {
    /// Spawn the shipped binary with no mode flag.
    ///
    /// `--interface 127.0.0.1` keeps the multicast join on an interface that
    /// always exists, the same reason the transport tests join on loopback; the
    /// socket is still bound to the wildcard address, so frames addressed to
    /// `127.0.0.1:3610` arrive.
    fn spawn() -> Result<Self, Box<dyn Error>> {
        Self::spawn_with(&[])
    }

    /// Spawn the shipped binary with `extra` arguments.
    fn spawn_with(extra: &[&str]) -> Result<Self, Box<dyn Error>> {
        let mut child = Command::new(BINARY)
            .args(["--interface", "127.0.0.1"])
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("child stdout was not piped")?;
        let stderr = child.stderr.take().ok_or("child stderr was not piped")?;
        Ok(Self {
            child,
            stdout: forward_lines(stdout),
            stderr: forward_lines(stderr),
        })
    }

    /// Wait for the child to finish and report its exit status.
    fn wait(&mut self) -> Result<ExitStatus, Box<dyn Error>> {
        Ok(self.child.wait()?)
    }

    /// Block until the child reports that it is listening, so no frame is
    /// injected before the socket exists (a bound socket buffers early frames).
    fn wait_until_listening(&self) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let mut seen = Vec::new();
        loop {
            match self
                .stderr
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) if line.contains("listening on") => return Ok(()),
                Ok(line) => seen.push(line),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!("child never reported listening: {seen:?}").into());
                },
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("child exited before listening: {seen:?}").into());
                },
            }
        }
    }

    /// Read the child's stdout until it logged `expected` lines from `source`,
    /// or `timeout` elapses, splitting what it logged by sender.
    fn collect_lines_from(
        &self,
        source: SocketAddr,
        expected: usize,
        timeout: Duration,
    ) -> Collected {
        let source = source.to_string();
        let deadline = Instant::now() + timeout;
        let mut collected = Collected::default();
        while collected.from_peer.len() < expected {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.stdout.recv_timeout(remaining) {
                Ok(line) => collected.push(line, &source),
                // A disconnected pipe means the child is gone.
                Err(_) => break,
            }
        }
        // Anything else already buffered belongs in a failure report.
        while let Ok(line) = self.stdout.try_recv() {
            collected.push(line, &source);
        }
        collected
    }

    /// Block until the child logs a stderr line containing `needle`.
    fn wait_for_stderr_containing(
        &self,
        needle: &str,
    ) -> Result<String, Box<dyn Error>> {
        let deadline = Instant::now() + ATTEMPT_TIMEOUT;
        let mut seen = Vec::new();
        loop {
            match self
                .stderr
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(line) if line.contains(needle) => return Ok(line),
                Ok(line) => seen.push(line),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    return Err(format!("no stderr line containing {needle:?}: {seen:?}").into());
                },
            }
        }
    }
}

impl Drop for Cli {
    fn drop(&mut self) {
        // A leaked child would keep port 3610 bound and steal datagrams from
        // later runs.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What the child logged during one read: lines from the injecting peer, and
/// lines from anything else on port 3610 (a failure report needs both).
#[derive(Debug, Default)]
struct Collected {
    from_peer: Vec<String>,
    others: Vec<String>,
}

impl Collected {
    /// File a line under the sender it came from.
    fn push(
        &mut self,
        line: String,
        source: &str,
    ) {
        if line.split(' ').nth(1) == Some(source) {
            self.from_peer.push(line);
        } else {
            self.others.push(line);
        }
    }
}

/// A socket that injects telegrams into the child, as a LAN peer would.
struct Peer {
    socket: UdpSocket,
    source: SocketAddr,
    destination: SocketAddr,
}

impl Peer {
    fn bind() -> Result<Self, Box<dyn Error>> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        Ok(Self {
            source: socket.local_addr()?,
            socket,
            destination: SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)),
        })
    }

    fn send(
        &self,
        bytes: &[u8],
    ) -> Result<(), Box<dyn Error>> {
        self.socket.send_to(bytes, self.destination)?;
        Ok(())
    }
}

/// Read `reader` line by line on a background thread into a channel.
fn forward_lines<R: Read + Send + 'static>(reader: R) -> Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

/// Serialize a frame as it appears on the wire.
fn encode(
    header: FrameHeader,
    properties: &[Property<'_>],
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut buffer = [0u8; 256];
    let length = write(header, properties, &mut buffer)
        .map_err(|error| format!("frame does not fit the buffer: {error}"))?;
    Ok(buffer[..length].to_vec())
}

/// Build a frame header for one telegram.
const fn header(
    tid: u16,
    seoj: Eoj,
    deoj: Eoj,
    esv: Esv,
) -> FrameHeader {
    FrameHeader {
        tid,
        seoj,
        deoj,
        esv,
    }
}

/// The telegrams injected into the listener.
///
/// Together they cover a device notification, a request whose property belongs
/// to the addressed object rather than the sender, a value labelled with a unit
/// from the MRA tables, an object absent from those tables, a telegram with two
/// properties, a fractional value, and a property no table defines.
fn injections() -> Result<Vec<Injection>, Box<dyn Error>> {
    let controller = Eoj::new(0x05, 0xFF, 0x01);
    let air_conditioner = Eoj::new(0x01, 0x30, 0x01);
    let water_heater = Eoj::new(0x02, 0x6B, 0x01);
    let temperature_sensor = Eoj::new(0x00, 0x11, 0x01);
    let unknown_object = Eoj::new(0xFF, 0xFF, 0x01);

    Ok(vec![
        Injection {
            bytes: encode(
                header(1, air_conditioner, controller, Esv::PropertyNotification),
                &[Property {
                    epc: 0x80,
                    edt: &[0x30],
                }],
            )?,
            expected: "0x013001->0x05FF01 ESV=0x63 [EPC=0x80 Operation status ON]",
        },
        Injection {
            bytes: encode(
                header(
                    2,
                    controller,
                    air_conditioner,
                    Esv::PropertyWriteRequestResponseRequired,
                ),
                &[Property {
                    epc: 0x80,
                    edt: &[0x31],
                }],
            )?,
            expected: "0x05FF01->0x013001 ESV=0x62 [EPC=0x80 Operation status OFF]",
        },
        Injection {
            bytes: encode(
                header(3, water_heater, controller, Esv::PropertyNotification),
                &[Property {
                    epc: 0xD1,
                    edt: &[0x28],
                }],
            )?,
            expected: "0x026B01->0x05FF01 ESV=0x63 \
                       [EPC=0xD1 Temperature of supplied water setting 40 Celsius]",
        },
        Injection {
            bytes: encode(
                header(4, unknown_object, controller, Esv::PropertyNotification),
                &[Property {
                    epc: 0xE1,
                    edt: &[0x01, 0xAF],
                }],
            )?,
            expected: "0xFFFF01->0x05FF01 ESV=0x63 [EPC=0xE1 01 AF]",
        },
        Injection {
            bytes: encode(
                header(5, air_conditioner, controller, Esv::PropertyNotification),
                &[
                    Property {
                        epc: 0x80,
                        edt: &[0x30],
                    },
                    Property {
                        epc: 0xB0,
                        edt: &[0x41],
                    },
                ],
            )?,
            expected: "0x013001->0x05FF01 ESV=0x63 \
                       [EPC=0x80 Operation status ON, EPC=0xB0 Operation mode setting auto]",
        },
        Injection {
            bytes: encode(
                header(6, temperature_sensor, controller, Esv::PropertyNotification),
                &[Property {
                    epc: 0xE0,
                    edt: &[0x01, 0x06],
                }],
            )?,
            expected: "0x001101->0x05FF01 ESV=0x63 \
                       [EPC=0xE0 Measured temperature value 26.2 Celsius]",
        },
    ])
}

/// Send every telegram from a fresh peer until the child logs one decoded line
/// per telegram, and assert those lines carry a timestamp and the decoded body.
///
/// Returns the peer that reached the child, so later steps can reuse a 4-tuple
/// the kernel has already shown to hash to this child.
fn assert_telegrams_are_logged(
    cli: &Cli,
    injections: &[Injection],
) -> Result<Peer, Box<dyn Error>> {
    let mut last_attempt = String::from("no attempt was made");
    for attempt in 1..=DELIVERY_ATTEMPTS {
        let peer = Peer::bind()?;
        for injection in injections {
            peer.send(&injection.bytes)?;
        }
        let collected = cli.collect_lines_from(peer.source, injections.len(), ATTEMPT_TIMEOUT);
        if collected.from_peer.len() == injections.len() {
            assert_lines_are_decoded(&collected.from_peer, peer.source, injections);
            return Ok(peer);
        }
        last_attempt = format!(
            "attempt {attempt}: logged {} of {} telegrams from {} (other senders: {:#?})",
            collected.from_peer.len(),
            injections.len(),
            peer.source,
            collected.others
        );
    }
    Err(format!("the child did not log every telegram ({last_attempt})").into())
}

/// Assert each line starts with an ISO 8601 timestamp and decodes the telegram
/// it answers.
fn assert_lines_are_decoded(
    lines: &[String],
    source: SocketAddr,
    injections: &[Injection],
) {
    for line in lines {
        let timestamp = line.split(' ').next().unwrap_or_default();
        assert!(
            DateTime::parse_from_rfc3339(timestamp).is_ok(),
            "line does not start with an ISO 8601 timestamp: {line}"
        );
    }
    for injection in injections {
        let expected = format!("{source} {}", injection.expected);
        assert!(
            lines.iter().any(|line| line.ends_with(&expected)),
            "missing line {expected:?} in {lines:#?}"
        );
    }
}

/// Assert that a datagram which is not an ECHONET Lite frame is reported and
/// skipped, and that the listener keeps decoding what arrives next.
fn assert_malformed_datagram_is_skipped(
    cli: &Cli,
    peer: &Peer,
) -> Result<(), Box<dyn Error>> {
    // Port 3610 is shared with other applications, so truncated or foreign
    // datagrams are normal traffic.
    peer.send(&[0x10, 0x81, 0x00])?;

    let reported = cli.wait_for_stderr_containing("ignored malformed datagram")?;
    assert!(
        reported.contains(&peer.source.to_string()),
        "the malformed datagram was not attributed to its sender: {reported}"
    );

    let recovery = encode(
        header(
            7,
            Eoj::new(0x02, 0x6B, 0x01),
            Eoj::new(0x05, 0xFF, 0x01),
            Esv::PropertyWriteRequestResponseRequired,
        ),
        &[Property {
            epc: 0xD1,
            edt: &[0x2B],
        }],
    )?;
    peer.send(&recovery)?;

    let collected = cli.collect_lines_from(peer.source, 1, ATTEMPT_TIMEOUT);
    assert_eq!(
        collected.from_peer.len(),
        1,
        "the listener stopped after a malformed datagram: {:#?}",
        collected.from_peer
    );
    let expected = format!(
        "{} 0x026B01->0x05FF01 ESV=0x62 \
         [EPC=0xD1 Temperature of supplied water setting 43 Celsius]",
        peer.source
    );
    assert!(
        collected.from_peer[0].ends_with(&expected),
        "unexpected line after the malformed datagram: {:#?}",
        collected.from_peer
    );
    Ok(())
}

#[test]
fn logs_one_decoded_line_per_frame_on_port_3610() -> Result<(), Box<dyn Error>> {
    let _port = exclusive_port();

    let cli = Cli::spawn()?;
    cli.wait_until_listening()?;

    let peer = assert_telegrams_are_logged(&cli, &injections()?)?;
    assert_malformed_datagram_is_skipped(&cli, &peer)?;

    Ok(())
}

/// A request as a device receives it: its header, sender, and properties.
type Request = (FrameHeader, SocketAddr, Vec<(u8, Vec<u8>)>);

/// A stand-in device: it reports a Set property map, serves one write, applies
/// it to the value it holds, and answers the way a device does.
struct FakeDevice {
    socket: UdpSocket,
    address: SocketAddr,
    /// The value the device holds after the write it answered.
    value: Vec<u8>,
}

impl FakeDevice {
    fn bind() -> Result<Self, Box<dyn Error>> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
        socket.set_read_timeout(Some(DEVICE_TIMEOUT))?;
        Ok(Self {
            address: socket.local_addr()?,
            socket,
            value: Vec::new(),
        })
    }

    /// Read one request addressed to this device.
    fn recv_request(&self) -> Result<Request, Box<dyn Error>> {
        let mut buffer = [0u8; 256];
        let (length, source) = self.socket.recv_from(&mut buffer)?;
        let frame = parse(&buffer[..length])
            .map_err(|error| format!("the device received a malformed datagram: {error}"))?;
        let properties = frame
            .properties()
            .map(|property| (property.epc, property.edt.to_vec()))
            .collect();
        Ok((frame.header(), source, properties))
    }

    /// Answer a request the way a device does.
    fn answer(
        &self,
        request: FrameHeader,
        source: SocketAddr,
        code: u8,
        properties: &[(u8, Vec<u8>)],
    ) -> Result<(), Box<dyn Error>> {
        let properties: Vec<Property<'_>> = properties
            .iter()
            .map(|(epc, edt)| Property { epc: *epc, edt })
            .collect();
        let mut reply = [0u8; 256];
        let length = write(
            FrameHeader {
                tid: request.tid,
                seoj: Eoj::new(0x02, 0x6B, 0x01),
                deoj: request.seoj,
                esv: Esv::from_code(code),
            },
            &properties,
            &mut reply,
        )
        .map_err(|error| format!("the reply does not fit the buffer: {error}"))?;
        self.socket.send_to(&reply[..length], source)?;
        Ok(())
    }

    /// Report a Set property map, then serve one write.
    ///
    /// Returns the write as `(header, epc, edt)`, with the value the device
    /// applied.
    fn serve_map_and_write(&mut self) -> Result<(FrameHeader, u8, Vec<u8>), Box<dyn Error>> {
        // The tool reads the map before it writes, because a device refuses a
        // write for a property outside it.
        let (request, source, properties) = self.recv_request()?;
        assert_eq!(
            request.esv.code(),
            0x62,
            "the Set property map is read with a Get request"
        );
        assert_eq!(
            properties,
            vec![(0x9E, Vec::new())],
            "the map request names EPC 0x9E"
        );
        self.answer(request, source, 0x72, &[(0x9E, vec![1, 0xD1])])?;

        let (request, source, properties) = self.recv_request()?;
        let (epc, edt) = properties
            .first()
            .cloned()
            .ok_or("the device received a write without properties")?;
        // Apply the write, then answer with the value the device now holds.
        self.value.clone_from(&edt);
        self.answer(request, source, 0x71, &[(epc, self.value.clone())])?;
        Ok((request, epc, edt))
    }
}

#[test]
fn writes_a_property_and_prints_the_devices_answer() -> Result<(), Box<dyn Error>> {
    let _port = exclusive_port();

    let mut device = FakeDevice::bind()?;
    let mut cli = Cli::spawn_with(&[
        "--to",
        &device.address.to_string(),
        "--eoj",
        "0x026B01",
        "--set",
        "0xD1=0x2A",
    ])?;

    let (header, epc, edt) = device.serve_map_and_write()?;
    assert_eq!(header.seoj, Eoj::new(0x05, 0xFF, 0x01));
    assert_eq!(header.deoj, Eoj::new(0x02, 0x6B, 0x01));
    assert_eq!(
        header.esv.code(),
        0x61,
        "a write request carries the standard SetC service code"
    );
    assert_eq!((epc, edt.as_slice()), (0xD1, [0x2A].as_slice()));
    assert_eq!(
        device.value,
        vec![0x2A],
        "the device did not apply the write the binary sent"
    );

    let status = cli.wait()?;
    let answer =
        "0x026B01->0x05FF01 ESV=0x71 [EPC=0xD1 Temperature of supplied water setting 42 Celsius]";
    let collected = cli.collect_lines_from(device.address, 1, ATTEMPT_TIMEOUT);
    assert_eq!(
        collected.from_peer.len(),
        1,
        "the binary did not print the device's answer: {collected:#?}"
    );
    assert!(
        collected.from_peer[0].ends_with(&answer),
        "unexpected answer line: {:#?}",
        collected.from_peer
    );
    assert!(
        status.success(),
        "write mode exited with {status} after a successful write"
    );

    Ok(())
}
