//! Integration tests exercising the ECHONET Lite UDP transport over loopback.
//!
//! These tests avoid relying on real multicast delivery (which is environment
//! dependent) and instead route frames over the loopback unicast path, while a
//! separate test verifies that binding and joining the multicast group succeeds.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

use echonet_lite::frame::{Eoj, Esv, FrameHeader, Property, parse, write};
use echonet_lite_udp::{EchoNetSocket, MULTICAST_GROUP};

fn read_request_header(tid: u16) -> FrameHeader {
    FrameHeader {
        tid,
        seoj: Eoj::new(0x05, 0xFF, 0x01),
        deoj: Eoj::new(0x01, 0x30, 0x01),
        esv: Esv::PropertyReadRequest,
    }
}

fn response_header(tid: u16) -> FrameHeader {
    FrameHeader {
        tid,
        seoj: Eoj::new(0x01, 0x30, 0x01),
        deoj: Eoj::new(0x05, 0xFF, 0x01),
        esv: Esv::PropertyReadResponse,
    }
}

fn multicast_group() -> SocketAddr {
    SocketAddr::from((MULTICAST_GROUP, 0))
}

#[tokio::test]
async fn bind_default_multicast_joins_group() {
    let socket = EchoNetSocket::bind_default_multicast(Ipv4Addr::LOCALHOST)
        .await
        .expect("default multicast bind/join should succeed on loopback");
    assert_eq!(socket.multicast_addr().ip(), MULTICAST_GROUP);
    assert_eq!(socket.multicast_addr().port(), 3610);
}

#[tokio::test]
async fn bind_multicast_joins_group() {
    let socket = EchoNetSocket::bind_multicast(multicast_group(), Ipv4Addr::LOCALHOST)
        .await
        .expect("multicast bind/join should succeed on loopback");
    assert_eq!(socket.multicast_addr().ip(), MULTICAST_GROUP);
    assert!(socket.local_addr().is_ok());
}

#[tokio::test]
async fn send_frame_to_is_received_and_parsed() {
    let a = EchoNetSocket::bind_multicast(multicast_group(), Ipv4Addr::LOCALHOST)
        .await
        .unwrap();

    let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
    let recv_addr = recv.local_addr().unwrap();

    let header = read_request_header(0x1234);
    a.send_frame_to(
        header,
        &[Property {
            epc: 0x80,
            edt: &[],
        }],
        recv_addr,
    )
    .await
    .unwrap();

    let mut buf = [0u8; 256];
    let (n, src) = recv
        .recv_from(&mut buf)
        .expect("receiver should get datagram");
    assert_eq!(src.ip(), Ipv4Addr::LOCALHOST);

    let frame = parse(&buf[..n]).unwrap();
    assert_eq!(frame.header(), header);
    assert_eq!(
        frame.properties().collect::<Vec<_>>(),
        vec![Property {
            epc: 0x80,
            edt: &[],
        }]
    );
}

#[tokio::test]
async fn recv_frame_parses_loopback_datagram() {
    let a = EchoNetSocket::bind_multicast(multicast_group(), Ipv4Addr::LOCALHOST)
        .await
        .unwrap();
    let a_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), a.local_addr().unwrap().port());

    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let header = response_header(9);
    let mut buf = [0u8; 256];
    let n = write(
        header,
        &[Property {
            epc: 0x80,
            edt: &[0x30],
        }],
        &mut buf,
    )
    .unwrap();
    sender.send_to(&buf[..n], a_addr).unwrap();

    let mut rbuf = [0u8; 256];
    let (frame, src) = a
        .recv_frame(&mut rbuf)
        .await
        .expect("multicast socket should receive loopback datagram");
    assert_eq!(src.ip(), Ipv4Addr::LOCALHOST);
    assert_eq!(frame.header(), header);
    assert_eq!(
        frame.properties().collect::<Vec<_>>(),
        vec![Property {
            epc: 0x80,
            edt: &[0x30],
        }]
    );
}

#[tokio::test]
async fn full_duplex_exchange_between_two_sockets() {
    // Two multicast-joined sockets communicate over loopback unicast, exercising
    // the encode/send path on one end and the recv/parse path on the other.
    let a = EchoNetSocket::bind_multicast(multicast_group(), Ipv4Addr::LOCALHOST)
        .await
        .unwrap();
    let b = EchoNetSocket::bind_multicast(multicast_group(), Ipv4Addr::LOCALHOST)
        .await
        .unwrap();
    let b_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), b.local_addr().unwrap().port());

    let request = read_request_header(42);
    a.send_frame_to(
        request,
        &[Property {
            epc: 0x80,
            edt: &[],
        }],
        b_addr,
    )
    .await
    .unwrap();

    let mut buf = [0u8; 256];
    let (recv_frame, _) = b.recv_frame(&mut buf).await.unwrap();
    assert_eq!(recv_frame.header(), request);

    // Respond back.
    let response = response_header(42);
    let a_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), a.local_addr().unwrap().port());
    b.send_frame_to(
        response,
        &[Property {
            epc: 0x80,
            edt: &[0x30],
        }],
        a_addr,
    )
    .await
    .unwrap();

    let mut buf = [0u8; 256];
    let (recv_frame, _) = a.recv_frame(&mut buf).await.unwrap();
    assert_eq!(recv_frame.header(), response);
    assert_eq!(
        recv_frame.properties().collect::<Vec<_>>(),
        vec![Property {
            epc: 0x80,
            edt: &[0x30],
        }]
    );
}

#[test]
fn ipv6_group_is_rejected() {
    let group = SocketAddr::from(([0xFF00, 0, 0, 0, 0, 0, 0, 1], 3610));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let err = rt
        .block_on(EchoNetSocket::bind_multicast(group, Ipv4Addr::LOCALHOST))
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}
