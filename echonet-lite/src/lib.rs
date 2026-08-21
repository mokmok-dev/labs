//! `no_std` ECHONET Lite property parser.
//!
//! This crate parses and validates ECHONET Lite property data (EDT) for the
//! property/object classes defined in the ECHONET Lite Machine Readable Appendix
//! (MRA). The class/property enumeration is **code-generated** from the appendix by
//! the `echonet-lite-codegen` tool, so the generated surface tracks the official
//! appendix without hand maintenance.
//!
//! # no_std
//!
//! Everything here is `no_std` (`#![no_std]`) and allocation-free.

#![no_std]

pub mod ecodec;
pub mod frame;

pub use ecodec::{
    Access, DataKind, DecodeError, Decoded, EdtValue, Eoj, PropertyInfo, StateVariant, decode,
    decode_eoj, decode_with, lookup,
};
