//! Toy probe — returns `true` if the file body contains the literal string
//! `"This is a test string"`, `false` otherwise.
//!
//! Output field type is `bool` (CBOR boolean). Useful as a smoke test for
//! the upload-a-probe-via-the-UI flow described in
//! `docs/design/19-web-frontend.md`.

use ciborium::value::Value as Cbor;

pub use probe_common::{alloc, free};

const NEEDLE: &[u8] = b"This is a test string";

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: u32, in_len: u32) -> i64 {
    let input = probe_common::slice_from_raw(in_ptr, in_len);
    let (bytes, _kwargs) = probe_common::decode_input(input);
    let hit = contains_subslice(&bytes, NEEDLE);
    probe_common::return_value(Cbor::Bool(hit))
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
