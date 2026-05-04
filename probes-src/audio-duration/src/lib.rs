//! Duration in seconds for WAV and MP3 audio files.
//!
//! WAV: parse the canonical RIFF/WAVE header. Reads the `fmt ` chunk for the
//! byte rate and the `data` chunk for payload size; duration = data / rate.
//!
//! MP3: walk frame headers from the first sync word (after any ID3v2 tag),
//! summing per-frame durations. A Xing/Info VBR header in the first frame is
//! honored when present (header `frames` field × frame samples / sample rate).
//!
//! Returns CBOR null for any other format or any unparseable file.

use ciborium::value::Value as Cbor;

pub use probe_common::{alloc, free};

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: u32, in_len: u32) -> i64 {
    let input = probe_common::slice_from_raw(in_ptr, in_len);
    let (bytes, _) = probe_common::decode_input(input);

    if let Some(secs) = wav_duration_seconds(&bytes) {
        return probe_common::return_value(cbor_float(secs));
    }
    if let Some(secs) = mp3_duration_seconds(&bytes) {
        return probe_common::return_value(cbor_float(secs));
    }
    probe_common::return_null()
}

fn cbor_float(f: f64) -> Cbor {
    Cbor::Float(f)
}

// ---------- WAV ----------

fn wav_duration_seconds(bytes: &[u8]) -> Option<f64> {
    if bytes.len() < 44 {
        return None;
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }

    let mut byte_rate: Option<u32> = None;
    let mut data_size: Option<u32> = None;
    let mut i = 12usize;
    while i + 8 <= bytes.len() {
        let chunk_id = &bytes[i..i + 4];
        let chunk_size = u32::from_le_bytes([
            bytes[i + 4],
            bytes[i + 5],
            bytes[i + 6],
            bytes[i + 7],
        ]) as usize;
        let body = i + 8;
        if chunk_id == b"fmt " && body + 16 <= bytes.len() {
            byte_rate = Some(u32::from_le_bytes([
                bytes[body + 8],
                bytes[body + 9],
                bytes[body + 10],
                bytes[body + 11],
            ]));
        } else if chunk_id == b"data" {
            data_size = Some(chunk_size as u32);
            break;
        }
        i = body.saturating_add(chunk_size);
        if chunk_size % 2 == 1 {
            i += 1;
        }
    }

    let rate = byte_rate? as f64;
    let size = data_size? as f64;
    if rate == 0.0 {
        return None;
    }
    Some(size / rate)
}

// ---------- MP3 ----------

fn mp3_duration_seconds(bytes: &[u8]) -> Option<f64> {
    let start = skip_id3v2(bytes);
    if start >= bytes.len() {
        return None;
    }

    let mut i = start;
    let mut total = 0.0f64;
    let mut frames_seen = 0usize;

    // Look for first frame; if the first frame contains a Xing/Info header,
    // use it for VBR-aware duration.
    while i + 4 <= bytes.len() {
        if let Some(frame) = Mp3Frame::parse(&bytes[i..]) {
            // Xing/Info header lookup.
            if frames_seen == 0 {
                if let Some(xing_frames) = find_xing_frames(&bytes[i..i + frame.frame_size.min(bytes.len() - i)]) {
                    let samples_per_frame = frame.samples_per_frame() as f64;
                    let sr = frame.sample_rate as f64;
                    if sr > 0.0 {
                        return Some(xing_frames as f64 * samples_per_frame / sr);
                    }
                }
            }
            total += frame.duration_seconds();
            frames_seen += 1;
            i += frame.frame_size.max(1);
        } else {
            i += 1;
        }
    }

    if frames_seen == 0 {
        return None;
    }
    Some(total)
}

fn skip_id3v2(bytes: &[u8]) -> usize {
    if bytes.len() < 10 || &bytes[0..3] != b"ID3" {
        return 0;
    }
    // Synchsafe size: bytes 6..10, each byte's top bit is zero, 7 bits each.
    let size = ((bytes[6] as usize) << 21)
        | ((bytes[7] as usize) << 14)
        | ((bytes[8] as usize) << 7)
        | (bytes[9] as usize);
    10 + size
}

struct Mp3Frame {
    sample_rate: u32,
    frame_size: usize,
    mpeg_version: u8, // 1, 2, or 25 (for MPEG 2.5)
    layer: u8,        // 1, 2, 3
}

impl Mp3Frame {
    fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 4 {
            return None;
        }
        let h = ((buf[0] as u32) << 24)
            | ((buf[1] as u32) << 16)
            | ((buf[2] as u32) << 8)
            | (buf[3] as u32);
        // Sync: 11 bits set.
        if h & 0xFFE00000 != 0xFFE00000 {
            return None;
        }
        let version_bits = (h >> 19) & 0x3;
        let layer_bits = (h >> 17) & 0x3;
        let bitrate_bits = (h >> 12) & 0xF;
        let samplerate_bits = (h >> 10) & 0x3;
        let padding = ((h >> 9) & 0x1) as usize;

        if layer_bits == 0 || bitrate_bits == 0 || bitrate_bits == 0xF || samplerate_bits == 0x3 {
            return None;
        }

        let mpeg_version = match version_bits {
            0 => 25,
            2 => 2,
            3 => 1,
            _ => return None,
        };
        let layer = match layer_bits {
            1 => 3,
            2 => 2,
            3 => 1,
            _ => return None,
        };

        let bitrate_kbps = bitrate_table(mpeg_version, layer, bitrate_bits as usize)?;
        let sample_rate = samplerate_table(mpeg_version, samplerate_bits as usize)?;
        let bitrate_bps = bitrate_kbps * 1000;

        let frame_size = if layer == 1 {
            (12 * bitrate_bps / sample_rate + padding as u32) as usize * 4
        } else {
            let samples = if mpeg_version == 1 { 144 } else { 72 };
            (samples * bitrate_bps / sample_rate) as usize + padding
        };

        if frame_size < 4 {
            return None;
        }

        Some(Mp3Frame {
            sample_rate,
            frame_size,
            mpeg_version,
            layer,
        })
    }

    fn samples_per_frame(&self) -> u32 {
        match (self.mpeg_version, self.layer) {
            (1, 1) => 384,
            (1, 2) | (1, 3) => 1152,
            (_, 1) => 384,
            (_, 2) => 1152,
            (_, 3) => 576,
            _ => 1152,
        }
    }

    fn duration_seconds(&self) -> f64 {
        self.samples_per_frame() as f64 / self.sample_rate as f64
    }
}

fn bitrate_table(version: u8, layer: u8, idx: usize) -> Option<u32> {
    // Returns kbps for (version, layer, bitrate index 1..14).
    if !(1..=14).contains(&idx) {
        return None;
    }
    // MPEG 1
    const V1L1: [u32; 15] = [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448];
    const V1L2: [u32; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
    const V1L3: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    // MPEG 2 / 2.5
    const V2L1: [u32; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256];
    const V2L23: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
    let table = match (version, layer) {
        (1, 1) => &V1L1,
        (1, 2) => &V1L2,
        (1, 3) => &V1L3,
        (_, 1) => &V2L1,
        (_, _) => &V2L23,
    };
    Some(table[idx])
}

fn samplerate_table(version: u8, idx: usize) -> Option<u32> {
    if idx > 2 {
        return None;
    }
    let rates: [u32; 3] = match version {
        1 => [44100, 48000, 32000],
        2 => [22050, 24000, 16000],
        25 => [11025, 12000, 8000],
        _ => return None,
    };
    Some(rates[idx])
}

fn find_xing_frames(frame: &[u8]) -> Option<u32> {
    // The Xing/Info header lives at a fixed offset inside the first frame
    // body. Rather than computing the exact MPEG-version-dependent offset, we
    // search the frame buffer for the 4-byte tag — cheap and reliable.
    for i in 0..frame.len().saturating_sub(8) {
        let tag = &frame[i..i + 4];
        if tag == b"Xing" || tag == b"Info" {
            // After the tag, 4 bytes of flags, then conditional frames/bytes/toc/quality.
            let flags = u32::from_be_bytes([frame[i + 4], frame[i + 5], frame[i + 6], frame[i + 7]]);
            if flags & 0x1 == 0 {
                return None; // no frames count
            }
            let off = i + 8;
            if off + 4 > frame.len() {
                return None;
            }
            let frames =
                u32::from_be_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]);
            return Some(frames);
        }
    }
    None
}
