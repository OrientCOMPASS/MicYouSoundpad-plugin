//! Real-time multi-voice mixer: a fixed table of playback slots shared by
//! three thread contexts with lock-free handoffs.
//!
//! | Context | Allowed slot operations |
//! | --- | --- |
//! | bus thread (`handle_message`, watchdog tick) | `reserve`, `begin_stop`, `set_gain`, `set_loop`, `set_pos`, `snapshot`, `reclaim` |
//! | decode worker thread | `publish` (LOADING → ACTIVE), `fail` (LOADING → EMPTY) |
//! | host real-time audio thread | `process_slice` only: reads state/track/pos/gain, advances pos, fades env, frees nothing |
//! | local-output (cpal) callback thread | `render_local_mono` — ONLY while the host chain is provably
//!   inactive (`host_mix_frames()` frozen for > 150 ms), so exactly one
//!   renderer ever advances positions/envelopes at a time |
//!
//! Two render paths share the same slot machine:
//! * **Host path** ([`process_slice`]): voices are rendered into a static
//!   scratch buffer with per-session gains, scaled by the master gain into
//!   the mic frame, and — when the local-output tap is armed — the
//!   pre-master mono mixdown is pushed into [`tap_ring`] for the headphones
//!   stream (see `monitor.rs`).
//! * **Local path** ([`render_local_mono`]): used by the cpal callback when
//!   MicYou is NOT streaming (host audio thread idle), so pad sounds remain
//!   audible for offline preview. Applies no master gain (the monitor module
//!   scales by its own local volume).
//!
//! Memory discipline: decoded [`Track`]s travel as `Box<Arc<Track>>` leaked
//! into the slot's atomic pointer while the slot is live. Only the bus thread
//! ever converts the pointer back (`reclaim`), and only after the audio
//! thread has parked the slot in `ST_EMPTY` (Release/Acquire handoff) — the
//! audio thread itself never frees. Tracks are additionally kept in a
//! decode-side cache (see `decode.rs`), so the same sound replayed twice
//! decodes once.
//!
//! Envelope: every start/stop/natural-end ramps over [`FADE_SAMPLES`]
//! (10 ms @ 48 kHz) — click-free even when many voices overlap.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use crate::ring::AudioRing;

/// Host DSP chain runs at a fixed 48 kHz (micyou-plugin `PluginDspBridge`).
pub const TARGET_RATE: u32 = 48_000;
/// Simultaneous voices. 16 covers any realistic pad mashing; slots are
/// ~100 bytes each, so the table is trivially small.
pub const SLOT_COUNT: usize = 16;
/// Fade in/out length for click-free start & stop (10 ms @ 48 kHz).
pub const FADE_SAMPLES: f32 = 480.0;

pub const ST_EMPTY: u8 = 0;
pub const ST_LOADING: u8 = 1;
pub const ST_ACTIVE: u8 = 2;
pub const ST_STOPPING: u8 = 3;

/// Decoded, resampled sound: interleaved f32 @ 48 kHz, 1 or 2 channels.
pub struct Track {
    pub samples: Box<[f32]>,
    /// 1 = mono, 2 = stereo (the decode worker downmixes anything wider).
    pub channels: usize,
    pub frames: usize,
}

impl Track {
    #[allow(dead_code)] // 诊断辅助（测试使用）
    pub fn duration_ms(&self) -> u64 {
        self.frames as u64 * 1000 / TARGET_RATE as u64
    }
}

pub struct Slot {
    state: AtomicU8,
    /// `Box<Arc<Track>>` leaked while the slot is live; null when EMPTY.
    track: AtomicPtr<Arc<Track>>,
    /// Current playback position in frames (audio thread advances; bus seeks).
    pos: AtomicU64,
    /// Total frames of the published track (0 until published).
    len: AtomicU64,
    /// Per-session linear gain as f32 bits.
    gain: AtomicU32,
    looping: AtomicBool,
    /// Set by the bus thread when a LOADING session is stopped: the worker
    /// then skips publishing and parks the slot.
    cancel: AtomicBool,
    /// Session id for diagnostics (bus-owned; audio never reads it).
    sid: AtomicU32,
    /// Fade envelope as f32 bits. Written only by the ACTIVE renderer (host
    /// audio thread, or the local-output callback while the host chain is
    /// provably idle — see the module docs); atomic so the ownership handoff
    /// can never tear.
    env: AtomicU32,
}

// SAFETY: every field is an atomic.
unsafe impl Sync for Slot {}

impl Slot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(ST_EMPTY),
            track: AtomicPtr::new(std::ptr::null_mut()),
            pos: AtomicU64::new(0),
            len: AtomicU64::new(0),
            gain: AtomicU32::new(1.0f32.to_bits()),
            looping: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            sid: AtomicU32::new(0),
            env: AtomicU32::new(0),
        }
    }

    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }
    pub fn position_frames(&self) -> u64 {
        self.pos.load(Ordering::Relaxed)
    }
    pub fn length_frames(&self) -> u64 {
        self.len.load(Ordering::Relaxed)
    }
}

static SLOTS: [Slot; SLOT_COUNT] = [const { Slot::new() }; SLOT_COUNT];

/// Master mix gain as f32 bits (bus thread writes, audio thread reads).
static MASTER_BITS: AtomicU32 = AtomicU32::new(1.0f32.to_bits());

pub fn slot(i: usize) -> &'static Slot {
    &SLOTS[i]
}

pub fn set_master_gain(g: f32) {
    MASTER_BITS.store(g.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
}

/// Reset every slot to EMPTY **without** freeing tracks. Only legal from
/// `init`/`deinit`, where the host guarantees no concurrent `process()`
/// (DSP (un)registration blocks until in-flight frames finish).
///
/// # Safety
/// Caller must ensure no audio-thread `process_slice` is running, and must
/// have reclaimed (or intentionally leaked) any live tracks — `deinit` uses
/// [`reclaim_all`].
pub unsafe fn reset_all() {
    arm_scratch();
    *DUCK.0.get() = (1.0, 0);
    for s in SLOTS.iter() {
        s.state.store(ST_EMPTY, Ordering::Relaxed);
        s.pos.store(0, Ordering::Relaxed);
        s.len.store(0, Ordering::Relaxed);
        s.cancel.store(false, Ordering::Relaxed);
        s.sid.store(0, Ordering::Relaxed);
        s.env.store(0, Ordering::Relaxed);
    }
}

/// Free the track of every parked slot (deinit path). Live (non-EMPTY) slots
/// are force-stopped first; per the `reset_all` contract no audio thread is
/// concurrent, so the fade never runs — dropping is immediate.
pub fn reclaim_all() {
    for s in SLOTS.iter() {
        s.state.store(ST_EMPTY, Ordering::Release);
        reclaim_slot(s);
    }
}

// ─────────────────────────── bus-thread operations ────────────────────────

/// Find a free slot and arm it for a new session. Returns the slot index, or
/// `None` when all [`SLOT_COUNT`] voices are busy.
pub fn reserve(sid: u32, gain: f32, looping: bool) -> Option<usize> {
    for (i, s) in SLOTS.iter().enumerate() {
        if s.state.load(Ordering::Acquire) == ST_EMPTY && s.track.load(Ordering::Relaxed).is_null()
        {
            s.pos.store(0, Ordering::Relaxed);
            s.len.store(0, Ordering::Relaxed);
            s.gain.store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
            s.looping.store(looping, Ordering::Relaxed);
            s.cancel.store(false, Ordering::Relaxed);
            s.sid.store(sid, Ordering::Relaxed);
            // LOADING last: the worker's publish CAS keys off it.
            s.state.store(ST_LOADING, Ordering::Release);
            return Some(i);
        }
    }
    None
}

/// Request a fade-out stop. ACTIVE voices ramp down over 10 ms; LOADING
/// voices are cancelled (the worker parks the slot when the decode ends).
pub fn begin_stop(i: usize) {
    let s = &SLOTS[i];
    s.cancel.store(true, Ordering::Release);
    if s.state.load(Ordering::Acquire) == ST_ACTIVE {
        s.state.store(ST_STOPPING, Ordering::Release);
    }
}

pub fn set_gain(i: usize, gain: f32) {
    SLOTS[i]
        .gain
        .store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
}

pub fn set_looping(i: usize, looping: bool) {
    SLOTS[i].looping.store(looping, Ordering::Relaxed);
}

/// Seek to an absolute frame position (clamped to the track).
pub fn set_pos_frames(i: usize, frames: u64) {
    let s = &SLOTS[i];
    let len = s.len.load(Ordering::Relaxed);
    s.pos.store(frames.min(len.saturating_sub(1)), Ordering::Relaxed);
}

/// Free a parked slot's track (EMPTY only). Safe from the bus/worker thread
/// once the audio thread has released the slot.
pub fn reclaim(i: usize) {
    reclaim_slot(&SLOTS[i]);
}

/// Test-only: park a slot regardless of envelope state (unit tests have no audio
/// thread to run the fade) and free its track.
#[cfg(test)]
pub fn force_park(i: usize) {
    SLOTS[i].state.store(ST_EMPTY, Ordering::Release);
    reclaim_slot(&SLOTS[i]);
}

fn reclaim_slot(s: &Slot) {
    if s.state.load(Ordering::Acquire) != ST_EMPTY {
        return;
    }
    let p = s.track.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if !p.is_null() {
        // SAFETY: the pointer was created by `publish` via Box::into_raw and
        // the slot is EMPTY — the audio thread no longer references it.
        unsafe { drop(Box::from_raw(p)) };
    }
    s.len.store(0, Ordering::Relaxed);
    s.pos.store(0, Ordering::Relaxed);
    s.cancel.store(false, Ordering::Relaxed);
}

// ───────────────────────── worker-thread operations ───────────────────────

/// Hand the decoded track to the slot (LOADING → ACTIVE). Returns false when
/// the session was cancelled meanwhile — the caller keeps ownership of `track`
/// (it goes back to the cache or is dropped).
pub fn publish(i: usize, track: Arc<Track>) -> bool {
    let s = &SLOTS[i];
    if s.cancel.load(Ordering::Acquire) {
        // Bus already cancelled: park the slot so it can be reused.
        s.state.store(ST_EMPTY, Ordering::Release);
        return false;
    }
    let frames = track.frames as u64;
    let raw = Box::into_raw(Box::new(track));
    s.track.store(raw, Ordering::Relaxed);
    s.len.store(frames, Ordering::Relaxed);
    match s
        .state
        .compare_exchange(ST_LOADING, ST_ACTIVE, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => {
            // Stop may have landed between the cancel check and the CAS.
            if s.cancel.load(Ordering::Acquire) {
                s.state.store(ST_STOPPING, Ordering::Release);
            }
            true
        }
        Err(_) => {
            // Slot was parked underneath us (cancel raced): take the box back.
            // SAFETY: we just created it and never published it.
            unsafe { drop(Box::from_raw(raw)) };
            s.state.store(ST_EMPTY, Ordering::Release);
            false
        }
    }
}

/// Decode failed / job cancelled while LOADING: park the slot.
pub fn fail(i: usize) {
    let s = &SLOTS[i];
    if s.state.load(Ordering::Acquire) == ST_LOADING {
        s.state.store(ST_EMPTY, Ordering::Release);
    }
}

// ─────────────────────── audio-thread mix (real-time) ─────────────────────

/// Max frames per scratch render pass (covers every realistic host frame
/// size; longer calls are chunked).
const MAX_SCRATCH_FRAMES: usize = 8192;
/// Local-output tap: ~0.34 s of mono 48 kHz (power of two).
const TAP_CAP: usize = 16_384;
/// Consumer-side latency clamp for the tap ring (100 ms @ 48 kHz).
pub const TAP_BACKLOG: usize = 4_800;

/// Scratch render buffer — exclusive to the host real-time audio thread
/// (`process_slice` is serialized on a single thread). Sized by
/// [`arm_scratch`] during init/deinit (never on the audio thread).
/// Layout: `[0 .. MAX*2)` stereo render area, `[MAX*2 .. MAX*3)` mono tap
/// downmix area.
struct Scratch(std::cell::UnsafeCell<Vec<f32>>);
// SAFETY: single-thread (audio) ownership documented above.
unsafe impl Sync for Scratch {}
static SCRATCH: Scratch = Scratch(std::cell::UnsafeCell::new(Vec::new()));

/// Pre-master mono mixdown handed to the local-output (headphones) stream.
static TAP: OnceLock<AudioRing> = OnceLock::new();
/// Armed while the local-output stream exists (monitor.rs owns the flag).
static TAP_ON: AtomicBool = AtomicBool::new(false);
/// Liveness beacon: total frames mixed by the HOST audio thread. The local
/// output gate watches this freeze to decide when it may self-render.
static HOST_MIX_FRAMES: AtomicU64 = AtomicU64::new(0);

/// Size the audio-thread scratch (init/deinit only — no concurrent render).
pub fn arm_scratch() {
    let s = unsafe { &mut *SCRATCH.0.get() };
    let need = MAX_SCRATCH_FRAMES * 3;
    if s.len() < need {
        s.resize(need, 0.0);
    }
}

pub fn tap_arm() {
    let r = TAP.get_or_init(|| AudioRing::new(TAP_CAP));
    r.clear();
    TAP_ON.store(true, Ordering::Release);
}

pub fn tap_disarm() {
    TAP_ON.store(false, Ordering::Release);
}

/// The tap ring, when armed (host audio thread producer side).
fn tap_ring() -> Option<&'static AudioRing> {
    if TAP_ON.load(Ordering::Acquire) {
        TAP.get()
    } else {
        None
    }
}

/// The tap ring regardless of arming (local-output consumer side), plus the
/// consumer's ability to clear it on mode transitions.
pub fn tap() -> Option<&'static AudioRing> {
    TAP.get()
}

/// Frames mixed by the host audio thread so far (liveness beacon).
pub fn host_mix_frames() -> u64 {
    HOST_MIX_FRAMES.load(Ordering::Relaxed)
}

// ── VAD anti-collision ducking (host render path only) ────────────────────
//
// The pad node sits after the host's AEC/VAD stages, so the frame handed to
// `process_slice` is the (cleaned) MIC signal BEFORE our pad audio is added
// — the perfect VAD source: zero latency, no Host API calls, and the pad can
// never trigger itself. When speech is detected the pad mixdown is ducked to
// 20% (attack 0.15/frame, release 0.02/frame — same feel as the classic
// MicYouSoundpad client), leaving the voice dominant. Mic passthrough is
// untouched. The local self-render path (`render_local_mono`, host idle)
// never ducks: no stream = no mic = nothing to collide with.

/// Anti-collision master switch (bus thread writes from config/commands).
static VAD_GUARD_ON: AtomicBool = AtomicBool::new(false);
/// Speech RMS threshold as f32 bits (bus thread writes).
static VAD_THRESHOLD_BITS: AtomicU32 = AtomicU32::new(0.06f32.to_bits());
/// Duck envelope + hangover counter — host audio-thread exclusive
/// (process_slice is serialized), hence UnsafeCell is sound.
struct DuckCell(std::cell::UnsafeCell<(f32, u32)>);
// SAFETY: single-thread (host audio) ownership documented above.
unsafe impl Sync for DuckCell {}
static DUCK: DuckCell = DuckCell(std::cell::UnsafeCell::new((1.0, 0)));

const DUCK_TARGET: f32 = 0.2;
const DUCK_ATTACK: f32 = 0.15;
const DUCK_RELEASE: f32 = 0.02;
/// Speech hangover: keep ducking this many frames below threshold
/// (~300 ms @ 10 ms frames) so ducking doesn't pump between syllables.
const DUCK_HOLD_FRAMES: u32 = 30;

pub fn set_vad_guard(on: bool) {
    // The envelope itself is host-audio-thread exclusive; when the guard is
    // off, `process_slice` re-pins it to unity, so toggling needs no reset.
    VAD_GUARD_ON.store(on, Ordering::Release);
}

pub fn set_vad_threshold(t: f32) {
    VAD_THRESHOLD_BITS.store(t.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

/// True when any slot holds a live (mixing or fading) voice.
fn any_live() -> bool {
    SLOTS.iter().any(|s| {
        let st = s.state.load(Ordering::Acquire);
        st == ST_ACTIVE || st == ST_STOPPING
    })
}

/// Core voice renderer: mix every live slot into `data` (`frames * channels`
/// interleaved f32) with per-session gains (NO master/local scaling — the
/// calling path applies its own output gain). Advances positions and fades
/// envelopes; allocation-free, lock-free, bounded.
fn mix_voices(data: &mut [f32], channels: usize) -> bool {
    let frames = data.len() / channels.max(1);
    if frames == 0 {
        return false;
    }
    let mut any = false;

    for s in SLOTS.iter() {
        let st = s.state.load(Ordering::Acquire);
        if st == ST_EMPTY {
            if s.env.load(Ordering::Relaxed) != 0 {
                s.env.store(0, Ordering::Relaxed);
            }
            continue;
        }
        if st == ST_LOADING {
            continue; // nothing published yet; envelope stays at 0
        }
        any = true;
        let track = unsafe { &*s.track.load(Ordering::Relaxed) }; // valid: state ≥ ACTIVE
        let tf = track.frames;
        let tch = track.channels.max(1);
        let gain = f32::from_bits(s.gain.load(Ordering::Relaxed));
        let looping = s.looping.load(Ordering::Relaxed);
        let mut pos = s.pos.load(Ordering::Relaxed) as usize;
        let mut env = f32::from_bits(s.env.load(Ordering::Relaxed));
        let mut target = if st == ST_ACTIVE { 1.0 } else { 0.0 };
        let mut ended = false;
        let step = 1.0 / FADE_SAMPLES;

        for fi in 0..frames {
            // Envelope ramp (per sample, like Focus-Capture's mix callback).
            if env < target {
                env = (env + step).min(target);
            } else if env > target {
                env = (env - step).max(target);
                // Snap: 480 accumulated float subtractions can leave a ~1e-9
                // residue, which would keep a finished voice one extra call
                // alive (and its slot unparked). Inaudible either way.
                if env - target < 1e-4 {
                    env = target;
                }
            }
            if env <= 0.0 {
                if target == 0.0 {
                    break; // fully faded out
                }
                continue; // ramping up from silence: nothing to add yet
            }
            if pos >= tf {
                if looping && tf > 0 {
                    pos %= tf;
                } else {
                    // Natural end: fade out from here (state flip is seen by
                    // the watchdog; the renderer keeps envelope math local).
                    target = 0.0;
                    ended = true;
                    if env <= 0.0 {
                        break;
                    }
                    // No more source data: just ride the fade to zero.
                    continue;
                }
            }
            let scale = gain * env;
            if scale != 0.0 {
                let base = pos * tch;
                let out = &mut data[fi * channels..fi * channels + channels];
                match (tch, channels) {
                    (1, _) => {
                        let v = track.samples[base] * scale;
                        for o in out.iter_mut() {
                            *o += v;
                        }
                    }
                    (_, 1) => {
                        let mut sum = 0.0f32;
                        for c in 0..tch {
                            sum += track.samples[base + c];
                        }
                        out[0] += sum * (1.0 / tch as f32) * scale;
                    }
                    (2, _) => {
                        out[0] += track.samples[base] * scale;
                        if channels > 1 {
                            out[1] += track.samples[base + 1] * scale;
                        }
                    }
                    _ => {
                        for c in 0..tch.min(channels) {
                            out[c] += track.samples[base + c] * scale;
                        }
                    }
                }
            }
            pos += 1;
        }

        s.pos.store(pos as u64, Ordering::Relaxed);
        s.env.store(env.to_bits(), Ordering::Relaxed);
        if ended || target == 0.0 {
            if env <= 0.0 {
                // Last operation on the slot: Release pairs with the bus
                // thread's Acquire in `reclaim`.
                s.state.store(ST_EMPTY, Ordering::Release);
            } else if st == ST_ACTIVE && ended {
                s.state.store(ST_STOPPING, Ordering::Release);
            }
        }
    }
    any
}

/// HOST real-time path: render all voices with the master gain into the mic
/// frame, and feed the pre-master mono mixdown to the local-output tap.
/// Allocation-free (static scratch), lock-free, bounded.
///
/// # Safety contract (FFI callers)
/// Runs exclusively on the host real-time audio thread; `data` spans
/// `frames * channels` writable interleaved f32s.
pub fn process_slice(data: &mut [f32], channels: usize) -> bool {
    let channels = channels.max(1);
    let frames = data.len() / channels;
    if frames == 0 {
        return false;
    }
    // Liveness beacon first: even a fully bypassed frame proves the host
    // audio thread is running (the local-output gate depends on it).
    HOST_MIX_FRAMES.fetch_add(frames as u64, Ordering::Relaxed);
    if !any_live() {
        return false;
    }
    let scratch = unsafe { &mut *SCRATCH.0.get() };
    if scratch.len() < MAX_SCRATCH_FRAMES * 3 {
        // arm_scratch() never ran (init contract broken) — fail silent-safe.
        return false;
    }
    let master = f32::from_bits(MASTER_BITS.load(Ordering::Relaxed));
    let render_ch = channels.min(2);
    let mono_off = MAX_SCRATCH_FRAMES * 2;

    // Anti-collision duck gain for this frame (computed from the MIC input,
    // i.e. `data` before we add the pad).
    let duck = if VAD_GUARD_ON.load(Ordering::Acquire) {
        vad_duck_step(&data[..frames * channels])
    } else {
        // Keep the envelope settled at unity so enabling mid-stream is smooth.
        let cell = unsafe { &mut *DUCK.0.get() };
        cell.0 = 1.0;
        cell.1 = 0;
        1.0
    };

    for chunk_start in (0..frames).step_by(MAX_SCRATCH_FRAMES) {
        let n = (frames - chunk_start).min(MAX_SCRATCH_FRAMES);
        let (render_area, rest) = scratch.split_at_mut(n * render_ch);
        render_area.fill(0.0);
        mix_voices(render_area, render_ch);
        if duck != 1.0 {
            for v in render_area.iter_mut() {
                *v *= duck;
            }
        }

        // Tap: pre-master mono mixdown for the headphones stream.
        if let Some(ring) = tap_ring() {
            if render_ch == 1 {
                ring.push(render_area);
            } else {
                let mono = &mut rest[mono_off - n * render_ch..][..n];
                for fi in 0..n {
                    mono[fi] = (render_area[fi * 2] + render_area[fi * 2 + 1]) * 0.5;
                }
                ring.push(mono);
            }
        }

        // Expand-add into the mic frame with the master gain.
        let dst = &mut data[chunk_start * channels..(chunk_start + n) * channels];
        if channels == render_ch {
            for i in 0..n * render_ch {
                dst[i] += render_area[i] * master;
            }
        } else {
            // channels > 2: L/R to the front pair, mono to the rest.
            for fi in 0..n {
                let l = render_area[fi * 2] * master;
                let r = render_area[fi * 2 + 1] * master;
                let base = fi * channels;
                dst[base] += l;
                dst[base + 1] += r;
                let m = (l + r) * 0.5;
                for c in 2..channels {
                    dst[base + c] += m;
                }
            }
        }
    }
    true
}

/// One VAD + duck-envelope step (host audio thread only).
/// RMS over the incoming mic frame (stride-2 for cost), threshold compare
/// with hangover, then the attack/release envelope — allocation-free.
fn vad_duck_step(data: &[f32]) -> f32 {
    let thr = f32::from_bits(VAD_THRESHOLD_BITS.load(Ordering::Relaxed));
    // Normalized RMS of the mic frame (stride 2 keeps it ~240 MACs/frame).
    let mut sum = 0.0f64;
    let mut cnt = 0u32;
    let mut i = 0;
    while i < data.len() {
        let n = data[i];
        sum += (n * n) as f64;
        cnt += 1;
        i += 2;
    }
    let rms = if cnt > 0 { (sum / cnt as f64).sqrt() as f32 } else { 0.0 };
    let cell = unsafe { &mut *DUCK.0.get() };
    if rms >= thr {
        cell.1 = DUCK_HOLD_FRAMES;
    } else if cell.1 > 0 {
        cell.1 -= 1;
    }
    let target = if rms >= thr || cell.1 > 0 { DUCK_TARGET } else { 1.0 };
    let env = &mut cell.0;
    if *env > target {
        *env = (*env - DUCK_ATTACK).max(target);
    } else if *env < target {
        *env = (*env + DUCK_RELEASE).min(target);
    }
    *env
}

/// LOCAL real-time path (cpal callback, host chain provably idle): render
/// all voices into a mono buffer WITHOUT master gain — the monitor module
/// applies its own local volume. Same slot machine, same advancement rules;
/// the temporal gate (see module docs) guarantees a single active renderer.
pub fn render_local_mono(out: &mut [f32]) -> bool {
    mix_voices(out, 1)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The slot table and master gain are process-wide statics; cargo runs
    /// tests on parallel threads, so every test that touches the mixer (in
    /// ANY module) serializes here.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct Guard<'a>(std::sync::MutexGuard<'a, ()>);
    pub(crate) fn lock() -> Guard<'static> {
        // A poisoned lock only means a prior test panicked mid-way — recover.
        let g = Guard(TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner()));
        // Wipe any leaked voices from a panicked test so failures never
        // cascade (the audio thread is not running under tests, so
        // reclaim_all's force-park is sound here).
        arm_scratch();
        reclaim_all();
        set_master_gain(1.0);
        g
    }

    fn sine_track(frames: usize, channels: usize) -> Arc<Track> {
        let mut samples = Vec::with_capacity(frames * channels);
        for i in 0..frames {
            let v = (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 48000.0).sin() as f32;
            for _ in 0..channels {
                samples.push(v);
            }
        }
        Arc::new(Track {
            samples: samples.into_boxed_slice(),
            channels,
            frames,
        })
    }

    /// Arm a SPECIFIC slot (tests must not fight over `reserve`'s scan).
    pub(crate) fn arm(i: usize, sid: u32, gain: f32, looping: bool) {
        let s = &SLOTS[i];
        assert_eq!(s.state.load(Ordering::Acquire), ST_EMPTY, "slot {i} busy");
        s.pos.store(0, Ordering::Relaxed);
        s.len.store(0, Ordering::Relaxed);
        s.gain.store(gain.to_bits(), Ordering::Relaxed);
        s.looping.store(looping, Ordering::Relaxed);
        s.cancel.store(false, Ordering::Relaxed);
        s.sid.store(sid, Ordering::Relaxed);
        s.state.store(ST_LOADING, Ordering::Release);
    }

    fn start(i: usize, sid: u32, track: Arc<Track>, gain: f32, looping: bool) {
        arm(i, sid, gain, looping);
        assert!(publish(i, track), "publish must succeed on a fresh slot");
    }

    fn drain(i: usize, calls: usize) {
        begin_stop(i);
        for _ in 0..calls {
            let mut buf = vec![0.0f32; 480];
            process_slice(&mut buf, 1);
        }
        assert_eq!(slot(i).state(), ST_EMPTY, "slot {i} must park after fade");
        reclaim(i);
    }

    fn rms(v: &[f32]) -> f32 {
        (v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32).sqrt()
    }

    fn peak(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, x| m.max(x.abs()))
    }

    #[test]
    fn idle_bypasses() {
        let _g = lock();
        let mut buf = vec![0.0f32; 480];
        assert!(!process_slice(&mut buf, 1));
        assert!(buf.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn publish_mix_and_natural_end() {
        let _g = lock();
        let i = 0;
        start(i, 1, sine_track(960, 1), 1.0, false);
        // 960 frames of signal, then the end is DETECTED on the first frame
        // of call 2 (target flips to 0 there) — the 10 ms fade therefore
        // completes one call later, exactly like on the live audio thread.
        let mut out = vec![0.0f32; 480 * 4];
        for c in 0..4 {
            assert!(process_slice(&mut out[c * 480..(c + 1) * 480], 1), "call {c}");
        }
        assert!(rms(&out[..480]) > 0.05, "first call too quiet");
        // Signal present late in the track (frame ~900).
        assert!(peak(&out[850..960]) > 0.05);
        // After natural end + fade the tail is silent (no source data past
        // frame 960; the fade only rides the envelope).
        assert!(
            out[960..].iter().all(|&x| x.abs() < 1e-4),
            "tail not silent"
        );
        assert_eq!(slot(i).state(), ST_EMPTY);
        reclaim(i);
    }

    #[test]
    fn stop_fades_out_and_parks() {
        let _g = lock();
        let i = 1;
        start(i, 2, sine_track(48000, 1), 1.0, false);
        let mut out = vec![0.0f32; 480 * 4];
        for c in 0..4 {
            if c == 1 {
                begin_stop(i);
            }
            let mixed = process_slice(&mut out[c * 480..(c + 1) * 480], 1);
            // Call 1 rides the 10 ms fade to silence and parks the slot;
            // later calls see an empty table.
            assert_eq!(mixed, c < 2, "call {c} mixed={mixed}");
        }
        // Fade (480 frames) fully consumed call 1: everything after is silent.
        assert!(out[2 * 480..].iter().all(|&x| x.abs() < 1e-4));
        assert_eq!(slot(i).state(), ST_EMPTY);
        reclaim(i);
    }

    #[test]
    fn loop_wraps_and_keeps_playing() {
        let _g = lock();
        let i = 2;
        start(i, 3, sine_track(240, 1), 1.0, true);
        for c in 0..4 {
            let mut buf = vec![0.0f32; 480];
            process_slice(&mut buf, 1);
            assert!(rms(&buf[64..]) > 0.05, "call {c} went silent");
        }
        // The wrap happens on the NEXT read after pos reaches the end, so at
        // a call boundary pos may transiently equal the track length.
        assert!(slot(i).position_frames() <= 240, "position must wrap");
        drain(i, 4);
    }

    #[test]
    fn seek_moves_position() {
        let _g = lock();
        let i = 3;
        start(i, 4, sine_track(48000, 1), 1.0, false);
        set_pos_frames(i, 24000);
        let mut buf = vec![0.0f32; 480];
        process_slice(&mut buf, 1);
        let pos = slot(i).position_frames();
        assert!((24000..24000 + 481).contains(&pos), "pos {pos}");
        set_pos_frames(i, 999_999);
        assert!(slot(i).position_frames() < 48000, "seek must clamp");
        drain(i, 4);
    }

    #[test]
    fn gain_and_master_scale_output() {
        let _g = lock();
        let i = 4;
        start(i, 5, sine_track(48000, 1), 1.0, false);
        set_master_gain(1.0);
        let loud = peak_after_warmup();
        set_gain(i, 0.5);
        let quiet = peak_after_warmup();
        assert!(
            (quiet - loud * 0.5).abs() < loud * 0.1 + 1e-4,
            "session gain: {quiet} vs {loud}"
        );
        set_master_gain(0.0);
        let muted = peak_after_warmup();
        assert!(muted < 1e-5, "master 0 must mute, got {muted}");
        set_master_gain(1.0);
        drain(i, 4);
    }

    fn peak_after_warmup() -> f32 {
        for _ in 0..2 {
            let mut buf = vec![0.0f32; 480];
            process_slice(&mut buf, 1);
        }
        let mut buf = vec![0.0f32; 480];
        process_slice(&mut buf, 1);
        peak(&buf)
    }

    #[test]
    fn stereo_track_to_mono_out_downmixes() {
        let _g = lock();
        let i = 5;
        start(i, 6, sine_track(4800, 2), 1.0, false);
        let mut buf = vec![0.0f32; 480];
        process_slice(&mut buf, 1);
        assert!(rms(&buf[100..]) > 0.05);
        drain(i, 4);
    }

    #[test]
    fn mono_track_to_stereo_out_fills_both() {
        let _g = lock();
        let i = 6;
        start(i, 7, sine_track(4800, 1), 1.0, false);
        let mut buf = vec![0.0f32; 960];
        process_slice(&mut buf, 2);
        for f in buf.chunks(2).skip(100) {
            assert!((f[0] - f[1]).abs() < 1e-6, "mono must duplicate");
        }
        begin_stop(i);
        for _ in 0..4 {
            let mut buf = vec![0.0f32; 960];
            process_slice(&mut buf, 2);
        }
        assert_eq!(slot(i).state(), ST_EMPTY);
        reclaim(i);
    }

    #[test]
    fn cancel_before_publish_parks_slot() {
        let _g = lock();
        let i = 7;
        arm(i, 8, 1.0, false);
        begin_stop(i); // cancel while LOADING
        assert!(!publish(i, sine_track(480, 1)));
        assert_eq!(slot(i).state(), ST_EMPTY);
        reclaim(i);
    }

    #[test]
    fn voices_mix_additively() {
        let _g = lock();
        set_master_gain(1.0);
        // Phase 1: one voice → baseline peak after an identical warmup.
        start(8, 11, sine_track(48000, 1), 1.0, false);
        let single = peak_after_warmup();
        drain(8, 4);
        // Phase 2: two voices started together (same positions, same envelope
        // schedule, same sine phase) → peak must double.
        start(8, 12, sine_track(48000, 1), 1.0, false);
        start(9, 13, sine_track(48000, 1), 1.0, false);
        let both = peak_after_warmup();
        assert!(
            (both - 2.0 * single).abs() < 0.02 * single + 1e-3,
            "additive mix: both {both} vs single {single}"
        );
        drain(9, 4);
        drain(8, 4);
    }

    #[test]
    fn local_render_advances_without_host_beacon() {
        let _g = lock();
        let i = 10;
        start(i, 21, sine_track(48000, 1), 1.0, false);
        let before = host_mix_frames();
        let mut buf = vec![0.0f32; 480];
        assert!(render_local_mono(&mut buf));
        assert_eq!(host_mix_frames(), before, "local path must not feed the beacon");
        assert!(rms(&buf[64..]) > 0.05, "local render silent");
        let pos = slot(i).position_frames();
        assert!((1..=480).contains(&pos), "pos {pos}");
        // Stop + park through the local path as well.
        begin_stop(i);
        for _ in 0..4 {
            let mut b = vec![0.0f32; 480];
            render_local_mono(&mut b);
        }
        assert_eq!(slot(i).state(), ST_EMPTY);
        reclaim(i);
    }

    #[test]
    fn vad_guard_ducks_pad_on_loud_input() {
        let _g = lock();
        set_master_gain(1.0);
        start(11, 31, sine_track(48000, 1), 1.0, false);
        set_vad_threshold(0.5);
        set_vad_guard(true);
        // Loud "mic" input (rms ~0.71 > 0.5): feed 40 frames so the attack
        // envelope (0.15/frame) converges to the 0.2 duck target.
        let mic = loud_input(480);
        for _ in 0..40 {
            let mut buf = mic.clone();
            process_slice(&mut buf, 1);
        }
        let mut buf = mic.clone();
        process_slice(&mut buf, 1);
        // Extract the pad contribution: output = mic + pad*duck. Re-run the
        // same input through an empty table for the pure mic baseline.
        let ducked_peak = residual_peak(&buf, &mic);
        begin_stop(11);
        for _ in 0..6 {
            let mut b = vec![0.0f32; 480];
            process_slice(&mut b, 1);
        }
        reclaim(11);
        // Baseline: pad alone at full gain.
        start(11, 32, sine_track(48000, 1), 1.0, false);
        set_vad_guard(false); // unity duck
        for _ in 0..3 {
            let mut b = vec![0.0f32; 480];
            process_slice(&mut b, 1);
        }
        let mut full = vec![0.0f32; 480];
        process_slice(&mut full, 1);
        let full_peak = peak(&full);
        drain(11, 4);
        set_vad_guard(false);
        set_vad_threshold(0.06);
        let ratio = ducked_peak / full_peak.max(1e-6);
        assert!(
            (ratio - 0.2).abs() < 0.08,
            "duck ratio {ratio} (expected ~0.2)"
        );
    }

    #[test]
    fn vad_guard_ignores_quiet_input() {
        let _g = lock();
        set_master_gain(1.0);
        start(12, 33, sine_track(48000, 1), 1.0, false);
        set_vad_threshold(0.5);
        set_vad_guard(true);
        // Silent mic: no duck — pad stays at full gain.
        for _ in 0..3 {
            let mut b = vec![0.0f32; 480];
            process_slice(&mut b, 1);
        }
        let mut out = vec![0.0f32; 480];
        process_slice(&mut out, 1);
        assert!(peak(&out) > 0.8, "quiet input must not duck: {}", peak(&out));
        drain(12, 4);
        set_vad_guard(false);
    }

    fn loud_input(frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (2.0 * std::f64::consts::PI * 123.0 * i as f64 / 48000.0).sin() as f32)
            .collect()
    }

    /// Peak of (mixed - original mic) = the pad contribution's peak.
    fn residual_peak(mixed: &[f32], mic: &[f32]) -> f32 {
        mixed
            .iter()
            .zip(mic)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max)
    }

    #[test]
    fn reserve_returns_none_when_full() {
        let _g = lock();
        for i in 0..SLOT_COUNT {
            arm(i, 100 + i as u32, 1.0, false);
            assert!(publish(i, sine_track(4800, 1)));
        }
        assert!(reserve(999, 1.0, false).is_none());
        for i in 0..SLOT_COUNT {
            drain(i, 4);
        }
        assert!(reserve(999, 1.0, false).is_some());
        // Park the reserved slot again (reserve armed it as LOADING).
        let picked = (0..SLOT_COUNT)
            .find(|&i| slot(i).state() == ST_LOADING)
            .unwrap();
        fail(picked);
    }
}
