//! ECHONET Lite property codec operating on the generated class/property tables.

mod classes;
mod properties;

pub use classes::*;
use core::fmt;

/// Access rule for a property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Property must be supported (Get/Set/Announcement as applicable).
    Required,
    /// Property may be supported.
    Optional,
    /// Property is not applicable for this operation.
    NotApplicable,
}

/// High-level shape of a property's EDT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataKind {
    /// A fixed-width unsigned or signed integer, with an optional SI scale.
    Number,
    /// An enumeration of named byte values.
    State,
    /// An opaque byte string.
    Raw,
    /// A raw byte interpreted as a level (base + offset).
    Level,
    /// BCD-encoded time.
    Time,
    /// BCD-encoded date.
    Date,
    /// A concatenation of sub-fields.
    Object,
    /// A repeated fixed-size item.
    Array,
}

/// A named value in a state property's EDT enumeration.
#[derive(Debug, Clone, Copy)]
pub struct StateVariant {
    /// The EDT byte sequence for this value.
    pub edt: &'static [u8],
    /// Canonical name of the value.
    pub name: &'static str,
}

/// Static metadata describing one property of a class.
#[derive(Debug, Clone, Copy)]
pub struct PropertyInfo {
    /// Two-byte class code (class group + class).
    pub class: u16,
    /// EPC byte.
    pub epc: u8,
    /// English display name.
    pub name: &'static str,
    /// Get access rule.
    pub get: Access,
    /// Set access rule.
    pub set: Access,
    /// Announcement access rule.
    pub inf: Access,
    /// Shape of the EDT.
    pub kind: DataKind,
    /// Fixed width in bytes; `0` when the width is variable.
    pub size: u8,
    /// Minimum width in bytes.
    pub min_size: u8,
    /// Maximum width in bytes.
    pub max_size: u8,
    /// SI scale numerator (e.g. `1` for `0.1`).
    pub scale_num: u32,
    /// SI scale denominator (e.g. `10` for `0.1`).
    pub scale_den: u32,
    /// Whether the number is signed.
    pub signed: bool,
    /// Raw integer minimum.
    pub min: i64,
    /// Raw integer maximum.
    pub max: i64,
    /// SI unit string (e.g. `"kWh"`, `"%"`), empty when none.
    pub unit: &'static str,
    /// Base byte for a `Level` shape (0 for others).
    pub level_base: u8,
    /// Maximum offset for a `Level` shape (0 for others).
    pub level_max: u8,
    /// English description.
    pub doc: &'static str,
}

/// The result of decoding a property's EDT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdtValue<'a> {
    /// A numeric EDT, as its raw integer plus an optional SI scale and unit.
    Number {
        /// Raw (unscaled) integer value.
        raw: i64,
        /// SI scale numerator (1 when unscaled).
        scale_num: u32,
        /// SI scale denominator (1 when unscaled).
        scale_den: u32,
        /// SI unit string (empty when none).
        unit: &'static str,
    },
    /// A state EDT that matched a known value; holds its canonical name.
    /// The raw EDT bytes remain available on [`Decoded::edt`].
    State(&'static str),
    /// A non-numeric EDT passed through unchanged (raw/level/time/date/object/
    /// array, or an unmatched state byte).
    Raw(&'a [u8]),
}

/// Errors produced when decoding or validating a property EDT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The class is not in this appendix version.
    UnknownClass,
    /// The class is known, but the EPC is not defined on it.
    UnknownProperty,
    /// The EDT length is outside the allowed range for the property.
    Length,
    /// A numeric EDT is outside the property's allowed range.
    OutOfRange,
}

impl fmt::Display for DecodeError {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownClass => "unknown class for this appendix version",
            Self::UnknownProperty => "unknown property for class",
            Self::Length => "EDT length out of range",
            Self::OutOfRange => "EDT numeric value out of range",
        })
    }
}

/// The decoded value of a property.
///
/// Produced by [`decode`]; keeps the raw EDT bytes available to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded<'a> {
    /// The EDT bytes as validated against the property shape. This is the
    /// authoritative payload; [`EdtValue`] is a convenience interpretation of it.
    pub edt: &'a [u8],
    /// Interpretation of the EDT.
    pub value: EdtValue<'a>,
}

/// Look up property metadata for an EPC on a class.
///
/// Returns `None` if the class or EPC is unknown.
#[must_use]
pub fn lookup(
    class: u16,
    epc: u8,
) -> Option<&'static PropertyInfo> {
    properties::class_properties(class)
        .iter()
        .find(|p| p.epc == epc)
}

/// Decode and validate an EDT against a property's shape.
///
/// Performs a length check against the size range, and for numeric/state shapes a
/// range/value check. The caller supplies the class and EPC; the property is looked
/// up in the generated table.
///
/// # Errors
///
/// - [`DecodeError::UnknownClass`] if the class is not in this appendix version.
/// - [`DecodeError::UnknownProperty`] if the class is known but the EPC is not.
/// - [`DecodeError::Length`] if the EDT length is outside the property's range.
/// - [`DecodeError::OutOfRange`] if a numeric EDT is outside the allowed values.
pub fn decode<'a>(
    class: u16,
    epc: u8,
    edt: &'a [u8],
) -> Result<Decoded<'a>, DecodeError> {
    let info = lookup(class, epc).ok_or_else(|| {
        if properties::class_properties(class).is_empty() {
            DecodeError::UnknownClass
        } else {
            DecodeError::UnknownProperty
        }
    })?;
    decode_with(info, edt)
}

/// Typed variant of [`decode`] that takes an [`Eoj`] instead of a raw class code.
///
/// # Errors
///
/// Same as [`decode`]; [`DecodeError::UnknownClass`] can occur when the [`Eoj`]
/// is an [`Eoj::Unknown`] code not present in this appendix version.
pub fn decode_eoj<'a>(
    eoj: Eoj,
    epc: u8,
    edt: &'a [u8],
) -> Result<Decoded<'a>, DecodeError> {
    decode(eoj.class_code(), epc, edt)
}

/// Decode and validate an EDT against a known [`PropertyInfo`].
///
/// # Errors
///
/// - [`DecodeError::Length`] if the EDT length is outside the property's range.
/// - [`DecodeError::OutOfRange`] if a numeric EDT is outside the allowed values.
pub fn decode_with<'a>(
    info: &'static PropertyInfo,
    edt: &'a [u8],
) -> Result<Decoded<'a>, DecodeError> {
    if edt.len() < usize::from(info.min_size) || edt.len() > usize::from(info.max_size) {
        return Err(DecodeError::Length);
    }
    let value = match info.kind {
        DataKind::Number => {
            let raw = read_int(edt, info.signed);
            if raw < info.min
                || raw > info.max
                || !properties::number_allowed(info.class, info.epc, raw)
            {
                return Err(DecodeError::OutOfRange);
            }
            EdtValue::Number {
                raw,
                scale_num: info.scale_num,
                scale_den: info.scale_den,
                unit: info.unit,
            }
        },
        DataKind::State => {
            let variants = properties::state_variants(info.class, info.epc);
            match variants.iter().find(|v| v.edt == edt) {
                Some(v) => EdtValue::State(v.name),
                None => EdtValue::Raw(edt),
            }
        },
        _ => EdtValue::Raw(edt),
    };
    Ok(Decoded { edt, value })
}

/// Read a big-endian integer from `edt`, sign-extending when `signed`.
fn read_int(
    edt: &[u8],
    signed: bool,
) -> i64 {
    let mut acc = 0i64;
    for &b in edt {
        acc = (acc << 8) | i64::from(b);
    }
    if signed {
        // Sign-extend from the top byte.
        let bits = edt.len().saturating_mul(8);
        if bits < 64 && bits > 0 {
            let shift = 64 - bits;
            acc = (acc << shift) >> shift;
        }
    }
    acc
}
