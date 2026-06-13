//! `core` — pure, I/O-free protocol logic. This is the main test surface:
//! every item here is unit-testable without a network or a real printer.
//!
//! Submodules are added one TDD slice at a time.

pub mod firmware;
