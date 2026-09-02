//! Frame pacing: event-driven capture -> fixed-rate encode.
//!
//! Two things about the input make this non-trivial.
//!
//! **The rate does not divide.** A 165 Hz source at 60 fps output is a ratio of
//! 2.75, so alternating runs of 2 and 3 frames have to be discarded no matter
//! what. That judder cannot be removed, only distributed. Taking "the first frame
//! at or after each tick" is causal and adds no latency, but the selection error
//! swings across a full input interval (6.1 ms at 165 Hz) in a pattern that beats
//! against the output rate -- visible as irregular stutter. Holding one frame back
//! lets each tick pick whichever of its two neighbours is closer, which halves the
//! worst-case error to +/-3 ms and makes it uniform. The cost is one input frame
//! of latency, irrelevant for recording.
//!
//! Capping the game at 120 fps makes the ratio 2:1 and this half disappears.
//!
//! **Frames only arrive on change.** WGC is driven by the compositor, not by
//! vblank: a static screen produces no frames at all. Measured on the target
//! machine, an idle 165 Hz desktop delivers around 48 fps. A fixed-rate stream
//! therefore has to fill the empty slots itself, or a paused game becomes a gap in
//! the timeline and everything after it drifts. The pacer reports those slots as
//! [`Step::Emit::gap_slots`] and leaves the policy to the caller -- a recording
//! wants the previous frame repeated, a live view might rather skip.

/// Which of the two candidate frames won the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The frame held back from last time.
    Held,
    /// The frame just passed in.
    Incoming,
}

/// What the caller should do with the frame it just passed to [`Pacer::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing to encode. Keep the incoming frame as the new candidate and
    /// release whatever was held before.
    Hold,
    Emit {
        /// Presentation timestamp for the frame being emitted now.
        pts_ns: u64,
        /// Whether to encode the held frame or the incoming one.
        source: Source,
        /// Output slots that passed with no new frame, because nothing on screen
        /// changed. To keep the stream at a constant rate, re-encode the
        /// previously emitted frame once per slot -- at
        /// `previous_pts + period_ns * 1 ..= previous_pts + period_ns * gap_slots`
        /// -- before emitting this one. Repeats of identical content cost almost
        /// no bitrate.
        gap_slots: u64,
    },
}

#[derive(Debug)]
pub struct Pacer {
    period_ns: u64,
    /// Capture-clock time of the next output slot.
    next_tick_ns: u64,
    /// Output frame index. PTS is always `index * period_ns`, so the encoded
    /// stream is strictly CFR regardless of how ragged the input was.
    index: u64,
    /// Best candidate for the current slot.
    held_ns: Option<u64>,
    anchored: bool,

    /// Frames discarded because a closer neighbour won the slot. At 165 -> 60 this
    /// is expected to be most of them.
    pub dropped: u64,
    /// Slots that had no new frame. Not an error -- it is what a static screen
    /// looks like -- but a large number during gameplay means capture is starving.
    pub gaps: u64,
}

impl Pacer {
    /// `fps_num / fps_den` is the target output rate, e.g. `60, 1` or `60000, 1001`.
    pub fn new(fps_num: u32, fps_den: u32) -> Self {
        assert!(fps_num > 0 && fps_den > 0, "invalid output rate");
        Self {
            period_ns: 1_000_000_000u64 * fps_den as u64 / fps_num as u64,
            next_tick_ns: 0,
            index: 0,
            held_ns: None,
            anchored: false,
            dropped: 0,
            gaps: 0,
        }
    }

    pub fn period_ns(&self) -> u64 {
        self.period_ns
    }

    /// Feed one captured frame, identified by its QPC capture timestamp.
    pub fn step(&mut self, t_ns: u64) -> Step {
        if !self.anchored {
            // The first frame defines the phase of the output grid, so slot 0
            // lands exactly on it and the recording starts on a real frame.
            self.anchored = true;
            self.next_tick_ns = t_ns;
        }

        if t_ns < self.next_tick_ns {
            // Still short of the slot. A later frame is by definition a closer
            // candidate than the one we were holding.
            if self.held_ns.is_some() {
                self.dropped += 1;
            }
            self.held_ns = Some(t_ns);
            return Step::Hold;
        }

        // Count whole slots that went by without a frame, and advance the grid
        // past them. They are reported so the caller can fill them.
        let mut gap_slots = 0;
        if t_ns >= self.next_tick_ns + self.period_ns {
            gap_slots = (t_ns - self.next_tick_ns) / self.period_ns;
            self.gaps += gap_slots;
            self.index += gap_slots;
            self.next_tick_ns += gap_slots * self.period_ns;
        }

        // The slot now sits between the held candidate and the incoming frame.
        let source = match self.held_ns {
            Some(h) if (self.next_tick_ns - h) <= (t_ns - self.next_tick_ns) => Source::Held,
            _ => Source::Incoming,
        };

        let pts_ns = self.index * self.period_ns;
        self.index += 1;
        self.next_tick_ns += self.period_ns;

        match source {
            Source::Held => self.held_ns = Some(t_ns),
            Source::Incoming => {
                if self.held_ns.is_some() {
                    self.dropped += 1;
                }
                self.held_ns = None;
            }
        }

        Step::Emit {
            pts_ns,
            source,
            gap_slots,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the pacer with a regular input rate and collect every PTS that would
    /// end up in the file, gap fills included.
    fn run(input_hz: u64, out_num: u32, out_den: u32, frames: u64) -> (Vec<u64>, Pacer) {
        let mut p = Pacer::new(out_num, out_den);
        let period = p.period_ns();
        let step_ns = 1_000_000_000 / input_hz;
        let mut pts = Vec::new();
        let mut last: Option<u64> = None;

        for i in 0..frames {
            if let Step::Emit {
                pts_ns, gap_slots, ..
            } = p.step(i * step_ns)
            {
                if let Some(prev) = last {
                    for k in 1..=gap_slots {
                        pts.push(prev + period * k);
                    }
                }
                pts.push(pts_ns);
                last = Some(pts_ns);
            }
        }
        (pts, p)
    }

    #[test]
    fn output_is_strictly_cfr() {
        let (pts, p) = run(165, 60, 1, 1650);
        assert!(pts.len() > 2);
        for w in pts.windows(2) {
            assert_eq!(w[1] - w[0], p.period_ns(), "output must be evenly spaced");
        }
    }

    #[test]
    fn emits_close_to_target_count() {
        // 1650 frames at 165 Hz is 10 s, so ~600 output frames. The one frame of
        // lookahead means the final slot may still be pending.
        let (pts, _) = run(165, 60, 1, 1650);
        assert!((598..=600).contains(&pts.len()), "got {} frames", pts.len());
    }

    #[test]
    fn integer_ratio_is_exact() {
        // 120 -> 60 is 2:1 and must select every second frame with no ambiguity.
        let (pts, p) = run(120, 60, 1, 1200);
        assert_eq!(p.gaps, 0);
        assert!((598..=600).contains(&pts.len()), "got {} frames", pts.len());
    }

    #[test]
    fn selection_error_stays_within_half_an_input_interval() {
        // The whole point of holding a frame back: no tick is ever more than half
        // an input interval away from the frame chosen for it.
        let input_hz = 165u64;
        let step_ns = 1_000_000_000 / input_hz;
        let mut p = Pacer::new(60, 1);
        let mut worst = 0i64;

        for i in 0..1650u64 {
            let t = (i * step_ns) as i64;
            let tick = p.next_tick_ns as i64;
            match p.step(i * step_ns) {
                Step::Hold => {}
                Step::Emit { source, .. } => {
                    let chosen = match source {
                        // The held frame is exactly one input interval earlier.
                        Source::Held => t - step_ns as i64,
                        Source::Incoming => t,
                    };
                    worst = worst.max((chosen - tick).abs());
                }
            }
        }

        let half = (step_ns / 2) as i64 + 1;
        assert!(
            worst <= half,
            "worst selection error {worst} ns exceeds half an input interval ({half} ns)"
        );
    }

    #[test]
    fn slower_input_than_output_still_yields_a_full_rate_stream() {
        // A static screen delivers well under the output rate. Measured on the
        // target machine an idle 165 Hz desktop gives ~48 fps; the stream must
        // still come out at 60 by repeating frames, not by leaving holes.
        let (pts, p) = run(48, 60, 1, 480);
        assert!(p.gaps > 0, "expected gaps at 48 -> 60");

        let period = p.period_ns();
        for w in pts.windows(2) {
            assert_eq!(w[1] - w[0], period, "gap fills must keep the stream CFR");
        }

        // 480 frames at 48 Hz is 10 s, so ~600 slots once the gaps are filled.
        assert!((595..=601).contains(&pts.len()), "got {} frames", pts.len());
    }

    #[test]
    fn a_stall_is_reported_as_gap_slots() {
        let mut p = Pacer::new(60, 1);
        p.step(0);
        // Nothing for 100 ms: six 60 fps slots pass with no frame at all.
        match p.step(100_000_000) {
            Step::Emit { gap_slots, .. } => assert!(gap_slots >= 5, "got {gap_slots}"),
            Step::Hold => panic!("a frame 100 ms later must emit"),
        }
        assert!(p.gaps >= 5);
    }
}
