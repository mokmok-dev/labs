//! ECHONET Lite radar service for the GPUI and web shell.
//!
//! The headless discovery and polling logic lives in [`echonet_radar_core`];
//! this crate re-exports it so the shell keeps using `echonet_radar::*`.
pub use echonet_radar_core::*;
