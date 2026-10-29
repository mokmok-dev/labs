use std::time::Duration;

use echonet_lite::frame::{Eoj, Esv, FrameHeader};

/// The controller EOJ used as the source of requests.
pub const CONTROLLER_EOJ: Eoj = Eoj::new(0x05, 0xFF, 0x01);
/// The wildcard node-profile EOJ used for discovery requests.
pub const DISCOVERY_NODE_PROFILE_EOJ: Eoj = Eoj::new(0x0E, 0xF0, 0x00);
/// The controller class: the class of the node that drives other objects.
pub const CONTROLLER_CLASS_CODE: u16 = 0x05FF;
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

pub const EMPTY_EDT: &[u8] = &[];

const MAX_INSTANCE_LIST_ITEMS: usize = 84;

/// Whether a response service code answers a GET request.
///
/// `0x52` (GET response with status) is used when at least one requested
/// property could not be read; the properties that could be read still carry
/// their EDT, so the frame must not be discarded as a whole.
pub const fn is_get_response_code(code: u8) -> bool {
    code == GET_RESPONSE_ESV_CODE || code == GET_RESPONSE_WITH_STATUS_ESV_CODE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    MissingCount,
    TooManyInstances,
    InvalidInstanceListLength,
    InvalidPropertyMapLength,
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

#[must_use]
pub const fn discovery_header(tid: u16) -> FrameHeader {
    FrameHeader {
        tid,
        seoj: CONTROLLER_EOJ,
        deoj: DISCOVERY_NODE_PROFILE_EOJ,
        esv: Esv::Unknown(GET_ESV_CODE),
    }
}

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

/// # Errors
///
/// [`ProtocolError::MissingCount`] when the count byte is absent,
/// [`ProtocolError::TooManyInstances`] above the MRA limit, and
/// [`ProtocolError::InvalidInstanceListLength`] when the length disagrees with
/// the count.
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

/// A count of 15 or less uses the compact EPC-list representation; larger
/// counts use the 16-byte bitmap covering EPC `0x80..=0xFF`.
///
/// # Errors
///
/// [`ProtocolError::MissingCount`] when the count byte is absent, and
/// [`ProtocolError::InvalidPropertyMapLength`] or
/// [`ProtocolError::InvalidPropertyMapCount`] when the body disagrees with it.
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

#[cfg(test)]
mod tests {
    use super::*;
    use echonet_lite::frame::{Property, write};

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
}
