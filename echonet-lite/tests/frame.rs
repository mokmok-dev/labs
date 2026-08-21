//! Tests for the ECHONET Lite frame codec.

use echonet_lite::frame::{EHD1, EHD2, Eoj, Esv, FrameError, FrameHeader, Property, parse, write};

/// Serialize a frame into an owned buffer (test helper).
fn encode(
    header: FrameHeader,
    props: &[Property<'_>],
) -> Vec<u8> {
    let mut buf = vec![0u8; 1024];
    let n = write(header, props, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

#[test]
fn round_trip_read_request() {
    let header = FrameHeader {
        tid: 0x1234,
        seoj: Eoj::new(0x05, 0xFF, 0x01),
        deoj: Eoj::new(0x01, 0x30, 0x01),
        esv: Esv::PropertyReadRequest,
    };
    let props = [Property {
        epc: 0x80,
        edt: &[],
    }];
    let bytes = encode(header, &props);

    let parsed = parse(&bytes).unwrap();
    assert_eq!(parsed.header(), header);
    let collected: Vec<_> = parsed.properties().collect();
    assert_eq!(collected, props);

    // Verify the exact wire layout.
    assert_eq!(&bytes[..2], &[EHD1, EHD2]);
    assert_eq!(&bytes[2..4], &[0x12, 0x34]);
    assert_eq!(&bytes[4..7], &[0x05, 0xFF, 0x01]);
    assert_eq!(&bytes[7..10], &[0x01, 0x30, 0x01]);
    assert_eq!(bytes[10], 0x60);
    assert_eq!(bytes[11], 1);
    assert_eq!(&bytes[12..], &[0x80, 0x00]);
}

#[test]
fn write_response_with_data() {
    let header = FrameHeader {
        tid: 7,
        seoj: Eoj::new(0x01, 0x30, 0x01),
        deoj: Eoj::new(0x05, 0xFF, 0x01),
        esv: Esv::PropertyReadResponse,
    };
    let props = [Property {
        epc: 0x80,
        edt: &[0x30],
    }];
    let bytes = encode(header, &props);
    let parsed = parse(&bytes).unwrap();
    assert_eq!(parsed.header().esv, Esv::PropertyReadResponse);
    assert_eq!(
        parsed.properties().collect::<Vec<_>>(),
        vec![Property {
            epc: 0x80,
            edt: &[0x30]
        }]
    );
}

#[test]
fn parse_rejects_bad_ehd() {
    let good = encode(
        FrameHeader {
            tid: 0,
            seoj: Eoj::new(0x05, 0xFF, 0x01),
            deoj: Eoj::default(),
            esv: Esv::PropertyNotification,
        },
        &[],
    );
    assert_eq!(
        parse(&good).unwrap().header().esv,
        Esv::PropertyNotification
    );

    let mut bad = good.clone();
    bad[0] = 0x00;
    assert_eq!(parse(&bad), Err(FrameError::BadEhd1));

    let mut bad = good;
    bad[1] = 0x00;
    assert_eq!(parse(&bad), Err(FrameError::BadEhd2));
}

#[test]
fn parse_rejects_truncated_property() {
    // OPC says 1 property but the payload has none.
    let mut bytes = encode(
        FrameHeader {
            tid: 0,
            seoj: Eoj::new(0x05, 0xFF, 0x01),
            deoj: Eoj::default(),
            esv: Esv::PropertyReadRequest,
        },
        &[],
    );
    bytes.push(0x80); // OPC byte placeholder
    // Set OPC = 1 but append no property body.
    bytes[11] = 1;
    assert_eq!(parse(&bytes), Err(FrameError::TruncatedProperty));
}

#[test]
fn parse_rejects_trailing_bytes() {
    let mut bytes = encode(
        FrameHeader {
            tid: 0,
            seoj: Eoj::new(0x05, 0xFF, 0x01),
            deoj: Eoj::default(),
            esv: Esv::PropertyNotification,
        },
        &[Property {
            epc: 0x80,
            edt: &[0x30],
        }],
    );
    bytes.push(0xFF);
    assert_eq!(parse(&bytes), Err(FrameError::TrailingData));
}

#[test]
fn parse_rejects_short_header() {
    assert_eq!(parse(&[0x10, 0x81]), Err(FrameError::TruncatedHeader));
}

#[test]
fn write_rejects_too_many_properties() {
    let props = (0..=255)
        .map(|epc| Property { epc, edt: &[] })
        .collect::<Vec<_>>();
    let mut buf = vec![0u8; 4096];
    assert_eq!(
        write(
            FrameHeader {
                tid: 0,
                seoj: Eoj::default(),
                deoj: Eoj::default(),
                esv: Esv::PropertyReadRequest,
            },
            &props,
            &mut buf
        ),
        Err(FrameError::TooManyProperties)
    );
}

#[test]
fn write_rejects_small_buffer() {
    let header = FrameHeader {
        tid: 0,
        seoj: Eoj::default(),
        deoj: Eoj::default(),
        esv: Esv::PropertyNotification,
    };
    let props = [Property {
        epc: 0x80,
        edt: &[0x30],
    }];
    let mut buf = vec![0u8; 13];
    assert_eq!(
        write(header, &props, &mut buf),
        Err(FrameError::BufferTooSmall)
    );
}

#[test]
fn write_rejects_edt_too_long() {
    let header = FrameHeader {
        tid: 0,
        seoj: Eoj::default(),
        deoj: Eoj::default(),
        esv: Esv::PropertyNotification,
    };
    let edt = vec![0u8; 256];
    let props = [Property {
        epc: 0x80,
        edt: &edt,
    }];
    let mut buf = vec![0u8; 1024];
    assert_eq!(write(header, &props, &mut buf), Err(FrameError::EdtTooLong));
}

#[test]
fn esv_code_and_from_code() {
    assert_eq!(Esv::PropertyReadRequest.code(), 0x60);
    assert_eq!(Esv::from_code(0x71), Esv::PropertyReadResponse);
    assert_eq!(
        Esv::from_code(0x7E),
        Esv::PropertyReadWriteResponseExtension
    );
    assert_eq!(Esv::from_code(0x99), Esv::Unknown(0x99));
    assert_eq!(Esv::Unknown(0x99).code(), 0x99);
}

#[test]
fn eoj_class_code_and_default() {
    let eoj = Eoj::new(0x01, 0x30, 0x01);
    assert_eq!(eoj.class_code(), 0x0130);
    assert_eq!(Eoj::default().class_code(), 0x0EF0);
    assert_eq!(Eoj::default().instance, 0x01);

    let from_code = Eoj::from_class_code(0x0130, 0x02);
    assert_eq!(from_code.class_group, 0x01);
    assert_eq!(from_code.class, 0x30);
    assert_eq!(from_code.instance, 0x02);
}

#[test]
fn eoj_from_typed_class_code() {
    use echonet_lite::ecodec::Eoj as ClassCode;
    let eoj = Eoj::from(ClassCode::HomeAirConditioner);
    assert_eq!(eoj.class_code(), 0x0130);
    assert_eq!(eoj.instance, 0x01);
}

#[test]
fn unknown_esv_passthrough_round_trip() {
    let header = FrameHeader {
        tid: 0,
        seoj: Eoj::default(),
        deoj: Eoj::default(),
        esv: Esv::Unknown(0x99),
    };
    let bytes = encode(header, &[]);
    assert_eq!(bytes[10], 0x99);
    assert_eq!(parse(&bytes).unwrap().header().esv, Esv::Unknown(0x99));
}
