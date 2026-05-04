use ciborium::value::Value as Cbor;

pub use probe_common::{alloc, free};

#[no_mangle]
pub unsafe extern "C" fn probe_run(in_ptr: u32, in_len: u32) -> i64 {
    let input = probe_common::slice_from_raw(in_ptr, in_len);
    let (bytes, _) = probe_common::decode_input(input);
    match imagesize::blob_size(&bytes) {
        Ok(dim) => {
            let map = vec![
                (
                    Cbor::Text("width".into()),
                    Cbor::Integer((dim.width as i64).into()),
                ),
                (
                    Cbor::Text("height".into()),
                    Cbor::Integer((dim.height as i64).into()),
                ),
            ];
            probe_common::return_value(Cbor::Map(map))
        }
        Err(_) => probe_common::return_null(),
    }
}
