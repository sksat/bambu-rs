//! `core` — pure, I/O-free protocol logic. This is the main test surface:
//! every item here is unit-testable without a network or a real printer.
//!
//! Submodules are added one TDD slice at a time.

pub mod capability;
pub mod command;
#[cfg(test)]
pub(crate) mod fake;
pub mod firmware;
pub mod hms;
pub mod model;
pub mod project;
pub mod report;
pub mod safety;
pub mod session;
pub mod stage;
pub mod start;
pub mod status;
pub mod timelapse;
pub mod verify;
pub mod version;
