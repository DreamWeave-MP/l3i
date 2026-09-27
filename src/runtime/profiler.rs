//! Profiler statistics primitives: the averaging and peak-decay rules OpenMW's script stats
//! use (`ScriptsContainer::catchUp`, `updateProfilerAverage`). Pure data; no VM involved.
//!
//! What the crate ships is the generic core: per-frame accumulators folded into rolling
//! averages and a decaying peak, seeded averages for phase timings, and the constants. Report
//! text, UI, engine parts, and event attribution stay in the host.

/// Averaging weight: approximately the last 30 frames.
pub const AVERAGE_COEFFICIENT: f64 = 1.0 / 30.0;
/// Per-frame peak decay: a spike fades to about 5% over approximately 180 frames.
pub const PEAK_DECAY: f64 = 0.983;

/// A rolling average seeded by its first sample (`updateProfilerAverage`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RollingAverage {
    value: f64,
    seeded: bool,
}

impl RollingAverage {
    pub const fn new() -> RollingAverage {
        RollingAverage { value: 0.0, seeded: false }
    }

    pub fn update(&mut self, sample: f64) {
        self.value =
            if self.seeded { self.value * (1.0 - AVERAGE_COEFFICIENT) + sample * AVERAGE_COEFFICIENT } else { sample };
        self.seeded = true;
    }

    pub fn get(&self) -> f64 {
        self.value
    }

    pub fn is_seeded(&self) -> bool {
        self.seeded
    }
}

/// Wall time of a phase and the script time within it, averaged together so both cover the
/// same frames (`ScopedProfilerPhase`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PhaseStats {
    pub total_ms: f64,
    pub scripts_ms: f64,
    pub last_total_ms: f64,
    pub last_scripts_ms: f64,
    seeded: bool,
}

impl PhaseStats {
    pub fn record(&mut self, total_ms: f64, scripts_ms: f64) {
        let keep = if self.seeded { 1.0 - AVERAGE_COEFFICIENT } else { 0.0 };
        let take = if self.seeded { AVERAGE_COEFFICIENT } else { 1.0 };
        self.total_ms = self.total_ms * keep + total_ms * take;
        self.scripts_ms = self.scripts_ms * keep + scripts_ms * take;
        self.seeded = true;
        self.last_total_ms = total_ms;
        self.last_scripts_ms = scripts_ms;
    }
}

/// Per-context statistics kept across frames (`ScriptsContainer::ScriptStats`): this frame's
/// accumulators plus the averages and peak they fold into.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameStats {
    pub avg_call_time_ms: f64,
    pub avg_queued_time_ms: f64,
    pub avg_allocation_activity: f64,
    pub avg_calls: f64,
    pub peak_call_time_ms: f64,
    pub call_time_this_frame_ms: f64,
    pub queued_time_this_frame_ms: f64,
    pub allocation_activity_this_frame: u64,
    pub calls_this_frame: u64,
    /// The frame the accumulators belong to.
    pub frame: u64,
}

impl FrameStats {
    /// Fresh statistics attributed to `frame`.
    pub fn new(frame: u64) -> FrameStats {
        FrameStats { frame, ..FrameStats::default() }
    }

    /// Records a call that ran during the current frame.
    pub fn add_call(&mut self, milliseconds: f64) {
        self.call_time_this_frame_ms += milliseconds;
        self.calls_this_frame += 1;
    }

    /// Records time spent in queued (deferred) work attributed to this context.
    pub fn add_queued(&mut self, milliseconds: f64) {
        self.queued_time_this_frame_ms += milliseconds;
    }

    pub fn add_allocation_activity(&mut self, bytes: u64) {
        self.allocation_activity_this_frame += bytes;
    }

    /// Brings the statistics up to `frame`: folds the recorded frame into the averages and
    /// peak, then decays them for the frames in between in which nothing ran, exactly as if
    /// every frame had been folded. Does nothing when already at or past `frame`.
    pub fn catch_up(&mut self, frame: u64) {
        if self.frame >= frame {
            return;
        }
        let keep = 1.0 - AVERAGE_COEFFICIENT;
        self.avg_call_time_ms = self.avg_call_time_ms * keep + self.call_time_this_frame_ms * AVERAGE_COEFFICIENT;
        self.avg_allocation_activity =
            self.avg_allocation_activity * keep + self.allocation_activity_this_frame as f64 * AVERAGE_COEFFICIENT;
        self.peak_call_time_ms =
            (self.peak_call_time_ms * PEAK_DECAY).max(self.call_time_this_frame_ms + self.queued_time_this_frame_ms);
        self.avg_queued_time_ms = self.avg_queued_time_ms * keep + self.queued_time_this_frame_ms * AVERAGE_COEFFICIENT;
        self.avg_calls = self.avg_calls * keep + self.calls_this_frame as f64 * AVERAGE_COEFFICIENT;
        let idle_frames = frame - self.frame - 1;
        if idle_frames > 0 {
            let idle = idle_frames as f64;
            let average_decay = keep.powf(idle);
            self.avg_call_time_ms *= average_decay;
            self.avg_allocation_activity *= average_decay;
            self.avg_calls *= average_decay;
            self.avg_queued_time_ms *= average_decay;
            self.peak_call_time_ms *= PEAK_DECAY.powf(idle);
        }
        self.call_time_this_frame_ms = 0.0;
        self.queued_time_this_frame_ms = 0.0;
        self.allocation_activity_this_frame = 0;
        self.calls_this_frame = 0;
        self.frame = frame;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_average_seeds_then_blends() {
        let mut average = RollingAverage::new();
        assert!(!average.is_seeded());
        average.update(30.0);
        assert_eq!(average.get(), 30.0);
        average.update(0.0);
        assert!((average.get() - 29.0).abs() < 1e-9);
    }

    #[test]
    fn phase_stats_average_both_series_together() {
        let mut phase = PhaseStats::default();
        phase.record(10.0, 4.0);
        assert_eq!((phase.total_ms, phase.scripts_ms), (10.0, 4.0));
        phase.record(40.0, 34.0);
        assert!((phase.total_ms - 11.0).abs() < 1e-9);
        assert!((phase.scripts_ms - 5.0).abs() < 1e-9);
        assert_eq!((phase.last_total_ms, phase.last_scripts_ms), (40.0, 34.0));
    }

    #[test]
    fn frame_stats_fold_decay_and_clear() {
        let mut stats = FrameStats::new(0);
        stats.add_call(3.0);
        stats.add_call(3.0);
        stats.add_queued(1.0);
        stats.add_allocation_activity(300);
        stats.catch_up(0);
        assert_eq!(stats.calls_this_frame, 2, "same frame: nothing folded");
        stats.catch_up(1);
        assert!((stats.avg_call_time_ms - 6.0 / 30.0).abs() < 1e-9);
        assert!((stats.avg_queued_time_ms - 1.0 / 30.0).abs() < 1e-9);
        assert!((stats.avg_allocation_activity - 10.0).abs() < 1e-9);
        assert!((stats.avg_calls - 2.0 / 30.0).abs() < 1e-9);
        assert_eq!(stats.peak_call_time_ms, 7.0);
        assert_eq!((stats.calls_this_frame, stats.call_time_this_frame_ms, stats.frame), (0, 0.0, 1));

        // Ten idle frames decay the averages as if ten empty frames were folded.
        let (avg_before, peak_before) = (stats.avg_call_time_ms, stats.peak_call_time_ms);
        stats.catch_up(12);
        let expected_avg = avg_before * (1.0 - AVERAGE_COEFFICIENT).powi(11);
        let expected_peak = peak_before * PEAK_DECAY.powi(11);
        assert!((stats.avg_call_time_ms - expected_avg).abs() < 1e-9);
        assert!((stats.peak_call_time_ms - expected_peak).abs() < 1e-9);
        assert_eq!(stats.frame, 12);
    }
}
