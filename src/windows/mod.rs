//! Windows async COM
//!
//! Modeled after mio's windows TCP module.
#![cfg(windows)]

mod serial;
mod from_raw_arc;

pub use self::serial::Serial;