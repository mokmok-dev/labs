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

use std::io;
use std::net::{Ipv4Addr, SocketAddr};

use echonet_lite::frame::{Frame, FrameError, FrameHeader, Property, parse, write};
use socket2::{Domain, Protocol, Socket, Type};
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
    multicast: SocketAddr,
}

impl EchoNetSocket {
    /// Bind to the local ECHONET Lite port and join `group` on `interface`.
    ///
    /// `interface` is the local IPv4 address of the network interface to join
    /// the multicast group on. Pass [`Ipv4Addr::UNSPECIFIED`] to let the OS
    /// choose.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the socket cannot be created, bound, or
    /// joined to the multicast group.
    pub async fn bind_multicast(
        group: SocketAddr,
        interface: Ipv4Addr,
    ) -> io::Result<Self> {
        let port = group.port();
        let group_v4 = ipv4_addr(&group)?;
        let std_socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        std_socket.set_reuse_address(true)?;
        std_socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
        std_socket.join_multicast_v4(&group_v4, &interface)?;
        std_socket.set_multicast_if_v4(&interface)?;
        std_socket.set_nonblocking(true)?;
        let std_socket: std::net::UdpSocket = std_socket.into();
        let socket = UdpSocket::from_std(std_socket)?;
        Ok(Self {
            socket,
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
        self.socket.recv_from(buf).await
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
