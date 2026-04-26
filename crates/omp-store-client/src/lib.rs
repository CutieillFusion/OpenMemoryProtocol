//! `omp-store-client` — a sync `ObjectStore` impl backed by gRPC.
//!
//! Every `omp_core::api::Repo` accepts an `Arc<dyn ObjectStore>`. Today the
//! disk-backed `DiskStore` is one such impl; this crate adds a second:
//! `RemoteStore` makes Repo work against the `omp-store` gRPC service from a
//! different process or pod. The split designed in
//! `docs/design/14-microservice-decomposition.md` is structurally enabled by
//! exactly this abstraction.
//!
//! ## Sync ↔ async bridge
//!
//! `ObjectStore` is intentionally sync because every caller (axum handlers,
//! the CLI, in-process tests) treats it that way. tonic clients are async.
//! Rather than refactoring the whole trait, `RemoteStore` owns a small
//! dedicated tokio runtime on its own thread; each sync method dispatches the
//! gRPC call onto that runtime and blocks the caller on a oneshot reply.
//! This works from *any* calling context (sync test, async handler on a
//! multi-thread runtime, blocking thread pool) because the runtime we block
//! on is never the runtime the caller lives on.

use std::sync::{Arc, Mutex};

use omp_core::error::{OmpError, Result as OmpResult};
use omp_core::hash::Hash;
use omp_core::store::ObjectStore;
use omp_proto::store::v1::{
    store_client::StoreClient, DeleteRefRequest, GetRequest, HasRequest, IterRefsRequest,
    PutRequest, ReadHeadRequest, ReadRefRequest, WriteHeadRequest, WriteRefRequest,
};
use omp_tenant_ctx::HEADER_NAME as TENANT_CTX_METADATA_KEY;
use thiserror::Error;
use tokio::runtime::Handle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("rpc: {0}")]
    Rpc(String),
    #[error("invalid hash: {0}")]
    BadHash(String),
}

impl From<RemoteError> for OmpError {
    fn from(e: RemoteError) -> Self {
        OmpError::internal(e.to_string())
    }
}

/// A sync handle to a remote `omp-store` service.
///
/// Cloneable; clones share the underlying channel + runtime.
#[derive(Clone)]
pub struct RemoteStore {
    inner: Arc<Inner>,
}

struct Inner {
    handle: Handle,
    channel: Channel,
    /// Optional `X-OMP-Tenant-Context` header attached to every request.
    tenant_ctx: Mutex<Option<String>>,
    _runtime: tokio::runtime::Runtime,
}

impl RemoteStore {
    /// Connect to a `omp-store` service at the given endpoint.
    ///
    /// `endpoint` is a tonic endpoint string, e.g. `"http://127.0.0.1:9001"`.
    pub fn connect(endpoint: impl Into<String>) -> Result<Self, RemoteError> {
        let endpoint = endpoint.into();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("omp-remote-store-rt")
            .build()
            .map_err(|e| RemoteError::Transport(e.to_string()))?;
        let handle = runtime.handle().clone();

        let channel = handle.block_on(async {
            Channel::from_shared(endpoint)
                .map_err(|e| RemoteError::Connect(e.to_string()))?
                .connect()
                .await
                .map_err(|e| RemoteError::Connect(e.to_string()))
        })?;

        Ok(Self {
            inner: Arc::new(Inner {
                handle,
                channel,
                tenant_ctx: Mutex::new(None),
                _runtime: runtime,
            }),
        })
    }

    /// Set the `X-OMP-Tenant-Context` header attached to every outgoing call.
    /// Pass `None` to clear.
    pub fn set_tenant_context(&self, ctx_b64: Option<String>) {
        let mut g = self.inner.tenant_ctx.lock().expect("tenant ctx lock");
        *g = ctx_b64;
    }

    fn client(&self) -> StoreClient<Channel> {
        StoreClient::new(self.inner.channel.clone())
    }

    fn attach_ctx<T>(&self, mut req: tonic::Request<T>) -> tonic::Request<T> {
        if let Some(ctx) = self.inner.tenant_ctx.lock().ok().and_then(|g| g.clone()) {
            if let Ok(val) = ctx.parse() {
                req.metadata_mut().insert(TENANT_CTX_METADATA_KEY, val);
            }
        }
        req
    }

    fn block<F, T, E>(&self, fut: F) -> std::result::Result<T, E>
    where
        F: std::future::Future<Output = std::result::Result<T, E>> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.inner.handle.spawn(async move {
            let _ = tx.send(fut.await);
        });
        rx.recv().expect("remote store runtime crashed")
    }
}

fn rpc<E: std::fmt::Display>(e: E) -> RemoteError {
    RemoteError::Rpc(e.to_string())
}

impl ObjectStore for RemoteStore {
    fn put(&self, type_: &str, content: &[u8]) -> OmpResult<Hash> {
        let mut client = self.client();
        let req = self.attach_ctx(tonic::Request::new(PutRequest {
            r#type: type_.to_string(),
            content: content.to_vec(),
        }));
        let resp = self
            .block(async move { client.put(req).await.map_err(rpc) })
            .map_err(OmpError::from)?;
        resp.into_inner()
            .hash
            .parse::<Hash>()
            .map_err(|e| OmpError::internal(format!("server returned bad hash: {e}")))
    }

    fn get(&self, hash: &Hash) -> OmpResult<Option<(String, Vec<u8>)>> {
        let mut client = self.client();
        let req = self.attach_ctx(tonic::Request::new(GetRequest {
            hash: hash.to_string(),
        }));
        let resp = self
            .block(async move { client.get(req).await.map_err(rpc) })
            .map_err(OmpError::from)?;
        let inner = resp.into_inner();
        if inner.found {
            Ok(Some((inner.r#type, inner.content)))
        } else {
            Ok(None)
        }
    }

    fn has(&self, hash: &Hash) -> OmpResult<bool> {
        let mut client = self.client();
        let req = self.attach_ctx(tonic::Request::new(HasRequest {
            hash: hash.to_string(),
        }));
        let resp = self
            .block(async move { client.has(req).await.map_err(rpc) })
            .map_err(OmpError::from)?;
        Ok(resp.into_inner().present)
    }

    fn iter_refs(&self) -> OmpResult<Box<dyn Iterator<Item = (String, Hash)> + '_>> {
        let mut client = self.client();
        let req = self.attach_ctx(tonic::Request::new(IterRefsRequest {}));
        let pairs = self
            .block(async move {
                let stream = client.iter_refs(req).await.map_err(rpc)?.into_inner();
                let mut out = Vec::new();
                let mut s = stream;
                while let Some(item) = s.next().await {
                    let item = item.map_err(rpc)?;
                    let h: Hash = item
                        .hash
                        .parse()
                        .map_err(|e| RemoteError::BadHash(format!("{e}")))?;
                    out.push((item.name, h));
                }
                Ok::<Vec<(String, Hash)>, RemoteError>(out)
            })
            .map_err(OmpError::from)?;
        Ok(Box::new(pairs.into_iter()))
    }

    fn read_ref(&self, name: &str) -> OmpResult<Option<Hash>> {
        let mut client = self.client();
        let req = self.attach_ctx(tonic::Request::new(ReadRefRequest {
            name: name.to_string(),
        }));
        let resp = self
            .block(async move { client.read_ref(req).await.map_err(rpc) })
            .map_err(OmpError::from)?;
        let inner = resp.into_inner();
        if inner.found {
            inner
                .hash
                .parse::<Hash>()
                .map(Some)
                .map_err(|e| OmpError::internal(format!("server returned bad hash: {e}")))
        } else {
            Ok(None)
        }
    }

    fn write_ref(&self, name: &str, commit: &Hash) -> OmpResult<()> {
        let mut client = self.client();
        let req = self.attach_ctx(tonic::Request::new(WriteRefRequest {
            name: name.to_string(),
            hash: commit.to_string(),
        }));
        self.block(async move { client.write_ref(req).await.map_err(rpc) })
            .map_err(OmpError::from)?;
        Ok(())
    }

    fn delete_ref(&self, name: &str) -> OmpResult<()> {
        let mut client = self.client();
        let req = self.attach_ctx(tonic::Request::new(DeleteRefRequest {
            name: name.to_string(),
        }));
        self.block(async move { client.delete_ref(req).await.map_err(rpc) })
            .map_err(OmpError::from)?;
        Ok(())
    }

    fn read_head(&self) -> OmpResult<String> {
        let mut client = self.client();
        let req = self.attach_ctx(tonic::Request::new(ReadHeadRequest {}));
        let resp = self
            .block(async move { client.read_head(req).await.map_err(rpc) })
            .map_err(OmpError::from)?;
        Ok(resp.into_inner().value)
    }

    fn write_head(&self, value: &str) -> OmpResult<()> {
        let mut client = self.client();
        let req = self.attach_ctx(tonic::Request::new(WriteHeadRequest {
            value: value.to_string(),
        }));
        self.block(async move { client.write_head(req).await.map_err(rpc) })
            .map_err(OmpError::from)?;
        Ok(())
    }
}
