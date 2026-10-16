//! Soundpad — MicYou native DSP plugin (`opss.soundpad`).
//!
//! Phone-controlled soundpad: the MicYou Android app discovers this plugin
//! over the cross-device plugin bus and sends play / stop / seek / volume /
//! loop commands; decoded sounds are mixed into the microphone stream by the
//! real-time [`mixer`] inside `micyou_plugin_process`, so call peers (and the
//! local monitor) hear them exactly like mic audio.
//!
//! Thread discipline (mirrors Focus-Capture; see docs/TECHNICAL.md):
//!
//! | Thread | Touches | Host APIs |
//! | --- | --- | --- |
//! | host bus thread (init / deinit / handle_message / handle_event / interval tick) | session registry, [`abi`], mixer slot control planes | ✅ only here |
//! | decode worker ([`decode`]) | file I/O, decode, resample, track cache, `publish`/`fail` | ❌ never |
//! | host real-time audio thread (process) | mixer slot audio planes only | ❌ never, zero allocation |
//!
//! The bus dispatcher holds the plugin-instance lock across `handle_message`
//! while the audio thread blocks on that same lock in `process_all` — so
//! every message handler here is allocation-light and non-blocking (JSON in,
//! atomics + mpsc out, microseconds).

#![allow(non_camel_case_types)]

use std::ffi::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

mod abi;
mod decode;
mod mixer;
mod monitor;
mod protocol;
mod ring;
mod symph;
#[macro_use]
mod trace;
mod resample;
mod wav;

use abi::mpl_host_api_t;
use decode::{DecodeWorker, Job, JobResult, SoundEntry, JOB_PUBLISHED};
use mixer::TARGET_RATE;
use protocol::Cmd;

const PLUGIN_ID: &[u8] = b"opss.soundpad\0";
const PLUGIN_ID_STR: &str = "opss.soundpad";
const PLUGIN_VERSION: &[u8] = concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes();
const PLUGIN_VERSION_STR: &str = env!("CARGO_PKG_VERSION");

/// Watchdog cadence: session reconciliation + rescan pacing + state pushes.
const WATCHDOG_MS: u64 = 250;
const WATCHDOG_PAYLOAD: &str = "spwd";
/// Decode jobs stuck in LOADING longer than this are abandoned.
const DECODE_TIMEOUT: Duration = Duration::from_secs(20);
/// Join budget for the decode worker in deinit (the library is about to be
/// unmapped — a thread still executing our code would crash the host).
const WORKER_SHUTDOWN_BUDGET: Duration = Duration::from_secs(6);
/// Decoded-track cache ceiling (MiB).
const CACHE_CAP_MB: usize = 192;

// ─────────────────────────── session registry ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Loading,
    Playing,
    Stopping,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Loading => "loading",
            Phase::Playing => "playing",
            Phase::Stopping => "stopping",
        }
    }
}

struct SessionRecord {
    sid: u64,
    slot: usize,
    sound_id: String,
    label: String,
    duration_ms: u64,
    volume: f64,
    looping: bool,
    phase: Phase,
    res: Arc<JobResult>,
    started_at: Instant,
    /// Set once the phone was told about the playing state (dedupe pushes).
    reported_playing: bool,
}

struct State {
    sessions: Vec<SessionRecord>,
    next_sid: u64,
    /// Plugin id of the phone peer that last sent a command (reply target).
    peer: Option<String>,
    interval_id: u64,
    ticks: u64,
    worker: Option<DecodeWorker>,
    sounds_dir: PathBuf,
    /// Raw `soundsDir` config value ("" = built-in <plugin dir>/sounds).
    sounds_dir_cfg: String,
    last_error: String,
    master_volume: f64,
    local_output: bool,
    local_volume: f64,
    /// Last local-output device error (surfaced in sp_state for the panel).
    local_error: String,
    /// Per-sound volume memory (sound id → 0..2), persisted in the
    /// `soundVolumes` config key. A play without an explicit volume uses it.
    volumes: std::collections::HashMap<String, f64>,
    /// VAD anti-collision guard (duck pad to 20% while speech is detected).
    vad_guard: bool,
    vad_threshold: f64,
    /// Last `sp_state` JSON written to config (dedupe: write only on change).
    last_panel_json: String,
    /// Registry version already pushed to the phone (list-change detection).
    pushed_registry_version: u64,
}

impl State {
    fn new() -> Self {
        Self {
            sessions: Vec::new(),
            next_sid: 1,
            peer: None,
            interval_id: 0,
            ticks: 0,
            worker: None,
            sounds_dir: PathBuf::new(),
            sounds_dir_cfg: String::new(),
            last_error: String::new(),
            master_volume: 1.0,
            local_output: true,
            local_volume: 1.0,
            local_error: String::new(),
            volumes: std::collections::HashMap::new(),
            vad_guard: false,
            vad_threshold: 0.06,
            last_panel_json: String::new(),
            pushed_registry_version: 0,
        }
    }
    fn find(&self, sid: u64) -> Option<usize> {
        self.sessions.iter().position(|r| r.sid == sid)
    }
}

/// Host ear-return (监听) state; v2 hosts report it via `get_monitoring` +
/// `monitoring_changed` events. While ON, the host already routes the chain
/// (pad mix included) to the user's headphones — our own local output would
/// double it, so it is auto-suppressed (LOCAL_WANTED gate).
static HOST_MONITORING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

static STATE: std::sync::LazyLock<Mutex<State>> =
    std::sync::LazyLock::new(|| Mutex::new(State::new()));
static NEXT_REQ: AtomicU64 = AtomicU64::new(1);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|p| p.into_inner())
}

/// Catch every panic at the FFI boundary — unwinding across `extern "C"` is
/// UB and would take the whole host process down.
fn guard<F: FnOnce() -> abi::mpl_result_t>(f: F) -> abi::mpl_result_t {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(abi::mpl_result_t::MPL_ERR_RUNTIME)
}

// ────────────────────────── required entry points ─────────────────────────

#[no_mangle]
pub extern "C" fn micyou_plugin_info() -> *const abi::mpl_plugin_info_t {
    static INFO: abi::mpl_plugin_info_t = abi::mpl_plugin_info_t {
        abi_version: abi::MPL_ABI_VERSION,
        api_version: abi::MPL_API_VERSION,
        id: PLUGIN_ID.as_ptr() as *const c_char,
        version: PLUGIN_VERSION.as_ptr() as *const c_char,
    };
    &INFO
}

/// # Safety
/// `host` must point to a valid `mpl_host_api_t` for the duration of the
/// call; we copy it by value and never retain the pointer.
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_init(host: *const mpl_host_api_t) -> abi::mpl_result_t {
    guard(|| {
        sptrace!("init enter (SP_TRACE={})", trace::enabled());
        if host.is_null() {
            return abi::mpl_result_t::MPL_ERR_INVALID_ARG;
        }
        abi::store_host(unsafe { *host });

        // Fresh mixer timeline. Safe without audio-thread synchronization:
        // init runs before the plugin is (re-)registered in the DSP chain
        // (same invariant Focus-Capture relies on).
        unsafe { mixer::reset_all() };
        mixer::set_master_gain(1.0);

        let mut s = state();
        *s = State::new();

        // Sound library: `soundsDir` config override, else <plugin dir>/sounds.
        let dir = abi::plugin_dir();
        s.sounds_dir_cfg = config_string("soundsDir", "");
        let custom = !s.sounds_dir_cfg.trim().is_empty();
        if custom {
            s.sounds_dir = PathBuf::from(s.sounds_dir_cfg.trim());
        } else if dir.is_empty() {
            s.last_error = "plugin_dir unavailable — sound library disabled".into();
            abi::log_error(&s.last_error);
        } else {
            s.sounds_dir = PathBuf::from(&dir).join("sounds");
            // First-run UX: seed demo sounds into the BUILT-IN dir only
            // (never write into a user-chosen custom folder).
            ensure_demo_sounds(&s.sounds_dir);
        }

        // Host API v2? Enables monitoring-state observation (auto-suppress
        // local output while the host ear-return already plays the pad).
        let v2 = abi::detect_v2();
        let mon0 = abi::get_monitoring().unwrap_or(false);
        HOST_MONITORING.store(mon0, std::sync::atomic::Ordering::Relaxed);
        monitor::set_suppressed(mon0);
        sptrace!("init: host api v2={v2} monitoring={}", HOST_MONITORING.load(std::sync::atomic::Ordering::Relaxed));

        // Config: master volume, local headphone output, label overrides.
        s.master_volume = config_f64("masterVolume", 1.0).clamp(0.0, 2.0);
        mixer::set_master_gain(s.master_volume as f32);
        s.local_output = config_bool("localOutput", true);
        s.local_volume = config_f64("localVolume", 1.0).clamp(0.0, 2.0);
        monitor::set_local_gain(s.local_volume);
        s.vad_guard = config_bool("vadGuard", false);
        s.vad_threshold = config_f64("vadThreshold", 0.06).clamp(0.0, 1.0);
        mixer::set_vad_guard(s.vad_guard);
        mixer::set_vad_threshold(s.vad_threshold as f32);
        s.volumes = serde_json::from_str(&abi::get_config("soundVolumes"))
            .unwrap_or_else(|_| serde_json::from_str::<serde_json::Value>("{}")
                .ok()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default());
        let labels = label_map_from_config();

        // Decode worker + initial synchronous scan (init may do file I/O;
        // message handlers may not).
        let worker = DecodeWorker::spawn(s.sounds_dir.clone(), CACHE_CAP_MB);
        worker.set_labels(labels.clone());
        if !s.sounds_dir.as_os_str().is_empty() {
            let initial = decode::scan_sounds(&s.sounds_dir, &labels);
            worker.registry.replace(initial);
        }
        worker.submit(Job::Rescan(s.sounds_dir.clone()));
        if s.local_output {
            // Device open happens on the worker thread (never here, and
            // never in a message handler); failures land in local_error.
            worker.submit(Job::LocalSet(true));
        }
        s.worker = Some(worker);

        // Watchdog: reconciliation, rescan pacing, state pushes.
        s.interval_id = abi::set_interval(WATCHDOG_MS, WATCHDOG_PAYLOAD);
        if s.interval_id == 0 {
            abi::log_warn(
                "host interval unavailable — playback still works, but phone state pushes are delayed until the next inbound message",
            );
        }

        abi::set_panel_icon("console", "🎵");
        let n = s
            .worker
            .as_ref()
            .map(|w| w.registry.list().len())
            .unwrap_or(0);
        write_panel_state(&mut s);
        abi::log_info(&format!(
            "Soundpad v{PLUGIN_VERSION_STR} ready — {n} sound(s) in {}",
            s.sounds_dir.display()
        ));
        sptrace!("init leave OK ({n} sounds)");
        abi::mpl_result_t::MPL_OK
    })
}

#[no_mangle]
pub extern "C" fn micyou_plugin_deinit() {
    let _ = guard(|| {
        sptrace!("deinit enter");
        let mut s = state();
        // Stop every voice, then shut the worker down (bounded join — the
        // library is about to be unmapped). The worker's exit drops the
        // local-output stream (closing the device, disarming the tap).
        monitor::set_wanted(false);
        for r in s.sessions.iter() {
            mixer::begin_stop(r.slot);
        }
        if let Some(w) = s.worker.take() {
            if !w.shutdown(WORKER_SHUTDOWN_BUDGET) {
                abi::log_error("decode worker did not exit within budget");
            }
        }
        if s.interval_id != 0 {
            abi::clear_interval(s.interval_id);
        }
        // No concurrent process() is possible once we are out of the DSP
        // chain, so force-reclaiming every slot's track is safe here.
        mixer::reclaim_all();
        HOST_MONITORING.store(false, std::sync::atomic::Ordering::Relaxed);
        monitor::set_suppressed(false);
        *s = State::new();
        drop(s);
        abi::clear_host();
        sptrace!("deinit leave");
        abi::mpl_result_t::MPL_OK
    });
}

// ────────────────────────── optional entry points ─────────────────────────

/// Real-time DSP: multi-voice additive mix of the active pad voices.
/// Allocation-free, lock-free, bounded (see `mixer::process_slice`).
///
/// # Safety
/// `data` spans `samples` writable interleaved f32s; `bypass` a u32.
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_process(
    data: *mut f32,
    samples: u32,
    channels: u32,
    _queued_ms: f64,
    bypass: *mut u32,
) -> abi::mpl_result_t {
    guard(|| {
        if data.is_null() || bypass.is_null() || channels == 0 || samples == 0 {
            if !bypass.is_null() {
                unsafe { *bypass = 1 };
            }
            return abi::mpl_result_t::MPL_ERR_INVALID_ARG;
        }
        let channels = channels as usize;
        let frames = samples as usize / channels;
        if frames == 0 {
            unsafe { *bypass = 1 };
            return abi::mpl_result_t::MPL_OK;
        }
        let slice = unsafe { std::slice::from_raw_parts_mut(data, frames * channels) };
        let mixed = mixer::process_slice(slice, channels);
        unsafe { *bypass = if mixed { 0 } else { 1 } };
        abi::mpl_result_t::MPL_OK
    })
}

/// Host events: announce ourselves when a device connects so the phone can
/// show its Soundpad button without waiting for its next discovery poll.
///
/// # Safety
/// Both arguments are NUL-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_handle_event(
    type_name: *const c_char,
    _json: *const c_char,
) -> abi::mpl_result_t {
    // (json parsed per-event below via json_arg)
    guard(|| {
        let t = unsafe { abi::cstr_or(type_name, "") };
        match t {
            "device_connected" => {
                let mut s = state();
                // A (re)connected device may be a fresh phone client: push an
                // unsolicited hello broadcast. Keep any known peer as the
                // directed target when we have one.
                push_hello(&s, 0);
                s.last_error.clear();
            }
            "device_disconnected" => {
                let mut s = state();
                s.peer = None;
                // 手机是音效板的主控端：连接断开即停止全部播放（10ms 淡出，
                // 槽位由 watchdog 回收）。面板本地播放无设备事件，不受影响。
                if !s.sessions.is_empty() {
                    let n = s.sessions.len();
                    stop_all(&mut s);
                    update_wanted(&s);
                    write_panel_state(&mut s);
                    abi::log_info(&format!(
                        "device disconnected — stopped {n} playing sound(s)"
                    ));
                }
            }
            "monitoring_changed" => {
                // Payload shapes: {"enabled":b} or the serde enum wrapper
                // {"MonitoringChanged":{"enabled":b}} — accept both.
                if let Some(json) = unsafe { json_arg(_json) } {
                    if let Some(on) = parse_enabled(&json) {
                        HOST_MONITORING.store(on, std::sync::atomic::Ordering::Relaxed);
                        // Instant path: the cpal callback checks this every
                        // ~10 ms and mutes while the host chain renders.
                        monitor::set_suppressed(on);
                        let mut s = state();
                        update_wanted(&s);
                        write_panel_state(&mut s);
                        sptrace!("monitoring_changed → {on}");
                    }
                }
            }
            _ => {}
        }
        abi::mpl_result_t::MPL_OK
    })
}

/// Bus messages: phone commands (topic `cmd`), panel actions (`ui:*`),
/// watchdog ticks (`interval:tick`) and config hot-reload (`config:changed`).
///
/// Runs on a host-dispatched thread while the instance lock is held (the
/// audio thread blocks on it in `process_all`) — everything here must stay
/// in the microsecond range: no file I/O, no decoding, no long loops.
///
/// # Safety
/// `topic` is NUL-terminated; `payload` spans `payload_len` bytes.
#[no_mangle]
pub unsafe extern "C" fn micyou_plugin_handle_message(
    source: *const c_char,
    topic: *const c_char,
    payload: *const u8,
    payload_len: u32,
) -> abi::mpl_result_t {
    guard(|| {
        let topic = unsafe { abi::cstr_or(topic, "") };
        let source = unsafe { abi::cstr_or(source, "") };
        let body = unsafe { payload_bytes(payload, payload_len) };
        sptrace!("handle_message topic={topic} source={source} len={}", body.len());

        if topic == protocol::TOPIC_CMD {
            let mut s = state();
            if !source.is_empty() && source != "host" && source != "ui" {
                s.peer = Some(source.to_string());
            }
            handle_cmd(&mut s, source, body);
            return abi::mpl_result_t::MPL_OK;
        }
        if topic == "interval:tick" {
            let payload_str = String::from_utf8_lossy(body);
            if let Some((id, pl)) = parse_tick(&payload_str) {
                let mut s = state();
                if s.interval_id != 0 && id == s.interval_id && pl == WATCHDOG_PAYLOAD {
                    watchdog(&mut s);
                }
            }
            return abi::mpl_result_t::MPL_OK;
        }
        if topic == "config:changed" {
            let mut s = state();
            apply_config(&mut s);
            return abi::mpl_result_t::MPL_OK;
        }
        if topic.starts_with("ui:") {
            let action = protocol::parse_ui_action(topic, body);
            let mut s = state();
            handle_ui(&mut s, action);
            return abi::mpl_result_t::MPL_OK;
        }
        abi::mpl_result_t::MPL_OK
    })
}

unsafe fn payload_bytes<'a>(payload: *const u8, len: u32) -> &'a [u8] {
    if payload.is_null() || len == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(payload, len as usize) }
}

/// `interval:tick` payload is `{"interval":<id>,"payload":"<tag>"}`.
fn parse_tick(payload_json: &str) -> Option<(u64, &str)> {
    if payload_json.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(payload_json).ok()?;
    Some((
        v.get("interval")?.as_u64()?,
        v.get("payload")?.as_str()?.to_owned().leak(),
    ))
}

// ─────────────────────────── command handling (bus thread) ────────────────

fn handle_cmd(s: &mut State, source: &str, body: &[u8]) {
    if body.is_empty() {
        return;
    }
    let cmd = match protocol::parse_cmd(body) {
        Ok(c) => c,
        Err(e) => {
            send_to_peer(s, &protocol::error_reply(0, &e));
            return;
        }
    };
    match cmd {
        Cmd::Hello { req } => push_hello(s, req),
        Cmd::List { req } => {
            // Trigger a rescan; the watchdog pushes the fresh list when the
            // registry version changes. Reply now with what we have.
            request_rescan(s);
            send_to_peer(s, &protocol::list_reply(req, sounds_json(s)));
        }
        Cmd::State { req } => {
            send_to_peer(s, &protocol::state_reply(req, sessions_json(s), s.vad_guard))
        }
        Cmd::Play { req, id, volume, r#loop } => {
            start_play(s, source, req, &id, volume, r#loop, false);
        }
        Cmd::Stop { req, session_id } => {
            match session_id {
                Some(sid) => {
                    stop_session(s, sid);
                }
                None => stop_all(s),
            }
            send_to_peer(s, &protocol::stop_reply(req));
            push_state(s, 0);
        }
        Cmd::Seek { req, session_id, position_ms } => {
            if let Some(i) = s.find(session_id) {
                let slot = s.sessions[i].slot;
                let frames = position_ms * TARGET_RATE as u64 / 1000;
                mixer::set_pos_frames(slot, frames);
                send_to_peer(s, &protocol::ok_reply(req, "seek"));
                push_state(s, 0);
            } else {
                send_to_peer(s, &protocol::error_reply(req, &format!("unknown session {session_id}")));
            }
        }
        Cmd::Volume { req, session_id, volume } => {
            if let Some(i) = s.find(session_id) {
                s.sessions[i].volume = volume;
                mixer::set_gain(s.sessions[i].slot, volume as f32);
                // 音量记忆：同一音效下次播放沿用（持久化到 soundVolumes）
                let sid = s.sessions[i].sound_id.clone();
                if s.volumes.get(&sid).copied().map_or(true, |old| (old - volume).abs() > 1e-6) {
                    s.volumes.insert(sid, volume);
                    persist_volumes(s);
                }
                send_to_peer(s, &protocol::ok_reply(req, "volume"));
                push_state(s, 0);
            } else {
                send_to_peer(s, &protocol::error_reply(req, &format!("unknown session {session_id}")));
            }
        }
        Cmd::Loop { req, session_id, r#loop } => {
            if let Some(i) = s.find(session_id) {
                s.sessions[i].looping = r#loop;
                mixer::set_looping(s.sessions[i].slot, r#loop);
                send_to_peer(s, &protocol::ok_reply(req, "loop"));
                push_state(s, 0);
            } else {
                send_to_peer(s, &protocol::error_reply(req, &format!("unknown session {session_id}")));
            }
        }
        Cmd::Vad { req, enable } => {
            s.vad_guard = enable;
            mixer::set_vad_guard(enable);
            abi::set_config("vadGuard", if enable { "true" } else { "false" });
            abi::log_info(&format!(
                "anti-collision guard {} (by {source})",
                if enable { "enabled" } else { "disabled" }
            ));
            send_to_peer(s, &protocol::ok_reply(req, "vad"));
            push_state(s, 0);
            write_panel_state(s);
        }
        Cmd::Unknown { req, op } => {
            send_to_peer(s, &protocol::error_reply(req, &format!("unknown op {op:?}")));
        }
    }
}

fn handle_ui(s: &mut State, action: protocol::UiAction) {
    use protocol::UiAction;
    match action {
        UiAction::Play { id } => {
            let req = NEXT_REQ.fetch_add(1, Ordering::Relaxed);
            start_play(s, "ui", req, &id, None, false, true);
        }
        UiAction::Stop { session_id } => match session_id {
            Some(sid) => {
                stop_session(s, sid);
                push_state(s, 0);
            }
            None => {
                stop_all(s);
                push_state(s, 0);
            }
        },
        UiAction::Rescan => request_rescan(s),
        UiAction::Apply => apply_config(s),
        UiAction::Ignored => {}
    }
}

/// Start one voice. `volume = None` → the per-sound remembered level (else
/// 1.0); an explicit volume also updates the memory. `from_ui` routes
/// failures to the log instead of the phone.
fn start_play(s: &mut State, source: &str, req: u64, id: &str, volume: Option<f64>, looping: bool, from_ui: bool) {
    let volume = match volume {
        Some(v) => {
            if s.volumes.get(id).copied().map_or(true, |old| (old - v).abs() > 1e-6) {
                s.volumes.insert(id.to_string(), v);
                persist_volumes(s);
            }
            v
        }
        None => s.volumes.get(id).copied().unwrap_or(1.0),
    };
    let Some(worker) = s.worker.as_ref() else {
        fail_play(s, req, source, "worker not running", from_ui);
        return;
    };
    let Some(entry) = worker.registry.find(id) else {
        fail_play(s, req, source, &format!("unknown sound {id:?}"), from_ui);
        return;
    };
    let sid = s.next_sid;
    let Some(slot) = mixer::reserve(sid as u32, volume as f32, looping) else {
        fail_play(s, req, source, "all voices busy (16 max)", from_ui);
        return;
    };
    s.next_sid = s.next_sid.wrapping_add(1);
    let res = JobResult::new();
    let submitted = worker.submit(Job::Play {
        slot,
        path: entry.path.clone(),
        res: res.clone(),
    });
    if !submitted {
        mixer::fail(slot);
        fail_play(s, req, source, "decode worker unavailable", from_ui);
        return;
    }
    let record = SessionRecord {
        sid,
        slot,
        sound_id: entry.id.clone(),
        label: entry.label.clone(),
        duration_ms: entry.duration_ms,
        volume,
        looping,
        phase: Phase::Loading,
        res,
        started_at: Instant::now(),
        reported_playing: false,
    };
    let session = session_to_json(&record, 0);
    s.sessions.push(record);
    sptrace!("play: sid={sid} id={id} slot={slot} vol={volume} loop={looping}");
    if !from_ui {
        send_to_peer(s, &protocol::play_reply(req, session));
    }
    write_panel_state(s);
}

fn fail_play(s: &mut State, req: u64, source: &str, msg: &str, from_ui: bool) {
    s.last_error = msg.to_string();
    if from_ui {
        abi::log_warn(&format!("play rejected: {msg}"));
    } else {
        send_to_peer(s, &protocol::error_reply(req, msg));
    }
    let _ = source;
    write_panel_state(s);
}

fn stop_session(s: &mut State, sid: u64) -> bool {
    let Some(i) = s.find(sid) else {
        return false;
    };
    let slot = s.sessions[i].slot;
    s.sessions[i].phase = Phase::Stopping;
    mixer::begin_stop(slot);
    sptrace!("stop: sid={sid} slot={slot}");
    true
}

fn stop_all(s: &mut State) {
    let ids: Vec<u64> = s.sessions.iter().map(|r| r.sid).collect();
    for sid in ids {
        stop_session(s, sid);
    }
}

fn request_rescan(s: &mut State) {
    // The flag path rescans the worker's last-known dir; a pending soundsDir
    // change travels as an explicit Rescan job from apply_config.
    if let Some(w) = s.worker.as_ref() {
        w.request_rescan();
    }
}

// ─────────────────────────── watchdog (bus thread) ────────────────────────

fn watchdog(s: &mut State) {
    s.ticks += 1;
    let mut changed = false;

    // 1) Reconcile session phases with job results / mixer slot states.
    let mut i = 0;
    while i < s.sessions.len() {
        let done;
        {
            let r = &s.sessions[i];
            match r.phase {
                Phase::Loading => {
                    if r.res.phase.load(Ordering::Acquire) == JOB_PUBLISHED {
                        done = Some((i, Phase::Playing, String::new()));
                    } else if r.res.is_done() {
                        let err = r.res.error_message();
                        done = Some((i, Phase::Stopping, err));
                    } else if r.started_at.elapsed() > DECODE_TIMEOUT {
                        mixer::fail(r.slot);
                        done = Some((i, Phase::Stopping, "decode timed out".into()));
                    } else {
                        done = None;
                    }
                }
                Phase::Playing => {
                    if mixer::slot(r.slot).state() == mixer::ST_EMPTY {
                        // Voice finished naturally (mixer parked the slot).
                        mixer::reclaim(r.slot);
                        done = Some((i, Phase::Stopping, String::new()));
                    } else if !r.reported_playing {
                        done = Some((i, Phase::Playing, String::new()));
                    } else {
                        done = None;
                    }
                }
                Phase::Stopping => {
                    if mixer::slot(r.slot).state() == mixer::ST_EMPTY {
                        mixer::reclaim(r.slot);
                        done = Some((i, Phase::Stopping, String::new()));
                    } else {
                        done = None;
                    }
                }
            }
        }
        match done {
            Some((idx, Phase::Playing, _)) => {
                s.sessions[idx].phase = Phase::Playing;
                s.sessions[idx].reported_playing = true;
                changed = true;
                sptrace!("watchdog: sid={} now playing", s.sessions[idx].sid);
            }
            Some((idx, _, err)) => {
                let rec = s.sessions.remove(idx);
                if rec.phase == Phase::Loading {
                    // Failed before playback: slot parked by the worker
                    // (fail/cancel path) — make sure it is reclaimable.
                    mixer::reclaim(rec.slot);
                } else if !err.is_empty() {
                    s.last_error = err.clone();
                }
                changed = true;
                sptrace!("watchdog: sid={} removed (err={err:?})", rec.sid);
                continue; // vec shifted; re-check index i
            }
            None => {}
        }
        i += 1;
    }

    // 2) Registry version change → push a fresh list to the phone.
    if let Some(w) = s.worker.as_ref() {
        let v = w.registry.version();
        if v != s.pushed_registry_version {
            s.pushed_registry_version = v;
            if s.peer.is_some() {
                let req = NEXT_REQ.fetch_add(1, Ordering::Relaxed);
                send_to_peer(s, &protocol::list_reply(req, sounds_json(s)));
            }
            changed = true;
        }
    }

    // 3) Local-output play/pause gate (worker applies it with a 3 s idle
    //    hysteresis). Auto-suppressed while the host ear-return is on.
    update_wanted(s);

    // 4) Periodic housekeeping (~every 2 s): cheap config re-read (settings
    //    edited outside the UI also land without re-enabling) + rescan pacing.
    if s.ticks % 8 == 0 {
        apply_config(s);
        request_rescan(s);
        let lerr = s
            .worker
            .as_ref()
            .and_then(|w| w.local_error.lock().ok())
            .map(|e| e.clone())
            .unwrap_or_default();
        if lerr != s.local_error {
            s.local_error = lerr;
            changed = true;
        }
    }

    // 5) Publish state on discrete changes (phone + panel).
    if changed {
        push_state(s, 0);
        write_panel_state(s);
    }
}

// ─────────────────────────── outbound helpers ─────────────────────────────

/// Safe read of the handle_event JSON argument.
unsafe fn json_arg(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let c = unsafe { std::ffi::CStr::from_ptr(p) };
    c.to_str().ok().map(|s| s.to_string())
}

fn parse_enabled(json: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if let Some(b) = v.get("enabled").and_then(|x| x.as_bool()) {
        return Some(b);
    }
    v.as_object()?
        .values()
        .find_map(|inner| inner.get("enabled").and_then(|x| x.as_bool()))
}

/// LOCAL_WANTED = user toggle ∧ live sessions. Host-monitor suppression is
/// applied inside the cpal callback (instant silence while the host chain
/// renders) — pausing the stream here instead would lag by the worker's
/// housekeeping cycle and freeze the offline preview.
fn update_wanted(s: &State) {
    monitor::set_wanted(s.local_output && !s.sessions.is_empty());
}

/// Persist the per-sound volume memory (discrete user actions only).
fn persist_volumes(s: &State) {
    if let Ok(json) = serde_json::to_string(&s.volumes) {
        abi::set_config("soundVolumes", &json);
    }
}

fn send_to_peer(s: &State, msg: &serde_json::Value) {
    let bytes = msg.to_string();
    let target = match &s.peer {
        Some(p) => format!("{{\"type\":\"remote\",\"pluginId\":{}}}", quote(p)),
        None => "{\"type\":\"broadcast\"}".to_string(),
    };
    if !abi::send_message(&target, bytes.as_bytes()) {
        sptrace!("send_to_peer failed (device offline?) target={target}");
    }
}

/// Minimal JSON string quoting (ids are validated to a safe charset, but be
/// robust regardless).
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn push_hello(s: &State, req: u64) {
    send_to_peer(
        s,
        &protocol::hello_reply(
            req,
            PLUGIN_ID_STR,
            PLUGIN_VERSION_STR,
            sounds_json(s),
            sessions_json(s),
            s.vad_guard,
        ),
    );
}

fn push_state(s: &State, req: u64) {
    send_to_peer(s, &protocol::state_reply(req, sessions_json(s), s.vad_guard));
}

fn sounds_json(s: &State) -> Vec<serde_json::Value> {
    s.worker
        .as_ref()
        .map(|w| {
            w.registry
                .list()
                .iter()
                .map(|e: &SoundEntry| protocol::sound_json(&e.id, &e.label, e.duration_ms, &e.dir))
                .collect()
        })
        .unwrap_or_default()
}

fn sessions_json(s: &State) -> Vec<serde_json::Value> {
    s.sessions.iter().map(|r| session_snapshot_json(r)).collect()
}

fn session_snapshot_json(r: &SessionRecord) -> serde_json::Value {
    let pos_ms = if r.phase == Phase::Loading {
        0
    } else {
        mixer::slot(r.slot).position_frames() * 1000 / TARGET_RATE as u64
    };
    let dur_frames = mixer::slot(r.slot).length_frames();
    let duration_ms = if dur_frames > 0 {
        dur_frames * 1000 / TARGET_RATE as u64
    } else {
        r.duration_ms
    };
    session_to_json_with(r, pos_ms, duration_ms)
}

fn session_to_json(r: &SessionRecord, pos_ms: u64) -> serde_json::Value {
    session_to_json_with(r, pos_ms, r.duration_ms)
}

fn session_to_json_with(r: &SessionRecord, pos_ms: u64, duration_ms: u64) -> serde_json::Value {
    protocol::session_json(
        r.sid,
        &r.sound_id,
        &r.label,
        r.phase.as_str(),
        pos_ms,
        duration_ms,
        r.volume,
        r.looping,
    )
}

/// Persist the machine-readable status snapshot the desktop panel polls
/// (`get_config` → `sp_state`). One `set_config` call per state CHANGE (not
/// per tick) keeps host-side file I/O low; the string dedupe below skips
/// no-op writes entirely.
fn write_panel_state(s: &mut State) {
    let sounds: Vec<serde_json::Value> = s
        .worker
        .as_ref()
        .map(|w| {
            w.registry
                .list()
                .iter()
                .map(|e| protocol::sound_json(&e.id, &e.label, e.duration_ms, &e.dir))
                .collect()
        })
        .unwrap_or_default();
    let st = serde_json::json!({
        "sessions": sessions_json(s),
        "sounds": sounds,
        "lastError": s.last_error,
        "masterVolume": s.master_volume,
        "soundsDir": s.sounds_dir.display().to_string(),
        "peer": s.peer.clone().unwrap_or_default(),
        "localOutput": s.local_output,
        "localVolume": s.local_volume,
        "localError": s.local_error,
        "monitorOn": HOST_MONITORING.load(std::sync::atomic::Ordering::Relaxed),
        "vadGuard": s.vad_guard,
        "vadThreshold": s.vad_threshold,
    });
    let text = st.to_string();
    if text != s.last_panel_json {
        s.last_panel_json = text.clone();
        abi::set_config("sp_state", &text);
    }
}

// ─────────────────────────── config helpers ───────────────────────────────

fn config_f64(key: &str, default: f64) -> f64 {
    let raw = abi::get_config(key);
    serde_json::from_str::<f64>(&raw).unwrap_or(default)
}

fn config_bool(key: &str, default: bool) -> bool {
    let raw = abi::get_config(key);
    serde_json::from_str::<bool>(&raw).unwrap_or(default)
}

fn config_string(key: &str, default: &str) -> String {
    let raw = abi::get_config(key);
    serde_json::from_str::<String>(&raw).unwrap_or_else(|_| default.to_string())
}

/// Label overrides from `config.sounds`: accepts `[{id,label}]` entries
/// (file paths are ignored — the directory scan owns the file list).
fn label_map_from_config() -> std::collections::HashMap<String, String> {
    let raw = abi::get_config("sounds");
    let mut map = std::collections::HashMap::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
        if let Some(arr) = v.as_array() {
            for item in arr {
                let id = item
                    .get("id")
                    .and_then(|x| x.as_str())
                    .or_else(|| item.get("file").and_then(|x| x.as_str()))
                    .map(|x| x.trim_end_matches(".wav").to_string());
                let label = item.get("label").and_then(|x| x.as_str());
                if let (Some(id), Some(label)) = (id, label) {
                    if !label.is_empty() {
                        map.insert(id, label.to_string());
                    }
                }
            }
        }
    }
    map
}

/// Re-read mutable config (master volume, local output, sounds dir, labels).
/// Called on `config:changed`, `ui:apply` and periodically from the watchdog.
/// All device/file work is handed to the decode worker — this stays fast.
fn apply_config(s: &mut State) {
    let mv = config_f64("masterVolume", 1.0).clamp(0.0, 2.0);
    if (mv - s.master_volume).abs() > 1e-6 {
        s.master_volume = mv;
        mixer::set_master_gain(mv as f32);
    }
    let lv = config_f64("localVolume", 1.0).clamp(0.0, 2.0);
    if (lv - s.local_volume).abs() > 1e-6 {
        s.local_volume = lv;
        monitor::set_local_gain(lv);
    }
    let mut peer_sync = false;
    let vg = config_bool("vadGuard", false);
    if vg != s.vad_guard {
        s.vad_guard = vg;
        mixer::set_vad_guard(vg);
        peer_sync = true; // 手机端开关随 state 推送保持同步
        abi::log_info(&format!("anti-collision guard {}", if vg { "enabled" } else { "disabled" }));
    }
    let vt = config_f64("vadThreshold", 0.06).clamp(0.0, 1.0);
    if (vt - s.vad_threshold).abs() > 1e-9 {
        s.vad_threshold = vt;
        mixer::set_vad_threshold(vt as f32);
    }
    let lo = config_bool("localOutput", true);
    if lo != s.local_output {
        s.local_output = lo;
        if let Some(w) = s.worker.as_ref() {
            w.submit(Job::LocalSet(lo));
        }
        abi::log_info(&format!(
            "local headphone output {}",
            if lo { "enabled" } else { "disabled" }
        ));
    }
    let dir_cfg = config_string("soundsDir", "");
    if dir_cfg.trim() != s.sounds_dir_cfg.trim() {
        s.sounds_dir_cfg = dir_cfg.clone();
        let custom = !dir_cfg.trim().is_empty();
        let new_dir = if custom {
            PathBuf::from(dir_cfg.trim())
        } else {
            let base = abi::plugin_dir();
            if base.is_empty() {
                s.sounds_dir.clone()
            } else {
                PathBuf::from(base).join("sounds")
            }
        };
        if new_dir != s.sounds_dir {
            s.sounds_dir = new_dir.clone();
            abi::log_info(&format!("sounds directory → {}", new_dir.display()));
            if let Some(w) = s.worker.as_ref() {
                w.submit(Job::Rescan(new_dir));
            }
        }
    }
    if let Some(w) = s.worker.as_ref() {
        w.set_labels(label_map_from_config());
        w.request_rescan();
    }
    if peer_sync {
        push_state(s, 0);
    }
    write_panel_state(s);
}

/// First-run UX: when the sounds directory does not exist or holds no WAVs,
/// drop in three short demo tones (same approach as the official example
/// plugin). Runs in `init` where file I/O is allowed.
fn ensure_demo_sounds(dir: &PathBuf) {
    let has_wav = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                e.path()
                    .extension()
                    .map(|x| x.to_string_lossy().eq_ignore_ascii_case("wav"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if has_wav {
        return;
    }
    match wav::generate_demo_sounds(dir) {
        Ok(names) => abi::log_info(&format!(
            "sounds/ was empty — generated demo sounds: {}",
            names.join(", ")
        )),
        Err(e) => abi::log_warn(&format!("demo sound generation failed: {e}")),
    }
}
