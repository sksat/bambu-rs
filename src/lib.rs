//! `bambu-rs` — a clean-room library and CLI for monitoring and driving
//! [Bambu Lab](https://bambulab.com/) 3D printers over the LAN.
//!
//! This crate is built **clean-room** from the protocol documentation
//! ([OpenBambuAPI](https://github.com/Doridian/OpenBambuAPI)) and direct
//! observation of real hardware — it does not depend on, nor copy from, any
//! existing Bambu library implementation.
//!
//! The library ([`core`]) is the first-class, reusable artifact: it owns the
//! pure protocol logic (state model, delta merge, command envelopes, the
//! firmware capability registry, the safety policy and `verify` predicates)
//! with **no I/O**. The `bambu` binary is a thin consumer of this library, and
//! a future MCP server will be another.
//!
//! Reliable *control* requires the printer to be in **LAN-only + Developer
//! Mode** (since the Jan-2025 Authorization Control System); read-only
//! telemetry is broadly available.

pub mod camera;
#[cfg(feature = "cli")]
pub mod cli;
pub mod client;
pub mod config;
pub mod core;
pub mod ftp;
#[cfg(feature = "server")]
pub mod server;
