//! Local headphone output ("播放到耳机") — plays pad voices on the default
//! output device WITHOUT requiring the host's monitor/ear-return toggle, so
//! the user hears exactly the pad (not their own mic) while callers hear it
//! through the mic mix.
//!
//! Two render modes, gated on host audio-thread liveness (`HOST_MIX_FRAMES`):
//!
//! * **Host streaming** (beacon advanced within the last 150 ms): the host
//!   real-time thread renders the voices and pushes the pre-master mono
//!   mixdown into the SPSC tap ring (`mixer::tap`); this module's cpal
//!   callback only consumes the ring (bounded latency via backlog purge,
//!   prime-before-play to absorb clock drift between the two devices).
//! * **Host idle** (beacon frozen — MicYou not streaming): the cpal callback
//!   self-renders via `mixer::render_local_mono` (it becomes the exclusive
//!   position/envelope advancer), so pads stay audible for offline preview.
//!
//! The mode switch is edge-detected in the callback: rising edge is instant
//! (first beacon change), falling edge waits 150 ms, and the ring is cleared
//! on the falling edge (consumer-owned). A ≤ 1 callback overlap on the rising
//! edge is inaudible (positions advance ~10 ms extra, once per stream start).
//!
//! Threading: the cpal callback is real-time — it never calls Host APIs,
//! never allocates in steady state (all scratch is pre-sized at build), and
//! only touches atomics, the tap ring and the mixer slot table. Stream
//! lifecycle (open/play/pause/close — blocking device calls) runs exclusively
//! on the decode worker thread via `Job::LocalSet` and the `LOCAL_WANTED`
//! idle gate; the bus thread never opens devices (instance-lock discipline).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::mixer::{self, TARGET_RATE, TAP_BACKLOG};
use crate::resample::SincResampler;

/// Local output volume as f32 bits (bus thread writes on config change).
static LOCAL_GAIN_BITS: AtomicU32 = AtomicU32::new(1.0f32.to_bits());
/// Set by the watchdog while any voice is live; the worker uses it (with a
/// 3 s idle hysteresis) to play/pause the stream.
static LOCAL_WANTED: AtomicBool = AtomicBool::new(false);

pub fn set_local_gain(g: f64) {
    LOCAL_GAIN_BITS.store((g.clamp(0.0, 2.0) as f32).to_bits(), Ordering::Relaxed);
}

pub fn set_wanted(v: bool) {
    LOCAL_WANTED.store(v, Ordering::Release);
}

pub fn wanted() -> bool {
    LOCAL_WANTED.load(Ordering::Acquire)
}

/// Idle hysteresis before pausing the stream (avoid device churn between
/// rapid pad presses).
pub const IDLE_PAUSE: Duration = Duration::from_secs(3);
/// Prime the tap ring with this much audio before consuming (absorbs the
/// producer/consumer device-clock difference; 40 ms).
const PRIME_SAMPLES: usize = 1_920;
/// Beacon freeze window that flips the callback to self-rendering.
const HOST_IDLE_AFTER: Duration = Duration::from_millis(150);

/// Per-stream callback state (owned by the closure; single cpal thread).
struct CbState {
    channels: usize,
    /// Device sample rate (48 kHz passthrough when equal to TARGET_RATE).
    rate: u32,
    /// 48 kHz mono source block.
    src48: Vec<f32>,
    /// Device-rate mono scratch.
    mono: Vec<f32>,
    /// Device-rate FIFO smoothing the resampler's block granularity.
    fifo: Vec<f32>,
    /// Resampler scratch (process() clears its output each call).
    rtmp: Vec<f32>,
    resamp: Option<SincResampler>,
    // Host-liveness gate.
    last_beacon: u64,
    last_change: Instant,
    host_active: bool,
    primed: bool,
}

impl CbState {
    fn new(channels: usize, rate: u32) -> Self {
        let resamp = (rate != TARGET_RATE).then(|| SincResampler::new(TARGET_RATE, rate));
        Self {
            channels: channels.max(1),
            rate,
            src48: vec![0.0; 4096],
            mono: vec![0.0; 4096],
            fifo: Vec::with_capacity(8192),
            rtmp: Vec::with_capacity(8192),
            resamp,
            last_beacon: mixer::host_mix_frames(),
            last_change: Instant::now(),
            host_active: false,
            primed: false,
        }
    }

    /// Fill `self.mono[..frames]` with device-rate mono pad audio.
    fn fill_mono(&mut self, frames: usize) {
        // ── gate update ──
        let beacon = mixer::host_mix_frames();
        if beacon != self.last_beacon {
            self.last_beacon = beacon;
            self.last_change = Instant::now();
            if !self.host_active {
                self.host_active = true;
                self.primed = false; // fresh stream of tap audio: prime again
            }
        } else if self.host_active && self.last_change.elapsed() > HOST_IDLE_AFTER {
            self.host_active = false;
            if let Some(ring) = mixer::tap() {
                ring.clear(); // consumer-owned: drop stale tap audio
            }
            self.fifo.clear();
        }

        if self.rate == TARGET_RATE {
            self.fill_mono_48(frames);
            return;
        }
        // ── resampled path: pull 48 k blocks through the FIFO ──
        while self.fifo.len() < frames {
            let need = frames - self.fifo.len();
            let n48 = (need as u64 * TARGET_RATE as u64 / self.rate as u64 + 64)
                .min(self.src48.len() as u64) as usize;
            self.fill_src48(n48);
            if let Some(r) = self.resamp.as_mut() {
                r.process(&self.src48[..n48], &mut self.rtmp);
                self.fifo.extend_from_slice(&self.rtmp);
            }
            if self.fifo.len() < frames && n48 == 0 {
                break;
            }
        }
        let take = self.fifo.len().min(frames);
        for i in 0..take {
            self.mono[i] = self.fifo[i];
        }
        self.fifo.drain(..take);
        for v in &mut self.mono[take..frames] {
            *v = 0.0;
        }
    }

    /// 48 kHz source block into `src48[..n]` from ring or self-render.
    fn fill_src48(&mut self, n: usize) {
        self.src48[..n].fill(0.0);
        if self.host_active {
            if let Some(ring) = mixer::tap() {
                ring.mix_mono_into(&mut self.src48[..n], n, 1, TAP_BACKLOG, |_| 1.0);
            }
        } else {
            mixer::render_local_mono(&mut self.src48[..n]);
        }
    }

    /// Passthrough (48 kHz device) variant straight into `mono`.
    fn fill_mono_48(&mut self, frames: usize) {
        self.mono[..frames].fill(0.0);
        if self.host_active {
            if let Some(ring) = mixer::tap() {
                // Prime: hold silence until the producer buffered PRIME_SAMPLES,
                // so clock drift doesn't machine-gun underruns.
                if !self.primed {
                    if ring.available() >= PRIME_SAMPLES {
                        self.primed = true;
                    } else {
                        return;
                    }
                }
                ring.mix_mono_into(&mut self.mono[..frames], frames, 1, TAP_BACKLOG, |_| 1.0);
            }
        } else {
            mixer::render_local_mono(&mut self.mono[..frames]);
        }
    }
}

/// Expand mono → interleaved device buffer with the local gain applied.
fn write_out_f32(data: &mut [f32], mono: &[f32], channels: usize) {
    let gain = f32::from_bits(LOCAL_GAIN_BITS.load(Ordering::Relaxed));
    let frames = data.len() / channels.max(1);
    if gain == 0.0 {
        for v in data.iter_mut() {
            *v = 0.0;
        }
        return;
    }
    for fi in 0..frames {
        let v = mono[fi.min(mono.len().saturating_sub(1))] * gain;
        for c in 0..channels {
            data[fi * channels + c] = v;
        }
    }
}

fn write_out_i16(data: &mut [i16], mono: &[f32], channels: usize) {
    let gain = f32::from_bits(LOCAL_GAIN_BITS.load(Ordering::Relaxed));
    let frames = data.len() / channels.max(1);
    for fi in 0..frames {
        let v = mono[fi.min(mono.len().saturating_sub(1))] * gain;
        let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        for c in 0..channels {
            data[fi * channels + c] = s;
        }
    }
}

/// A live local-output stream (owned by the decode worker thread; dropping
/// closes the device and disarms the tap).
pub struct LocalOut {
    stream: cpal::Stream,
    playing: bool,
}

impl LocalOut {
    /// Open the default output device (paused). Device I/O — worker thread
    /// only.
    pub fn build() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device".to_string())?;
        let supported = pick_config(&device)?;
        let cfg: cpal::StreamConfig = supported.config();
        let channels = cfg.channels as usize;
        let rate = cfg.sample_rate;
        let fmt = supported.sample_format();
        crate::sptrace!("monitor: open device ch={channels} rate={rate} fmt={fmt:?}");

        let err_cb = |e: cpal::Error| {
            crate::sptrace!("monitor: stream error: {e}");
        };
        let stream = match fmt {
            cpal::SampleFormat::F32 => {
                let mut st = CbState::new(channels, rate);
                device
                    .build_output_stream::<f32, _, _>(
                        cfg.clone(),
                        move |data: &mut [f32], _| {
                            let frames = data.len() / st.channels.max(1);
                            ensure_capacity(&mut st, frames);
                            st.fill_mono(frames);
                            write_out_f32(data, &st.mono, st.channels);
                        },
                        err_cb,
                        None,
                    )
                    .map_err(|e| e.to_string())?
            }
            cpal::SampleFormat::I16 => {
                let mut st = CbState::new(channels, rate);
                device
                    .build_output_stream::<i16, _, _>(
                        cfg.clone(),
                        move |data: &mut [i16], _| {
                            let frames = data.len() / st.channels.max(1);
                            ensure_capacity(&mut st, frames);
                            st.fill_mono(frames);
                            write_out_i16(data, &st.mono, st.channels);
                        },
                        err_cb,
                        None,
                    )
                    .map_err(|e| e.to_string())?
            }
            f => return Err(format!("unsupported output sample format {f:?}")),
        };
        mixer::tap_arm();
        Ok(Self {
            stream,
            playing: false,
        })
    }

    /// play()/pause() the device stream (idempotent).
    pub fn set_playing(&mut self, want: bool) {
        if want == self.playing {
            return;
        }
        let r = if want {
            self.stream.play()
        } else {
            self.stream.pause()
        };
        match r {
            Ok(()) => {
                self.playing = want;
                crate::sptrace!("monitor: stream {}", if want { "playing" } else { "paused" });
            }
            Err(e) => crate::sptrace!("monitor: play/pause failed: {e}"),
        }
    }
}

impl Drop for LocalOut {
    fn drop(&mut self) {
        mixer::tap_disarm();
    }
}

/// Callback scratch may be undersized for a large device buffer (rare, but
/// some backends ask for 1024+ frames): grow once, in place. Vec growth here
/// allocates, but only on the first oversized callback — steady state is
/// allocation-free. (A hard real-time purist would pre-size to the max
/// supported buffer; the audible worst case of one realloc is a single
/// ~10 ms hiccup at stream start.)
fn ensure_capacity(st: &mut CbState, frames: usize) {
    if st.mono.len() < frames {
        st.mono.resize(frames, 0.0);
    }
    if st.src48.len() < frames {
        st.src48.resize(frames, 0.0);
    }
}

/// Prefer F32 @ 48 kHz; fall back to I16 @ 48 kHz, then to the device
/// default (the callback resamples anything != 48 kHz).
fn pick_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig, String> {
    let default = device
        .default_output_config()
        .map_err(|e| format!("default output config: {e}"))?;
    let Ok(ranges) = device.supported_output_configs() else {
        return Ok(default);
    };
    let mut best: Option<(u8, cpal::SupportedStreamConfig)> = None;
    for range in ranges {
        let fmt_score = match range.sample_format() {
            cpal::SampleFormat::F32 => 2u8,
            cpal::SampleFormat::I16 => 1,
            _ => 0,
        };
        if fmt_score == 0 {
            continue;
        }
        // Only consider ranges that can run at the chain rate (48 kHz) —
        // anything else would force the resampled path for no benefit.
        if range.min_sample_rate() > TARGET_RATE || TARGET_RATE > range.max_sample_rate() {
            continue;
        }
        let cfg = range.with_sample_rate(TARGET_RATE);
        if best.as_ref().map_or(true, |(s, _)| fmt_score > *s) {
            best = Some((fmt_score, cfg));
        }
    }
    Ok(best.map(|(_, c)| c).unwrap_or(default))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_scaling() {
        set_local_gain(0.5);
        let mono = [1.0f32, -1.0, 0.5, 0.0];
        let mut out = [0.0f32; 8];
        write_out_f32(&mut out, &mono, 2);
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6, "mono duplicates to both ch");
        assert!((out[2] + 0.5).abs() < 1e-6);
        set_local_gain(0.0);
        write_out_f32(&mut out, &mono, 2);
        assert!(out.iter().all(|&x| x == 0.0));
        set_local_gain(1.0);
    }

    #[test]
    fn i16_conversion_clamps() {
        set_local_gain(1.0);
        let mono = [2.0f32, -2.0];
        let mut out = [0i16; 2];
        write_out_i16(&mut out, &mono, 1);
        assert_eq!(out[0], 32767);
        assert_eq!(out[1], -32767);
    }

    #[test]
    fn wanted_flag_roundtrip() {
        set_wanted(true);
        assert!(wanted());
        set_wanted(false);
        assert!(!wanted());
    }
}
