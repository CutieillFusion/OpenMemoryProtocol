//! Minimal ISO-BMFF (MP4) box reader. Pulls just enough out of `moov`/`mvhd`
//! and the video track's `tkhd` to compute duration in seconds and pixel
//! dimensions. No allocation beyond what the input slice already owns.

/// Top-level box scan helper. Returns the *body* of the first box matching
/// `target` at this level (without the 8-byte size+type header).
pub fn find_box<'a>(mut data: &'a [u8], target: &[u8; 4]) -> Option<&'a [u8]> {
    while data.len() >= 8 {
        let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let typ = &data[4..8];
        let (header_len, body_len) = match size {
            1 => {
                if data.len() < 16 {
                    return None;
                }
                let large = u64::from_be_bytes([
                    data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
                ]);
                if large < 16 {
                    return None;
                }
                (16usize, (large as usize).saturating_sub(16))
            }
            0 => (8, data.len() - 8),
            n if (n as usize) < 8 => return None,
            n => (8, (n as usize) - 8),
        };
        if header_len + body_len > data.len() {
            return None;
        }
        if typ == target {
            return Some(&data[header_len..header_len + body_len]);
        }
        data = &data[header_len + body_len..];
    }
    None
}

/// Iterator over all boxes at this level matching `target`.
pub fn iter_boxes<'a>(data: &'a [u8], target: &'a [u8; 4]) -> impl Iterator<Item = &'a [u8]> {
    BoxIter {
        data,
        target: *target,
    }
}

struct BoxIter<'a> {
    data: &'a [u8],
    target: [u8; 4],
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<&'a [u8]> {
        while self.data.len() >= 8 {
            let size = u32::from_be_bytes([self.data[0], self.data[1], self.data[2], self.data[3]]);
            let typ = [self.data[4], self.data[5], self.data[6], self.data[7]];
            let (header_len, body_len) = match size {
                1 => {
                    if self.data.len() < 16 {
                        return None;
                    }
                    let large = u64::from_be_bytes([
                        self.data[8],
                        self.data[9],
                        self.data[10],
                        self.data[11],
                        self.data[12],
                        self.data[13],
                        self.data[14],
                        self.data[15],
                    ]);
                    if large < 16 {
                        return None;
                    }
                    (16usize, (large as usize).saturating_sub(16))
                }
                0 => (8, self.data.len() - 8),
                n if (n as usize) < 8 => return None,
                n => (8, (n as usize) - 8),
            };
            if header_len + body_len > self.data.len() {
                return None;
            }
            let body_start = header_len;
            let body_end = header_len + body_len;
            let matched = typ == self.target;
            let body = &self.data[body_start..body_end];
            self.data = &self.data[body_end..];
            if matched {
                return Some(body);
            }
        }
        None
    }
}

/// Movie duration in seconds, derived from `mvhd` inside `moov`.
pub fn mp4_duration_seconds(bytes: &[u8]) -> Option<f64> {
    let moov = find_box(bytes, b"moov")?;
    let mvhd = find_box(moov, b"mvhd")?;
    if mvhd.is_empty() {
        return None;
    }
    let version = mvhd[0];
    let (timescale, duration) = match version {
        0 => {
            if mvhd.len() < 20 {
                return None;
            }
            let ts = u32::from_be_bytes([mvhd[12], mvhd[13], mvhd[14], mvhd[15]]) as u64;
            let dur = u32::from_be_bytes([mvhd[16], mvhd[17], mvhd[18], mvhd[19]]) as u64;
            (ts, dur)
        }
        1 => {
            if mvhd.len() < 32 {
                return None;
            }
            let ts = u32::from_be_bytes([mvhd[20], mvhd[21], mvhd[22], mvhd[23]]) as u64;
            let dur = u64::from_be_bytes([
                mvhd[24], mvhd[25], mvhd[26], mvhd[27], mvhd[28], mvhd[29], mvhd[30], mvhd[31],
            ]);
            (ts, dur)
        }
        _ => return None,
    };
    if timescale == 0 {
        return None;
    }
    Some(duration as f64 / timescale as f64)
}

/// Pixel (width, height) of the first video track. tkhd width/height are
/// 16.16 fixed-point; we round to the nearest integer.
pub fn mp4_video_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let moov = find_box(bytes, b"moov")?;
    for trak in iter_boxes(moov, b"trak") {
        if !trak_is_video(trak) {
            continue;
        }
        let tkhd = find_box(trak, b"tkhd")?;
        if tkhd.is_empty() {
            return None;
        }
        let version = tkhd[0];
        let (w_off, h_off) = match version {
            0 => (76, 80),
            1 => (88, 92),
            _ => return None,
        };
        if tkhd.len() < h_off + 4 {
            return None;
        }
        let w_fixed = u32::from_be_bytes([
            tkhd[w_off],
            tkhd[w_off + 1],
            tkhd[w_off + 2],
            tkhd[w_off + 3],
        ]);
        let h_fixed = u32::from_be_bytes([
            tkhd[h_off],
            tkhd[h_off + 1],
            tkhd[h_off + 2],
            tkhd[h_off + 3],
        ]);
        // Convert from 16.16 fixed-point with rounding.
        let width = (w_fixed + 0x8000) >> 16;
        let height = (h_fixed + 0x8000) >> 16;
        if width == 0 || height == 0 {
            return None;
        }
        return Some((width, height));
    }
    None
}

fn trak_is_video(trak: &[u8]) -> bool {
    let Some(mdia) = find_box(trak, b"mdia") else {
        return false;
    };
    let Some(hdlr) = find_box(mdia, b"hdlr") else {
        return false;
    };
    // hdlr layout: version(1) + flags(3) + predefined(4) + handler_type(4) + …
    if hdlr.len() < 12 {
        return false;
    }
    &hdlr[8..12] == b"vide"
}
