//! Opt-in field diagnostics: `SP_TRACE=1` makes the plugin print a
//! timestamped, thread-tagged trace of every lifecycle step to **stderr**
//! (visible when running the CLI/TUI frontends, or in GUI console output),
//! plus a file sink under `%TEMP%` / `/tmp` for GUI sessions without a
//! console.
//!
//! Never call `sptrace!` from the real-time audio thread: it allocates and
//! performs I/O. All call sites are on bus/worker threads.

use std::io::{stderr, Write};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static ENABLED: OnceLock<bool> = OnceLock::new();

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var("SP_TRACE")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

fn file_sink() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        std::env::var("TEMP")
            .ok()
            .or_else(|| std::env::var("TMP").ok())
            .map(|d| std::path::Path::new(&d).join("soundpad-trace.log"))
    }
    #[cfg(not(windows))]
    {
        Some(std::path::PathBuf::from("/tmp/soundpad-trace.log"))
    }
}

pub fn trace(msg: &str) {
    if !enabled() {
        return;
    }
    let wall_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!(
        "[sp {wall_ms} tid={:?}] {msg}",
        std::thread::current().id()
    );
    // Sink 1: stderr (CLI/TUI frontends show it directly).
    let mut err = stderr();
    let _ = writeln!(err, "{line}");
    let _ = err.flush();
    // Sink 2: trace file — the GUI is a windowed app with no console
    // attached; a file sink keeps the last steps recoverable.
    if let Some(path) = file_sink() {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

#[macro_export]
macro_rules! sptrace {
    ($($arg:tt)*) => {
        if $crate::trace::enabled() {
            $crate::trace::trace(&format!($($arg)*))
        }
    };
}
