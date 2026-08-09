//! The printer's control surface, read as a game controller.
//!
//! `bambu serve --emulate` already sees every command a LAN client sends before
//! it is forwarded ([`crate::core::emulate`]). This turns those commands into
//! DOOM input instead: a jog on Y is a step forward, the home button is the
//! trigger, the chamber light opens doors. The picture goes back the other way,
//! through the camera port ([`crate::core::camerad`]), so a client that thinks
//! it is watching a print is watching E1M1.
//!
//! Pure, like the rest of `core`: a request payload in, key presses out. The
//! clock belongs to the caller — every entry point takes a monotonic
//! millisecond stamp — so the hold logic is driven from a test without waiting
//! for anything.
//!
//! **A press has a duration**, which is the one thing that makes this more than
//! a lookup table. DOOM moves you for as long as a key is down, so a button
//! that is only ever tapped would leave the player twitching on the spot. The
//! duration comes from the jog itself: the millimetres the client asked the
//! machine to move become the milliseconds the key is held, so Studio's 1 mm
//! and 10 mm buttons are a nudge and a step.
//!
//! **The game comes back the other way too**, and not only as pictures: the
//! player's health is reported as the nozzle temperature, so a client that
//! knows nothing about any of this still shows the game state on the readout it
//! puts in the largest type on the screen. See [`Vitals`].

pub mod input;
pub mod vitals;
pub mod wire;

// The pieces are one feature and are used together, so they are re-exported
// flat: splitting the file should not move anything for a caller.
pub use input::{Event, Hold, Key, KeyPad, holds_for};
pub use vitals::{Vitals, bed_for_armour, nozzle_for_health, report_vitals};
pub use wire::{
    MAX_STATUS, Record, RecordError, STATUS_MAGIC, classify_record, parse_vitals, status_header,
    vitals_payload,
};
