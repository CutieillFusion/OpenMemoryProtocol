//! OMP protocol buffer crate.
//!
//! Generated code lives in `OUT_DIR` at build time; we re-export it under
//! stable paths so callers can use `omp_proto::store::v1::*`.

pub mod store {
    pub mod v1 {
        tonic::include_proto!("omp.store.v1");
    }
}

pub mod events {
    pub mod v1 {
        tonic::include_proto!("omp.events.v1");
    }
}
