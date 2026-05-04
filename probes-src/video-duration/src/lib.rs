use ciborium::value::Value as Cbor;

pub use probe_common::{alloc, free};

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: u32, in_len: u32) -> i64 {
    let input = probe_common::slice_from_raw(in_ptr, in_len);
    let (bytes, _) = probe_common::decode_input(input);
    match probe_mp4::mp4_duration_seconds(&bytes) {
        Some(secs) => probe_common::return_value(Cbor::Float(secs)),
        None => probe_common::return_null(),
    }
}
