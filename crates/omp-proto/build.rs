// Generates Rust code from `proto/store.proto` (and any future proto files)
// at build time. Output goes into `OUT_DIR`; `src/lib.rs` re-exports it.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("proto"))
        .expect("locating proto/ directory");

    let store_proto = proto_root.join("store.proto");
    let events_proto = proto_root.join("events.proto");

    println!("cargo:rerun-if-changed={}", store_proto.display());
    println!("cargo:rerun-if-changed={}", events_proto.display());

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[store_proto, events_proto], &[proto_root])?;

    Ok(())
}
