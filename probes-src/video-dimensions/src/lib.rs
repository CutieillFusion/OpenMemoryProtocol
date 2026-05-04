use ciborium::value::Value as Cbor;

pub use probe_common::{alloc, free};

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: u32, in_len: u32) -> i64 {
    let input = probe_common::slice_from_raw(in_ptr, in_len);
    let (bytes, _) = probe_common::decode_input(input);
    match probe_mp4::mp4_video_dimensions(&bytes) {
        Some((w, h)) => {
            let map = vec![
                (Cbor::Text("width".into()), Cbor::Integer((w as i64).into())),
                (Cbor::Text("height".into()), Cbor::Integer((h as i64).into())),
            ];
            probe_common::return_value(Cbor::Map(map))
        }
        None => probe_common::return_null(),
    }
}
