//! Lock-free single-producer / single-consumer ring buffer for f32 mono audio.
//!
//! Producer: the host real-time audio thread (`push` — the soundpad tap in
//! `mixer::process_slice` writes the pad-only mixdown here while streaming).
//! Consumer: the local-output (cpal) callback thread (`mix_mono_into`) — and,
//! only while the stream is stopped, the decode worker thread (`clear`).
//!
//! Copied from Focus-Capture (same producer/consumer discipline, roles
//! swapped); see its TECHNICAL.md §8 for the design rationale.
//!
//! Design notes (real-time safety):
//! * Positions are *monotonic* 64-bit counters; the slot index is `pos & mask`
//!   (capacity is a power of two). Full vs empty is unambiguous without
//!   sacrificing a slot, and the counters cannot realistically wrap
//!   (2^64 samples @ 48 kHz ≈ 12 million years).
//! * The producer publishes samples with a Release store on `write`; the
//!   consumer reads them after an Acquire load — the slot ranges
//!   `[read, write)` (consumer) and `[write, read + cap)` (producer) never
//!   overlap, so no locks, no CAS, no allocation on either hot path.
//! * `mix_mono_into` loads `write` exactly once per audio frame and stores
//!   `read` exactly once — constant atomic traffic regardless of frame size.
//! * Backlog clamping (drop-oldest) is done exclusively by the consumer, so
//!   advancing `read` past unread samples is race-free.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub struct AudioRing {
    buf: UnsafeCell<Vec<f32>>,
    mask: usize,
    /// Monotonic write position (producer-owned; consumer loads Acquire).
    write: AtomicUsize,
    /// Monotonic read position (consumer-owned; producer loads Acquire).
    read: AtomicUsize,
    /// Samples dropped by the producer because the ring was full (diagnostics).
    pub dropped: AtomicU64,
    /// Underruns (silent frames) observed by the consumer (diagnostics).
    pub underruns: AtomicU64,
}

// SAFETY: the SPSC discipline above guarantees `buf` slots are accessed by at
// most one thread at a time; the atomics synchronize ownership hand-over.
unsafe impl Send for AudioRing {}
unsafe impl Sync for AudioRing {}

impl AudioRing {
    /// `capacity` must be a power of two (panics otherwise — called once at
    /// init with a constant, not on any hot path).
    pub fn new(capacity: usize) -> Self {
        assert!(capacity.is_power_of_two() && capacity >= 2, "ring capacity must be a power of two >= 2");
        Self {
            buf: UnsafeCell::new(vec![0.0; capacity]),
            mask: capacity - 1,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
        }
    }

    fn cap(&self) -> usize {
        self.mask + 1
    }

    /// Samples ready to be consumed (Acquire-paired with the producer's store).
    #[inline]
    #[allow(dead_code)] // used by unit tests / diagnostics
    pub fn available(&self) -> usize {
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Relaxed); // consumer owns `read`
        w.wrapping_sub(r)
    }

    /// Producer: append samples. When the ring is full the *newest* samples
    /// are dropped (the consumer keeps latency bounded from the other side by
    /// purging the oldest — see `mix_mono_into`). Returns the number written.
    pub fn push(&self, data: &[f32]) -> usize {
        if data.is_empty() {
            return 0;
        }
        let w = self.write.load(Ordering::Relaxed); // producer owns `write`
        let r = self.read.load(Ordering::Acquire);
        let free = self.cap().saturating_sub(w.wrapping_sub(r));
        let n = data.len().min(free);
        if n < data.len() {
            self.dropped
                .fetch_add((data.len() - n) as u64, Ordering::Relaxed);
        }
        if n == 0 {
            return 0;
        }
        let buf = unsafe { &mut *self.buf.get() };
        let start = w & self.mask;
        let first = (self.cap() - start).min(n);
        buf[start..start + first].copy_from_slice(&data[..first]);
        if n > first {
            buf[..n - first].copy_from_slice(&data[first..n]);
        }
        // Release: the samples above are visible before the position update.
        self.write.store(w.wrapping_add(n), Ordering::Release);
        n
    }

    /// Consumer: reset the timeline (only legal while the audio thread is not
    /// consuming, i.e. before capture starts — `CAPTURING == false`).
    pub fn clear(&self) {
        let w = self.write.load(Ordering::Acquire);
        self.read.store(w, Ordering::Release);
    }

    /// Consumer: mix up to `frames` mono samples into interleaved `data`
    /// (`channels` per frame), scaling each by `coef(frame_index)`.
    ///
    /// * Stale audio older than `backlog` samples is discarded first, so a
    ///   paused/stalled consumer never injects seconds-old sound on resume
    ///   (bounded-latency guarantee).
    /// * Missing samples (underrun) contribute silence; the count is tracked
    ///   for diagnostics.
    /// * Zero heap allocation, two atomic operations total.
    ///
    /// Returns the number of frames fed with real (non-silent) samples.
    pub fn mix_mono_into(
        &self,
        data: &mut [f32],
        frames: usize,
        channels: usize,
        backlog: usize,
        mut coef: impl FnMut(usize) -> f32,
    ) -> usize {
        debug_assert!(channels >= 1);
        let w = self.write.load(Ordering::Acquire);
        let mut r = self.read.load(Ordering::Relaxed);

        // Clamp latency: drop oldest beyond the backlog budget.
        let avail0 = w.wrapping_sub(r);
        if avail0 > backlog {
            r = r.wrapping_add(avail0 - backlog);
        }

        let take = w.wrapping_sub(r).min(frames);
        if take < frames {
            self.underruns
                .fetch_add((frames - take) as u64, Ordering::Relaxed);
        }
        if take == 0 {
            // Still run the envelope callback so fades keep advancing? No —
            // the caller tracks the envelope; nothing to mix here.
            self.read.store(r, Ordering::Release);
            return 0;
        }

        let buf = unsafe { &*self.buf.get() };
        let start = r & self.mask;
        let first = (self.cap() - start).min(take);
        // Segment 1 (no wrap) then segment 2 (wrapped head of the buffer).
        mix_segment(&buf[start..start + first], data, channels, 0, &mut coef);
        if take > first {
            mix_segment(&buf[..take - first], data, channels, first, &mut coef);
        }

        self.read.store(r.wrapping_add(take), Ordering::Release);
        take
    }
}

/// Add `seg[i] * coef(frame_base + i)` to every channel of interleaved frame
/// `frame_base + i` in `data`.
fn mix_segment(
    seg: &[f32],
    data: &mut [f32],
    channels: usize,
    frame_base: usize,
    coef: &mut impl FnMut(usize) -> f32,
) {
    for (i, &s) in seg.iter().enumerate() {
        let frame = frame_base + i;
        let g = coef(frame);
        if g == 0.0 {
            continue;
        }
        let base = frame * channels;
        for c in 0..channels {
            // Bounds are guaranteed by the caller: data.len() >= frames*channels.
            if let Some(slot) = data.get_mut(base + c) {
                *slot += s * g;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_roundtrip() {
        let ring = AudioRing::new(16);
        assert_eq!(ring.push(&[1.0, 2.0, 3.0]), 3);
        assert_eq!(ring.available(), 3);
        let mut out = [0.0f32; 6]; // 6 frames mono
        let fed = ring.mix_mono_into(&mut out, 6, 1, 16, |_| 1.0);
        assert_eq!(fed, 3);
        assert_eq!(out, [1.0, 2.0, 3.0, 0.0, 0.0, 0.0]);
        assert_eq!(ring.underruns.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn wraparound_is_seamless() {
        let ring = AudioRing::new(8);
        ring.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]); // fill 6 of 8
        let mut out = [0.0f32; 6];
        ring.mix_mono_into(&mut out, 6, 1, 8, |_| 1.0); // drain → read at 6
        ring.push(&[7.0, 8.0, 9.0, 10.0]); // wraps over the seam
        let mut out2 = [0.0f32; 4];
        let fed = ring.mix_mono_into(&mut out2, 4, 1, 8, |_| 1.0);
        assert_eq!(fed, 4);
        assert_eq!(out2, [7.0, 8.0, 9.0, 10.0]);
    }

    #[test]
    fn full_ring_drops_newest_and_counts() {
        let ring = AudioRing::new(4);
        assert_eq!(ring.push(&[1.0, 2.0, 3.0, 4.0, 5.0]), 4);
        assert_eq!(ring.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(ring.push(&[6.0]), 0);
    }

    #[test]
    fn consumer_purges_stale_backlog() {
        let ring = AudioRing::new(64);
        ring.push(&(0..32).map(|i| i as f32).collect::<Vec<_>>());
        let mut out = [0.0f32; 4];
        // backlog 4 → the 28 oldest samples must be dropped, keeping the tail.
        let fed = ring.mix_mono_into(&mut out, 4, 1, 4, |_| 1.0);
        assert_eq!(fed, 4);
        assert_eq!(out, [28.0, 29.0, 30.0, 31.0]);
    }

    #[test]
    fn stereo_upmix_adds_to_all_channels() {
        let ring = AudioRing::new(16);
        ring.push(&[0.5, 1.0]);
        let mut out = [0.25f32; 4]; // 2 stereo frames
        let fed = ring.mix_mono_into(&mut out, 2, 2, 16, |_| 2.0);
        assert_eq!(fed, 2);
        assert_eq!(out, [1.25, 1.25, 2.25, 2.25]);
    }
}
