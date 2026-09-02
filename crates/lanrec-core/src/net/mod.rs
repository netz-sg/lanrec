pub mod bind;
pub mod nic;

/// Fraction of a link's nominal rate that a real stream can occupy.
///
/// Framing overhead, interrupt coalescing and the receiver's own scheduling mean
/// throughput never reaches the negotiated line rate. Every bandwidth decision in
/// the project goes through this constant so the UI and the validator cannot
/// disagree about what "fits".
pub const USABLE_LINK_FRACTION: f64 = 0.85;
