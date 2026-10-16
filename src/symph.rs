//! Multi-format audio decoding via **symphonia** (pure Rust, no C deps):
//! MP3 · FLAC · OGG Vorbis · AAC (M4A / ISO-MP4) — plus format probing for
//! duration estimates used by the sound list.
//!
//! WAV intentionally does NOT go through symphonia (its reader features are
//! disabled): the built-in `wav.rs` decoder is smaller, faster to probe and
//! fully unit-tested. This module runs ONLY on the decode worker thread or
//! during `init` — never on the bus dispatch or audio threads.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Raw decoded audio: interleaved f32 in [-1, 1].
pub struct RawAudio {
    pub sample_rate: u32,
    pub channels: usize,
    pub samples: Vec<f32>,
}

/// Extensions this plugin lists and plays (besides WAV handled by `wav.rs`).
pub const SUPPORTED_EXTS: &[&str] = &[
    "wav", "mp3", "flac", "ogg", "oga", "m4a", "mp4", "aac",
];

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .map(|e| {
            let l = e.to_string_lossy().to_lowercase();
            SUPPORTED_EXTS.iter().any(|x| *x == l)
        })
        .unwrap_or(false)
}

/// Decode the whole file to interleaved f32 (first audio track).
pub fn decode(path: &Path) -> Result<RawAudio, String> {
    let file = File::open(path).map_err(|e| format!("open: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !ext.is_empty() {
        hint.with_extension(&ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("unsupported or corrupt file ({e})"))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| "file has no audio track".to_string())?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("no decoder for this codec ({e})"))?;
    let track_id = track.id;

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut rate = 0u32;
    let mut channels = 0usize;
    let mut out: Vec<f32> = Vec::new();
    let mut decoded_packets = 0u64;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // End of stream (or a read error after we already have audio).
            Err(_) => break,
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue, // skip undecodable packets (broken frames)
        };
        let spec = *decoded.spec();
        if sample_buf.is_none() {
            rate = spec.rate;
            channels = spec.channels.count();
        } else if spec.rate != rate || spec.channels.count() != channels {
            return Err("stream format changes mid-file are not supported".into());
        }
        let need = decoded.capacity();
        let nframes = decoded.frames();
        if sample_buf
            .as_ref()
            .map_or(true, |b| b.capacity() < need * channels)
        {
            sample_buf = Some(SampleBuffer::<f32>::new(need as u64, spec));
        }
        let buf = sample_buf.as_mut().expect("buffer just created");
        buf.copy_interleaved_ref(decoded);
        out.extend_from_slice(&buf.samples()[..nframes * channels]);
        decoded_packets += 1;
    }

    if out.is_empty() || decoded_packets == 0 {
        return Err("no audio data could be decoded".into());
    }
    Ok(RawAudio {
        sample_rate: rate,
        channels,
        samples: out,
    })
}

/// Duration probe WITHOUT full decode: exact when the container/codec header
/// carries frame counts (FLAC, Xing-tagged MP3, M4A); OGG falls back to the
/// last page's granule position (also exact); otherwise Err — callers use 0
/// ("unknown") and the session snapshot corrects it once the track decodes.
pub fn probe_duration_ms(path: &Path) -> Result<u64, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !ext.is_empty() {
        hint.with_extension(&ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| e.to_string())?;
    let track = probed
        .format
        .default_track()
        .ok_or_else(|| "no audio track".to_string())?;
    let cp = &track.codec_params;

    // Exact: frame count + time base.
    if let (Some(frames), Some(tb)) = (cp.n_frames, cp.time_base) {
        if tb.denom > 0 && frames > 0 {
            let ms = (frames as u128 * tb.numer as u128 * 1000) / tb.denom as u128;
            return Ok(ms.min(u64::MAX as u128) as u64);
        }
    }
    let rate = cp.sample_rate.unwrap_or(0);
    drop(probed);

    // OGG (Vorbis / Opus pages): last-page granule position = total PCM
    // samples at the codec rate (48 kHz for Opus by spec).
    if ext == "ogg" || ext == "oga" || rate > 0 {
        if let Some(ms) = ogg_granule_duration_ms(path) {
            return Ok(ms);
        }
    }
    Err("duration unknown".into())
}

/// Exact OGG duration: parse the identification header for the sample rate,
/// then the LAST complete page's granule position (total samples streamed).
/// Returns None for non-OGG / truncated files. Reads ≤ 128 KiB total.
fn ogg_granule_duration_ms(path: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len < 58 {
        return None;
    }
    // ── head: rate from the Vorbis identification / OpusHead packet ──
    let mut head = vec![0u8; (len.min(65536)) as usize];
    f.read_exact(&mut head).ok()?;
    if &head[0..4] != b"OggS" {
        return None;
    }
    let rate;
    if let Some(p) = find_bytes(&head, b"OpusHead") {
        let _ = p;
        rate = 48_000u64; // Opus granules are always 48 kHz
    } else if let Some(p) = find_bytes(&head, b"\x01vorbis") {
        // vorbis identification header: type(1) "vorbis"(6) version(4)
        // channels(1) rate(u32 LE @ p+12)
        if p + 16 > head.len() {
            return None;
        }
        rate = u32::from_le_bytes([head[p + 12], head[p + 13], head[p + 14], head[p + 15]]) as u64;
    } else {
        return None;
    }
    if rate == 0 {
        return None;
    }
    // ── tail: last page's granule position ──
    let tail_len = (len.min(65536)) as usize;
    f.seek(SeekFrom::End(-(tail_len as i64))).ok()?;
    let mut tail = vec![0u8; tail_len];
    f.read_exact(&mut tail).ok()?;
    let mut best: Option<u64> = None;
    let mut i = 0;
    while i + 27 <= tail.len() {
        if &tail[i..i + 4] == b"OggS" {
            // page header: OggS(4) ver(1) flags(1) granule(8 @ +6)
            let granule = u64::from_le_bytes(tail[i + 6..i + 14].try_into().ok()?);
            if granule != u64::MAX {
                best = Some(granule); // keep scanning forward → last wins
            }
            let segments = tail[i + 26] as usize;
            if i + 27 + segments > tail.len() {
                break;
            }
            let body: usize = tail[i + 27..i + 27 + segments].iter().map(|&x| x as usize).sum();
            i += 27 + segments + body;
        } else {
            i += 1;
        }
    }
    let granule = best?;
    Some(granule * 1000 / rate)
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
