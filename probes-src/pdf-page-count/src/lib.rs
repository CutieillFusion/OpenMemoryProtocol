//! Best-effort PDF page count via byte scan for `/Type /Page` occurrences.
//!
//! Counts non-`/Pages` page-object markers. This handles the common case of
//! uncompressed cross-reference tables; PDFs that store all page nodes inside
//! compressed object streams can read 0. Returning `null` for "we don't know"
//! is preferable to a confidently-wrong number, so we only emit a count when
//! we found at least one matching marker.

use ciborium::value::Value as Cbor;

pub use probe_common::{alloc, free};

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: u32, in_len: u32) -> i64 {
    let input = probe_common::slice_from_raw(in_ptr, in_len);
    let (bytes, _) = probe_common::decode_input(input);
    if !looks_like_pdf(&bytes) {
        return probe_common::return_null();
    }
    let count = count_page_objects(&bytes);
    if count == 0 {
        return probe_common::return_null();
    }
    probe_common::return_value(Cbor::Integer((count as i64).into()))
}

fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
}

fn count_page_objects(bytes: &[u8]) -> usize {
    // Match `/Type /Page` (with optional whitespace between `/Type` and `/Page`)
    // but exclude `/Pages` and `/PageLayout`. We accept any whitespace byte
    // (space, tab, CR, LF, FF) between the two name tokens to match real PDFs.
    const KEY: &[u8] = b"/Type";
    const VAL: &[u8] = b"/Page";
    let mut count = 0usize;
    let mut i = 0usize;
    while i + KEY.len() < bytes.len() {
        if &bytes[i..i + KEY.len()] == KEY {
            let mut j = i + KEY.len();
            while j < bytes.len() && is_pdf_ws(bytes[j]) {
                j += 1;
            }
            if j + VAL.len() <= bytes.len() && &bytes[j..j + VAL.len()] == VAL {
                let after = bytes.get(j + VAL.len()).copied().unwrap_or(0);
                // Reject `/Pages`, `/PageLayout`, etc. The next byte after
                // `/Page` should be a delimiter — whitespace, `/`, `<`, `>`,
                // `[`, `]`, `(`, `)`, end-of-buffer.
                if is_pdf_name_end(after) {
                    count += 1;
                }
            }
            i = j + VAL.len();
            continue;
        }
        i += 1;
    }
    count
}

fn is_pdf_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0c | 0x00)
}

fn is_pdf_name_end(b: u8) -> bool {
    if b == 0 {
        return true; // treat end-of-buffer as terminator
    }
    is_pdf_ws(b) || matches!(b, b'/' | b'<' | b'>' | b'[' | b']' | b'(' | b')')
}
