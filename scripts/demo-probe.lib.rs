// Demo probe for the omp-builder UI: counts whitespace-separated words in
// a text input. Paste the contents of this file into the "lib.rs" field at
// /ui/probes/build, and `demo-probe.probe.toml` into the manifest field.
//
// After publish + install, this probe is referenceable as `text.word_count`
// from a schema's `[fields.<name>]` block (`source = "probe"`).

use ciborium::value::Value as Cbor;

pub use probe_common::{alloc, free};

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: u32, in_len: u32) -> i64 {
    let input = probe_common::slice_from_raw(in_ptr, in_len);
    let (bytes, _) = probe_common::decode_input(input);
    let count = count_words(&bytes);
    probe_common::return_value(Cbor::Integer((count as i64).into()))
}

fn count_words(bytes: &[u8]) -> usize {
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    s.split_whitespace().count()
}
