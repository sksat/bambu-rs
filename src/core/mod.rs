//! `core` — pure, I/O-free protocol logic. This is the main test surface:
//! every item here is unit-testable without a network or a real printer.
//!
//! Submodules are added one TDD slice at a time.

pub mod camerad;
pub mod capability;
pub mod command;
#[cfg(feature = "relay")]
pub mod detect;
#[cfg(feature = "relay")]
pub mod emulate;
#[cfg(test)]
pub(crate) mod fake;
pub mod firmware;
#[cfg(feature = "relay")]
pub mod ftpd;
pub mod hms;
pub mod model;
#[cfg(feature = "relay")]
pub mod mqtt;
pub mod park;
pub mod project;
pub mod report;
pub mod safety;
pub mod sequence;
pub mod session;
pub mod settle;
pub mod slice;
pub mod stage;
pub mod start;
pub mod status;
pub mod timelapse;
pub mod verify;
pub mod version;
