//! Time-remaining estimation for byte-count progress.
//!
//! The naive "elapsed × (total − done) / done" extrapolation used to run over
//! *file counts*, which is wildly wrong for a triage scan: file sizes span many
//! orders of magnitude (a 4 KB note next to a 40 GB disk image) and hashing runs
//! on several threads at once, so "half the files done" says almost nothing about
//! how much wall-clock work is left.
//!
//! [`EtaEstimator`] instead tracks progress in **bytes** and models the observed
//! throughput (bytes per wall-clock second). Because the byte counter advances as
//! files complete across *all* worker threads, the measured throughput already
//! folds in however many lanes are running — there is no need to model lane count
//! or per-lane duration separately. Throughput is smoothed with a time-based
//! exponential moving average so a single huge (or tiny) file doesn't whipsaw the
//! estimate, and the *displayed* value is only refreshed on a fixed cadence so it
//! doesn't flicker every frame.
//!
//! The estimator is deliberately clock-agnostic: callers pass the elapsed seconds
//! since the operation started, which keeps it trivially unit-testable against
//! synthetic throughput curves.

use std::time::Duration;

/// Time constant (seconds) of the throughput EWMA. Larger reacts more slowly to
/// a change in file sizes or disk speed but is steadier; ~20 s averages the rate
/// over roughly the last half-minute of work.
const DEFAULT_TAU_SECS: f64 = 20.0;

/// Minimum seconds between refreshes of the *displayed* ETA, so the shown value
/// updates a few times a minute rather than every frame.
const DEFAULT_REFRESH_SECS: f64 = 5.0;

/// Estimates the wall-clock time remaining for a byte-measured operation.
///
/// Feed it monotonically increasing `(done_bytes, elapsed_secs)` samples with
/// [`record`](Self::record); read the smoothed, cadence-held estimate back with
/// [`eta`](Self::eta).
pub struct EtaEstimator {
    total_bytes: u64,
    /// Time constant of the throughput EWMA, in seconds.
    tau: f64,
    /// Seconds between refreshes of the displayed estimate.
    refresh_secs: f64,
    /// Smoothed throughput in bytes/sec; `None` until the first interval seen.
    throughput: Option<f64>,
    /// Previous sample `(done_bytes, elapsed_secs)`, used to derive an interval.
    last: Option<(u64, f64)>,
    /// Elapsed time at which the displayed estimate may next be refreshed.
    next_refresh_secs: f64,
    /// The currently displayed estimate, refreshed only on the cadence.
    shown: Option<Duration>,
}

impl EtaEstimator {
    /// A new estimator for an operation that will process `total_bytes` bytes,
    /// using the default smoothing and refresh cadence.
    pub fn new(total_bytes: u64) -> Self {
        Self::with_params(total_bytes, DEFAULT_TAU_SECS, DEFAULT_REFRESH_SECS)
    }

    /// Like [`new`](Self::new) but with an explicit EWMA time constant and
    /// display-refresh cadence (both in seconds). Primarily for testing.
    pub fn with_params(total_bytes: u64, tau_secs: f64, refresh_secs: f64) -> Self {
        Self {
            total_bytes,
            tau: tau_secs.max(f64::MIN_POSITIVE),
            refresh_secs: refresh_secs.max(0.0),
            throughput: None,
            last: None,
            next_refresh_secs: 0.0,
            shown: None,
        }
    }

    /// Record cumulative progress: `done_bytes` bytes processed at `elapsed_secs`
    /// seconds since the operation began. Samples that don't advance in time
    /// (duplicate or out-of-order) are ignored, so it's safe to call this once
    /// per UI frame with the latest coalesced counter.
    pub fn record(&mut self, done_bytes: u64, elapsed_secs: f64) {
        match self.last {
            None => self.last = Some((done_bytes, elapsed_secs)),
            Some((prev_bytes, prev_secs)) => {
                let dt = elapsed_secs - prev_secs;
                if dt <= 0.0 {
                    // No time has advanced (or the clock went backwards): keep the
                    // earlier sample so the next interval spans real wall time.
                    return;
                }
                let instant = done_bytes.saturating_sub(prev_bytes) as f64 / dt;
                self.throughput = Some(match self.throughput {
                    None => instant,
                    // Time-based EWMA: the weight of the newest sample grows with
                    // the interval it covers, so irregular sample spacing (many
                    // tiny files, then one huge one) is handled correctly.
                    Some(prev) => {
                        let weight = 1.0 - (-dt / self.tau).exp();
                        prev + weight * (instant - prev)
                    }
                });
                self.last = Some((done_bytes, elapsed_secs));
            }
        }

        // Refresh the shown estimate on the first reading and then on cadence.
        if self.throughput.is_some()
            && (self.shown.is_none() || elapsed_secs >= self.next_refresh_secs)
        {
            self.shown = self.compute_eta(done_bytes);
            self.next_refresh_secs = elapsed_secs + self.refresh_secs;
        }
    }

    /// The current smoothed estimate of time remaining, or `None` before enough
    /// progress has been seen to estimate a throughput.
    pub fn eta(&self) -> Option<Duration> {
        self.shown
    }

    fn compute_eta(&self, done_bytes: u64) -> Option<Duration> {
        let throughput = self.throughput?;
        if throughput <= 0.0 {
            return None;
        }
        let remaining = self.total_bytes.saturating_sub(done_bytes) as f64;
        let secs = remaining / throughput;
        // Guard against a near-zero throughput producing a non-finite or
        // absurd duration that `Duration::from_secs_f64` would reject.
        if !secs.is_finite() || secs > u64::MAX as f64 {
            return None;
        }
        Some(Duration::from_secs_f64(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A steady 1 MB/s throughput over a 10 MB job predicts the remaining time
    /// from the measured rate, not from a file count.
    #[test]
    fn steady_throughput_predicts_remaining_seconds() {
        // Tight refresh so every sample updates the shown value in this test.
        let mut eta = EtaEstimator::with_params(10_000_000, 20.0, 0.0);
        eta.record(0, 0.0);
        eta.record(1_000_000, 1.0); // 1 MB in 1 s → 1 MB/s
        eta.record(2_000_000, 2.0);
        let remaining = eta.eta().expect("estimate after two intervals");
        // 8 MB left at ~1 MB/s ≈ 8 s. EWMA seeds on the first interval and both
        // intervals are identical, so this is essentially exact.
        assert!(
            (remaining.as_secs_f64() - 8.0).abs() < 0.5,
            "expected ~8 s remaining, got {remaining:?}"
        );
    }

    /// When the rate slows down (large files late in the job), the estimate must
    /// grow rather than cling to the fast early average — the exact failure the
    /// old file-count extrapolation exhibited.
    #[test]
    fn slowing_throughput_lengthens_estimate() {
        // Short tau so the EWMA reacts within a few samples.
        let mut eta = EtaEstimator::with_params(100_000_000, 3.0, 0.0);
        // Fast phase: 10 MB/s for 5 s → 50 MB done.
        for s in 1..=5 {
            eta.record(s as u64 * 10_000_000, s as f64);
        }
        let fast = eta.eta().expect("estimate during fast phase");
        // Slow phase: 1 MB/s for the next 5 s.
        for s in 6..=10 {
            let done = 50_000_000 + (s - 5) as u64 * 1_000_000;
            eta.record(done, s as f64);
        }
        let slow = eta.eta().expect("estimate during slow phase");
        assert!(
            slow > fast,
            "estimate should grow as throughput drops: fast={fast:?} slow={slow:?}"
        );
    }

    /// No estimate is offered before a throughput can be measured, and repeated
    /// samples at the same instant don't fabricate one.
    #[test]
    fn no_estimate_without_a_time_interval() {
        let mut eta = EtaEstimator::new(1_000_000);
        assert!(eta.eta().is_none(), "nothing recorded yet");
        eta.record(0, 0.0);
        assert!(eta.eta().is_none(), "one sample is not an interval");
        eta.record(500_000, 0.0); // same instant → ignored
        assert!(eta.eta().is_none(), "zero-duration interval yields no rate");
    }

    /// The displayed value only changes on the refresh cadence even though the
    /// underlying throughput is updated on every sample.
    #[test]
    fn displayed_estimate_holds_between_refreshes() {
        let mut eta = EtaEstimator::with_params(10_000_000, 20.0, 5.0);
        eta.record(0, 0.0);
        eta.record(1_000_000, 1.0); // first estimate published here
        let first = eta.eta().expect("first estimate");
        // A sample 2 s later would change the raw estimate, but we're inside the
        // 5 s refresh window, so the shown value must be unchanged.
        eta.record(1_100_000, 3.0);
        assert_eq!(eta.eta(), Some(first), "held between refreshes");
        // Past the cadence, it refreshes.
        eta.record(5_000_000, 6.0);
        assert_ne!(eta.eta(), Some(first), "refreshed after the cadence");
    }
}
