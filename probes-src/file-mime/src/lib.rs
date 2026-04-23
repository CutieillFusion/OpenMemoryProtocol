use ciborium::value::Value as Cbor;

pub use probe_common::{alloc, free};

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: u32, in_len: u32) -> i64 {
    let input = probe_common::slice_from_raw(in_ptr, in_len);
    let (bytes, _) = probe_common::decode_input(input);
    let mime = infer::get(&bytes)
        .map(|kind| kind.mime_type().to_string())
        .unwrap_or_else(|| {
            if looks_like_text(&bytes) {
                "text/plain".to_string()
            } else {
                "application/octet-stream".to_string()
            }
        });
    probe_common::return_value(Cbor::Text(mime))
}

fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let sample = &bytes[..bytes.len().min(4096)];
    // Heuristic: no NULs and mostly printable ASCII / common whitespace.
    if sample.contains(&0) {
        return false;
    }
    let printable = sample
        .iter()
        .filter(|&&b| b == b'\n' || b == b'\r' || b == b'\t' || (b >= 0x20 && b < 0x7F) || b >= 0x80)
        .count();
    printable * 100 / sample.len() >= 95
}
