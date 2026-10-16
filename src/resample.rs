//! Small allocation-light sample-rate converter for the decode worker thread.
//!
//! The host DSP chain runs at a fixed 48 kHz (see `PluginDspBridge::hook` in
//! micyou-plugin/src/dsp.rs), while sound files may come at any rate —
//! typically 44.1 kHz or 48 kHz (fast path: no conversion at all).
//! Conversion happens once per file on the (non-real-time) decode worker,
//! never on the audio thread.
//!
//! Two stages, both running on the (non-real-time) decode thread:
//! 1. **Integer pre-decimation** when `from / to >= 2`: box-average N
//!    consecutive samples. This is a crude but alias-suppressing filter that
//!    keeps the following interpolation stage near unity ratio (96k→48k needs
//!    nothing else; 176.4k→48k becomes 176.4/3=58.8k→48k).
//! 2. **Windowed-sinc interpolation** with an 8-tap Hann-windowed kernel and a
//!    256-phase lookup table (8 KiB, built once per decode job). Hann
//!    windowing gives ≈ -31 dB worst-case sidelobes — transparent for
//!    speech/game audio upmixing (the 44.1k→48k case), and clearly better
//!    than linear interpolation, which aliases the whole band.
//!
//! `process` reuses its buffers; the only steady-state work is the polyphase
//! FIR itself. State (`tail` history) is carried across calls so chunk
//! boundaries are continuous (no zipper noise).

const TAPS: usize = 8;
const HALF: usize = TAPS / 2; // taps span [center-3, center+4]
/// Phase lookup resolution. 512 phases → worst-case interpolation error
/// ≈ |x'|/1024 (~ -60 dB below a full-scale 20 kHz tone); the table costs
/// 16 KiB and is built once per decode job (not on the audio thread).
const PHASES: usize = 512;

pub struct SincResampler {
    /// Input samples consumed per output sample (after pre-decimation).
    step: f64,
    /// Fractional read position, relative to `buf[0]`.
    pos: f64,
    /// `PHASES * TAPS` Hann-windowed sinc coefficients.
    table: Box<[f32]>,
    /// Sliding input window: [left history | not-yet-consumed samples].
    buf: Vec<f32>,
    /// Integer pre-decimation factor (1 = disabled).
    pre: u32,
    /// Running sum/count of the partially filled pre-decimation group.
    pre_acc: f64,
    pre_cnt: u32,
}

impl SincResampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        let pre = (from_rate / to_rate.max(1)).max(1);
        let effective_from = from_rate as f64 / pre as f64;
        let step = effective_from / to_rate as f64;
        let mut buf = Vec::with_capacity(8192);
        // Zero-pad the left history so the very first output samples are
        // computed from a well-defined (silent) pre-roll instead of being
        // skipped — costs HALF samples (~83 µs) of latency, once.
        buf.resize(HALF, 0.0);
        Self {
            step,
            pos: HALF as f64,
            table: build_table(),
            buf,
            pre,
            pre_acc: 0.0,
            pre_cnt: 0,
        }
    }

    /// True when this instance is a pure passthrough (rates already equal).
    #[allow(dead_code)] // used by unit tests / diagnostics
    pub fn is_passthrough(&self) -> bool {
        self.pre == 1 && (self.step - 1.0).abs() < f64::EPSILON
    }

    /// Convert `input`, appending results to `out` (which is cleared first).
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();

        // Stage 1: integer box-average decimation (streaming, keeps the
        // partial group across calls).
        if self.pre > 1 {
            // Scratch is reused across calls; worst case len/pre + 1 samples.
            // To stay allocation-light we write straight into `out`.
            for &s in input {
                self.pre_acc += s as f64;
                self.pre_cnt += 1;
                if self.pre_cnt == self.pre {
                    out.push((self.pre_acc / self.pre as f64) as f32);
                    self.pre_acc = 0.0;
                    self.pre_cnt = 0;
                }
            }
            self.decimate_scratch(out, input);
            return;
        }

        // Stage 2 only (pre == 1).
        self.buf.extend_from_slice(input);
        self.run_sinc(out);
    }

    /// When pre-decimation is active, feed the decimated block through the
    /// sinc stage (skipped entirely for a 1:1 post-decimation ratio).
    fn decimate_scratch(&mut self, decimated: &mut Vec<f32>, _input: &[f32]) {
        if self.is_passthrough_post() {
            return; // decimated stream is already at the target rate
        }
        let block = std::mem::take(decimated);
        self.buf.extend_from_slice(&block);
        self.run_sinc(decimated);
    }

    fn is_passthrough_post(&self) -> bool {
        (self.step - 1.0).abs() < f64::EPSILON
    }

    fn run_sinc(&mut self, out: &mut Vec<f32>) {
        let n = self.buf.len();
        // Output while the full tap window [center-(HALF-1), center+HALF]
        // fits inside the buffer.
        loop {
            let center = self.pos.floor();
            if center < 0.0 {
                self.pos += self.step;
                continue;
            }
            let c = center as usize;
            if c + HALF >= n {
                break;
            }
            let frac = self.pos - center;
            let phase = ((frac * PHASES as f64) as usize).min(PHASES - 1);
            let row = &self.table[phase * TAPS..phase * TAPS + TAPS];
            let i0 = c - (HALF - 1);
            let mut acc = 0.0f32;
            // Fixed 8-tap dot product; the compiler unrolls this cleanly.
            for (k, &coef) in row.iter().enumerate() {
                acc += self.buf[i0 + k] * coef;
            }
            out.push(acc);
            self.pos += self.step;
        }

        // Drop consumed prefix: everything before `floor(pos) - (HALF-1)`
        // can no longer be reached by the tap window. Keep the buffer tiny.
        let keep_from = (self.pos.floor() as usize).saturating_sub(HALF - 1).min(n);
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }
}

/// Hann-windowed sinc lookup: `table[p*TAPS + k] = h(frac_p - offset_k)`
/// where `offset_k = k as f64 - (HALF - 1) as f64 ∈ {-3..=4}` and
/// `h(x) = sinc(x) · hann(x / TAPS·2⁻¹…)` — window support [-HALF, HALF].
fn build_table() -> Box<[f32]> {
    let mut t = vec![0.0f32; PHASES * TAPS];
    for p in 0..PHASES {
        let frac = p as f64 / PHASES as f64;
        for k in 0..TAPS {
            let offset = k as f64 - (HALF - 1) as f64;
            let x = frac - offset; // distance from tap to the fractional position
            let sinc = if x.abs() < 1e-9 {
                1.0
            } else {
                let px = std::f64::consts::PI * x;
                px.sin() / px
            };
            // Hann window over the full tap span [-HALF, HALF].
            let w = 0.5 * (1.0 + (std::f64::consts::PI * x / HALF as f64).cos());
            t[p * TAPS + k] = (sinc * w) as f32;
        }
    }
    t.into_boxed_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sine at `freq` Hz, `rate` sample rate, `n` samples.
    fn sine(freq: f64, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32)
            .collect()
    }

    fn rms(v: &[f32]) -> f64 {
        (v.iter().map(|&x| x as f64 * x as f64).sum::<f64>() / v.len().max(1) as f64).sqrt()
    }

    #[test]
    fn identity_ratio_preserves_signal() {
        let mut r = SincResampler::new(48000, 48000);
        assert!(r.is_passthrough());
        let input = sine(1000.0, 48000, 4800);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // The 8-tap window holds the last HALF samples until the next call
        // (constant ~83 µs group delay); a one-shot run is HALF short.
        assert!((out.len() as i64 - input.len() as i64).abs() <= HALF as i64);
        // Allow the HALF-sample preroll shift: compare RMS, not phase.
        assert!((rms(&out) - rms(&input)).abs() < 0.01);
    }

    #[test]
    fn upsample_441_to_480_keeps_tone_and_length() {
        let mut r = SincResampler::new(44100, 48000);
        assert!(!r.is_passthrough());
        let input = sine(997.0, 44100, 4410); // 100 ms
        let mut out = Vec::new();
        r.process(&input, &mut out);
        // ~100 ms at 48 kHz, minus filter edges.
        assert!((out.len() as i64 - 4800).abs() <= 16, "got {}", out.len());
        let target = rms(&input);
        assert!(
            (rms(&out[64..out.len() - 64]) - target).abs() / target < 0.02,
            "rms drift: {} vs {}",
            rms(&out),
            target
        );
    }

    #[test]
    fn downsample_96k_to_48k_halves_length() {
        let mut r = SincResampler::new(96000, 48000);
        let input = sine(1000.0, 96000, 9600);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert!((out.len() as i64 - 4800).abs() <= 4, "got {}", out.len());
        assert!((rms(&out[32..]) - rms(&input)).abs() / rms(&input) < 0.05);
    }

    #[test]
    fn chunk_boundaries_are_continuous() {
        // Feeding the signal in small chunks must equal feeding it at once.
        let input = sine(1234.0, 44100, 8820);
        let mut whole = SincResampler::new(44100, 48000);
        let mut out_whole = Vec::new();
        whole.process(&input, &mut out_whole);

        let mut chunked = SincResampler::new(44100, 48000);
        let mut out_chunked = Vec::new();
        let mut tmp = Vec::new();
        for c in input.chunks(147) {
            chunked.process(c, &mut tmp);
            out_chunked.extend_from_slice(&tmp);
        }
        let n = out_whole.len().min(out_chunked.len());
        assert!(n > 100);
        let err = out_whole[..n]
            .iter()
            .zip(&out_chunked[..n])
            .map(|(a, b)| (a - b).abs() as f64)
            .fold(0.0f64, f64::max);
        // Both runs are mathematically identical; the residual is pure
        // phase-table quantization (positions accumulate in a different
        // order and may land on adjacent table phases near frac ≈ 1 ≡ 0).
        // Bound: slope × 1/(2·PHASES) ≈ 0.16 × 1/1024 for a 1.2 kHz tone.
        assert!(err < 5e-4, "max boundary error {err}");
    }
}

