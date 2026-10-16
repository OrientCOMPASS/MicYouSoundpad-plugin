//! Decode worker: one dedicated thread that owns every blocking operation —
//! audio file I/O (WAV via `wav.rs`; MP3/FLAC/OGG/M4A via `symph.rs`),
//! decoding, resampling to 48 kHz, sound-directory rescans and the
//! decoded-track cache.
//!
//! Why a worker: the bus dispatcher holds the plugin-instance lock across
//! `handle_message`, and the host audio thread blocks on that same lock in
//! `process_all`. Doing file I/O or a multi-megabyte decode inside
//! `handle_message` would stall the audio thread (crackling) — so the bus
//! thread only enqueues [`Job`]s (non-blocking) and the watchdog observes
//! results through atomics, exactly like Focus-Capture's capture worker.
//!
//! The worker NEVER calls MicYou host APIs; results travel back through
//! [`JobResult`] atomics and the shared [`SoundRegistry`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::mixer::{self, Track, TARGET_RATE};
use crate::resample::SincResampler;
use crate::wav;

/// Decode job outcomes observed by the bus-thread watchdog.
pub const JOB_LOADING: u8 = 0;
pub const JOB_PUBLISHED: u8 = 1;
pub const JOB_FAILED: u8 = 2;

/// Shared outcome of one play job (bus thread polls it; worker writes it).
pub struct JobResult {
    pub phase: AtomicU8,
    pub error: Mutex<String>,
}

impl JobResult {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            phase: AtomicU8::new(JOB_LOADING),
            error: Mutex::new(String::new()),
        })
    }
    pub fn is_done(&self) -> bool {
        self.phase.load(Ordering::Acquire) != JOB_LOADING
    }
    #[allow(dead_code)] // 诊断辅助（测试使用）
    pub fn published(&self) -> bool {
        self.phase.load(Ordering::Acquire) == JOB_PUBLISHED
    }
    pub fn error_message(&self) -> String {
        self.error
            .lock()
            .map(|e| e.clone())
            .unwrap_or_else(|_| "unknown error".into())
    }
}

pub enum Job {
    /// Decode `path` and publish it into mixer slot `slot`; report via `res`.
    Play {
        slot: usize,
        path: PathBuf,
        res: Arc<JobResult>,
    },
    /// Re-walk the sounds directory (cheap; runs every ~2 s).
    Rescan,
    Shutdown,
}

/// One playable sound known to the pad.
#[derive(Clone, Debug, PartialEq)]
pub struct SoundEntry {
    /// Stable id: the file stem (e.g. `beep` for `sounds/beep.wav`).
    pub id: String,
    /// Display name (config label override, else the id).
    pub label: String,
    /// File name relative to the sounds directory (e.g. `beep.wav`).
    pub file: String,
    /// Absolute path (resolved by the worker; never leaves the sounds dir).
    pub path: PathBuf,
    pub duration_ms: u64,
}

/// The pad's sound list: worker-thread writer, bus-thread reader. Small
/// `Vec` behind a `Mutex` — held for microseconds on both sides. `version`
/// bumps on every content change so the watchdog can push list updates to
/// the phone without diffing on the bus thread.
#[derive(Default)]
pub struct SoundRegistry {
    inner: Mutex<Vec<SoundEntry>>,
    version: AtomicU64,
}

impl SoundRegistry {
    pub fn list(&self) -> Vec<SoundEntry> {
        self.inner
            .lock()
            .map(|v| v.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }
    pub fn find(&self, id: &str) -> Option<SoundEntry> {
        self.list().into_iter().find(|s| s.id == id)
    }
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }
    pub fn replace(&self, entries: Vec<SoundEntry>) {
        if let Ok(mut v) = self.inner.lock() {
            if *v != entries {
                *v = entries;
                self.version.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Decoded-track cache: decode-once per file, bounded total memory.
/// Key = path + mtime + size, so edited files invalidate naturally.
struct TrackCache {
    map: HashMap<String, Arc<Track>>,
    bytes: usize,
    cap: usize,
}

impl TrackCache {
    fn new(cap_mb: usize) -> Self {
        Self {
            map: HashMap::new(),
            bytes: 0,
            cap: cap_mb * 1024 * 1024,
        }
    }
    fn key(path: &Path) -> String {
        let (mut mtime, mut size) = (0u64, 0u64);
        if let Ok(m) = std::fs::metadata(path) {
            size = m.len();
            if let Ok(t) = m.modified() {
                mtime = t
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
            }
        }
        format!("{}|{mtime}|{size}", path.display())
    }
    fn get(&self, path: &Path) -> Option<Arc<Track>> {
        self.map.get(&Self::key(path)).cloned()
    }
    fn put(&mut self, path: &Path, track: Arc<Track>) {
        let bytes = track.samples.len() * 4;
        // Simple FIFO-ish eviction until the new track fits.
        if bytes <= self.cap {
            while self.bytes + bytes > self.cap {
                let Some(victim) = self.map.keys().next().cloned() else {
                    break;
                };
                if let Some(t) = self.map.remove(&victim) {
                    self.bytes -= t.samples.len() * 4;
                }
            }
            self.bytes += bytes;
            self.map.insert(Self::key(path), track);
        }
    }
}

/// Walk the sounds directory and probe every `.wav` (case-insensitive).
/// `labels` maps sound id → display label (from plugin config).
pub fn scan_sounds(dir: &Path, labels: &HashMap<String, String>) -> Vec<SoundEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if !crate::symph::is_supported(&path) {
            continue;
        }
        let Some(stem) = path.file_stem() else { continue };
        let id = stem.to_string_lossy().to_string();
        if id.is_empty() || id.len() > 120 {
            continue;
        }
        // Header/metadata probe only (no audio data read). Undecodable files
        // are listed with duration 0 so the phone can surface the error on
        // play.
        let duration_ms = probe_any(&path).unwrap_or(0);
        let file = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        let label = labels
            .get(&id)
            .cloned()
            .or_else(|| labels.get(&file).cloned())
            .unwrap_or_else(|| id.clone());
        out.push(SoundEntry {
            id,
            label,
            file,
            path,
            duration_ms,
        });
    }
    // Stable, human-friendly order; on id collisions (beep.wav + beep.mp3)
    // the alphabetically-first file wins deterministically.
    out.sort_by(|a, b| {
        a.id
            .to_lowercase()
            .cmp(&b.id.to_lowercase())
            .then_with(|| a.file.to_lowercase().cmp(&b.file.to_lowercase()))
    });
    out.dedup_by(|a, b| a.id == b.id);
    out
}

/// Duration probe dispatched by extension (WAV header vs container metadata).
fn probe_any(path: &Path) -> Result<u64, String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext == "wav" {
        wav::probe_duration_ms(path)
    } else {
        crate::symph::probe_duration_ms(path)
    }
}

/// Read + decode any supported file to `(rate, channels, interleaved f32)`.
/// WAV goes through the built-in decoder; everything else through symphonia;
/// extensionless files are sniffed (WAV first, then format probing).
fn read_raw(path: &Path) -> Result<(u32, usize, Vec<f32>), String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext == "wav" {
        let d = wav::decode(path)?;
        return Ok((d.sample_rate, d.channels, d.samples));
    }
    if ext.is_empty() {
        if let Ok(d) = wav::decode(path) {
            return Ok((d.sample_rate, d.channels, d.samples));
        }
    }
    let r = crate::symph::decode(path)?;
    Ok((r.sample_rate, r.channels, r.samples))
}

/// Decode + resample + downmix-to-≤2ch an audio file into a mixer [`Track`].
fn load_track(path: &Path, cache: &mut TrackCache) -> Result<Arc<Track>, String> {
    if let Some(t) = cache.get(path) {
        return Ok(t);
    }
    let (src_rate, src_channels, samples) = read_raw(path)?;
    if samples.is_empty() || src_channels == 0 || src_rate == 0 {
        return Err("file contains no audio data".into());
    }
    let channels = src_channels.clamp(1, 2);
    // Downmix to the stored channel count first (average / front-pair), then
    // resample each plane with the shared sinc converter.
    let src_frames = samples.len() / src_channels;
    let mono_or_stereo: Vec<Vec<f32>> = {
        let mut planes = vec![Vec::with_capacity(src_frames); channels];
        if src_channels == channels {
            for frame in samples.chunks_exact(channels) {
                for (c, &v) in frame.iter().enumerate() {
                    planes[c].push(v);
                }
            }
        } else if channels == 2 {
            // N > 2 channels: keep the front pair.
            for frame in samples.chunks_exact(src_channels) {
                let l = frame[0];
                let r = frame.get(1).copied().unwrap_or(l);
                planes[0].push(l);
                planes[1].push(r);
            }
        } else {
            for frame in samples.chunks_exact(src_channels) {
                let sum: f32 = frame.iter().sum();
                planes[0].push(sum / src_channels as f32);
            }
        }
        planes
    };
    let resampled: Vec<Vec<f32>> = mono_or_stereo
        .into_iter()
        .map(|plane| {
            if src_rate == TARGET_RATE {
                plane
            } else {
                let mut r = SincResampler::new(src_rate, TARGET_RATE);
                let mut out =
                    Vec::with_capacity(plane.len() * TARGET_RATE as usize / src_rate.max(1) as usize + 64);
                // Feed in chunks to keep the resampler buffer small; it
                // preserves continuity across boundaries (see its tests).
                // NOTE: process() clears its output vector each call.
                let mut tmp = Vec::new();
                for chunk in plane.chunks(8192) {
                    r.process(chunk, &mut tmp);
                    out.extend_from_slice(&tmp);
                }
                out
            }
        })
        .collect();
    let frames = resampled.iter().map(|p| p.len()).min().unwrap_or(0);
    if frames == 0 {
        return Err("resampling produced no frames".into());
    }
    let mut interleaved = Vec::with_capacity(frames * channels);
    for fi in 0..frames {
        for plane in &resampled {
            interleaved.push(plane[fi]);
        }
    }
    let track = Arc::new(Track {
        samples: interleaved.into_boxed_slice(),
        channels,
        frames,
    });
    cache.put(path, track.clone());
    Ok(track)
}

/// Worker loop: jobs in, atomics out. Spawned once per `init`, joined in
/// `deinit` (the library is about to be unmapped — a thread still executing
/// our code would crash the host).
fn worker_loop(
    rx: Receiver<Job>,
    registry: Arc<SoundRegistry>,
    sounds_dir: PathBuf,
    labels: Arc<Mutex<HashMap<String, String>>>,
    rescan_flag: Arc<AtomicBool>,
    cache_cap_mb: usize,
    stop: Arc<AtomicBool>,
) {
    let mut cache = TrackCache::new(cache_cap_mb);
    let mut last_rescan = std::time::Instant::now()
        .checked_sub(Duration::from_secs(3600))
        .unwrap_or_else(std::time::Instant::now);
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let job = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(j) => j,
            Err(RecvTimeoutError::Timeout) => {
                // Idle housekeeping: honor a rescan request at most every 2 s
                // (check the cooldown BEFORE consuming the flag, so an early
                // request is honored on a later pass instead of being lost).
                if last_rescan.elapsed() >= Duration::from_secs(2)
                    && rescan_flag.swap(false, Ordering::AcqRel)
                {
                    last_rescan = std::time::Instant::now();
                    let labels = labels
                        .lock()
                        .map(|m| m.clone())
                        .unwrap_or_else(|p| p.into_inner().clone());
                    let fresh = scan_sounds(&sounds_dir, &labels);
                    registry.replace(fresh);
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match job {
            Job::Shutdown => break,
            Job::Rescan => {
                let labels = labels
                    .lock()
                    .map(|m| m.clone())
                    .unwrap_or_else(|p| p.into_inner().clone());
                let fresh = scan_sounds(&sounds_dir, &labels);
                registry.replace(fresh);
                last_rescan = std::time::Instant::now();
            }
            Job::Play { slot, path, res } => {
                crate::sptrace!("worker: decode {}", path.display());
                match load_track(&path, &mut cache) {
                    Ok(track) => {
                        if mixer::publish(slot, track) {
                            res.phase.store(JOB_PUBLISHED, Ordering::Release);
                        } else {
                            // Cancelled while decoding: slot parked by publish().
                            if let Ok(mut e) = res.error.lock() {
                                *e = "cancelled".into();
                            }
                            res.phase.store(JOB_FAILED, Ordering::Release);
                        }
                    }
                    Err(err) => {
                        crate::sptrace!("worker: decode failed: {err}");
                        mixer::fail(slot);
                        if let Ok(mut e) = res.error.lock() {
                            *e = err;
                        }
                        res.phase.store(JOB_FAILED, Ordering::Release);
                    }
                }
            }
        }
    }
    crate::sptrace!("worker: loop exited");
}

/// Handle to the running decode worker (created in `init`).
pub struct DecodeWorker {
    tx: Sender<Job>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    pub registry: Arc<SoundRegistry>,
    pub rescan_flag: Arc<AtomicBool>,
    pub labels: Arc<Mutex<HashMap<String, String>>>,
}

impl DecodeWorker {
    pub fn spawn(sounds_dir: PathBuf, cache_cap_mb: usize) -> Self {
        let (tx, rx) = channel::<Job>();
        let registry = Arc::new(SoundRegistry::default());
        let labels: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let rescan_flag = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let registry = registry.clone();
            let labels = labels.clone();
            let rescan_flag = rescan_flag.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("soundpad-decode".into())
                .spawn(move || {
                    worker_loop(rx, registry, sounds_dir, labels, rescan_flag, cache_cap_mb, stop)
                })
                .ok()
        };
        Self {
            tx,
            stop,
            handle,
            registry,
            rescan_flag,
            labels,
        }
    }

    /// Non-blocking job submission (bus thread). False when the worker died.
    pub fn submit(&self, job: Job) -> bool {
        self.tx.send(job).is_ok()
    }

    pub fn set_labels(&self, labels: HashMap<String, String>) {
        if let Ok(mut m) = self.labels.lock() {
            *m = labels;
        }
    }

    /// Ask for a directory rescan on the worker's next idle pass.
    pub fn request_rescan(&self) {
        self.rescan_flag.store(true, Ordering::Release);
    }

    /// Stop and join the worker (deinit). Bounded wait; a wedged worker is
    /// reported but never blocks the host forever.
    pub fn shutdown(mut self, budget: Duration) -> bool {
        self.stop.store(true, Ordering::Release);
        let _ = self.tx.send(Job::Shutdown);
        drop(self.tx);
        let Some(h) = self.handle.take() else {
            return true;
        };
        let deadline = std::time::Instant::now() + budget;
        while !h.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !h.is_finished() {
            return false;
        }
        h.join().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::{encode_wav_pcm16, generate_demo_sounds};
    use std::collections::HashMap;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sp-decode-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_wav(dir: &Path, name: &str, seconds: f64, rate: u32) -> PathBuf {
        let n = (seconds * rate as f64) as usize;
        let samples: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / rate as f64).sin() as f32)
            .collect();
        let path = dir.join(name);
        std::fs::write(&path, encode_wav_pcm16(&samples, rate)).unwrap();
        path
    }

    #[test]
    fn scan_lists_only_wav_with_durations() {
        let dir = tmp_dir("scan");
        write_wav(&dir, "beep.wav", 0.5, 44100);
        write_wav(&dir, "LOUD.WAV", 0.25, 48000);
        std::fs::write(dir.join("notes.txt"), b"ignore me").unwrap();
        let mut labels = HashMap::new();
        labels.insert("beep".to_string(), "Beep Label".to_string());
        let list = scan_sounds(&dir, &labels);
        assert_eq!(list.len(), 2);
        let beep = list.iter().find(|s| s.id == "beep").unwrap();
        assert_eq!(beep.label, "Beep Label");
        assert!((beep.duration_ms as i64 - 500).abs() <= 2);
        let loud = list.iter().find(|s| s.id == "LOUD").unwrap();
        assert_eq!(loud.label, "LOUD", "falls back to id without override");
        assert!((loud.duration_ms as i64 - 250).abs() <= 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_track_resamples_441_to_480() {
        let dir = tmp_dir("resample");
        let path = write_wav(&dir, "s.wav", 1.0, 44100);
        let mut cache = TrackCache::new(64);
        let t = load_track(&path, &mut cache).unwrap();
        // 1 s @ 48 kHz (± sinc edge tolerance).
        assert!((t.frames as i64 - 48000).abs() <= 32, "{}", t.frames);
        assert_eq!(t.channels, 1);
        assert!((t.duration_ms() as i64 - 1000).abs() <= 2);
        // Cached: second load returns the same Arc (no re-decode).
        let t2 = load_track(&path, &mut cache).unwrap();
        assert!(Arc::ptr_eq(&t, &t2));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_track_keeps_stereo() {
        let dir = tmp_dir("chans");
        let n = 4800usize; // 0.1 s @ 48 kHz
        let data_len = n * 4;
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&2u16.to_le_bytes()); // stereo
        w.extend_from_slice(&48000u32.to_le_bytes());
        w.extend_from_slice(&(48000u32 * 4).to_le_bytes());
        w.extend_from_slice(&4u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data_len as u32).to_le_bytes());
        for i in 0..n {
            let l = ((i % 100) as f32 / 100.0 * 32767.0) as i16; // ramp L
            let r = (-((i % 100) as f32) / 100.0 * 32767.0) as i16; // ramp R
            w.extend_from_slice(&l.to_le_bytes());
            w.extend_from_slice(&r.to_le_bytes());
        }
        let stereo = dir.join("stereo.wav");
        std::fs::write(&stereo, &w).unwrap();
        let mut cache = TrackCache::new(64);
        let t = load_track(&stereo, &mut cache).unwrap();
        assert_eq!(t.channels, 2);
        assert_eq!(t.frames, n);
        // Passthrough rate: samples land verbatim (f32 conversion only).
        assert!(t.samples[0].abs() < 1e-3, "L0 ~ 0");
        assert!(t.samples[1].abs() < 1e-3, "R0 ~ 0");
        assert!(t.samples[2 * 50] > 0.4 && t.samples[2 * 50] < 0.6, "L50 ~ 0.5");
        assert!(t.samples[2 * 50 + 1] < -0.4, "R50 ~ -0.5");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
    }

    const FIXTURES: [&str; 5] = [
        "sample_mp3.mp3",
        "sample_flac.flac",
        "sample_ogg.ogg",
        "sample_m4a.m4a",
        "sample_stereo.mp3",
    ];

    #[test]
    fn fixtures_decode_all_formats() {
        let dir = fixtures_dir();
        let mut cache = TrackCache::new(64);
        for name in FIXTURES {
            let t = load_track(&dir.join(name), &mut cache)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            // 0.5 s @ 48 kHz = 24000 frames; allow ±10% for codec
            // padding/priming and encoder delay.
            assert!(
                (t.frames as i64 - 24000).abs() <= 2400,
                "{name}: {} frames",
                t.frames
            );
            let want_ch = if name.contains("stereo") { 2 } else { 1 };
            assert_eq!(t.channels, want_ch, "{name}: channels");
            let rms = (t.samples.iter().map(|x| x * x).sum::<f32>()
                / t.samples.len().max(1) as f32)
                .sqrt();
            assert!(rms > 0.05, "{name} decoded too quiet: {rms}");
        }
    }

    #[test]
    fn fixtures_probe_durations() {
        let dir = fixtures_dir();
        for name in FIXTURES {
            let ms = probe_any(&dir.join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!((ms as i64 - 500).abs() <= 150, "{name}: {ms} ms");
        }
    }

    #[test]
    fn scan_lists_supported_formats_only() {
        let dir = fixtures_dir();
        let list = scan_sounds(&dir, &HashMap::new());
        assert_eq!(list.len(), FIXTURES.len());
        for e in &list {
            assert!(e.duration_ms > 300, "{}: {} ms", e.id, e.duration_ms);
        }
    }

    #[test]
    fn worker_publishes_decoded_play_job() {
        // The play job publishes into the global mixer slot table.
        let _g = crate::mixer::tests::lock();
        let dir = tmp_dir("worker");
        generate_demo_sounds(&dir).unwrap();
        let worker = DecodeWorker::spawn(dir.clone(), 64);
        let list = {
            worker.submit(Job::Rescan);
            // Wait for the registry to fill (worker is idle-polling at 200 ms).
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let l = worker.registry.list();
                if l.len() == 3 || std::time::Instant::now() > deadline {
                    break l;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        assert_eq!(list.len(), 3, "demo sounds must be listed");
        let beep = list.iter().find(|s| s.id == "beep").unwrap().clone();

        let res = JobResult::new();
        let slot = 15; // tests run under the shared mixer lock
        crate::mixer::tests::arm(slot, 900, 1.0, false);
        worker.submit(Job::Play {
            slot,
            path: beep.path.clone(),
            res: res.clone(),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !res.is_done() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(res.published(), "play job failed: {}", res.error_message());
        assert_eq!(mixer::slot(slot).state(), mixer::ST_ACTIVE);
        assert!(mixer::slot(slot).length_frames() > 44100 / 4);
        // Clean up: unit tests have no audio thread to run the fade, so
        // force-park the slot (test-only helper) and reclaim its track.
        mixer::begin_stop(slot);
        mixer::force_park(slot);
        assert!(worker.shutdown(Duration::from_secs(3)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn worker_reports_bad_file() {
        let _g = crate::mixer::tests::lock();
        let dir = tmp_dir("badfile");
        let bad = dir.join("bad.wav");
        std::fs::write(&bad, b"definitely not a riff file").unwrap();
        let worker = DecodeWorker::spawn(dir.clone(), 64);
        let res = JobResult::new();
        let slot = 14;
        crate::mixer::tests::arm(slot, 901, 1.0, false);
        worker.submit(Job::Play {
            slot,
            path: bad,
            res: res.clone(),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !res.is_done() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(res.is_done());
        assert!(!res.published());
        assert!(!res.error_message().is_empty());
        assert_eq!(mixer::slot(slot).state(), mixer::ST_EMPTY);
        assert!(worker.shutdown(Duration::from_secs(3)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
