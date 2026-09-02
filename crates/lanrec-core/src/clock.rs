//! The capture clock.
//!
//! WGC timestamps frames with `SystemRelativeTime`, which is derived from the
//! performance counter. Anything that wants to reason about "how much time has
//! passed since that frame" has to read the same counter, not `Instant`, or the
//! two drift apart in ways that only show up as timing errors much later.

use std::sync::OnceLock;

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// Ticks per second. Fixed at boot, so it is read once.
fn frequency() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // Documented as never failing on Windows XP and later.
        unsafe { QueryPerformanceFrequency(&mut f) }.ok();
        if f <= 0 { 10_000_000 } else { f }
    })
}

/// Now, in the same timebase as [`crate::capture::CapturedFrame::timestamp_ns`].
pub fn now_ns() -> u64 {
    let mut c = 0i64;
    unsafe { QueryPerformanceCounter(&mut c) }.ok();
    // u128 on purpose: the counter times the uptime of the machine, and
    // multiplying it by 1e9 overflows u64 after a few seconds.
    ((c.max(0) as u128 * 1_000_000_000) / frequency() as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_and_does_not_overflow() {
        let a = now_ns();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = now_ns();
        assert!(b > a, "clock must advance");
        // 20 ms of sleep, generously bounded -- this is a sanity check on the
        // unit conversion, not a timing test.
        let delta = b - a;
        assert!(
            (10_000_000..500_000_000).contains(&delta),
            "20 ms measured as {delta} ns -- unit conversion is wrong"
        );
    }
}
