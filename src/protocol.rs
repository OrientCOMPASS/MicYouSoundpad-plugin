//! Phone ↔ desktop wire protocol for the soundpad.
//!
//! Transport: MicYou's cross-device `PluginMessage` (protobuf field 7 of the
//! TCP control `MessageWrapper`; see docs/plugins/api-reference.md §跨端通信).
//! The Android client sends `source = <phone plugin id>`, `target =
//! "opss.soundpad"`, `topic = "cmd"`; this plugin answers through the host's
//! `send_message` with target `{"type":"remote","pluginId":<peer>}` — the
//! host rewrites `topic` to the target plugin id on that path, so BOTH ends
//! route purely on `source` + the `op` field inside the JSON payload.
//!
//! Requests carry a client-chosen `req` number; replies echo it (`req: 0`
//! marks unsolicited pushes). All payloads are UTF-8 JSON.
//!
//! Phone → desktop (`op`):
//! * `hello`    — discovery handshake; also a keepalive while connected
//! * `list`     — request the sound list (triggers a directory rescan)
//! * `play`     — `{id, volume?, loop?}` start a voice
//! * `stop`     — `{sessionId?}` stop one voice (absent id = stop all)
//! * `seek`     — `{sessionId, positionMs}`
//! * `volume`   — `{sessionId, volume}` (0.0 – 2.0)
//! * `loop`     — `{sessionId, loop}`
//! * `vad`      — `{enable}` toggle the anti-collision guard (duck the pad
//!   to 20% while the mic carries speech; persisted as `vadGuard` config)
//! * `state`    — request the live session snapshot
//!
//! Desktop → phone (`op`):
//! * `hello`    — `{plugin, version, sounds[], sessions[]}` (reply or push on
//!   device connect; this is what makes the phone show the Soundpad button)
//! * `list`     — `{sounds[]}`
//! * `play`     — `{sessionId, session}` immediate ack (phase `loading`)
//! * `state`    — `{sessions[], vadGuard}` snapshot (reply or push on any
//!   change)
//! * `error`    — `{message}` with the echoed `req`
//!
//! `sounds[]` entry: `{id, label, durationMs}` ·
//! `sessions[]` entry: `{sessionId, id, label, phase, positionMs, durationMs,
//! volume, loop}` with phase ∈ `loading | playing | stopping`.

use serde_json::{json, Value};

/// Topic the phone uses for every request.
pub const TOPIC_CMD: &str = "cmd";

/// Parsed phone command.
#[derive(Debug, PartialEq)]
pub enum Cmd {
    Hello { req: u64 },
    List { req: u64 },
    State { req: u64 },
    Play { req: u64, id: String, volume: Option<f64>, r#loop: bool },
    Stop { req: u64, session_id: Option<u64> },
    Seek { req: u64, session_id: u64, position_ms: u64 },
    Volume { req: u64, session_id: u64, volume: f64 },
    Loop { req: u64, session_id: u64, r#loop: bool },
    /// Enable/disable the VAD anti-collision guard (ducking).
    Vad { req: u64, enable: bool },
    Unknown { req: u64, op: String },
}

pub fn parse_cmd(payload: &[u8]) -> Result<Cmd, String> {
    let v: Value = serde_json::from_slice(payload).map_err(|e| format!("invalid JSON: {e}"))?;
    let obj = v.as_object().ok_or("payload is not an object")?;
    let req = obj.get("req").and_then(Value::as_u64).unwrap_or(0);
    let op = obj
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match op.as_str() {
        "hello" => Ok(Cmd::Hello { req }),
        "list" => Ok(Cmd::List { req }),
        "state" => Ok(Cmd::State { req }),
        "play" => {
            let id = obj
                .get("id")
                .and_then(Value::as_str)
                .ok_or("play: missing id")?
                .to_string();
            if id.is_empty() || id.len() > 160 {
                return Err("play: bad id".into());
            }
            // volume 缺省 = 使用该音效的记忆音量（lib.rs 的 soundVolumes）
            let volume = obj.get("volume").and_then(Value::as_f64).map(|v| v.clamp(0.0, 2.0));
            let lo = obj.get("loop").and_then(Value::as_bool).unwrap_or(false);
            Ok(Cmd::Play { req, id, volume, r#loop: lo })
        }
        "stop" => {
            let sid = obj.get("sessionId").and_then(Value::as_u64);
            Ok(Cmd::Stop { req, session_id: sid })
        }
        "seek" => {
            let sid = obj
                .get("sessionId")
                .and_then(Value::as_u64)
                .ok_or("seek: missing sessionId")?;
            let pos = obj
                .get("positionMs")
                .and_then(Value::as_u64)
                .ok_or("seek: missing positionMs")?;
            Ok(Cmd::Seek { req, session_id: sid, position_ms: pos })
        }
        "volume" => {
            let sid = obj
                .get("sessionId")
                .and_then(Value::as_u64)
                .ok_or("volume: missing sessionId")?;
            let volume = obj
                .get("volume")
                .and_then(Value::as_f64)
                .ok_or("volume: missing volume")?
                .clamp(0.0, 2.0);
            Ok(Cmd::Volume { req, session_id: sid, volume })
        }
        "loop" => {
            let sid = obj
                .get("sessionId")
                .and_then(Value::as_u64)
                .ok_or("loop: missing sessionId")?;
            let lo = obj
                .get("loop")
                .and_then(Value::as_bool)
                .ok_or("loop: missing loop")?;
            Ok(Cmd::Loop { req, session_id: sid, r#loop: lo })
        }
        "vad" => {
            let enable = obj
                .get("enable")
                .and_then(Value::as_bool)
                .ok_or("vad: missing enable")?;
            Ok(Cmd::Vad { req, enable })
        }
        _ => Ok(Cmd::Unknown { req, op }),
    }
}

/// Panel action topics (host `trigger` bridge → `ui:<action>`).
#[derive(Debug, PartialEq)]
pub enum UiAction {
    Play { id: String },
    Stop { session_id: Option<u64> },
    Rescan,
    Apply,
    Ignored,
}

/// Panel triggers arrive as topic `ui:<action>` with payload either
/// `{"action":"play","payload":{…}}` (bridge wrapper) or the bare argument
/// object — accept both.
pub fn parse_ui_action(topic: &str, payload: &[u8]) -> UiAction {
    let Some(action) = topic.strip_prefix("ui:") else {
        return UiAction::Ignored;
    };
    let v: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
    let args: &Value = match v.get("payload") {
        Some(p) if p.is_object() => p,
        _ => &v,
    };
    match action {
        "play" => args
            .get("id")
            .and_then(Value::as_str)
            .map(|id| UiAction::Play { id: id.to_string() })
            .unwrap_or(UiAction::Ignored),
        "stop" => UiAction::Stop {
            session_id: args.get("sessionId").and_then(Value::as_u64),
        },
        "rescan" => UiAction::Rescan,
        "apply" => UiAction::Apply,
        _ => UiAction::Ignored,
    }
}

// ─────────────────────────── reply builders ───────────────────────────────

pub fn sound_json(id: &str, label: &str, duration_ms: u64, dir: &str) -> Value {
    json!({ "id": id, "label": label, "durationMs": duration_ms, "dir": dir })
}

pub fn session_json(
    sid: u64,
    id: &str,
    label: &str,
    phase: &str,
    position_ms: u64,
    duration_ms: u64,
    volume: f64,
    lo: bool,
) -> Value {
    json!({
        "sessionId": sid, "id": id, "label": label, "phase": phase,
        "positionMs": position_ms, "durationMs": duration_ms,
        "volume": volume, "loop": lo,
    })
}

pub fn hello_reply(
    req: u64,
    plugin_id: &str,
    version: &str,
    sounds: Vec<Value>,
    sessions: Vec<Value>,
    vad_guard: bool,
) -> Value {
    json!({
        "op": "hello", "req": req, "plugin": plugin_id, "version": version,
        "sounds": sounds, "sessions": sessions, "vadGuard": vad_guard,
    })
}

pub fn list_reply(req: u64, sounds: Vec<Value>) -> Value {
    json!({ "op": "list", "req": req, "sounds": sounds })
}

pub fn state_reply(req: u64, sessions: Vec<Value>, vad_guard: bool) -> Value {
    json!({ "op": "state", "req": req, "sessions": sessions, "vadGuard": vad_guard })
}

pub fn play_reply(req: u64, session: Value) -> Value {
    json!({ "op": "play", "req": req, "session": session })
}

pub fn stop_reply(req: u64) -> Value {
    json!({ "op": "stop", "req": req })
}

pub fn ok_reply(req: u64, op: &str) -> Value {
    json!({ "op": op, "req": req, "ok": true })
}

pub fn error_reply(req: u64, message: &str) -> Value {
    json!({ "op": "error", "req": req, "message": message })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(s: &str) -> Cmd {
        parse_cmd(s.as_bytes()).expect("valid command")
    }

    #[test]
    fn parses_every_op() {
        assert_eq!(cmd(r#"{"op":"hello","req":7}"#), Cmd::Hello { req: 7 });
        assert_eq!(cmd(r#"{"op":"list"}"#), Cmd::List { req: 0 });
        assert_eq!(cmd(r#"{"op":"state","req":1}"#), Cmd::State { req: 1 });
        assert_eq!(
            cmd(r#"{"op":"play","req":2,"id":"beep","volume":0.5,"loop":true}"#),
            Cmd::Play { req: 2, id: "beep".into(), volume: Some(0.5), r#loop: true }
        );
        // play defaults: no volume → None (caller resolves the remembered level)
        assert_eq!(
            cmd(r#"{"op":"play","id":"x"}"#),
            Cmd::Play { req: 0, id: "x".into(), volume: None, r#loop: false }
        );
        assert_eq!(
            cmd(r#"{"op":"stop","req":3,"sessionId":9}"#),
            Cmd::Stop { req: 3, session_id: Some(9) }
        );
        assert_eq!(cmd(r#"{"op":"stop"}"#), Cmd::Stop { req: 0, session_id: None });
        assert_eq!(
            cmd(r#"{"op":"seek","req":4,"sessionId":9,"positionMs":1200}"#),
            Cmd::Seek { req: 4, session_id: 9, position_ms: 1200 }
        );
        assert_eq!(
            cmd(r#"{"op":"volume","req":5,"sessionId":9,"volume":0.25}"#),
            Cmd::Volume { req: 5, session_id: 9, volume: 0.25 }
        );
        assert_eq!(
            cmd(r#"{"op":"loop","req":6,"sessionId":9,"loop":false}"#),
            Cmd::Loop { req: 6, session_id: 9, r#loop: false }
        );
        assert_eq!(
            cmd(r#"{"op":"vad","req":9,"enable":true}"#),
            Cmd::Vad { req: 9, enable: true }
        );
        assert!(parse_cmd(br#"{"op":"vad","req":9}"#).is_err(), "vad needs enable");
    }

    #[test]
    fn volume_clamped() {
        assert_eq!(
            cmd(r#"{"op":"volume","sessionId":1,"volume":99}"#),
            Cmd::Volume { req: 0, session_id: 1, volume: 2.0 }
        );
        assert_eq!(
            cmd(r#"{"op":"play","id":"x","volume":-3}"#),
            Cmd::Play { req: 0, id: "x".into(), volume: Some(0.0), r#loop: false }
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_cmd(b"").is_err());
        assert!(parse_cmd(b"[1,2]").is_err());
        assert!(parse_cmd(br#"{"op":"play"}"#).is_err(), "play needs id");
        assert!(parse_cmd(br#"{"op":"seek","sessionId":1}"#).is_err());
        assert_eq!(
            cmd(r#"{"op":"explode","req":8}"#),
            Cmd::Unknown { req: 8, op: "explode".into() }
        );
    }

    #[test]
    fn parses_ui_actions_both_shapes() {
        assert_eq!(
            parse_ui_action("ui:play", br#"{"id":"beep"}"#),
            UiAction::Play { id: "beep".into() }
        );
        assert_eq!(
            parse_ui_action(
                "ui:play",
                br#"{"action":"play","payload":{"id":"ding"}}"#
            ),
            UiAction::Play { id: "ding".into() }
        );
        assert_eq!(
            parse_ui_action("ui:stop", br#"{"sessionId":3}"#),
            UiAction::Stop { session_id: Some(3) }
        );
        assert_eq!(
            parse_ui_action("ui:stop", br#"{"action":"stop"}"#),
            UiAction::Stop { session_id: None }
        );
        assert_eq!(parse_ui_action("ui:rescan", b"{}"), UiAction::Rescan);
        assert_eq!(parse_ui_action("ui:apply", b""), UiAction::Apply);
        assert_eq!(parse_ui_action("ui:bogus", b"{}"), UiAction::Ignored);
        assert_eq!(parse_ui_action("cmd", br#"{"id":"x"}"#), UiAction::Ignored);
    }

    #[test]
    fn builders_shape() {
        let h = hello_reply(
            1,
            "opss.soundpad",
            "1.0.0",
            vec![sound_json("memes/beep", "Beep", 500, "memes")],
            vec![],
            true,
        );
        assert_eq!(h["op"], "hello");
        assert_eq!(h["sounds"][0]["durationMs"], 500);
        assert_eq!(h["sounds"][0]["dir"], "memes");
        assert_eq!(h["vadGuard"], true);
        assert_eq!(state_reply(0, vec![], false)["vadGuard"], false);
        let s = session_json(2, "beep", "Beep", "playing", 120, 500, 0.8, true);
        assert_eq!(s["sessionId"], 2);
        assert_eq!(s["phase"], "playing");
        assert_eq!(error_reply(3, "nope")["message"], "nope");
    }
}
