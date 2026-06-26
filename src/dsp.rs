//! Audio analysis: a sliding sample window → Hann window → FFT → frequency
//! bands (bass/mid/treble) → beat detection, with smoothing for stable visuals.

use std::sync::Arc;

use rustfft::{num_complex::Complex, Fft, FftPlanner};

/// Number of samples per FFT. Power of two for rustfft efficiency.
/// 2048 @ 48 kHz ≈ 43 ms window — good latency/resolution tradeoff.
pub const FFT_SIZE: usize = 2048;

/// Number of columns in the log-spaced spectrum (terrain width).
pub const SPECTRUM_COLS: usize = 128;

/// Frequency span mapped across the spectrum columns.
const SPECTRUM_LO_HZ: f32 = 30.0;
const SPECTRUM_HI_HZ: f32 = 16_000.0;

/// Features handed to the renderer each frame. All roughly normalized to 0..1.
/// The terrain shape comes from [`Analyzer::spectrum`]; these drive global
/// motion/brightness.
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioFeatures {
    /// Smoothed low-end energy — drives the camera bob.
    pub bass: f32,
    /// Overall spectrum energy — drives global brightness.
    pub level: f32,
    /// Beat envelope: jumps to 1.0 on a detected kick, then decays.
    pub beat: f32,
}

struct Band {
    lo_bin: usize,
    hi_bin: usize,
    smoothed: f32,
}

impl Band {
    fn new(lo_hz: f32, hi_hz: f32, sample_rate: f32) -> Self {
        let bin_hz = sample_rate / FFT_SIZE as f32;
        let lo_bin = (lo_hz / bin_hz).floor() as usize;
        let hi_bin = ((hi_hz / bin_hz).ceil() as usize).min(FFT_SIZE / 2);
        Self {
            lo_bin: lo_bin.max(1),
            hi_bin: hi_bin.max(lo_bin + 1),
            smoothed: 0.0,
        }
    }

    /// Mean magnitude across the band's bins.
    fn energy(&self, spectrum: &[Complex<f32>]) -> f32 {
        let mut sum = 0.0f32;
        for c in &spectrum[self.lo_bin..self.hi_bin] {
            sum += c.norm();
        }
        sum / (self.hi_bin - self.lo_bin) as f32
    }
}

pub struct Analyzer {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,        // Hann coefficients
    samples: Vec<f32>,       // sliding window of the most recent FFT_SIZE samples
    scratch: Vec<Complex<f32>>,
    fft_scratch: Vec<Complex<f32>>, // persistent FFT scratch (avoids per-frame alloc)
    // Log-spaced spectrum for the terrain: per-column FFT-bin ranges + output.
    col_bins: Vec<(usize, usize)>,
    spectrum: Vec<f32>,
    bass: Band, // low-end energy: beat detection + camera bob
    // Beat detection state.
    bass_history: Vec<f32>,
    history_pos: usize,
    beat_env: f32,
}

impl Analyzer {
    pub fn new(sample_rate: f32) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let fft_scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];

        // Hann window.
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|n| {
                let x = std::f32::consts::PI * n as f32 / (FFT_SIZE as f32 - 1.0);
                let s = x.sin();
                s * s
            })
            .collect();

        // ~1 second of bass-energy history at typical frame rates.
        let history_len = 60;

        // Log-spaced bin ranges: each column covers an octave-ish slice so bass
        // doesn't get crushed into one column and treble doesn't dominate.
        let bin_hz = sample_rate / FFT_SIZE as f32;
        let max_bin = FFT_SIZE / 2;
        let log_lo = SPECTRUM_LO_HZ.ln();
        let log_hi = SPECTRUM_HI_HZ.ln();
        let col_bins: Vec<(usize, usize)> = (0..SPECTRUM_COLS)
            .map(|c| {
                let f0 = (log_lo + (log_hi - log_lo) * c as f32 / SPECTRUM_COLS as f32).exp();
                let f1 =
                    (log_lo + (log_hi - log_lo) * (c + 1) as f32 / SPECTRUM_COLS as f32).exp();
                let lo = ((f0 / bin_hz).floor() as usize).clamp(1, max_bin - 1);
                let hi = ((f1 / bin_hz).ceil() as usize).clamp(lo + 1, max_bin);
                (lo, hi)
            })
            .collect();

        Self {
            fft,
            window,
            samples: vec![0.0; FFT_SIZE],
            scratch: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            fft_scratch,
            col_bins,
            spectrum: vec![0.0; SPECTRUM_COLS],
            bass: Band::new(20.0, 250.0, sample_rate),
            bass_history: vec![0.0; history_len],
            history_pos: 0,
            beat_env: 0.0,
        }
    }

    /// Feed newly captured mono samples; keeps only the most recent FFT_SIZE.
    pub fn feed(&mut self, new: &[f32]) {
        if new.is_empty() {
            return;
        }
        if new.len() >= FFT_SIZE {
            // Only the tail matters.
            self.samples
                .copy_from_slice(&new[new.len() - FFT_SIZE..]);
        } else {
            // Shift left, append new at the end.
            self.samples.copy_within(new.len().., 0);
            let start = FFT_SIZE - new.len();
            self.samples[start..].copy_from_slice(new);
        }
    }

    /// Run analysis on the current window and return smoothed features.
    pub fn analyze(&mut self) -> AudioFeatures {
        // Windowed samples → complex scratch.
        for (i, c) in self.scratch.iter_mut().enumerate() {
            *c = Complex::new(self.samples[i] * self.window[i], 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.scratch, &mut self.fft_scratch);

        // Low-end energy, gain-compensated and soft-clamped to 0..1.
        let raw_bass = compress(self.bass.energy(&self.scratch) * 0.05);

        // Peak-hold with decay: snap up instantly, ease down for smooth visuals.
        const DECAY: f32 = 0.90;
        self.bass.smoothed = raw_bass.max(self.bass.smoothed * DECAY);

        // --- Beat detection on instantaneous bass energy.
        let instant = raw_bass;
        let avg: f32 =
            self.bass_history.iter().copied().sum::<f32>() / self.bass_history.len() as f32;
        // Trigger when current bass clearly exceeds the local average.
        if avg > 0.02 && instant > avg * 1.4 {
            self.beat_env = 1.0;
        } else {
            self.beat_env *= 0.86;
        }
        self.bass_history[self.history_pos] = instant;
        self.history_pos = (self.history_pos + 1) % self.bass_history.len();

        // --- Log-spaced spectrum (one terrain row). Peak magnitude per column,
        // gain-compensated and compressed, with a mild per-column decay so the
        // scrolling ridges don't flicker frame to frame.
        for (c, &(lo, hi)) in self.col_bins.iter().enumerate() {
            let mut peak = 0.0f32;
            for bin in &self.scratch[lo..hi] {
                peak = peak.max(bin.norm());
            }
            // Treble bins carry less energy — tilt gain up with frequency.
            let tilt = 0.04 + 0.20 * (c as f32 / SPECTRUM_COLS as f32);
            let v = compress(peak * tilt);
            self.spectrum[c] = v.max(self.spectrum[c] * 0.80);
        }

        let level = self.spectrum.iter().sum::<f32>() / SPECTRUM_COLS as f32;

        AudioFeatures {
            bass: self.bass.smoothed,
            level,
            beat: self.beat_env,
        }
    }

    /// The most recent log-spaced spectrum row (length [`SPECTRUM_COLS`]),
    /// bass → treble, each value ~0..1. One terrain row per `analyze()` call.
    pub fn spectrum(&self) -> &[f32] {
        &self.spectrum
    }
}

/// Soft compression so loud transients don't blow past 1.0 while quiet detail
/// stays visible. tanh-like knee.
fn compress(x: f32) -> f32 {
    (x / (1.0 + x)).clamp(0.0, 1.0)
}
