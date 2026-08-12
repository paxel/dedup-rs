//! Pure playhead and transport math for the shared viewer's video tools: the
//! proportional filmstrip playhead and the discrete audio play-rate stops.
//! No egui, no I/O — this is the seam the alignment guarantee is tested at.

/// Frames per side in the Video tab's filmstrip overview. Denser than the
/// browse cards' 5 — a diff overview earns a fuller picture of the timeline.
pub const FILMSTRIP_FRAMES: usize = 8;

/// The audio transport's discrete play-rate stops; 1× is the default.
pub const RATE_STOPS: [f32; 6] = [0.25, 0.5, 0.75, 1.0, 1.5, 2.0];

/// The next rate stop after `rate`, wrapping past 2× back to 0.25×. A value
/// that is not a stop (drift, stale state) snaps back to the 1× default.
pub fn next_rate(rate: f32) -> f32 {
    match RATE_STOPS.iter().position(|s| (s - rate).abs() < 0.01) {
        Some(i) => RATE_STOPS[(i + 1) % RATE_STOPS.len()],
        None => 1.0,
    }
}

/// How long `total_ms` of source audio runs at `rate` (a 0.5× render plays
/// twice as long as the original).
pub fn rated_total_ms(total_ms: u64, rate: f32) -> u64 {
    if rate <= 0.0 {
        return total_ms;
    }
    (total_ms as f64 / f64::from(rate)) as u64
}

/// Map the shared playhead `fraction` (0–1) to a timestamp in *this* clip's
/// own timeline. Proportional by design: 50% of a 2:00 clip is 1:00 and 50%
/// of a 1:45 copy is 0:52.5, so a trimmed or re-encoded copy stays aligned at
/// the same relative moment instead of drifting.
pub fn scrub_timestamp_secs(fraction: f32, duration_secs: f64) -> f64 {
    f64::from(fraction.clamp(0.0, 1.0)) * duration_secs.max(0.0)
}

/// Which of `count` filmstrip slots the playhead `fraction` falls in.
pub fn filmstrip_slot(fraction: f32, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    ((fraction.clamp(0.0, 1.0) * count as f32) as usize).min(count - 1)
}

/// Clock display of a timestamp, for the enlarged frame's `A @ 1:00` label —
/// one formatter app-wide ([`crate::media_cell::fmt_ms`]), so a film past an
/// hour reads `1:15:03`, never `75:03`.
pub fn format_secs(secs: f64) -> String {
    // Round to whole seconds first (59.6 s reads 1:00, as before).
    crate::media_cell::fmt_ms(secs.max(0.0).round() as u64 * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alignment guarantee: one shared fraction maps to each clip's *own*
    /// duration, so copies of different length meet at the same relative
    /// moment — the highest seam of the proportional playhead.
    #[test]
    fn a_shared_fraction_maps_to_each_clips_own_timeline() {
        // 50% of a 2:00 clip is 1:00.
        assert_eq!(scrub_timestamp_secs(0.5, 120.0), 60.0);
        // 50% of a 1:45 copy is 0:52.5 — aligned, not drifted.
        assert_eq!(scrub_timestamp_secs(0.5, 105.0), 52.5);
        // The edges land on the edges.
        assert_eq!(scrub_timestamp_secs(0.0, 120.0), 0.0);
        assert_eq!(scrub_timestamp_secs(1.0, 120.0), 120.0);
        // Out-of-range input clamps instead of scrubbing past the clip.
        assert_eq!(scrub_timestamp_secs(1.5, 100.0), 100.0);
        assert_eq!(scrub_timestamp_secs(-0.5, 100.0), 0.0);
        // An unknown (zero/negative) duration stays at the start, not NaN land.
        assert_eq!(scrub_timestamp_secs(0.5, -3.0), 0.0);
    }

    /// The six stops cycle in order and wrap; 1× is the default and the safety
    /// net for a value that is no stop at all.
    #[test]
    fn rate_stops_cycle_through_all_six_and_wrap() {
        assert_eq!(RATE_STOPS, [0.25, 0.5, 0.75, 1.0, 1.5, 2.0]);
        // Starting from the 1× default, one full lap visits every stop once.
        let mut rate = 1.0;
        let mut seen = vec![rate];
        for _ in 0..RATE_STOPS.len() - 1 {
            rate = next_rate(rate);
            seen.push(rate);
        }
        seen.sort_by(f32::total_cmp);
        assert_eq!(seen, RATE_STOPS.to_vec(), "every stop is reachable");
        // 2× wraps to 0.25× rather than clamping at the top.
        assert_eq!(next_rate(2.0), 0.25);
        // A drifted value snaps back to the default.
        assert_eq!(next_rate(0.33), 1.0);
    }

    /// Slowing playback stretches the rendered runtime — the transport's total
    /// must follow the rendered file, not the source.
    #[test]
    fn rated_total_follows_the_rendered_runtime() {
        assert_eq!(rated_total_ms(60_000, 0.5), 120_000);
        assert_eq!(rated_total_ms(60_000, 0.25), 240_000);
        assert_eq!(rated_total_ms(60_000, 2.0), 30_000);
        assert_eq!(rated_total_ms(60_000, 1.0), 60_000);
        // A degenerate rate must not divide by zero.
        assert_eq!(rated_total_ms(60_000, 0.0), 60_000);
    }

    #[test]
    fn filmstrip_slots_cover_the_strip_without_overflow() {
        assert_eq!(filmstrip_slot(0.0, 8), 0);
        assert_eq!(filmstrip_slot(0.49, 8), 3);
        assert_eq!(filmstrip_slot(0.51, 8), 4);
        // The far edge stays in the last slot instead of indexing past it.
        assert_eq!(filmstrip_slot(1.0, 8), 7);
        assert_eq!(filmstrip_slot(0.5, 0), 0);
    }

    #[test]
    fn timestamps_format_as_minutes_and_seconds() {
        assert_eq!(format_secs(0.0), "0:00");
        assert_eq!(format_secs(52.5), "0:53");
        assert_eq!(format_secs(60.0), "1:00");
        assert_eq!(format_secs(605.0), "10:05");
    }
}
