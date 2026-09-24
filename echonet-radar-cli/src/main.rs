//! Observe ECHONET Lite state on the LAN from the terminal.
//!
//! The tool joins the ECHONET Lite multicast group (`224.0.23.0:3610`). By
//! default it is passive: it prints one line per received frame and never sends
//! anything, so it only reports traffic already on the wire. With `--active` it
//! runs the same discovery and value-polling service as the `echonet-radar` GUI
//! (`echonet-radar-core`), discovers devices, polls their properties and prints
//! each state change.
//!
//! If nothing arrives, check that the host firewall allows inbound UDP to
//! `224.0.23.0:3610` and that the host shares an L2 segment with the devices.
//!
//! ```text
//! echonet-radar-cli --interface 192.168.1.2
//! echonet-radar-cli --interface 192.168.1.2 --active
//! ```

use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, SecondsFormat, Utc};
use clap::Parser;
use echonet_lite::ecodec::lookup;
use echonet_lite::frame::{Eoj, Frame, FrameHeader, Property, parse};
use echonet_lite_udp::EchoNetSocket;
use echonet_radar_core::{
    ChangeEvent, DEFAULT_DISCOVERY_INTERVAL, DEFAULT_UPDATE_INTERVAL, DeviceEvent, RadarConfig,
    RadarEvent, format_edt, run_service,
};

/// Receive buffer size. ECHONET Lite frames are at most 256 bytes; the extra
/// room absorbs datagrams from other applications on port 3610.
const RECEIVE_BUFFER_LEN: usize = 512;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "echonet-radar-cli",
    about = "Observe ECHONET Lite state on the LAN",
    version
)]
struct Arguments {
    /// IPv4 interface used for multicast membership.
    #[arg(long, default_value = "0.0.0.0", value_name = "IP")]
    interface: Ipv4Addr,
    /// Actively discover devices and poll their values instead of only watching
    /// multicast traffic.
    #[arg(long)]
    active: bool,
    /// Discovery interval in seconds (active mode only).
    #[arg(
        long,
        default_value_t = DEFAULT_DISCOVERY_INTERVAL.as_secs(),
        value_name = "SECONDS"
    )]
    discovery_interval_seconds: u64,
    /// Value-polling interval in seconds (active mode only).
    #[arg(
        long,
        default_value_t = DEFAULT_UPDATE_INTERVAL.as_secs(),
        value_name = "SECONDS"
    )]
    update_interval_seconds: u64,
}

impl Arguments {
    const fn radar_config(&self) -> RadarConfig {
        RadarConfig {
            interface: self.interface,
            discovery_interval: Duration::from_secs(self.discovery_interval_seconds),
            update_interval: Duration::from_secs(self.update_interval_seconds),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let arguments = Arguments::parse();
    match run(arguments).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("echonet-radar-cli: {error}");
            ExitCode::FAILURE
        },
    }
}

/// Join the multicast group and run in passive or active mode.
async fn run(arguments: Arguments) -> io::Result<()> {
    let socket = EchoNetSocket::bind_default_multicast(arguments.interface)
        .await
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to join {}:{} on interface {}: {error}",
                    echonet_lite_udp::MULTICAST_GROUP,
                    echonet_lite_udp::MULTICAST_PORT,
                    arguments.interface,
                ),
            )
        })?;
    eprintln!(
        "echonet-radar-cli: listening on {} (interface {})",
        socket.multicast_addr(),
        arguments.interface
    );

    if arguments.active {
        eprintln!(
            "echonet-radar-cli: active mode (discovery {}s, polling {}s)",
            arguments.discovery_interval_seconds, arguments.update_interval_seconds
        );
        run_active(socket, arguments.radar_config()).await
    } else {
        run_passive(socket).await
    }
}

/// Print every received frame until interrupted or the output pipe closes.
async fn run_passive(socket: EchoNetSocket) -> io::Result<()> {
    let mut buffer = [0u8; RECEIVE_BUFFER_LEN];
    loop {
        let (length, source) = socket.recv(&mut buffer).await?;
        let Ok(frame) = parse(&buffer[..length]) else {
            eprintln!("echonet-radar-cli: ignored malformed datagram from {source}");
            continue;
        };
        let line = log_line(SystemTime::now(), source, &frame);
        // Take the stdout lock only for the write, so the (non-`Send`) guard is
        // never held across the receive await.
        let mut stdout = io::stdout().lock();
        if let Err(error) = writeln!(stdout, "{line}") {
            // A closed pipe (`| head`) is a normal way to stop the tool.
            if error.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error);
        }
    }
}

/// Discover devices and poll their values, printing every state change.
async fn run_active(
    socket: EchoNetSocket,
    config: RadarConfig,
) -> io::Result<()> {
    let (events, event_receiver) = mpsc::channel();
    // Hold the command and shutdown senders open: if either channel closes, the
    // service treats it as a stop request and exits immediately.
    let (_commands, command_receiver) = tokio::sync::mpsc::channel(8);
    let (_shutdown, shutdown_receiver) = tokio::sync::watch::channel(false);

    // The service publishes through a std channel; drain it on a blocking thread
    // so the current-thread runtime never stalls on terminal output.
    let printer = tokio::task::spawn_blocking(move || {
        for event in event_receiver {
            print_event(event);
        }
    });

    let result = run_service(socket, config, events, command_receiver, shutdown_receiver).await;
    let _ = printer.await;
    result
}

/// Render a passive frame line with an ISO 8601 timestamp.
fn log_line(
    at: SystemTime,
    source: SocketAddr,
    frame: &Frame<'_>,
) -> String {
    format!("{} {}", format_time(at), format_frame(source, frame))
}

/// Format an instant as an ISO 8601 timestamp in the local timezone.
fn format_time(at: SystemTime) -> String {
    DateTime::<Utc>::from(at)
        .with_timezone(&Local)
        .to_rfc3339_opts(SecondsFormat::Millis, false)
}

/// Render one frame as a single log line: source, EOJs, service code, then the
/// decoded properties.
fn format_frame(
    source: SocketAddr,
    frame: &Frame<'_>,
) -> String {
    let header = frame.header();
    let mut line = format!(
        "{source} {}->{} ESV=0x{:02X} [",
        format_eoj(header.seoj),
        format_eoj(header.deoj),
        header.esv.code(),
    );
    for (index, property) in frame.properties().enumerate() {
        if index > 0 {
            line.push_str(", ");
        }
        line.push_str(&format_property(header, property));
    }
    line.push(']');
    line
}

/// Render an EOJ in the canonical `0x013001` wire format.
fn format_eoj(eoj: Eoj) -> String {
    format!(
        "0x{:02X}{:02X}{:02X}",
        eoj.class_group, eoj.class, eoj.instance
    )
}

/// Render one property, decoding its value against the object that owns it.
fn format_property(
    header: FrameHeader,
    property: Property<'_>,
) -> String {
    let epc = property.epc;
    property_owner(header, epc).map_or_else(
        || format!("EPC=0x{epc:02X} {}", format_bytes(property.edt)),
        |class_code| {
            format!(
                "EPC=0x{epc:02X} {}",
                format_edt(class_code, epc, property.edt)
            )
        },
    )
}

/// The class code of whichever EOJ defines `epc` in this frame.
///
/// Responses and notifications carry the device in `seoj`, but requests carry it
/// in `deoj` (the sending controller owns no such property); the frame has no
/// value to decode against the other one.
fn property_owner(
    header: FrameHeader,
    epc: u8,
) -> Option<u16> {
    let seoj = header.seoj.class_code();
    if lookup(seoj, epc).is_some() {
        return Some(seoj);
    }
    let deoj = header.deoj.class_code();
    lookup(deoj, epc).map(|_| deoj)
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

/// Print one service event: state changes and discoveries on stdout, status
/// messages on stderr.
fn print_event(event: RadarEvent) {
    match event {
        RadarEvent::Change(change) => {
            let line = format_change(&change);
            let _ = writeln!(io::stdout(), "{line}");
        },
        RadarEvent::Device(device) => {
            let line = format_device(&device);
            let _ = writeln!(io::stdout(), "{line}");
        },
        RadarEvent::Status(status) => {
            eprintln!("echonet-radar-cli: {status}");
        },
    }
}

/// Render a state change observed by the active service.
fn format_change(change: &ChangeEvent) -> String {
    format!(
        "{} {} {} EPC=0x{:02X} {}",
        format_time(change.at),
        change.source,
        format_eoj(change.eoj),
        change.epc,
        change.edt,
    )
}

/// Render a newly discovered device object.
fn format_device(device: &DeviceEvent) -> String {
    format!(
        "{} {} discovered {}",
        format_time(SystemTime::now()),
        device.source,
        format_eoj(device.eoj),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use echonet_lite::frame::{Esv, write};

    /// Serialize a frame for the parser, as it would arrive on the wire.
    fn encode(
        header: FrameHeader,
        properties: &[Property<'_>],
    ) -> Vec<u8> {
        let mut buffer = [0u8; 256];
        let length = write(header, properties, &mut buffer).unwrap();
        buffer[..length].to_vec()
    }

    fn source() -> SocketAddr {
        "192.0.2.1:3610".parse().unwrap()
    }

    #[test]
    fn passive_is_the_default() {
        assert!(!Arguments::parse_from(["echonet-radar-cli"]).active);
    }

    #[test]
    fn default_interface_is_unspecified() {
        let arguments = Arguments::parse_from(["echonet-radar-cli"]);
        assert_eq!(arguments.interface, Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn interface_flag_parses_ipv4() {
        let arguments = Arguments::parse_from(["echonet-radar-cli", "--interface", "192.168.1.2"]);
        assert_eq!(arguments.interface, Ipv4Addr::new(192, 168, 1, 2));
    }

    #[test]
    fn active_flag_uses_radar_defaults() {
        let arguments = Arguments::parse_from(["echonet-radar-cli", "--active"]);
        assert!(arguments.active);
        assert_eq!(
            arguments.discovery_interval_seconds,
            DEFAULT_DISCOVERY_INTERVAL.as_secs()
        );
        assert_eq!(
            arguments.update_interval_seconds,
            DEFAULT_UPDATE_INTERVAL.as_secs()
        );
        let config = arguments.radar_config();
        assert_eq!(config.discovery_interval, DEFAULT_DISCOVERY_INTERVAL);
        assert_eq!(config.update_interval, DEFAULT_UPDATE_INTERVAL);
    }

    #[test]
    fn frame_line_decodes_known_property() {
        let bytes = encode(
            FrameHeader {
                tid: 1,
                seoj: Eoj::new(0x01, 0x30, 0x01),
                deoj: Eoj::new(0x05, 0xFF, 0x01),
                esv: Esv::PropertyNotification,
            },
            &[Property {
                epc: 0x80,
                edt: &[0x30],
            }],
        );
        let frame = parse(&bytes).unwrap();
        assert_eq!(
            format_frame(source(), &frame),
            "192.0.2.1:3610 0x013001->0x05FF01 ESV=0x63 [EPC=0x80 Operation status ON]"
        );
    }

    #[test]
    fn request_decodes_property_against_addressed_object() {
        // A Set request: the property belongs to DEOJ, not the controller SEOJ.
        let bytes = encode(
            FrameHeader {
                tid: 1,
                seoj: Eoj::new(0x05, 0xFF, 0x01),
                deoj: Eoj::new(0x01, 0x30, 0x01),
                esv: Esv::PropertyWriteRequestResponseRequired,
            },
            &[Property {
                epc: 0x80,
                edt: &[0x30],
            }],
        );
        let frame = parse(&bytes).unwrap();
        let line = format_frame(source(), &frame);
        assert!(line.contains("ESV=0x62"));
        assert!(line.contains("EPC=0x80 Operation status ON"));
    }

    #[test]
    fn unknown_property_falls_back_to_hex() {
        let bytes = encode(
            FrameHeader {
                tid: 1,
                seoj: Eoj::new(0xFF, 0xFF, 0x01),
                deoj: Eoj::new(0x05, 0xFF, 0x01),
                esv: Esv::PropertyNotification,
            },
            &[Property {
                epc: 0x8F,
                edt: &[0x01, 0xAF],
            }],
        );
        let frame = parse(&bytes).unwrap();
        assert_eq!(
            format_frame(source(), &frame),
            "192.0.2.1:3610 0xFFFF01->0x05FF01 ESV=0x63 [EPC=0x8F 01 AF]"
        );
    }

    #[test]
    fn read_request_marks_empty_edt() {
        let bytes = encode(
            FrameHeader {
                tid: 1,
                seoj: Eoj::new(0x05, 0xFF, 0x01),
                deoj: Eoj::new(0x0E, 0xF0, 0x00),
                esv: Esv::PropertyReadRequest,
            },
            &[Property {
                epc: 0xD6,
                edt: &[],
            }],
        );
        let frame = parse(&bytes).unwrap();
        let line = format_frame(source(), &frame);
        assert!(line.contains("ESV=0x60"));
        assert!(line.ends_with("(empty)]"));
    }

    #[test]
    fn frame_line_lists_every_property() {
        let bytes = encode(
            FrameHeader {
                tid: 2,
                seoj: Eoj::new(0x01, 0x30, 0x01),
                deoj: Eoj::new(0x05, 0xFF, 0x01),
                esv: Esv::PropertyNotification,
            },
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
        );
        let frame = parse(&bytes).unwrap();
        let line = format_frame(source(), &frame);
        assert_eq!(line.matches("EPC=").count(), 2);
        assert!(line.contains(", EPC=0xB0"));
    }

    #[test]
    fn frame_line_without_properties_still_prints_header() {
        let bytes = encode(
            FrameHeader {
                tid: 3,
                seoj: Eoj::new(0x0E, 0xF0, 0x01),
                deoj: Eoj::new(0x05, 0xFF, 0x01),
                esv: Esv::PropertyReadResponse,
            },
            &[],
        );
        let frame = parse(&bytes).unwrap();
        assert_eq!(
            format_frame(source(), &frame),
            "192.0.2.1:3610 0x0EF001->0x05FF01 ESV=0x71 []"
        );
    }

    #[test]
    fn change_line_carries_the_decoded_value() {
        let line = format_change(&ChangeEvent {
            at: SystemTime::UNIX_EPOCH,
            source: source(),
            eoj: Eoj::new(0x01, 0x30, 0x01),
            epc: 0x80,
            edt: String::from("Operation status ON"),
        });
        assert!(line.contains("192.0.2.1:3610 0x013001 EPC=0x80 Operation status ON"));
    }

    #[test]
    fn device_line_marks_the_discovery() {
        let line = format_device(&DeviceEvent {
            source: source(),
            eoj: Eoj::new(0x01, 0x30, 0x01),
        });
        assert!(line.ends_with("192.0.2.1:3610 discovered 0x013001"));
    }

    #[test]
    fn scaled_numeric_values_are_rendered_with_units() {
        // A temperature sensor reports 260 tenths of a degree.
        let bytes = encode(
            FrameHeader {
                tid: 6,
                seoj: Eoj::new(0x00, 0x11, 0x01),
                deoj: Eoj::new(0x05, 0xFF, 0x01),
                esv: Esv::PropertyNotification,
            },
            &[Property {
                epc: 0xE0,
                edt: &[0x01, 0x04],
            }],
        );
        let frame = parse(&bytes).unwrap();
        assert_eq!(
            format_frame(source(), &frame),
            "192.0.2.1:3610 0x001101->0x05FF01 ESV=0x63 \
             [EPC=0xE0 Measured temperature value 26 Celsius]"
        );
    }
}
