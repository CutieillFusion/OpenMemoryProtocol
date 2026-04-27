//! End-to-end test of the gRPC `Store` service.
//!
//! Spins up `omp-store::StoreService` over an in-process tonic server bound to
//! a free TCP port, connects a `RemoteStore` client to it, and exercises every
//! method of the `ObjectStore` trait. A `DiskStore` is opened on the same
//! repo root in parallel; whatever `RemoteStore` writes must be visible there
//! and vice versa. This is the keystone proof that the decomposition wire
//! works (see `docs/design/14-microservice-decomposition.md`).

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use omp_core::store::disk::DiskStore;
use omp_core::store::ObjectStore;
use omp_store::StoreService;
use omp_store_client::RemoteStore;
use tempfile::TempDir;

/// Pick a free port by opening a TCP listener and reading its bound port.
/// The listener is dropped before the caller binds, so the port is briefly
/// available; with SO_REUSEADDR semantics Linux is forgiving enough that
/// this works reliably for tests.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind random port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Spin up a tonic server on a dedicated thread with its own runtime.
/// Returns the bound address; the server runs until the process exits
/// (the dedicated runtime keeps it alive).
fn spawn_store_server(repo_root: std::path::PathBuf) -> SocketAddr {
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    thread::Builder::new()
        .name(format!("omp-store-test-{port}"))
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("build runtime");

            rt.block_on(async move {
                let disk = DiskStore::init(&repo_root).expect("init disk");
                let svc = StoreService::new(Arc::new(disk));
                ready_tx.send(()).expect("ready signal");
                omp_store::router(svc)
                    .serve(addr)
                    .await
                    .expect("serve gRPC");
            });
        })
        .expect("spawn server thread");

    ready_rx.recv().expect("server ready");
    // Tiny delay so the listener is actually accepting before the client
    // tries to connect. Avoids a race in CI on slower machines.
    thread::sleep(Duration::from_millis(50));
    addr
}

fn endpoint(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

#[test]
fn round_trip_put_get_has() {
    let dir = TempDir::new().unwrap();
    let addr = spawn_store_server(dir.path().to_path_buf());

    // Wait briefly for the server to be ready in case the loop above didn't
    // signal in time.
    let mut client = None;
    for _ in 0..40 {
        match RemoteStore::connect(endpoint(addr)) {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
    let client = client.expect("connect to gRPC store");

    // PUT through the wire.
    let payload = b"hello world".to_vec();
    let hash = client.put("blob", &payload).expect("remote put");

    // GET through the wire.
    let got = client.get(&hash).expect("remote get").expect("present");
    assert_eq!(got.0, "blob");
    assert_eq!(got.1, payload);

    // HAS through the wire.
    assert!(client.has(&hash).expect("remote has"));

    // Verify the object actually landed on disk via a parallel DiskStore.
    let disk = DiskStore::open(dir.path()).expect("open disk");
    let disk_got = disk.get(&hash).expect("disk get").expect("disk present");
    assert_eq!(disk_got.0, "blob");
    assert_eq!(disk_got.1, payload);
}

#[test]
fn refs_round_trip() {
    let dir = TempDir::new().unwrap();
    let addr = spawn_store_server(dir.path().to_path_buf());
    let client = RemoteStore::connect(endpoint(addr)).expect("connect");

    // Stash a fake commit blob to point a ref at — server only validates the
    // hash format on write_ref, not that the object exists.
    let commit_hash = client.put("commit", b"fake commit body").unwrap();

    // Initially the ref is absent.
    assert!(client.read_ref("refs/heads/main").unwrap().is_none());

    // Write it.
    client
        .write_ref("refs/heads/main", &commit_hash)
        .expect("remote write_ref");

    // Read it back.
    let got = client.read_ref("refs/heads/main").unwrap();
    assert_eq!(got, Some(commit_hash));

    // iter_refs sees it.
    let pairs: Vec<_> = client.iter_refs().unwrap().collect();
    assert!(pairs
        .iter()
        .any(|(n, h)| n == "refs/heads/main" && *h == commit_hash));

    // Delete and re-check.
    client.delete_ref("refs/heads/main").expect("delete ref");
    assert!(client.read_ref("refs/heads/main").unwrap().is_none());
}

#[test]
fn head_round_trip() {
    let dir = TempDir::new().unwrap();
    let addr = spawn_store_server(dir.path().to_path_buf());
    let client = RemoteStore::connect(endpoint(addr)).expect("connect");

    // DiskStore::init writes a default HEAD; just exercise the wire shape.
    let initial_head = client.read_head().expect("read head");
    assert!(!initial_head.is_empty());

    // Write a detached HEAD. We use a known commit hash format; the server
    // accepts any string for HEAD.
    client
        .write_head("ref: refs/heads/feature")
        .expect("write head");
    let after = client.read_head().unwrap();
    assert_eq!(after, "ref: refs/heads/feature");
}

#[test]
fn cross_client_visibility() {
    // Two RemoteStore clients pointing at the same omp-store see each other's
    // writes — sanity check that the server isn't accidentally per-connection.
    let dir = TempDir::new().unwrap();
    let addr = spawn_store_server(dir.path().to_path_buf());

    let a = RemoteStore::connect(endpoint(addr)).expect("connect a");
    let b = RemoteStore::connect(endpoint(addr)).expect("connect b");

    let payload = b"shared visibility".to_vec();
    let hash = a.put("blob", &payload).expect("a.put");
    let got = b.get(&hash).expect("b.get").expect("b sees");
    assert_eq!(got.1, payload);
}

#[test]
fn unknown_hash_returns_none() {
    let dir = TempDir::new().unwrap();
    let addr = spawn_store_server(dir.path().to_path_buf());
    let client = RemoteStore::connect(endpoint(addr)).expect("connect");

    let unknown: omp_core::hash::Hash =
        "0000000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap();
    assert_eq!(client.get(&unknown).unwrap(), None);
    assert!(!client.has(&unknown).unwrap());
}
