//! Polling rate estimators. Pure Rust with no Windows calls, so they can be
//! tested against simulated report streams. All times are in nanoseconds.

use std::collections::{HashSet, VecDeque};
use std::f64::consts::TAU;

pub const STANDARD_RATES: [u32; 7] = [125, 250, 500, 1000, 2000, 4000, 8000];

const MS: u64 = 1_000_000;

/// Returns the standard rate within 15% of `hz`, if any.
pub fn snap_rate(hz: f64) -> Option<u32> {
    STANDARD_RATES
        .iter()
        .copied()
        .find(|&r| (hz / r as f64 - 1.0).abs() <= 0.15)
}

// ---------------------------------------------------------------- mouse

const RATE_WINDOW: u64 = 500 * MS;
const LIVE_WINDOW: u64 = 250 * MS;
const SAMPLE_EVERY: u64 = 10 * MS;
const SAMPLE_HISTORY: u64 = 10_000 * MS;
const MAX_INTERVAL: u64 = 50 * MS;
const MIN_WINDOW_REPORTS: usize = 5;
const MIN_SAMPLES: usize = 50;
const STEADY_FRACTION: f64 = 0.3;
/// Samples near the peak (at one per 10 ms) that also count as steady, so slow
/// movement afterwards cannot undo a reading that already held for 1.5 seconds.
const STEADY_SAMPLES: u32 = 150;
/// The measured rate must have held for this many samples, about 1 second.
const PEAK_SAMPLES: u32 = 100;
const RATE_BIN_MIN_HZ: f64 = 10.0;
const RATE_BIN_WIDTH: f64 = 0.0025;
const RATE_BINS: usize = 3050;
const STEADY_BINS: usize = 38; // about 10% either side
const INTERVAL_RING: usize = 8192;

pub const INTERVAL_BINS: usize = 40;
const INTERVAL_BIN_MIN_NS: f64 = 50_000.0;
const INTERVAL_BIN_MAX_NS: f64 = 50_000_000.0;

/// Lower edge of a mouse interval histogram bin, in microseconds.
pub fn interval_bin_edge_us(bin: usize) -> f64 {
    let ratio = INTERVAL_BIN_MAX_NS / INTERVAL_BIN_MIN_NS;
    INTERVAL_BIN_MIN_NS * ratio.powf(bin as f64 / INTERVAL_BINS as f64) / 1000.0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseQuality {
    /// Not enough movement yet.
    Collecting,
    /// The rate held for a good share of the movement.
    Steady,
    /// Movement was too slow or uneven to trust the number.
    Unsteady,
}

#[derive(Clone, Debug)]
pub struct MouseReport {
    pub reports: u64,
    pub live_hz: f64,
    pub measured_hz: Option<f64>,
    pub snapped: Option<u32>,
    pub steady_fraction: f64,
    pub median_interval_us: Option<f64>,
    pub p99_interval_us: Option<f64>,
    pub quality: MouseQuality,
    pub histogram: [u32; INTERVAL_BINS],
}

#[derive(Default)]
pub struct MouseEstimator {
    reports: u64,
    window: VecDeque<u64>,
    live: VecDeque<u64>,
    samples: VecDeque<(u64, f64)>,
    next_sample: u64,
    rate_bins: Vec<u32>,
    rate_total: u32,
    intervals: Vec<u32>,
    interval_pos: usize,
    histogram: Vec<u32>,
}

impl MouseEstimator {
    pub fn new() -> Self {
        Self {
            histogram: vec![0; INTERVAL_BINS],
            rate_bins: vec![0; RATE_BINS],
            ..Self::default()
        }
    }

    fn record_rate(&mut self, hz: f64) {
        let pos = (hz / RATE_BIN_MIN_HZ).ln() / RATE_BIN_WIDTH;
        let bin = (pos.max(0.0) as usize).min(RATE_BINS - 1);
        self.rate_bins[bin] += 1;
        self.rate_total += 1;
    }

    /// The rate the mouse held for about a second at its fastest, kept for the
    /// whole session. Later slow movement, such as reaching for a button, must
    /// not replace it. Short sessions fall back to the 95th percentile.
    fn peak(&self) -> Option<(f64, f64, u32)> {
        if (self.rate_total as usize) < MIN_SAMPLES {
            return None;
        }
        let rank = (self.rate_total / 20).clamp(1, PEAK_SAMPLES);
        let mut seen = 0;
        let bin = (0..RATE_BINS).rev().find(|&b| {
            seen += self.rate_bins[b];
            seen >= rank
        })?;
        let hz = RATE_BIN_MIN_HZ * ((bin as f64 + 0.5) * RATE_BIN_WIDTH).exp();
        let lo = bin.saturating_sub(STEADY_BINS);
        let hi = (bin + STEADY_BINS).min(RATE_BINS - 1);
        let near: u32 = self.rate_bins[lo..=hi].iter().sum();
        Some((hz, near as f64 / self.rate_total as f64, near))
    }

    pub fn push(&mut self, t: u64) {
        if let Some(&last) = self.live.back() {
            let gap = t.saturating_sub(last);
            if gap < MAX_INTERVAL {
                self.record_interval(gap);
            }
        }
        self.reports += 1;
        self.window.push_back(t);
        self.live.push_back(t);
        while self.window.front().is_some_and(|&f| f + RATE_WINDOW < t) {
            self.window.pop_front();
        }
        while self.live.front().is_some_and(|&f| f + LIVE_WINDOW < t) {
            self.live.pop_front();
        }
        if t >= self.next_sample && self.window.len() >= MIN_WINDOW_REPORTS {
            let span = t - self.window[0];
            if span >= RATE_WINDOW / 2 {
                // Rate from first to last report in the window, so a window
                // edge falling between two reports does not skew slow rates.
                // The window is long enough that a delivery stall and its
                // catch-up burst average out instead of reading as a high rate.
                let hz = (self.window.len() - 1) as f64 * 1e9 / span as f64;
                self.samples.push_back((t, hz));
                self.record_rate(hz);
                self.next_sample = t + SAMPLE_EVERY;
                while self.samples.front().is_some_and(|s| s.0 + SAMPLE_HISTORY < t) {
                    self.samples.pop_front();
                }
            }
        }
    }

    fn record_interval(&mut self, gap: u64) {
        let ns = gap.max(1) as f64;
        let ratio = INTERVAL_BIN_MAX_NS / INTERVAL_BIN_MIN_NS;
        let pos = (ns / INTERVAL_BIN_MIN_NS).ln() / ratio.ln() * INTERVAL_BINS as f64;
        let bin = (pos.max(0.0) as usize).min(INTERVAL_BINS - 1);
        self.histogram[bin] += 1;
        let us_tenths = (gap / 100).min(u32::MAX as u64) as u32;
        if self.intervals.len() < INTERVAL_RING {
            self.intervals.push(us_tenths);
        } else {
            self.intervals[self.interval_pos] = us_tenths;
            self.interval_pos = (self.interval_pos + 1) % INTERVAL_RING;
        }
    }

    /// Recent per-window rates as (time, Hz), oldest first.
    pub fn samples(&self) -> impl Iterator<Item = (u64, f64)> + '_ {
        self.samples.iter().copied()
    }

    pub fn report(&self, now: u64) -> MouseReport {
        let live_count = self
            .live
            .iter()
            .rev()
            .take_while(|&&t| t + LIVE_WINDOW >= now)
            .count();
        let live_hz = live_count as f64 * 1e9 / LIVE_WINDOW as f64;

        let peak = self.peak();
        let measured_hz = peak.map(|p| p.0);
        let steady_fraction = peak.map_or(0.0, |p| p.1);
        let quality = match peak {
            None => MouseQuality::Collecting,
            Some((_, fraction, near)) if fraction >= STEADY_FRACTION || near >= STEADY_SAMPLES => MouseQuality::Steady,
            Some(_) => MouseQuality::Unsteady,
        };

        let (median_interval_us, p99_interval_us) = if self.intervals.is_empty() {
            (None, None)
        } else {
            let mut sorted = self.intervals.clone();
            sorted.sort_unstable();
            let at = |q: usize| sorted[(sorted.len() - 1) * q / 100] as f64 / 10.0;
            (Some(at(50)), Some(at(99)))
        };

        let mut histogram = [0; INTERVAL_BINS];
        histogram.copy_from_slice(&self.histogram);

        MouseReport {
            reports: self.reports,
            live_hz,
            measured_hz,
            snapped: measured_hz.and_then(snap_rate),
            steady_fraction,
            median_interval_us,
            p99_interval_us,
            quality,
            histogram,
        }
    }
}

// ------------------------------------------------------------- keyboard

const SAME_REPORT_NS: u64 = 60_000;
const MAX_GAP_NS: u64 = 200 * MS;
const GAP_RING: usize = 2000;
/// Shorter gaps are shown but not scored. Real keyboards produced a cluster near
/// 0.6 ms between changes of different keys, off even a 1 ms grid, so they say
/// more about delivery than about polling. Longer gaps still sit on any grid.
const MIN_SCORED_GAP_NS: u64 = 1_500_000;

/// Usable gaps needed before the keyboard estimator gives an answer.
pub const MIN_GAPS: usize = 40;
/// Score below which a period never counts as aligned, however many gaps. It is
/// low because jitter limits fine grids: a clean 1000 Hz keyboard scored only
/// 0.45 at the 4000 Hz period.
pub const MIN_SCORE: f64 = 0.25;
/// Rates at or above this are never called high confidence, because they sit
/// close to the timing noise of the delivery path.
const FINE_GRID_RATE: u32 = 4000;
/// Rayleigh statistic n * S^2 a period must exceed. For uniform phases the
/// chance of passing is about e^-14 per candidate.
const RAYLEIGH_Z: f64 = 14.0;

pub const GAP_BINS: usize = 200;
pub const GAP_BIN_US: f64 = 50.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyboardResult {
    /// More usable gaps are needed.
    Collecting,
    /// No standard period fits the timing.
    NotResolved,
    Resolved { rate: u32, confidence: Confidence, score: f64 },
}

#[derive(Clone, Debug)]
pub struct KeyboardReport {
    pub events: u64,
    pub repeats_ignored: u64,
    pub same_report: u64,
    pub gaps: usize,
    /// Alignment score per entry of `STANDARD_RATES`.
    pub scores: [f64; STANDARD_RATES.len()],
    /// Score a period must reach at the current gap count.
    pub threshold: f64,
    pub result: KeyboardResult,
    pub min_gap_us: Option<f64>,
    pub histogram: [u32; GAP_BINS],
}

#[derive(Default)]
pub struct KeyboardEstimator {
    down: HashSet<u32>,
    last: Option<u64>,
    events: u64,
    repeats_ignored: u64,
    same_report: u64,
    gaps: VecDeque<u64>,
}

impl KeyboardEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one key transition. `key` is used only to spot auto-repeat and is
    /// forgotten when the key is released.
    pub fn push(&mut self, t: u64, key: u32, down: bool) {
        if down {
            if !self.down.insert(key) {
                self.repeats_ignored += 1;
                return;
            }
        } else {
            self.down.remove(&key);
        }
        self.events += 1;
        match self.last {
            Some(last) if t.saturating_sub(last) < SAME_REPORT_NS => {
                // Same USB report; the first event already marked this time.
                self.same_report += 1;
                return;
            }
            Some(last) if t - last <= MAX_GAP_NS => {
                self.gaps.push_back(t - last);
                if self.gaps.len() > GAP_RING {
                    self.gaps.pop_front();
                }
            }
            _ => {}
        }
        self.last = Some(t);
    }

    pub fn report(&self) -> KeyboardReport {
        let scored = || self.gaps.iter().copied().filter(|&g| g >= MIN_SCORED_GAP_NS);
        let n = scored().count();
        let mut scores = [0.0; STANDARD_RATES.len()];
        for (score, &rate) in scores.iter_mut().zip(&STANDARD_RATES) {
            *score = alignment_score(scored(), rate);
        }
        let threshold = if n == 0 {
            1.0
        } else {
            MIN_SCORE.max((RAYLEIGH_Z / n as f64).sqrt())
        };

        // Rates ascend, so the first pass is the longest aligned period. A
        // grid of T also aligns at T/2 and T/4, so shorter passes are aliases.
        let result = if n < MIN_GAPS {
            KeyboardResult::Collecting
        } else {
            scores
                .iter()
                .position(|&s| s >= threshold)
                .map_or(KeyboardResult::NotResolved, |i| {
                    let (rate, score) = (STANDARD_RATES[i], scores[i]);
                    // Confidence rests on the weight of evidence and on how
                    // clearly the next longer period was rejected, not on the
                    // raw score, which jitter lowers whatever the sample size.
                    let evidence = n as f64 * score * score;
                    let longer = if i > 0 { scores[i - 1] } else { 0.0 };
                    let confidence = if evidence >= 50.0 && n >= 60 && score >= 3.0 * longer && rate < FINE_GRID_RATE {
                        Confidence::High
                    } else if evidence >= 25.0 {
                        Confidence::Medium
                    } else {
                        Confidence::Low
                    };
                    KeyboardResult::Resolved { rate, confidence, score }
                })
        };

        let mut histogram = [0; GAP_BINS];
        for &gap in &self.gaps {
            let bin = (gap as f64 / 1000.0 / GAP_BIN_US) as usize;
            if bin < GAP_BINS {
                histogram[bin] += 1;
            }
        }

        KeyboardReport {
            events: self.events,
            repeats_ignored: self.repeats_ignored,
            same_report: self.same_report,
            gaps: n,
            scores,
            threshold,
            result,
            min_gap_us: self.gaps.iter().min().map(|&g| g as f64 / 1000.0),
            histogram,
        }
    }
}

/// How tightly gaps cluster on multiples of the period of `rate`: 1 when all
/// sit on the grid, near 0 when they are spread evenly around it.
pub fn alignment_score(gaps: impl Iterator<Item = u64>, rate: u32) -> f64 {
    let (mut re, mut im, mut n) = (0.0, 0.0, 0u32);
    for gap in gaps {
        let phase = TAU * gap as f64 * rate as f64 / 1e9;
        re += phase.cos();
        im += phase.sin();
        n += 1;
    }
    if n == 0 {
        0.0
    } else {
        (re * re + im * im).sqrt() / n as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small deterministic generator, so failures reproduce exactly.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
        fn range(&mut self, lo: u64, hi: u64) -> u64 {
            lo + self.next() % (hi - lo + 1)
        }
        fn normal(&mut self) -> f64 {
            (-2.0 * self.unit().max(1e-12).ln()).sqrt() * (TAU * self.unit()).cos()
        }
    }

    /// Reports at `rate` for `secs`, with delivery jitter in nanoseconds.
    fn feed_mouse(est: &mut MouseEstimator, rng: &mut Rng, start: u64, rate: u32, secs: f64, jitter: f64) -> u64 {
        let period = 1e9 / rate as f64;
        let count = (secs * rate as f64) as u64;
        let mut last = start;
        for i in 0..count {
            let t = start as f64 + i as f64 * period + rng.normal().abs() * jitter;
            last = (t as u64).max(last);
            est.push(last);
        }
        last
    }

    #[test]
    fn snaps_within_tolerance_only() {
        assert_eq!(snap_rate(998.0), Some(1000));
        assert_eq!(snap_rate(860.0), Some(1000));
        assert_eq!(snap_rate(700.0), None);
        assert_eq!(snap_rate(8100.0), Some(8000));
    }

    #[test]
    fn mouse_reads_every_standard_rate_with_jitter() {
        for &rate in &STANDARD_RATES {
            let mut est = MouseEstimator::new();
            let mut rng = Rng(0x9E37_79B9 + rate as u64);
            let end = feed_mouse(&mut est, &mut rng, MS, rate, 3.0, 40_000.0);
            let report = est.report(end);
            assert_eq!(report.snapped, Some(rate), "{report:?}");
            assert_eq!(report.quality, MouseQuality::Steady, "rate {rate}");
            let measured = report.measured_hz.unwrap();
            assert!((measured / rate as f64 - 1.0).abs() < 0.05, "{rate} read {measured}");
        }
    }

    #[test]
    fn mouse_survives_burst_delivery_at_8k() {
        // Eight reports handed over together once a millisecond.
        let mut est = MouseEstimator::new();
        let mut t = MS;
        for _ in 0..3000 {
            for j in 0..8 {
                est.push(t + j * 1000);
            }
            t += MS;
        }
        assert_eq!(est.report(t).snapped, Some(8000));
    }

    #[test]
    fn mouse_peak_survives_slow_stretches() {
        let mut est = MouseEstimator::new();
        let mut rng = Rng(77);
        let a = feed_mouse(&mut est, &mut rng, MS, 300, 2.0, 20_000.0);
        let b = feed_mouse(&mut est, &mut rng, a + MS, 1000, 2.0, 20_000.0);
        assert_eq!(est.report(b).snapped, Some(1000));
    }

    #[test]
    fn mouse_is_not_inflated_by_stalls_and_catch_up() {
        // 4000 Hz, but every 150 ms delivery stalls for 30 ms and then bursts.
        let mut est = MouseEstimator::new();
        let period = 250_000u64;
        let mut last = 0;
        for i in 0..16_000u64 {
            let due = MS + i * period;
            let phase = due % (150 * MS);
            let t = if phase < 30 * MS { due - phase + 30 * MS + (i % 120) * 20_000 } else { due };
            last = t.max(last);
            est.push(last);
        }
        let report = est.report(last);
        assert_eq!(report.snapped, Some(4000), "{report:?}");
        assert_eq!(report.quality, MouseQuality::Steady);
    }

    #[test]
    fn mouse_reading_survives_later_slow_movement() {
        // A clean 8K run, then a long pause, then reaching for the close button.
        let mut est = MouseEstimator::new();
        let mut rng = Rng(31);
        let a = feed_mouse(&mut est, &mut rng, MS, 8000, 5.0, 10_000.0);
        let b = feed_mouse(&mut est, &mut rng, a + 60_000 * MS, 1800, 2.0, 20_000.0);
        let c = feed_mouse(&mut est, &mut rng, b + 10_000 * MS, 300, 12.0, 20_000.0);
        let report = est.report(c);
        assert_eq!(report.snapped, Some(8000), "{report:?}");
        assert_eq!(report.quality, MouseQuality::Steady);
    }

    #[test]
    fn keyboard_ignores_sub_millisecond_delivery_cluster() {
        // A 1000 Hz grid, plus changes delivered about 0.6 ms after the last.
        let mut est = KeyboardEstimator::new();
        let mut rng = Rng(13);
        let mut t = 10 * MS;
        let mut down = false;
        for i in 0..400 {
            t += rng.range(3, 40) * MS;
            down = !down;
            est.push(t + rng.normal().abs() as u64 * 20_000, 1, down);
            if i % 8 == 0 {
                t += rng.range(500_000, 750_000);
                est.push(t, 2, down);
            }
        }
        let report = est.report();
        assert!(matches!(report.result, KeyboardResult::Resolved { rate: 1000, .. }), "{report:?}");
        assert!(report.min_gap_us.unwrap() < 1000.0);
    }

    #[test]
    fn keyboard_calls_a_fine_grid_medium_at_best() {
        let report = feed_keys(4000, 800, 12_000.0, 0.0, 17).report();
        match report.result {
            KeyboardResult::Resolved { rate, confidence, .. } => {
                assert_eq!(rate, 4000);
                assert_eq!(confidence, Confidence::Medium);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mouse_flags_uneven_movement() {
        // Constantly changing speed: no rate holds for long.
        let mut est = MouseEstimator::new();
        let mut t = MS;
        for i in 0..4000u64 {
            let phase = i as f64 / 4000.0;
            t += (1e9 / (100.0 + 900.0 * phase * phase * phase * phase)) as u64;
            est.push(t);
        }
        assert_eq!(est.report(t).quality, MouseQuality::Unsteady);
    }

    #[test]
    fn mouse_needs_movement_before_answering() {
        let mut est = MouseEstimator::new();
        for i in 0..20 {
            est.push(MS + i * MS);
        }
        let report = est.report(21 * MS);
        assert_eq!(report.quality, MouseQuality::Collecting);
        assert_eq!(report.snapped, None);
    }

    #[test]
    fn mouse_live_rate_decays_when_idle() {
        let mut est = MouseEstimator::new();
        let mut rng = Rng(5);
        let end = feed_mouse(&mut est, &mut rng, MS, 1000, 1.0, 0.0);
        assert!(est.report(end).live_hz > 900.0);
        assert_eq!(est.report(end + 1000 * MS).live_hz, 0.0);
    }

    /// Key changes on a grid of `rate`, like a person mashing keys. Stamps
    /// carry jitter, and `drift_ppm` models the USB clock running off the CPU's.
    fn feed_keys(rate: u32, count: usize, jitter: f64, drift_ppm: f64, seed: u64) -> KeyboardEstimator {
        let mut est = KeyboardEstimator::new();
        let mut rng = Rng(seed);
        let period = 1e9 / rate as f64 * (1.0 + drift_ppm / 1e6);
        let mut slot = 1000u64;
        let mut down = [false; 8];
        for _ in 0..count {
            slot += (rng.range(4 * MS, 45 * MS) as f64 * rate as f64 / 1e9).max(1.0) as u64;
            let t = slot as f64 * period + rng.normal().abs() * jitter;
            let key = rng.range(0, 7) as usize;
            down[key] = !down[key];
            est.push(t as u64, key as u32, down[key]);
        }
        est
    }

    #[test]
    fn keyboard_resolves_rates_up_to_4k_with_realistic_jitter() {
        for &rate in &[125, 250, 500, 1000, 2000, 4000] {
            let report = feed_keys(rate, 150, 12_000.0, 0.0, 11 + rate as u64).report();
            match report.result {
                KeyboardResult::Resolved { rate: found, .. } => assert_eq!(found, rate, "{report:?}"),
                other => panic!("rate {rate}: {other:?}, scores {:?}", report.scores),
            }
        }
    }

    #[test]
    fn keyboard_ignores_clock_drift() {
        let report = feed_keys(1000, 300, 30_000.0, 100.0, 3).report();
        assert!(matches!(report.result, KeyboardResult::Resolved { rate: 1000, .. }), "{report:?}");
    }

    #[test]
    fn keyboard_refuses_when_jitter_buries_the_grid() {
        let report = feed_keys(4000, 300, 200_000.0, 0.0, 9).report();
        assert_eq!(report.result, KeyboardResult::NotResolved, "{:?}", report.scores);
    }

    #[test]
    fn keyboard_refuses_off_grid_timing() {
        let mut est = KeyboardEstimator::new();
        let mut rng = Rng(21);
        let mut t = MS;
        let mut down = false;
        for _ in 0..500 {
            t += rng.range(3 * MS, 40 * MS);
            down = !down;
            est.push(t, 1, down);
        }
        assert_eq!(est.report().result, KeyboardResult::NotResolved);
    }

    #[test]
    fn keyboard_waits_for_enough_gaps() {
        let report = feed_keys(1000, 20, 10_000.0, 0.0, 4).report();
        assert_eq!(report.result, KeyboardResult::Collecting);
    }

    #[test]
    fn keyboard_filters_auto_repeat() {
        let mut est = KeyboardEstimator::new();
        est.push(MS, 30, true);
        for i in 1..100 {
            est.push(MS + i * 33 * MS, 30, true);
        }
        est.push(4000 * MS, 30, false);
        let report = est.report();
        assert_eq!(report.events, 2);
        assert_eq!(report.repeats_ignored, 99);
        assert_eq!(report.gaps, 0);
    }

    #[test]
    fn keyboard_merges_events_from_one_report() {
        let mut est = KeyboardEstimator::new();
        est.push(10 * MS, 1, true);
        est.push(10 * MS + 5_000, 2, true);
        est.push(20 * MS, 1, false);
        let report = est.report();
        assert_eq!(report.same_report, 1);
        assert_eq!(report.gaps, 1);
        assert_eq!(report.min_gap_us, Some(10_000.0));
    }

    #[test]
    fn keyboard_forgets_released_keys() {
        let mut est = KeyboardEstimator::new();
        est.push(MS, 7, true);
        est.push(20 * MS, 7, false);
        assert!(est.down.is_empty());
    }
}
