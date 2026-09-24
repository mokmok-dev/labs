//! Observe ECHONET Lite state on the LAN from the terminal.
//!
//! The tool joins the ECHONET Lite multicast group (`224.0.23.0:3610`). By
//! default it is passive: it prints one line per received frame and never sends
//! anything, so it only reports traffic already on the wire. With `--active` it
//! runs the same discovery and value-polling service as the `echonet-radar` GUI
//! (`echonet-radar-core`), discovers devices, polls their properties and prints
//! each state change.
//!
//! `--to`, `--eoj` and `--set` write one property to a device and print the
//! device's answer, which is how appliances are controlled. The request is
//! checked against the ECHONET Lite MRA tables first, so a property the target
//! class cannot take (or a value outside its range) never reaches the
//! appliance. The write is sent from the standard port because devices answer
//! that port; a listener already bound to it may receive the answer instead.
//!
//! If nothing arrives, check that the host firewall allows inbound UDP to
//! `224.0.23.0:3610` and that the host shares an L2 segment with the devices.
//!
//! ```text
//! echonet-radar-cli --interface 192.168.1.2
//! echonet-radar-cli --interface 192.168.1.2 --active
//! echonet-radar-cli --to 192.168.1.20 --eoj 0x026B01 --set 0xD1=0x28
//! ```

use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, SecondsFormat, Utc};
use clap::Parser;
use echonet_lite::ecodec::{Access, decode, lookup};
use echonet_lite::frame::{Eoj, Esv, Frame, FrameHeader, Property, parse};
use echonet_lite_udp::EchoNetSocket;
use echonet_radar_core::{
    CONTROLLER_EOJ, ChangeEvent, DEFAULT_DISCOVERY_INTERVAL, DEFAULT_UPDATE_INTERVAL, DeviceEvent,
    RadarConfig, RadarEvent, format_edt, run_service,
};

/// Receive buffer size. ECHONET Lite frames are at most 256 bytes; the extra
/// room absorbs datagrams from other applications on port 3610.
const RECEIVE_BUFFER_LEN: usize = 512;

/// The standard ECHONET Lite write request service code (`SetC`, answered).
///
/// `echonet_lite::frame::Esv` predates the standard service-code names, so this
/// path sends wire codes directly, as the service does for Get (`0x62`).
const SET_REQUEST_ESV_CODE: u8 = 0x61;
/// The standard write response service code (`SetC_Res`).
const SET_RESPONSE_ESV_CODE: u8 = 0x71;
/// Time allowed for a device to answer a write.
const WRITE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// Write to a device and print its answer, instead of listening.
    ///
    /// The address may carry a port (`192.168.100.20:3610`); without one the
    /// standard ECHONET Lite port is used.
    #[arg(
        long,
        value_name = "IP[:PORT]",
        requires_all = ["eoj", "set"],
        value_parser = parse_device
    )]
    to: Option<SocketAddr>,
    /// Object to write to, e.g. `0x026B01`.
    #[arg(long, value_name = "EOJ", requires = "to", value_parser = parse_eoj)]
    eoj: Option<Eoj>,
    /// Property to write as `EPC=EDT` in hex, e.g. `0xD1=0x28`.
    #[arg(
        long,
        value_name = "EPC=EDT",
        requires = "to",
        value_parser = parse_property
    )]
    set: Option<WriteProperty>,
}

/// A property write: the EPC and the bytes to send as its EDT.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WriteProperty {
    epc: u8,
    edt: Vec<u8>,
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
    if let (Some(device), Some(eoj), Some(property)) =
        (arguments.to, arguments.eoj, arguments.set.as_ref())
    {
        return write_property(arguments.interface, device, eoj, property).await;
    }

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

/// Write one property to a device and print the device's answer.
///
/// The request is sent from a socket bound to the standard ECHONET Lite port:
/// the water heater on this LAN answers that port rather than the port the
/// request came from, so an ephemeral socket never sees the answer.
async fn write_property(
    interface: Ipv4Addr,
    device: SocketAddr,
    eoj: Eoj,
    property: &WriteProperty,
) -> io::Result<()> {
    validate_write(eoj.class_code(), property)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;

    let socket = EchoNetSocket::bind_default_multicast(interface).await?;

    let tid = write_tid();
    socket
        .send_frame_to(
            set_request_header(tid, eoj),
            &[Property {
                epc: property.epc,
                edt: &property.edt,
            }],
            device,
        )
        .await?;
    eprintln!(
        "echonet-radar-cli: wrote EPC=0x{:02X}={} to {} at {device}",
        property.epc,
        format_bytes(&property.edt),
        format_eoj(eoj),
    );

    let deadline = tokio::time::Instant::now() + WRITE_RESPONSE_TIMEOUT;
    let mut buffer = [0u8; RECEIVE_BUFFER_LEN];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let received = tokio::time::timeout(remaining, socket.recv(&mut buffer))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "no answer from {device} within {} seconds",
                        WRITE_RESPONSE_TIMEOUT.as_secs()
                    ),
                )
            })?;
        let (length, source) = received?;
        let Ok(frame) = parse(&buffer[..length]) else {
            continue;
        };
        if frame.header().tid != tid {
            continue;
        }
        println!("{}", log_line(SystemTime::now(), source, &frame));
        let code = frame.header().esv.code();
        return if code == SET_RESPONSE_ESV_CODE {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "the device did not accept the write (ESV=0x{code:02X})"
            )))
        };
    }
}

/// The header of the `SetC` telegram this tool sends to write a property.
const fn set_request_header(
    tid: u16,
    eoj: Eoj,
) -> FrameHeader {
    FrameHeader {
        tid,
        seoj: CONTROLLER_EOJ,
        deoj: eoj,
        esv: Esv::Unknown(SET_REQUEST_ESV_CODE),
    }
}

/// A transaction ID for this invocation.
///
/// A device may read a repeated transaction ID as a retransmission of a request
/// it has already answered, so the ID comes from the clock rather than from a
/// constant.
fn write_tid() -> u16 {
    let Ok(elapsed) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) else {
        return 1;
    };
    let nanos = elapsed.subsec_nanos().to_le_bytes();
    u16::from_le_bytes([nanos[0], nanos[1]]).max(1)
}

/// Refuse a write the MRA tables say the device cannot take.
///
/// The tables state which properties a class supports for Set and how long and
/// how large their values may be, so a typo is caught here rather than sent to
/// an appliance.
fn validate_write(
    class_code: u16,
    property: &WriteProperty,
) -> Result<(), String> {
    let epc = property.epc;
    let Some(info) = lookup(class_code, epc) else {
        return Err(format!(
            "EPC=0x{epc:02X} is not defined for class 0x{class_code:04X}"
        ));
    };
    if !matches!(info.set, Access::Required | Access::Optional) {
        return Err(format!(
            "EPC=0x{epc:02X} ({}) cannot be set on class 0x{class_code:04X}",
            info.name
        ));
    }
    decode(class_code, epc, &property.edt).map_err(|error| {
        format!(
            "the value for EPC=0x{epc:02X} ({}) is not valid: {error}",
            info.name
        )
    })?;
    Ok(())
}

/// Parse a device address, defaulting to the standard ECHONET Lite port.
fn parse_device(value: &str) -> Result<SocketAddr, String> {
    if let Ok(address) = value.parse::<SocketAddr>() {
        return Ok(address);
    }
    value
        .parse::<Ipv4Addr>()
        .map(|ip| SocketAddr::from((ip, echonet_lite_udp::MULTICAST_PORT)))
        .map_err(|_| format!("{value} is neither an IPv4 address nor address:port"))
}

/// Parse a three-byte EOJ in hex, with an optional `0x` prefix.
fn parse_eoj(value: &str) -> Result<Eoj, String> {
    match parse_hex(value)?.as_slice() {
        [class_group, class, instance] => Ok(Eoj::new(*class_group, *class, *instance)),
        _ => Err(format!("{value} is not a three-byte EOJ, e.g. 0x026B01")),
    }
}

/// Parse `EPC=EDT` in hex, e.g. `0xD1=0x28`.
fn parse_property(value: &str) -> Result<WriteProperty, String> {
    let (epc, edt) = value
        .split_once('=')
        .ok_or_else(|| format!("{value} is not in EPC=EDT form, e.g. 0xD1=0x28"))?;
    let epc = parse_hex(epc)?;
    let [epc] = epc.as_slice() else {
        return Err(format!("{value} must name a one-byte EPC, e.g. 0xD1=0x28"));
    };
    let edt = parse_hex(edt)?;
    if edt.is_empty() {
        return Err(format!("{value} leaves the value empty"));
    }
    Ok(WriteProperty { epc: *epc, edt })
}

/// Parse an even-length hex byte string, with an optional `0x` prefix.
fn parse_hex(value: &str) -> Result<Vec<u8>, String> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if digits.is_empty() || !digits.len().is_multiple_of(2) {
        return Err(format!("{value} is not an even number of hex digits"));
    }
    digits
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let text = String::from_utf8_lossy(pair);
            u8::from_str_radix(&text, 16).map_err(|error| format!("{value} is not hex: {error}"))
        })
        .collect()
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

    #[test]
    fn write_arguments_are_parsed() {
        assert_eq!(parse_eoj("0x026B01").unwrap(), Eoj::new(0x02, 0x6B, 0x01));
        assert_eq!(parse_eoj("026B01").unwrap(), Eoj::new(0x02, 0x6B, 0x01));
        assert!(parse_eoj("0x026B").is_err(), "a short EOJ must be refused");
        assert!(parse_eoj("0xZZ6B01").is_err(), "non-hex must be refused");

        assert_eq!(
            parse_property("0xD1=0x28").unwrap(),
            WriteProperty {
                epc: 0xD1,
                edt: vec![0x28],
            }
        );
        assert_eq!(
            parse_property("D1=0104").unwrap(),
            WriteProperty {
                epc: 0xD1,
                edt: vec![0x01, 0x04],
            }
        );
        assert!(
            parse_property("0xD1").is_err(),
            "a missing value must be refused"
        );
        assert!(
            parse_property("0xD1=").is_err(),
            "an empty value must be refused"
        );
        assert!(
            parse_property("0xD100=0x28").is_err(),
            "a two-byte EPC must be refused"
        );

        // A bare address takes the standard port; an explicit one wins.
        assert_eq!(
            parse_device("192.168.100.20").unwrap(),
            "192.168.100.20:3610".parse().unwrap()
        );
        assert_eq!(
            parse_device("127.0.0.1:3611").unwrap(),
            "127.0.0.1:3611".parse().unwrap()
        );
        assert!(parse_device("not-an-address").is_err());
    }

    #[test]
    fn write_mode_needs_the_target_the_object_and_the_value() {
        let arguments = Arguments::parse_from([
            "echonet-radar-cli",
            "--to",
            "192.168.100.20",
            "--eoj",
            "0x026B01",
            "--set",
            "0xD1=0x28",
        ]);
        assert_eq!(
            arguments.to.unwrap(),
            "192.168.100.20:3610".parse().unwrap()
        );
        assert!(arguments.eoj.is_some());

        // Listing one of the three without the others is a usage error.
        assert!(
            Arguments::try_parse_from(["echonet-radar-cli", "--to", "192.168.100.20"]).is_err()
        );
        assert!(Arguments::try_parse_from(["echonet-radar-cli", "--set", "0xD1=0x28"]).is_err());
    }

    #[test]
    fn writes_are_checked_against_the_tables_before_they_are_sent() {
        let heater = 0x026B;
        // The supplied water temperature setting accepts 0..100 Celsius.
        assert!(validate_write(heater, &parse_property("0xD1=0x28").unwrap()).is_ok());
        assert!(
            validate_write(heater, &parse_property("0xD1=0xFF").unwrap()).is_err(),
            "a value above the table range must be refused"
        );
        assert!(
            validate_write(heater, &parse_property("0xD1=0x2800").unwrap()).is_err(),
            "a value of the wrong length must be refused"
        );
        assert!(
            validate_write(heater, &parse_property("0x9F=0x00").unwrap()).is_err(),
            "a property the class does not define must be refused"
        );
        // Version information is readable but not settable on that class.
        assert!(
            validate_write(heater, &parse_property("0x82=0x00000001").unwrap()).is_err(),
            "a property the tables say is not settable must be refused"
        );
    }

    #[test]
    fn write_request_uses_the_standard_setc_service_code() {
        let bytes = encode(
            set_request_header(1, Eoj::new(0x02, 0x6B, 0x01)),
            &[Property {
                epc: 0xD1,
                edt: &[0x28],
            }],
        );
        assert_eq!(
            bytes,
            [
                0x10, 0x81, 0x00, 0x01, 0x05, 0xFF, 0x01, 0x02, 0x6B, 0x01, 0x61, 0x01, 0xD1, 0x01,
                0x28,
            ],
            "the write must go out as a SetC telegram"
        );
    }

    #[test]
    fn a_write_carries_a_usable_transaction_id() {
        // A device may treat a repeated transaction ID as a retransmission of a
        // request it already answered, so the ID is taken from the clock; zero
        // is not used.
        assert_ne!(write_tid(), 0);
    }
}
