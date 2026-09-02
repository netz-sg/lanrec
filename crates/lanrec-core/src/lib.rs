//! Core of lanrec: screen capture, hardware encode, pacing and link discovery.
//!
//! Deliberately free of any UI. The Tauri app and the headless CLI are both thin
//! shells over this crate, so the capture pipeline can be exercised and measured
//! without a window on screen.

pub mod capture;
pub mod clock;
pub mod config;
pub mod d3d;
pub mod net;
pub mod nvenc;
pub mod pace;
pub mod preview;
pub mod profile;
pub mod session;
