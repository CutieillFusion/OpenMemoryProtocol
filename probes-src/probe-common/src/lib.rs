//! Shared helpers for starter-pack WASM probes.
//!
//! Exposes `alloc` / `free` (required ABI exports) and CBOR encode/decode
//! helpers keyed to the input shape `{ "bytes": <bytes>, "kwargs": <map> }`
//! and a free-form CBOR output value.
//!
//! Each probe crate re-exports `alloc` / `free` from here and defines its own
//! `probe_run`.

use ciborium::value::Value as Cbor;

/// Packed i64 return value: high 32 bits = pointer, low 32 bits = length.
pub fn pack_return(ptr: u32, len: u32) -> i64 {
    ((ptr as u64) << 32 | (len as u64)) as i64
}

/// Leak a Vec<u8> into a raw ptr+len pair that the host will later pass back
/// to `free`.
pub fn leak_vec(v: Vec<u8>) -> (u32, u32) {
    let len = v.len() as u32;
    let boxed = v.into_boxed_slice();
    let ptr = Box::into_raw(boxed) as *mut u8 as u32;
    (ptr, len)
}

/// Host-callable ABI.
///
/// Safety: the host is responsible for matching every `alloc` with a `free`,
/// and for only passing `(ptr, len)` pairs that this module produced.
#[no_mangle]
pub unsafe extern "C" fn alloc(size: u32) -> u32 {
    let mut v: Vec<u8> = Vec::with_capacity(size as usize);
    v.set_len(size as usize);
    let (ptr, _) = leak_vec(v);
    ptr
}

#[no_mangle]
pub unsafe extern "C" fn free(ptr: u32, len: u32) {
    if ptr == 0 || len == 0 {
        return;
    }
    let slice = std::slice::from_raw_parts_mut(ptr as *mut u8, len as usize);
    let boxed: Box<[u8]> = Box::from_raw(slice);
    drop(boxed);
}

/// Reconstruct a Vec<u8> from a previously-allocated buffer so we can read
/// its contents.
///
/// Safety: pair must come from this module's `alloc`.
pub unsafe fn slice_from_raw(ptr: u32, len: u32) -> &'static [u8] {
    std::slice::from_raw_parts(ptr as *const u8, len as usize)
}

/// Decode the probe input CBOR. Expects a map with keys `bytes` and `kwargs`.
/// Returns `(bytes, kwargs)`.
pub fn decode_input(raw: &[u8]) -> (Vec<u8>, Cbor) {
    let value: Cbor = ciborium::de::from_reader(raw).unwrap_or(Cbor::Null);
    let mut bytes: Vec<u8> = Vec::new();
    let mut kwargs = Cbor::Map(Vec::new());
    if let Cbor::Map(entries) = value {
        for (k, v) in entries {
            match k {
                Cbor::Text(s) if s == "bytes" => {
                    if let Cbor::Bytes(b) = v {
                        bytes = b;
                    }
                }
                Cbor::Text(s) if s == "kwargs" => {
                    kwargs = v;
                }
                _ => {}
            }
        }
    }
    (bytes, kwargs)
}

/// Encode any CBOR value to bytes.
pub fn encode_output(v: &Cbor) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::ser::into_writer(v, &mut out).expect("cbor serialize");
    out
}

/// Return a CBOR null from a probe.
pub fn return_null() -> i64 {
    return_value(Cbor::Null)
}

pub fn return_value(v: Cbor) -> i64 {
    let bytes = encode_output(&v);
    let (ptr, len) = leak_vec(bytes);
    pack_return(ptr, len)
}

/// Helpers for common kwarg extraction.
pub fn kwarg_str<'a>(kwargs: &'a Cbor, key: &str) -> Option<&'a str> {
    if let Cbor::Map(entries) = kwargs {
        for (k, v) in entries {
            if let Cbor::Text(s) = k {
                if s == key {
                    if let Cbor::Text(t) = v {
                        return Some(t.as_str());
                    }
                }
            }
        }
    }
    None
}

pub fn kwarg_int(kwargs: &Cbor, key: &str) -> Option<i64> {
    if let Cbor::Map(entries) = kwargs {
        for (k, v) in entries {
            if let Cbor::Text(s) = k {
                if s == key {
                    if let Cbor::Integer(i) = v {
                        return i64::try_from(*i).ok();
                    }
                }
            }
        }
    }
    None
}
