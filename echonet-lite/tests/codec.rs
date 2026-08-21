//! End-to-end tests of the generated ECHONET Lite codec against the MRA data.

use echonet_lite::ecodec::Eoj;
use echonet_lite::ecodec::class_0000::Epc as Epc00;
use echonet_lite::ecodec::class_0130::Epc as Epc0130;
use echonet_lite::{Access, DataKind, DecodeError, EdtValue, decode, decode_eoj, lookup};

#[test]
fn superclass_operation_status_is_state() {
    // Super class 0x0000, EPC 0x80 (Operation status): state 0x30 -> "true".
    let d = decode(0x0000, 0x80, &[0x30]).unwrap();
    assert_eq!(d.value, EdtValue::State("true"));
    assert_eq!(d.edt, &[0x30]);
}

#[test]
fn superclass_operation_status_false() {
    let d = decode(0x0000, 0x80, &[0x31]).unwrap();
    assert_eq!(d.value, EdtValue::State("false"));
}

#[test]
fn unknown_state_byte_passes_through_raw() {
    // 0x32 is not a defined operation-status value.
    let d = decode(0x0000, 0x80, &[0x32]).unwrap();
    assert_eq!(d.value, EdtValue::Raw(&[0x32]));
}

#[test]
fn cumulative_energy_is_scaled_number() {
    // Super class 0x0000, EPC 0x85: uint32 with scale 1/1000 kWh.
    let d = decode(0x0000, 0x85, &[0x00, 0x00, 0x00, 0x0A]).unwrap();
    assert_eq!(
        d.value,
        EdtValue::Number {
            raw: 10,
            scale_num: 1,
            scale_den: 1000,
            unit: "kWh",
        }
    );
}

#[test]
fn wrong_length_is_rejected() {
    // 0x85 requires exactly 4 bytes.
    assert_eq!(
        decode(0x0000, 0x85, &[0x00, 0x01]),
        Err(DecodeError::Length)
    );
}

#[test]
fn unknown_class_or_epc_yields_error() {
    // An unknown class code is distinguishable from a known class with an
    // unknown EPC.
    assert_eq!(
        decode(0xFFFF, 0x80, &[0x30]),
        Err(DecodeError::UnknownClass)
    );
    // 0xF0 is not a defined EPC on the super class.
    assert_eq!(
        decode(0x0000, 0xF0, &[0x30]),
        Err(DecodeError::UnknownProperty)
    );
}

#[test]
fn eoj_from_code_and_typed_decode() {
    assert_eq!(Eoj::from_code(0x0130), Eoj::HomeAirConditioner);
    assert_eq!(Eoj::from_code(0xFFFF), Eoj::Unknown(0xFFFF));
    assert_eq!(Eoj::Unknown(0x1234).class_code(), 0x1234);

    // Typed entry point behaves identically to the raw-code variant.
    let raw = decode(0x0130, 0x80, &[0x30]).unwrap();
    let typed = decode_eoj(Eoj::HomeAirConditioner, 0x80, &[0x30]).unwrap();
    assert_eq!(raw, typed);
}

#[test]
fn enum_typed_number_validates_membership() {
    // 0x026B 0xC8 "Standard time to start heating" is an enum-typed number with
    // allowed values 1, 20..=24 — not a contiguous range.
    let ok = decode(0x026B, 0xC8, &[20]).unwrap();
    assert_eq!(
        ok.value,
        EdtValue::Number {
            raw: 20,
            scale_num: 1,
            scale_den: 1,
            unit: "",
        }
    );
    // 5 is between min and max but not in the allowed set.
    assert_eq!(decode(0x026B, 0xC8, &[5]), Err(DecodeError::OutOfRange));
}

#[test]
fn lookup_returns_metadata() {
    let p = lookup(0x0000, 0x80).expect("0x80 should be known on super class");
    assert_eq!(p.epc, 0x80);
    assert_eq!(p.name, "Operation status");
    assert_eq!(p.kind, DataKind::State);
    assert_eq!(p.get, Access::Required);
}

#[test]
fn lookup_unknown_returns_none() {
    assert!(lookup(0x0000, 0xFE).is_none());
    assert!(lookup(0xFFFF, 0x80).is_none());
}

#[test]
fn eoj_class_codes() {
    assert_eq!(Eoj::Common.class_code(), 0x0000);
    assert_eq!(Eoj::HomeAirConditioner.class_code(), 0x0130);
    assert_eq!(Eoj::NodeProfile.class_code(), 0x0EF0);
}

#[test]
fn epc_code_and_from_code() {
    // Super class Operation status is 0x80.
    assert_eq!(Epc00::OperationStatus.code(), 0x80);
    assert_eq!(Epc00::from_code(0x80), Epc00::OperationStatus);
    assert_eq!(Epc00::from_code(0x09), Epc00::Unknown(0x09));
    // Home air conditioner: values from the MRA.
    let code = Epc0130::from_code(0xB0);
    assert_eq!(code.code(), 0xB0);
}

#[test]
fn home_air_conditioner_operation_status() {
    let d = decode(0x0130, 0x80, &[0x30]).unwrap();
    assert_eq!(d.value, EdtValue::State("true"));
}

#[test]
fn node_profile_instance_list_passes_through() {
    // Node profile 0x0EF0, EPC 0xD5 (instance list notification): object of variable
    // length, passed through as raw after length validation.
    let edt = [0x02, 0x05, 0xFF, 0x01, 0x01, 0x02, 0x01];
    let d = decode(0x0EF0, 0xD5, &edt).unwrap();
    assert_eq!(d.value, EdtValue::Raw(&edt[..]));
}
