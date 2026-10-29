//! Property value rendering: MRA decoding, the union table and power helpers.

use std::fmt::Write as _;

use echonet_lite::ecodec::{EdtValue, decode, lookup};

/// Format an EDT as the text the shells display.
///
/// Uses the generated MRA codec, or the union table for the properties the
/// appendix defines as `oneOf [number, sentinel state]`. Boolean states render
/// as `ON`/`OFF`, a union sentinel renders as its appendix name (`NoData`,
/// `Undefined`, `Unmeasurable`), and anything undecodable renders as hex bytes.
///
/// # Examples
///
/// ```
/// # use echonet_radar_core::format_value;
/// assert_eq!(format_value(0x0130, 0x80, &[0x30]), "ON");
/// assert_eq!(
///     format_value(0x0287, 0xC6, &[0x00, 0x00, 0x02, 0x44]),
///     "580 W"
/// );
/// ```
#[must_use]
pub fn format_value(
    class_code: u16,
    epc: u8,
    edt: &[u8],
) -> String {
    if let Some(value) = union_value(class_code, epc, edt) {
        return match value {
            UnionValue::Number(union, raw) => format_with_unit(raw.to_string(), union.unit),
            UnionValue::Sentinel(union) => String::from(union.sentinel_name),
        };
    }
    match decode(class_code, epc, edt).map(|decoded| decoded.value) {
        Ok(EdtValue::Number {
            raw,
            scale_num,
            scale_den,
            unit,
        }) => format_number(raw, scale_num, scale_den, unit),
        Ok(EdtValue::State(name)) => state_name(name),
        Ok(EdtValue::Raw(bytes)) => format_bytes(bytes),
        Err(_) => format_bytes(edt),
    }
}

fn state_name(name: &str) -> String {
    match name {
        "true" => String::from("ON"),
        "false" => String::from("OFF"),
        _ => String::from(name),
    }
}

/// Build a human-readable English description of a property's EDT, e.g.
/// `"Operation status ON"` or `"Room temperature 26 Celsius"`.
#[must_use]
pub fn format_edt(
    class_code: u16,
    epc: u8,
    edt: &[u8],
) -> String {
    let name = lookup(class_code, epc).map_or_else(
        || format!("EPC 0x{epc:02X}"),
        |info| String::from(info.name),
    );
    format!("{name} {}", format_value(class_code, epc, edt))
}

fn format_number(
    raw: i64,
    scale_num: u32,
    scale_den: u32,
    unit: &str,
) -> String {
    if scale_den == 0 {
        return format_with_unit(raw.to_string(), unit);
    }

    let scaled = i128::from(raw) * i128::from(scale_num);
    let negative = scaled.is_negative();
    let magnitude = scaled.abs();
    let denominator = i128::from(scale_den);
    let whole = magnitude / denominator;
    let mut result = whole.to_string();
    let mut remainder = magnitude % denominator;

    if remainder != 0 {
        let mut fraction = String::new();
        for _ in 0..6 {
            remainder *= 10;
            let digit = remainder / denominator;
            remainder %= denominator;
            if let Ok(digit) = u8::try_from(digit) {
                fraction.push(char::from(b'0' + digit));
            }
            if remainder == 0 {
                break;
            }
        }
        while fraction.ends_with('0') {
            fraction.pop();
        }
        if !fraction.is_empty() {
            result.push('.');
            result.push_str(&fraction);
        }
    }

    if negative && scaled != 0 {
        result.insert(0, '-');
    }
    format_with_unit(result, unit)
}

fn format_with_unit(
    mut value: String,
    unit: &str,
) -> String {
    if !unit.is_empty() {
        value.push(' ');
        value.push_str(unit);
    }
    value
}

/// Render raw bytes as hex, each byte marked with `0x`.
///
/// A bare `10` reads as the number ten, and several properties that carry raw
/// bytes are temperatures or settings a reader would take for decimals.
fn format_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::from("(empty)");
    }
    let mut value = String::with_capacity(bytes.len() * 5 - 1);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            value.push(' ');
        }
        let _ = write!(value, "0x{byte:02X}");
    }
    value
}

/// Numbers the generated MRA tables cannot express: the appendix defines these
/// properties as `oneOf [number, sentinel state]`, and the codegen maps every
/// union to `DataKind::Raw`.
///
/// A key item for a union property needs a row here, or a codegen change that
/// emits the number arm with its range and sentinel.
struct UnionNumber {
    class: u16,
    epc: u8,
    width: u8,
    signed: bool,
    min: i64,
    max: i64,
    unit: &'static str,
    sentinel: &'static [u8],
    sentinel_name: &'static str,
}

const UNION_NUMBERS: &[UnionNumber] = &[
    // 0x0130 0xB3 set temperature, 0xBB room temperature (appendix unions).
    UnionNumber {
        class: 0x0130,
        epc: 0xB3,
        width: 1,
        signed: false,
        min: 0,
        max: 50,
        unit: "Celsius",
        sentinel: &[0xFD],
        sentinel_name: "Undefined",
    },
    UnionNumber {
        class: 0x0130,
        epc: 0xBB,
        width: 1,
        signed: true,
        min: -127,
        max: 125,
        unit: "Celsius",
        sentinel: &[0x7E],
        sentinel_name: "Unmeasurable",
    },
    // 0x0287 0xC6 board power, 0x0288 0xE7 smart meter power (signed, net).
    UnionNumber {
        class: 0x0287,
        epc: 0xC6,
        width: 4,
        signed: true,
        min: -2_147_483_647,
        max: 2_147_483_645,
        unit: "W",
        sentinel: &[0x7F, 0xFF, 0xFF, 0xFE],
        sentinel_name: "NoData",
    },
    UnionNumber {
        class: 0x0288,
        epc: 0xE7,
        width: 4,
        signed: true,
        min: -2_147_483_647,
        max: 2_147_483_645,
        unit: "W",
        sentinel: &[0x7F, 0xFF, 0xFF, 0xFE],
        sentinel_name: "NoData",
    },
];

enum UnionValue {
    Number(&'static UnionNumber, i64),
    Sentinel(&'static UnionNumber),
}

/// The EPC that reports instantaneous electric power for a class.
#[must_use]
pub const fn power_epc(class_code: u16) -> Option<u8> {
    match class_code {
        0x0279 => Some(0xE0),
        0x027D => Some(0xD3),
        0x0287 => Some(0xC6),
        0x0288 => Some(0xE7),
        _ => None,
    }
}

/// Instantaneous electric power in watts from the class's power property.
///
/// Returns `None` when the device reports the appendix's `NoData` sentinel or
/// the bytes do not decode as a measured value.
#[must_use]
pub fn instantaneous_watts(
    class_code: u16,
    edt: &[u8],
) -> Option<i64> {
    let epc = power_epc(class_code)?;
    match union_value(class_code, epc, edt) {
        Some(UnionValue::Number(_, raw)) => return Some(raw),
        Some(UnionValue::Sentinel(_)) => return None,
        None => {},
    }
    match decode(class_code, epc, edt).ok()?.value {
        EdtValue::Number {
            raw,
            scale_num,
            scale_den,
            unit: "W",
        } => scale_number(raw, scale_num, scale_den),
        _ => None,
    }
}

fn union_value(
    class_code: u16,
    epc: u8,
    edt: &[u8],
) -> Option<UnionValue> {
    let union = UNION_NUMBERS
        .iter()
        .find(|union| union.class == class_code && union.epc == epc)?;
    if edt == union.sentinel {
        return Some(UnionValue::Sentinel(union));
    }
    if edt.len() != usize::from(union.width) {
        return None;
    }
    let raw = read_int(edt, union.signed);
    (union.min..=union.max)
        .contains(&raw)
        .then_some(UnionValue::Number(union, raw))
}

fn read_int(
    edt: &[u8],
    signed: bool,
) -> i64 {
    let mut acc = 0i64;
    for &byte in edt {
        acc = (acc << 8) | i64::from(byte);
    }
    if signed {
        let bits = edt.len().saturating_mul(8);
        if bits > 0 && bits < 64 {
            let shift = 64 - bits;
            acc = (acc << shift) >> shift;
        }
    }
    acc
}

fn scale_number(
    raw: i64,
    scale_num: u32,
    scale_den: u32,
) -> Option<i64> {
    if scale_den == 0 {
        return None;
    }
    let scaled = i128::from(raw) * i128::from(scale_num) / i128::from(scale_den);
    i64::try_from(scaled).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_values_are_decoded_with_units() {
        assert_eq!(format_value(0x0011, 0xE0, &[0x01, 0x04]), "26 Celsius");
        assert_eq!(format_value(0x0012, 0xE0, &[55]), "55 %");
    }

    #[test]
    fn format_value_decodes_union_numbers() {
        assert_eq!(format_value(0x0130, 0xB3, &[0x1A]), "26 Celsius");
        assert_eq!(format_value(0x0130, 0xB3, &[0xFD]), "Undefined");
        assert_eq!(format_value(0x0130, 0xBB, &[0x7E]), "Unmeasurable");
        assert_eq!(
            format_value(0x0287, 0xC6, &[0x00, 0x00, 0x02, 0x44]),
            "580 W"
        );
        assert_eq!(
            format_value(0x0287, 0xC6, &[0x7F, 0xFF, 0xFF, 0xFE]),
            "NoData"
        );
        assert_eq!(format_value(0x0130, 0x80, &[0x30]), "ON");
        assert_eq!(format_value(0x0130, 0x80, &[0x31]), "OFF");
    }

    #[test]
    fn instantaneous_watts_reads_the_power_property() {
        assert_eq!(power_epc(0x0288), Some(0xE7));
        assert_eq!(
            instantaneous_watts(0x0287, &[0x00, 0x00, 0x02, 0x44]),
            Some(580)
        );
        assert_eq!(instantaneous_watts(0x0287, &[0x7F, 0xFF, 0xFF, 0xFE]), None);
        assert_eq!(instantaneous_watts(0x0287, &[0x02, 0x44]), None);
        assert_eq!(instantaneous_watts(0x0130, &[0x30]), None);
        assert_eq!(instantaneous_watts(0x0279, &[0x04, 0xD8]), Some(1240));
        assert_eq!(
            instantaneous_watts(0x027D, &[0xFF, 0xFF, 0xFF, 0x38]),
            Some(-200)
        );
    }

    #[test]
    fn unknown_values_are_kept_as_hex() {
        assert_eq!(format_value(0xFFFF, 0x80, &[0x01, 0xAF]), "0x01 0xAF");
    }

    #[test]
    fn format_edt_prepends_the_property_name() {
        // 0x80 on an air conditioner is "Operation status" with state ON/OFF.
        assert_eq!(format_edt(0x0130, 0x80, &[0x30]), "Operation status ON");
        assert_eq!(
            format_edt(0x0130, 0xBB, &[0x01, 0x1E]),
            "Measured value of room temperature 0x01 0x1E"
        );
        assert_eq!(format_edt(0xFFFF, 0x80, &[0x01]), "EPC 0x80 0x01");
    }
}
