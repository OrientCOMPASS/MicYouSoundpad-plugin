//! WAV file handling: fast header probing (duration for the sound list) and
//! full decoding to interleaved f32 (for the mixer).
//!
//! Supported formats (everything a soundpad realistically needs — convert
//! anything else with `ffmpeg -i in.mp3 out.wav`):
//! * PCM 8 / 16 / 24 / 32-bit
//! * IEEE float 32 / 64-bit
//! * any channel count (interleaved); any sample rate (the decode worker
//!   resamples to the host chain's 48 kHz afterwards)
//!
//! Runs ONLY on the decode worker thread (full decode) or during `init`
//! (demo generation) — never on the bus dispatch thread and never on the
//! real-time audio thread.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Fully decoded sound: interleaved f32 samples in [-1, 1].
pub struct DecodedWav {
    pub sample_rate: u32,
    pub channels: usize,
    pub samples: Vec<f32>,
}

impl DecodedWav {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels
        }
    }
    #[allow(dead_code)] // 诊断辅助（测试使用）
    pub fn duration_ms(&self) -> u64 {
        if self.sample_rate == 0 {
            return 0;
        }
        self.frames() as u64 * 1000 / self.sample_rate as u64
    }
}

/// Parsed `fmt ` chunk.
struct FmtChunk {
    format_tag: u16,
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    /// Valid bits per sample for PCM24-in-32 style padding (extensible).
    valid_bits: u16,
}

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Walk RIFF chunks, returning `(fmt, data_offset, data_len)`.
/// Reads only headers — the `data` payload is never loaded here.
fn parse_chunks(buf: &[u8]) -> Result<(FmtChunk, usize, usize), String> {
    if buf.len() < 44 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut fmt: Option<FmtChunk> = None;
    let mut pos = 12usize;
    while pos + 8 <= buf.len() {
        let id = &buf[pos..pos + 4];
        let len = rd_u32(buf, pos + 4) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            if body + 16 > buf.len() {
                return Err("truncated fmt chunk".into());
            }
            let format_tag = rd_u16(buf, body);
            let channels = rd_u16(buf, body + 2);
            let sample_rate = rd_u32(buf, body + 4);
            let bits_per_sample = rd_u16(buf, body + 14);
            let mut valid_bits = bits_per_sample;
            // WAVE_FORMAT_EXTENSIBLE (0xFFFE): real tag + valid bits live in
            // the extension.
            let tag = if format_tag == 0xFFFE {
                if body + 40 <= buf.len() && len >= 40 {
                    valid_bits = rd_u16(buf, body + 18);
                    rd_u16(buf, body + 24)
                } else {
                    return Err("truncated extensible fmt chunk".into());
                }
            } else {
                format_tag
            };
            if channels == 0 || sample_rate == 0 {
                return Err("invalid fmt chunk (0 channels or rate)".into());
            }
            fmt = Some(FmtChunk {
                format_tag: tag,
                channels,
                sample_rate,
                bits_per_sample,
                valid_bits,
            });
        } else if id == b"data" {
            let Some(f) = fmt else {
                return Err("data chunk before fmt chunk".into());
            };
            let avail = buf.len().saturating_sub(body);
            return Ok((f, body, len.min(avail)));
        }
        // Chunks are word-aligned.
        pos = body + len + (len & 1);
    }
    Err("no data chunk".into())
}

/// Fast header probe: duration in milliseconds without decoding audio data.
/// Reads at most the first 64 KiB of the file (chunks before `data`).
pub fn probe_duration_ms(path: &Path) -> Result<u64, String> {
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut head = [0u8; 12];
    f.read_exact(&mut head).map_err(|e| e.to_string())?;
    if &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    // Scan chunks by seeking; only fmt is fully read.
    let file_len = f.metadata().map(|m| m.len()).unwrap_or(0) as usize;
    let mut pos = 12usize;
    let mut fmt: Option<FmtChunk> = None;
    let mut hdr = [0u8; 8];
    while pos + 8 <= file_len {
        f.seek(SeekFrom::Start(pos as u64))
            .map_err(|e| e.to_string())?;
        f.read_exact(&mut hdr).map_err(|e| e.to_string())?;
        let len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        if &hdr[0..4] == b"fmt " {
            let mut fb = vec![0u8; len.min(64)];
            f.read_exact(&mut fb).map_err(|e| e.to_string())?;
            if fb.len() >= 16 {
                let format_tag = rd_u16(&fb, 0);
                let mut valid_bits = rd_u16(&fb, 14);
                let tag = if format_tag == 0xFFFE && fb.len() >= 40 {
                    valid_bits = rd_u16(&fb, 18);
                    rd_u16(&fb, 24)
                } else {
                    format_tag
                };
                fmt = Some(FmtChunk {
                    format_tag: tag,
                    channels: rd_u16(&fb, 2),
                    sample_rate: rd_u32(&fb, 4),
                    bits_per_sample: rd_u16(&fb, 14),
                    valid_bits,
                });
            }
        } else if &hdr[0..4] == b"data" {
            let Some(fm) = fmt else {
                return Err("data chunk before fmt chunk".into());
            };
            let bytes_per_frame =
                fm.channels as usize * ((fm.bits_per_sample + 7) / 8) as usize;
            if bytes_per_frame == 0 || fm.sample_rate == 0 {
                return Err("invalid format".into());
            }
            // data chunk size may be bogus (streaming rips put u32::MAX);
            // fall back to the remaining file length.
            let data_len = if len == 0 || pos + 8 + len > file_len {
                file_len.saturating_sub(pos + 8)
            } else {
                len
            };
            let frames = data_len / bytes_per_frame;
            return Ok(frames as u64 * 1000 / fm.sample_rate as u64);
        }
        pos += 8 + len + (len & 1);
    }
    Err("no data chunk".into())
}

/// Decode the whole file to interleaved f32.
pub fn decode(path: &Path) -> Result<DecodedWav, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a file".into());
    }
    // Sound files for a pad are small; refuse absurd inputs up front.
    const MAX_FILE: u64 = 512 * 1024 * 1024;
    if meta.len() > MAX_FILE {
        return Err(format!("file too large ({} bytes)", meta.len()));
    }
    let mut buf = Vec::with_capacity(meta.len().min(4 * 1024 * 1024) as usize);
    File::open(path)
        .map_err(|e| e.to_string())?
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    decode_bytes(&buf)
}

/// Decode from an in-memory RIFF image (shared by `decode` and tests).
pub fn decode_bytes(buf: &[u8]) -> Result<DecodedWav, String> {
    let (fmt, data_off, data_len) = parse_chunks(buf)?;
    let ch = fmt.channels as usize;
    let bits = fmt.bits_per_sample as usize;
    let data = &buf[data_off..data_off + data_len];
    let frame_bytes = ch * ((bits + 7) / 8);
    if frame_bytes == 0 {
        return Err("zero-byte frames".into());
    }
    let frames = data.len() / frame_bytes;
    let mut samples = Vec::with_capacity(frames * ch);

    // format_tag: 1 = PCM, 3 = IEEE float.
    match (fmt.format_tag, bits) {
        (1, 8) => {
            // 8-bit PCM is unsigned, centered at 128.
            for &b in &data[..frames * ch] {
                samples.push((b as f32 - 128.0) / 128.0);
            }
        }
        (1, 16) => {
            for i in (0..frames * ch * 2).step_by(2) {
                samples.push(i16::from_le_bytes([data[i], data[i + 1]]) as f32 / 32768.0);
            }
        }
        (1, 24) => {
            for i in (0..frames * ch * 3).step_by(3) {
                let v = (data[i] as i32) | ((data[i + 1] as i32) << 8) | ((data[i + 2] as i32) << 16);
                let v = if v & 0x80_0000 != 0 { v - 0x100_0000 } else { v };
                samples.push(v as f32 / 8388608.0);
            }
        }
        (1, 32) => {
            let scale = if fmt.valid_bits > 0 && fmt.valid_bits < 32 {
                1u64 << (32 - fmt.valid_bits as u32)
            } else {
                1
            };
            for i in (0..frames * ch * 4).step_by(4) {
                let v = i32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
                samples.push((v / scale as i32) as f32 / 2147483648.0);
            }
        }
        (3, 32) => {
            for i in (0..frames * ch * 4).step_by(4) {
                samples.push(f32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]));
            }
        }
        (3, 64) => {
            for i in (0..frames * ch * 8).step_by(8) {
                let mut b = [0u8; 8];
                b.copy_from_slice(&data[i..i + 8]);
                samples.push(f64::from_le_bytes(b) as f32);
            }
        }
        (tag, b) => return Err(format!("unsupported WAV format (tag {tag}, {b} bit)")),
    }
    Ok(DecodedWav {
        sample_rate: fmt.sample_rate,
        channels: ch,
        samples,
    })
}

// ─────────────────────────── demo sound generation ────────────────────────

/// Build a 16-bit mono 44.1 kHz WAV image (used for the first-run demo
/// sounds; mirrors the official example plugin's approach).
pub fn encode_wav_pcm16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut wav = Vec::with_capacity(data_len + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        wav.extend_from_slice(&v.to_le_bytes());
    }
    wav
}

/// One demo voice: sine stack with exponential decay and a 5 ms attack.
fn demo_voice(freqs: &[(f32, f32)], seconds: f64, decay: f64, sample_rate: u32) -> Vec<f32> {
    let n = (seconds * sample_rate as f64) as usize;
    let attack = (0.005 * sample_rate as f64) as usize;
    (0..n)
        .map(|i| {
            let t = i as f64 / sample_rate as f64;
            let mut v = 0.0f32;
            for (f, amp) in freqs {
                v += (t * *f as f64 * std::f64::consts::TAU).sin() as f32 * amp;
            }
            let env = (-decay * t).exp() as f32 * if i < attack { i as f32 / attack as f32 } else { 1.0 };
            v * env * 0.5
        })
        .collect()
}

/// Generate the three first-run demo sounds into `dir` (created as needed).
/// Returns the list of generated file names.
pub fn generate_demo_sounds(dir: &Path) -> Result<Vec<String>, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let rate = 44_100;
    let mut names = Vec::new();
    let mut put = |name: &str, samples: Vec<f32>| -> Result<(), String> {
        let wav = encode_wav_pcm16(&samples, rate);
        let file = format!("{name}.wav");
        std::fs::write(dir.join(&file), &wav).map_err(|e| e.to_string())?;
        names.push(file);
        Ok(())
    };
    put("beep", demo_voice(&[(440.0, 1.0)], 0.45, 0.0, rate))?;
    put("ding", demo_voice(&[(659.25, 1.0), (1318.5, 0.4)], 1.0, 3.0, rate))?;
    put(
        "chime",
        demo_voice(&[(880.0, 1.0), (1108.7, 0.7), (1318.5, 0.5)], 1.6, 2.2, rate),
    )?;
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, seconds: f64, rate: u32) -> Vec<f32> {
        let n = (seconds * rate as f64) as usize;
        (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32)
            .collect()
}

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("sp-wav-test-{}-{name}", std::process::id()))
    }

    fn write_16bit_stereo(path: &Path, frames: &[(i16, i16)], rate: u32) {
        let data_len = frames.len() * 4;
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes()); // stereo
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * 4).to_le_bytes());
        w.extend_from_slice(&4u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data_len as u32).to_le_bytes());
        for (l, r) in frames {
            w.extend_from_slice(&l.to_le_bytes());
            w.extend_from_slice(&r.to_le_bytes());
        }
        std::fs::write(path, &w).unwrap();
    }

    #[test]
    fn encode_decode_roundtrip_pcm16() {
        let path = tmp("roundtrip.wav");
        let input = sine(440.0, 0.25, 44100);
        std::fs::write(&path, encode_wav_pcm16(&input, 44100)).unwrap();
        let d = decode(&path).unwrap();
        assert_eq!(d.sample_rate, 44100);
        assert_eq!(d.channels, 1);
        assert_eq!(d.samples.len(), input.len());
        // 16-bit quantization error only.
        let err = d
            .samples
            .iter()
            .zip(&input)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 1e-3, "max quantization error {err}");
        let ms = probe_duration_ms(&path).unwrap();
        assert!((ms as i64 - 250).abs() <= 1, "probe said {ms} ms");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn decode_stereo_16bit_interleaves() {
        let path = tmp("stereo.wav");
        write_16bit_stereo(&path, &[(0, 32767), (-32768, 16384)], 48000);
        let d = decode(&path).unwrap();
        assert_eq!(d.channels, 2);
        assert_eq!(d.frames(), 2);
        assert!((d.samples[0] - 0.0).abs() < 1e-3);
        assert!((d.samples[1] - 1.0).abs() < 1e-3);
        assert!((d.samples[2] + 1.0).abs() < 1e-3);
        assert!((d.samples[3] - 0.5).abs() < 1e-3);
        // 2 frames @ 48 kHz = 0.041 ms → integer-truncated to 0.
        assert_eq!(probe_duration_ms(&path).unwrap(), 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn decode_float32_and_pcm24() {
        // float32 mono
        let mut w = Vec::new();
        let samples: [f32; 4] = [0.0, 0.5, -0.5, 1.0];
        let data_len = 16usize;
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&48000u32.to_le_bytes());
        w.extend_from_slice(&(48000u32 * 4).to_le_bytes());
        w.extend_from_slice(&4u16.to_le_bytes());
        w.extend_from_slice(&32u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data_len as u32).to_le_bytes());
        for s in samples {
            w.extend_from_slice(&s.to_le_bytes());
        }
        let path = tmp("f32.wav");
        std::fs::write(&path, &w).unwrap();
        let d = decode(&path).unwrap();
        assert_eq!(d.samples.len(), 4);
        assert!((d.samples[1] - 0.5).abs() < 1e-6);
        std::fs::remove_file(&path).ok();

        // PCM24: one full-scale-ish sample 0x7FFFFF → ~1.0
        let mut w = Vec::new();
        let data_len = 3usize;
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&44100u32.to_le_bytes());
        w.extend_from_slice(&(44100u32 * 3).to_le_bytes());
        w.extend_from_slice(&3u16.to_le_bytes());
        w.extend_from_slice(&24u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data_len as u32).to_le_bytes());
        w.extend_from_slice(&[0xFF, 0xFF, 0x7F]);
        let path = tmp("p24.wav");
        std::fs::write(&path, &w).unwrap();
        let d = decode(&path).unwrap();
        assert!((d.samples[0] - (8388607.0 / 8388608.0)).abs() < 1e-5);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn probe_and_decode_skip_padding_chunks() {
        // LIST chunk between fmt and data (real-world files carry metadata).
        let mut w = Vec::new();
        let samples = sine(220.0, 0.1, 44100);
        let data_len = samples.len() * 2;
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(0u32).to_le_bytes()); // size patched later
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&44100u32.to_le_bytes());
        w.extend_from_slice(&(44100u32 * 2).to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"LIST");
        w.extend_from_slice(&5u32.to_le_bytes()); // odd length → pad byte
        w.extend_from_slice(b"INFOX");
        w.push(0);
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data_len as u32).to_le_bytes());
        for s in &samples {
            w.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
        }
        let riff_len = (w.len() - 8) as u32;
        w[4..8].copy_from_slice(&riff_len.to_le_bytes());
        let path = tmp("list.wav");
        std::fs::write(&path, &w).unwrap();
        assert!((probe_duration_ms(&path).unwrap() as i64 - 100).abs() <= 1);
        assert_eq!(decode(&path).unwrap().samples.len(), samples.len());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_non_wav() {
        let path = tmp("bad.wav");
        std::fs::write(&path, b"not a wav file at all, definitely not riff").unwrap();
        assert!(decode(&path).is_err());
        assert!(probe_duration_ms(&path).is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn demo_generation_produces_probable_files() {
        let dir = tmp("demo-dir");
        let names = generate_demo_sounds(&dir).unwrap();
        assert_eq!(names.len(), 3);
        for n in &names {
            let ms = probe_duration_ms(&dir.join(n)).unwrap();
            assert!(ms > 200, "{n} too short: {ms}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
