//! ECHONET Lite frame (EHD/TID/EOJ/ESV/OPC) codec.
//!
//! This module parses and serializes the ECHONET Lite message layer defined in
//! Part II of the ECHONET Lite specification (Chapter 4, message format). It is
//! `no_std` and allocation-free: parsing borrows from the input buffer and
//! serializing writes into a caller-provided buffer.
//!
//! The property payloads (EDT) carried in a frame are opaque byte sequences here.
//! Interpret them with the property parser in [`crate::ecodec`].

/// EHD1 value identifying an ECHONET Lite frame (`0x10`).
pub const EHD1: u8 = 0x10;
/// EHD2 value for the ECHONET Lite frame format 1 (`0x81`).
pub const EHD2: u8 = 0x81;

/// Number of header bytes before the OPC / property region.
const HEADER_LEN: usize = 12;

/// ECHONET Lite service code (ESV).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Esv {
    /// Property read request (`0x60`).
    PropertyReadRequest,
    /// Property write request, no response (`0x61`).
    PropertyWriteRequestNoResponse,
    /// Property write request, response required (`0x62`).
    PropertyWriteRequestResponseRequired,
    /// Property notification (`0x63`).
    PropertyNotification,
    /// Property read/write request (`0x6E`).
    PropertyReadWriteRequest,
    /// Property read response (`0x71`).
    PropertyReadResponse,
    /// Property write response (`0x72`).
    PropertyWriteResponse,
    /// Property notification response (`0x73`).
    PropertyNotificationResponse,
    /// Property read/write response (`0x74`).
    PropertyReadWriteResponse,
    /// Property read/write response, extension (`0x7E`).
    PropertyReadWriteResponseExtension,
    /// An ESV value not defined by this version of the specification.
    Unknown(u8),
}

impl Esv {
    /// The one-byte service code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::PropertyReadRequest => 0x60,
            Self::PropertyWriteRequestNoResponse => 0x61,
            Self::PropertyWriteRequestResponseRequired => 0x62,
            Self::PropertyNotification => 0x63,
            Self::PropertyReadWriteRequest => 0x6E,
            Self::PropertyReadResponse => 0x71,
            Self::PropertyWriteResponse => 0x72,
            Self::PropertyNotificationResponse => 0x73,
            Self::PropertyReadWriteResponse => 0x74,
            Self::PropertyReadWriteResponseExtension => 0x7E,
            Self::Unknown(v) => v,
        }
    }

    /// Resolve a one-byte service code to a known variant, or
    /// [`Self::Unknown`] when the code is not defined.
    #[must_use]
    pub const fn from_code(code: u8) -> Self {
        match code {
            0x60 => Self::PropertyReadRequest,
            0x61 => Self::PropertyWriteRequestNoResponse,
            0x62 => Self::PropertyWriteRequestResponseRequired,
            0x63 => Self::PropertyNotification,
            0x6E => Self::PropertyReadWriteRequest,
            0x71 => Self::PropertyReadResponse,
            0x72 => Self::PropertyWriteResponse,
            0x73 => Self::PropertyNotificationResponse,
            0x74 => Self::PropertyReadWriteResponse,
            0x7E => Self::PropertyReadWriteResponseExtension,
            v => Self::Unknown(v),
        }
    }
}

/// A three-byte ECHONET Lite object (EOJ): class group code, class code, and
/// instance code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Eoj {
    /// Class group code (high byte of the two-byte class code).
    pub class_group: u8,
    /// Class code (low byte of the two-byte class code).
    pub class: u8,
    /// Instance code.
    pub instance: u8,
}

impl Eoj {
    /// Construct an EOJ from its three component codes.
    #[must_use]
    pub const fn new(
        class_group: u8,
        class: u8,
        instance: u8,
    ) -> Self {
        Self {
            class_group,
            class,
            instance,
        }
    }

    /// Construct an EOJ from a two-byte class code and an instance code.
    #[must_use]
    pub const fn from_class_code(
        class_code: u16,
        instance: u8,
    ) -> Self {
        Self {
            class_group: (class_code >> 8) as u8,
            class: class_code as u8,
            instance,
        }
    }

    /// The two-byte class code (class group + class).
    #[must_use]
    pub const fn class_code(self) -> u16 {
        ((self.class_group as u16) << 8) | self.class as u16
    }
}

impl From<crate::ecodec::Eoj> for Eoj {
    /// Build a frame EOJ from a typed class code, using instance `0x01` (the
    /// conventional instance for single-instance devices).
    fn from(class: crate::ecodec::Eoj) -> Self {
        Self::from_class_code(class.class_code(), 0x01)
    }
}

impl Default for Eoj {
    /// The node profile object, instance `0x01` (the standard node profile
    /// instance), a common default when no specific object is known.
    fn default() -> Self {
        Self::new(0x0E, 0xF0, 0x01)
    }
}

/// One property carried in a frame: an EPC and its EDT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Property<'a> {
    /// ECHONET property code (EPC).
    pub epc: u8,
    /// Property value data (EDT). Empty for a read request.
    pub edt: &'a [u8],
}

/// The header portion of an ECHONET Lite frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Transaction ID (TID).
    pub tid: u16,
    /// Source EOJ (SEOJ).
    pub seoj: Eoj,
    /// Destination EOJ (DEOJ).
    pub deoj: Eoj,
    /// Service code (ESV).
    pub esv: Esv,
}

/// A parsed ECHONET Lite frame, borrowing its property bytes from the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    header: FrameHeader,
    props: &'a [u8],
}

impl<'a> Frame<'a> {
    /// The parsed header.
    #[must_use]
    pub const fn header(&self) -> FrameHeader {
        self.header
    }

    /// Iterate over the properties carried in the frame.
    #[must_use]
    pub fn properties(&self) -> PropertyIter<'a> {
        PropertyIter { data: self.props }
    }
}

/// An iterator over the properties of a parsed [`Frame`].
#[derive(Debug, Clone, Copy)]
pub struct PropertyIter<'a> {
    data: &'a [u8],
}

impl<'a> Iterator for PropertyIter<'a> {
    type Item = Property<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.data.len() < 2 {
            return None;
        }
        let epc = self.data[0];
        let pdc = usize::from(self.data[1]);
        let edt = self.data.get(2..2 + pdc)?;
        self.data = &self.data[2 + pdc..];
        Some(Property { epc, edt })
    }
}

/// Errors produced when parsing or serializing a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The buffer is shorter than the fixed frame header.
    TruncatedHeader,
    /// EHD1 does not identify an ECHONET Lite frame.
    BadEhd1,
    /// EHD2 is not the supported ECHONET Lite frame format.
    BadEhd2,
    /// A property is truncated: the declared PDC overruns the buffer.
    TruncatedProperty,
    /// Bytes remain after the last declared property.
    TrailingData,
    /// More than 255 properties, or the OPC byte cannot represent them.
    TooManyProperties,
    /// A property EDT is longer than 255 bytes and cannot fit in a one-byte PDC.
    EdtTooLong,
    /// The output buffer is too small for the serialized frame.
    BufferTooSmall,
}

impl core::fmt::Display for FrameError {
    fn fmt(
        &self,
        f: &mut core::fmt::Formatter<'_>,
    ) -> core::fmt::Result {
        f.write_str(match self {
            Self::TruncatedHeader => "frame shorter than the fixed header",
            Self::BadEhd1 => "EHD1 is not an ECHONET Lite frame",
            Self::BadEhd2 => "EHD2 is not the supported ECHONET Lite frame format",
            Self::TruncatedProperty => "property data truncated",
            Self::TrailingData => "bytes remain after the last declared property",
            Self::TooManyProperties => "more than 255 properties",
            Self::EdtTooLong => "property EDT longer than 255 bytes",
            Self::BufferTooSmall => "output buffer too small",
        })
    }
}

/// Parse an ECHONET Lite frame from `data`.
///
/// The returned [`Frame`] borrows its property bytes from `data`.
///
/// # Errors
///
/// - [`FrameError::TruncatedHeader`] if `data` is shorter than the fixed header.
/// - [`FrameError::BadEhd1`] if EHD1 is not [`EHD1`].
/// - [`FrameError::BadEhd2`] if EHD2 is not [`EHD2`].
/// - [`FrameError::TruncatedProperty`] if a property overruns the buffer.
/// - [`FrameError::TrailingData`] if bytes remain after the declared properties.
pub fn parse(data: &[u8]) -> Result<Frame<'_>, FrameError> {
    let head = data.get(..HEADER_LEN).ok_or(FrameError::TruncatedHeader)?;
    if head[0] != EHD1 {
        return Err(FrameError::BadEhd1);
    }
    if head[1] != EHD2 {
        return Err(FrameError::BadEhd2);
    }
    let header = FrameHeader {
        tid: u16::from_be_bytes([head[2], head[3]]),
        seoj: Eoj {
            class_group: head[4],
            class: head[5],
            instance: head[6],
        },
        deoj: Eoj {
            class_group: head[7],
            class: head[8],
            instance: head[9],
        },
        esv: Esv::from_code(head[10]),
    };
    let opc = usize::from(head[11]);
    let props = data
        .get(HEADER_LEN..)
        .ok_or(FrameError::TruncatedProperty)?;
    validate_props(props, opc)?;
    Ok(Frame { header, props })
}

/// Validate that the property region contains exactly `opc` well-formed
/// properties.
fn validate_props(
    props: &[u8],
    opc: usize,
) -> Result<(), FrameError> {
    let mut rest = props;
    for _ in 0..opc {
        let epc_len = rest.get(..2).ok_or(FrameError::TruncatedProperty)?;
        let pdc = usize::from(epc_len[1]);
        rest = rest.get(2 + pdc..).ok_or(FrameError::TruncatedProperty)?;
    }
    if !rest.is_empty() {
        // Trailing bytes beyond the declared OPC are not a well-formed frame.
        return Err(FrameError::TrailingData);
    }
    Ok(())
}

/// Serialize a frame header and its properties into `out`.
///
/// Returns the number of bytes written.
///
/// # Errors
///
/// - [`FrameError::TooManyProperties`] if there are more than 255 properties.
/// - [`FrameError::EdtTooLong`] if a property EDT is longer than 255 bytes.
/// - [`FrameError::BufferTooSmall`] if `out` cannot hold the frame.
pub fn write(
    header: FrameHeader,
    properties: &[Property<'_>],
    out: &mut [u8],
) -> Result<usize, FrameError> {
    let opc = u8::try_from(properties.len()).map_err(|_| FrameError::TooManyProperties)?;
    let mut total = HEADER_LEN;
    for p in properties {
        if p.edt.len() > u8::MAX as usize {
            return Err(FrameError::EdtTooLong);
        }
        total += 2 + p.edt.len();
    }
    if total > out.len() {
        return Err(FrameError::BufferTooSmall);
    }
    out[0] = EHD1;
    out[1] = EHD2;
    out[2..4].copy_from_slice(&header.tid.to_be_bytes());
    out[4] = header.seoj.class_group;
    out[5] = header.seoj.class;
    out[6] = header.seoj.instance;
    out[7] = header.deoj.class_group;
    out[8] = header.deoj.class;
    out[9] = header.deoj.instance;
    out[10] = header.esv.code();
    out[11] = opc;
    let mut pos = HEADER_LEN;
    for p in properties {
        out[pos] = p.epc;
        out[pos + 1] = p.edt.len() as u8;
        out[pos + 2..pos + 2 + p.edt.len()].copy_from_slice(p.edt);
        pos += 2 + p.edt.len();
    }
    Ok(total)
}
