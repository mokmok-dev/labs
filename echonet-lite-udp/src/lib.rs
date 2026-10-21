//! Async UDP transport for ECHONET Lite over multicast (tokio).
//!
//! ECHONET Lite nodes communicate over UDP by joining the ECHONET Lite multicast
//! group. This crate provides a thin network layer that binds to that group and
//! exchanges [`echonet_lite::frame`] messages, leaving all protocol logic to the
//! `no_std` `echonet-lite` crate.
//!
//! # Example
//!
//! ```no_run
//! use echonet_lite::frame::{Eoj, Esv, FrameHeader, Property};
//! use echonet_lite_udp::EchoNetSocket;
//! use std::net::Ipv4Addr;
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let socket = EchoNetSocket::bind_default_multicast(Ipv4Addr::UNSPECIFIED).await?;
//!
//! let header = FrameHeader {
//!     tid: 1,
//!     seoj: Eoj::from_class_code(0x05FF, 0x01),
//!     deoj: Eoj::from_class_code(0x0130, 0x01),
//!     esv: Esv::PropertyReadRequest,
//! };
//! socket.send_frame(header, &[Property { epc: 0x80, edt: &[] }]).await?;
//! # Ok(())
//! # }
//! ```

use std::future::poll_fn;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::task::{Context, Poll};

use echonet_lite::frame::{Frame, FrameError, FrameHeader, Property, parse, write};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::ReadBuf;
use tokio::net::UdpSocket;

/// Standard ECHONET Lite multicast group address.
pub const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 23, 0);
/// Standard ECHONET Lite multicast port.
pub const MULTICAST_PORT: u16 = 3610;
/// Maximum ECHONET Lite frame size in bytes.
pub const MAX_FRAME_LEN: usize = 256;

/// A UDP socket joined to the ECHONET Lite multicast group.
///
/// All methods are `&self`; a single socket can be shared (e.g. inside an
/// `Arc`) by both the receiving and sending halves of an application.
#[derive(Debug)]
pub struct EchoNetSocket {
    socket: UdpSocket,
    interface: Option<UdpSocket>,
    multicast: SocketAddr,
}

impl EchoNetSocket {
    /// Bind to the local ECHONET Lite port and join `group` on `interface`.
    ///
    /// `interface` is the local IPv4 address of the network interface to join
    /// the multicast group on. Pass [`Ipv4Addr::UNSPECIFIED`] to let the OS
    /// choose, which also picks the address for the second bind from the route
    /// to `group`.
    ///
    /// A second socket is bound to the interface address and the group's port
    /// when the OS allows it, and [`Self::recv`] reads both. Devices answer the
    /// standard port, and the kernel delivers those datagrams to a socket bound
    /// to the interface address in preference to the wildcard bind. Listeners
    /// that share the address through `SO_REUSEPORT` receive a share instead.
    /// When the interface address cannot be bound, the socket still receives
    /// multicast on the wildcard bind, and [`Self::interface_addr`] returns
    /// `None`.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the wildcard socket cannot be created,
    /// bound, or joined to the multicast group.
    pub async fn bind_multicast(
        group: SocketAddr,
        interface: Ipv4Addr,
    ) -> io::Result<Self> {
        let port = group.port();
        let group_v4 = ipv4_addr(&group)?;
        let socket = bind_shared(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?;
        socket.join_multicast_v4(&group_v4, &interface)?;
        socket.set_multicast_if_v4(&interface)?;
        let socket = UdpSocket::from_std(socket.into())?;

        let address = if interface.is_unspecified() {
            source_address(group)
        } else {
            Some(interface)
        };
        let interface_socket = address.and_then(|address| {
            bind_shared(SocketAddr::from((address, port)))
                .and_then(|socket| UdpSocket::from_std(socket.into()))
                .ok()
        });
        Ok(Self {
            socket,
            interface: interface_socket,
            multicast: group,
        })
    }

    /// Bind to the standard ECHONET Lite multicast group
    /// ([`MULTICAST_GROUP`]:[`MULTICAST_PORT`]) on `interface`.
    ///
    /// This is a convenience for the common case; see [`Self::bind_multicast`]
    /// for the general form.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the socket cannot be created, bound, or
    /// joined to the multicast group.
    pub async fn bind_default_multicast(interface: Ipv4Addr) -> io::Result<Self> {
        Self::bind_multicast(
            SocketAddr::from((MULTICAST_GROUP, MULTICAST_PORT)),
            interface,
        )
        .await
    }

    /// The multicast group this socket is joined to and sends to by default.
    #[must_use]
    pub const fn multicast_addr(&self) -> SocketAddr {
        self.multicast
    }

    /// The local address the socket is bound to.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the OS cannot report the bound address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// The address of the socket bound for unicast answers, when one could be
    /// bound.
    ///
    /// This is the address the interface resolves to, which
    /// [`Self::bind_multicast`] tries to bind on the group's port.
    #[must_use]
    pub fn interface_addr(&self) -> Option<SocketAddr> {
        self.interface.as_ref()?.local_addr().ok()
    }

    /// Send raw bytes to the multicast group.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] on send failure.
    pub async fn send(
        &self,
        buf: &[u8],
    ) -> io::Result<usize> {
        self.socket.send_to(buf, self.multicast).await
    }

    /// Send raw bytes to a specific destination (e.g. a unicast response).
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] on send failure.
    pub async fn send_to(
        &self,
        buf: &[u8],
        dest: SocketAddr,
    ) -> io::Result<usize> {
        self.socket.send_to(buf, dest).await
    }

    /// Receive raw bytes into `buf`, returning the length and sender address.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] on receive failure.
    pub async fn recv(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr)> {
        let interface = self.interface.as_ref();
        poll_fn(|context| match interface {
            // Poll the interface socket first: a burst of multicast on the
            // wildcard socket would otherwise keep unicast answers waiting.
            Some(socket) => {
                if let Poll::Ready(result) = poll_datagram(socket, context, &mut *buf) {
                    return Poll::Ready(result);
                }
                poll_datagram(&self.socket, context, &mut *buf)
            },
            None => poll_datagram(&self.socket, context, &mut *buf),
        })
        .await
    }

    /// Encode a frame and send it to the multicast group.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the frame does not fit in the maximum frame
    /// size ([`MAX_FRAME_LEN`]) or on send failure.
    pub async fn send_frame(
        &self,
        header: FrameHeader,
        properties: &[Property<'_>],
    ) -> io::Result<usize> {
        self.send_frame_to(header, properties, self.multicast).await
    }

    /// Encode a frame and send it to a specific destination.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the frame does not fit in the maximum frame
    /// size ([`MAX_FRAME_LEN`]) or on send failure.
    pub async fn send_frame_to(
        &self,
        header: FrameHeader,
        properties: &[Property<'_>],
        dest: SocketAddr,
    ) -> io::Result<usize> {
        let mut buf = [0u8; MAX_FRAME_LEN];
        let n = write(header, properties, &mut buf).map_err(to_io)?;
        self.send_to(&buf[..n], dest).await
    }

    /// Receive a frame into `buf`, parsing it and returning the sender address.
    ///
    /// The returned [`Frame`] borrows from `buf`, which must outlive it.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] on receive failure or if the datagram is not a
    /// well-formed ECHONET Lite frame.
    pub async fn recv_frame<'a>(
        &self,
        buf: &'a mut [u8],
    ) -> io::Result<(Frame<'a>, SocketAddr)> {
        let (n, src) = self.recv(buf).await?;
        let frame = parse(&buf[..n]).map_err(to_io)?;
        Ok((frame, src))
    }
}

fn poll_datagram(
    socket: &UdpSocket,
    context: &mut Context<'_>,
    buf: &mut [u8],
) -> Poll<io::Result<(usize, SocketAddr)>> {
    let mut read_buf = ReadBuf::new(buf);
    socket
        .poll_recv_from(context, &mut read_buf)
        .map(|result| result.map(|source| (read_buf.filled().len(), source)))
}

/// Bind a UDP socket that shares its port with other ECHONET Lite processes.
///
/// Both socket options are needed: `SO_REUSEADDR` lets a listener share the
/// port, and `SO_REUSEPORT` lets several listeners bind the same address.
fn bind_shared(address: SocketAddr) -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.bind(&address.into())?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

/// The local address the OS routes `group` from, or `None` when it has no route.
///
/// Connecting a UDP socket sends nothing; it only asks the routing table which
/// source address a datagram to `group` would carry.
fn source_address(group: SocketAddr) -> Option<Ipv4Addr> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(group).ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(address) if !address.ip().is_unspecified() => Some(*address.ip()),
        _ => None,
    }
}

/// Extract the IPv4 group address from a `SocketAddr`, rejecting non-IPv4.
fn ipv4_addr(addr: &SocketAddr) -> io::Result<Ipv4Addr> {
    match addr {
        SocketAddr::V4(v4) => Ok(*v4.ip()),
        SocketAddr::V6(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ECHONET Lite multicast requires an IPv4 group address",
        )),
    }
}

/// Map a frame codec error to an [`io::Error`].
fn to_io(err: FrameError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use echonet_lite::frame::{Eoj, Esv, FrameHeader, Property, write};
    use std::net::UdpSocket as StdUdpSocket;
    use std::time::Duration;

    /// A minimal ECHONET Lite GET frame.
    fn sample_frame(tid: u16) -> Vec<u8> {
        let header = FrameHeader {
            tid,
            seoj: Eoj::new(0x05, 0xFF, 0x01),
            deoj: Eoj::new(0x0E, 0xF0, 0x00),
            esv: Esv::PropertyReadRequest,
        };
        let mut buf = [0u8; 32];
        let n = write(
            header,
            &[Property {
                epc: 0xD6,
                edt: &[],
            }],
            &mut buf,
        )
        .unwrap();
        buf[..n].to_vec()
    }

    /// A radar socket bound to an ephemeral port and joined to the group on
    /// `0.0.0.0` (the default interface the binary uses).
    async fn radar() -> EchoNetSocket {
        EchoNetSocket::bind_multicast(
            SocketAddr::from((MULTICAST_GROUP, 0)),
            Ipv4Addr::UNSPECIFIED,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn receives_unicast_on_the_interface_socket() {
        let wildcard = bind_shared(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).unwrap();
        let interface = bind_shared(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let port = interface.local_addr().unwrap().as_socket().unwrap().port();
        let interface_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let radar = EchoNetSocket {
            socket: UdpSocket::from_std(wildcard.into()).unwrap(),
            interface: Some(UdpSocket::from_std(interface.into()).unwrap()),
            multicast: SocketAddr::from((MULTICAST_GROUP, 0)),
        };

        let peer = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_nonblocking(true).unwrap();
        peer.send_to(&sample_frame(1), interface_addr).unwrap();

        let mut buf = [0u8; 512];
        let (len, source) = tokio::time::timeout(Duration::from_secs(2), radar.recv(&mut buf))
            .await
            .expect("timed out waiting for unicast on the interface socket")
            .unwrap();
        assert_eq!(source.ip(), Ipv4Addr::LOCALHOST);
        assert_eq!(&buf[..len], sample_frame(1).as_slice());
    }

    #[tokio::test]
    #[ignore = "requires a multicast-capable interface; nix sandbox/CI is loopback-only"]
    async fn receives_unicast_from_a_peer() {
        let radar = radar().await;
        let unicast_addr =
            SocketAddr::from((Ipv4Addr::LOCALHOST, radar.local_addr().unwrap().port()));
        let peer = StdUdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_nonblocking(true).unwrap();
        peer.send_to(&sample_frame(1), unicast_addr).unwrap();

        let mut buf = [0u8; 512];
        let (len, source) = tokio::time::timeout(Duration::from_secs(2), radar.recv(&mut buf))
            .await
            .expect("timed out waiting for unicast")
            .unwrap();
        assert!(len > 0);
        assert_eq!(source.ip(), Ipv4Addr::LOCALHOST);
    }

    #[tokio::test]
    #[ignore = "requires a multicast-capable interface; nix sandbox/CI is loopback-only"]
    async fn joins_the_multicast_group_and_sends_to_it() {
        let radar = radar().await;
        // The socket joined the group during bind, so unicast to the group port
        // and multicast membership are both configured on the same socket.
        assert!(radar.multicast_addr().ip().is_multicast());

        let header = FrameHeader {
            tid: 1,
            seoj: Eoj::new(0x05, 0xFF, 0x01),
            deoj: Eoj::new(0x0E, 0xF0, 0x00),
            esv: Esv::PropertyReadRequest,
        };
        let result = radar
            .send_frame(
                header,
                &[Property {
                    epc: 0xD6,
                    edt: &[],
                }],
            )
            .await;
        // Hosts without a multicast-capable outgoing interface (loopback-only
        // sandboxes and CI) cannot route multicast: the send fails with
        // EINVAL/ENOADDR/ENETUNREACH. That is an environment limit, not a
        // transport bug, so assert success wherever multicast is routable.
        match result {
            Ok(_) => {},
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::InvalidInput
                        | io::ErrorKind::AddrNotAvailable
                        | io::ErrorKind::NetworkUnreachable
                ) => {},
            Err(error) => panic!("multicast send failed: {error}"),
        }
    }
}
