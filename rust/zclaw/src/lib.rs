//! zclaw — mobile agent library (FFI v0.2)
//! Faithful rebuild of the HarmonyOS libzclaw.so contract:
//! init / chat / poll_chunks / is_running / cancel / get_sessions / get_messages / free / version

pub mod agent;
pub mod config;
pub mod ffi;
pub mod memory;
pub mod providers;
pub mod tools;
