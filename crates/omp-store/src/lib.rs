//! `omp-store` — gRPC service wrapping a single `omp_core::store::DiskStore`.
//!
//! Implements `omp.store.v1.Store` from `proto/store.proto`. Per
//! `docs/design/14-microservice-decomposition.md`, this is the only inter-service
//! gRPC surface in OMP — every other internal call is HTTP/JSON.
//!
//! Auth model: the service trusts whoever can reach it on the wire (mTLS at the
//! deployment layer). The signed tenant context from `omp-tenant-ctx` is parsed
//! when present so we can attach `tenant_id` to traces, but tenant
//! *isolation* lives at the directory layer — each tenant runs against its own
//! `DiskStore` rooted at a per-tenant path. The deployment topology decides
//! whether one `omp-store` process serves many tenants (one per Repo) or many
//! processes serve one tenant each.

use std::pin::Pin;
use std::sync::Arc;

use omp_core::hash::Hash;
use omp_core::store::ObjectStore;
use omp_proto::store::v1::{
    store_server::Store, DeleteRefRequest, DeleteRefResponse, GetRequest, GetResponse,
    HasRequest, HasResponse, IterRefsItem, IterRefsRequest, PutRequest, PutResponse,
    ReadHeadRequest, ReadHeadResponse, ReadRefRequest, ReadRefResponse, WriteHeadRequest,
    WriteHeadResponse, WriteRefRequest, WriteRefResponse,
};
use tonic::{Request, Response, Status};

/// gRPC service implementation backed by a single `ObjectStore`.
pub struct StoreService {
    inner: Arc<dyn ObjectStore>,
}

impl StoreService {
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self { inner }
    }
}

fn map_err<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

fn parse_hash(s: &str) -> Result<Hash, Status> {
    s.parse::<Hash>()
        .map_err(|e| Status::invalid_argument(format!("bad hash: {e}")))
}

#[tonic::async_trait]
impl Store for StoreService {
    async fn put(
        &self,
        req: Request<PutRequest>,
    ) -> Result<Response<PutResponse>, Status> {
        let req = req.into_inner();
        let inner = self.inner.clone();
        let hash = tokio::task::spawn_blocking(move || inner.put(&req.r#type, &req.content))
            .await
            .map_err(map_err)?
            .map_err(map_err)?;
        Ok(Response::new(PutResponse {
            hash: hash.to_string(),
        }))
    }

    async fn get(
        &self,
        req: Request<GetRequest>,
    ) -> Result<Response<GetResponse>, Status> {
        let req = req.into_inner();
        let hash = parse_hash(&req.hash)?;
        let inner = self.inner.clone();
        let opt = tokio::task::spawn_blocking(move || inner.get(&hash))
            .await
            .map_err(map_err)?
            .map_err(map_err)?;
        match opt {
            Some((ty, content)) => Ok(Response::new(GetResponse {
                found: true,
                r#type: ty,
                content,
            })),
            None => Ok(Response::new(GetResponse {
                found: false,
                r#type: String::new(),
                content: Vec::new(),
            })),
        }
    }

    async fn has(
        &self,
        req: Request<HasRequest>,
    ) -> Result<Response<HasResponse>, Status> {
        let req = req.into_inner();
        let hash = parse_hash(&req.hash)?;
        let inner = self.inner.clone();
        let present = tokio::task::spawn_blocking(move || inner.has(&hash))
            .await
            .map_err(map_err)?
            .map_err(map_err)?;
        Ok(Response::new(HasResponse { present }))
    }

    async fn read_ref(
        &self,
        req: Request<ReadRefRequest>,
    ) -> Result<Response<ReadRefResponse>, Status> {
        let req = req.into_inner();
        let inner = self.inner.clone();
        let opt = tokio::task::spawn_blocking(move || inner.read_ref(&req.name))
            .await
            .map_err(map_err)?
            .map_err(map_err)?;
        Ok(Response::new(match opt {
            Some(h) => ReadRefResponse {
                found: true,
                hash: h.to_string(),
            },
            None => ReadRefResponse {
                found: false,
                hash: String::new(),
            },
        }))
    }

    async fn write_ref(
        &self,
        req: Request<WriteRefRequest>,
    ) -> Result<Response<WriteRefResponse>, Status> {
        let req = req.into_inner();
        let hash = parse_hash(&req.hash)?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.write_ref(&req.name, &hash))
            .await
            .map_err(map_err)?
            .map_err(map_err)?;
        Ok(Response::new(WriteRefResponse {}))
    }

    async fn delete_ref(
        &self,
        req: Request<DeleteRefRequest>,
    ) -> Result<Response<DeleteRefResponse>, Status> {
        let req = req.into_inner();
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.delete_ref(&req.name))
            .await
            .map_err(map_err)?
            .map_err(map_err)?;
        Ok(Response::new(DeleteRefResponse {}))
    }

    type IterRefsStream =
        Pin<Box<dyn futures::Stream<Item = Result<IterRefsItem, Status>> + Send + 'static>>;

    async fn iter_refs(
        &self,
        _req: Request<IterRefsRequest>,
    ) -> Result<Response<Self::IterRefsStream>, Status> {
        // Materialize the iterator on the blocking pool — `iter_refs` borrows
        // from `self.inner` via the trait object, so we collect into a Vec
        // before constructing the stream.
        let inner = self.inner.clone();
        let pairs: Vec<(String, Hash)> = tokio::task::spawn_blocking(move || {
            inner
                .iter_refs()
                .map(|it| it.collect::<Vec<_>>())
        })
        .await
        .map_err(map_err)?
        .map_err(map_err)?;

        let stream = async_stream::try_stream! {
            for (name, hash) in pairs {
                yield IterRefsItem {
                    name,
                    hash: hash.to_string(),
                };
            }
        };
        Ok(Response::new(Box::pin(stream)))
    }

    async fn read_head(
        &self,
        _req: Request<ReadHeadRequest>,
    ) -> Result<Response<ReadHeadResponse>, Status> {
        let inner = self.inner.clone();
        let value = tokio::task::spawn_blocking(move || inner.read_head())
            .await
            .map_err(map_err)?
            .map_err(map_err)?;
        Ok(Response::new(ReadHeadResponse { value }))
    }

    async fn write_head(
        &self,
        req: Request<WriteHeadRequest>,
    ) -> Result<Response<WriteHeadResponse>, Status> {
        let req = req.into_inner();
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.write_head(&req.value))
            .await
            .map_err(map_err)?
            .map_err(map_err)?;
        Ok(Response::new(WriteHeadResponse {}))
    }
}

/// Build a tonic `Router` configured with the `StoreService`.
pub fn router(svc: StoreService) -> tonic::transport::server::Router {
    use omp_proto::store::v1::store_server::StoreServer;
    tonic::transport::Server::builder().add_service(StoreServer::new(svc))
}
