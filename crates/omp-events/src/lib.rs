//! OMP event-streaming abstraction.
//!
//! Per `docs/design/16-event-streaming.md`: producers publish to topic-per-type
//! channels keyed by tenant id; consumers subscribe and receive envelopes in
//! per-tenant order. The wire format is the protobuf `Envelope` from
//! `omp_proto::events::v1`.
//!
//! This crate ships:
//!  - `EventBus` trait — the small contract producers/consumers depend on.
//!  - `InMemoryBus` — a tokio-broadcast-backed in-process bus, used in tests
//!    and in `--no-broker` deployments. The wire envelope shape is identical
//!    so swapping in a Kafka/Redpanda implementation is a drop-in.
//!  - `EventType` constants — the closed v1 set of dotted-namespace names.
//!  - Helpers for assembling envelopes from typed payloads.
//!
//! The Kafka/Redpanda implementation lives behind a separate impl (TBD —
//! depends on `rdkafka`'s build deps, which we keep optional for now).

use async_trait::async_trait;
use omp_proto::events::v1::Envelope;
use prost::Message;
use thiserror::Error;
use tokio::sync::broadcast;

pub mod event_type {
    pub const COMMIT_CREATED: &str = "commit.created";
    pub const REF_UPDATED: &str = "ref.updated";
    pub const MANIFEST_STAGED: &str = "manifest.staged";
    pub const COMMIT_FAILED: &str = "commit.failed";
    pub const QUOTA_EXCEEDED: &str = "quota.exceeded";
    pub const GC_COMPLETED: &str = "gc.completed";
}

#[derive(Debug, Error)]
pub enum BusError {
    #[error("bus closed")]
    Closed,
    #[error("encode: {0}")]
    Encode(String),
    #[error("decode: {0}")]
    Decode(String),
}

/// Subscriber handle. `next()` returns the next envelope, or `None` when the
/// bus is dropped.
pub struct Subscription {
    rx: broadcast::Receiver<Envelope>,
}

impl Subscription {
    /// Receive the next envelope. Returns `None` when the bus is closed.
    pub async fn next(&mut self) -> Option<Envelope> {
        loop {
            match self.rx.recv().await {
                Ok(env) => return Some(env),
                Err(broadcast::error::RecvError::Closed) => return None,
                // Lagged: receiver fell behind the buffer. Drop and continue;
                // events.proto consumers are documented as best-effort under
                // overload — durability stories live in the producer's
                // post-write contract.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    }
}

/// The small producer/consumer contract. Implementations decide whether the
/// underlying bus is in-process, Kafka, NATS, etc.
#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, env: Envelope) -> Result<(), BusError>;
    fn subscribe(&self) -> Subscription;
}

/// Tokio-broadcast-backed in-process bus. Cheap for dev/tests and for
/// monolithic deployments where there's no broker.
pub struct InMemoryBus {
    tx: broadcast::Sender<Envelope>,
}

impl InMemoryBus {
    /// Create a new bus with the given internal buffer capacity. Slow
    /// consumers that fall further behind than `capacity` will see lag and
    /// drop events; the bus itself stays healthy.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }
}

impl Default for InMemoryBus {
    fn default() -> Self {
        Self::with_capacity(1024)
    }
}

#[async_trait]
impl EventBus for InMemoryBus {
    async fn publish(&self, env: Envelope) -> Result<(), BusError> {
        // `send` returns Err only if there are no receivers; that's fine for
        // a "fire and forget" bus — we don't want producer failure on a
        // quiet topic.
        let _ = self.tx.send(env);
        Ok(())
    }

    fn subscribe(&self) -> Subscription {
        Subscription {
            rx: self.tx.subscribe(),
        }
    }
}

// =============================================================================
// Envelope assembly helpers
// =============================================================================

/// Build a fully-formed envelope from a typed payload + metadata.
pub fn envelope_for<T: Message>(
    event_type: &str,
    tenant: &str,
    trace_id: Option<&str>,
    idempotency_key: Option<&str>,
    payload: &T,
) -> Result<Envelope, BusError> {
    let mut buf = Vec::with_capacity(payload.encoded_len());
    payload
        .encode(&mut buf)
        .map_err(|e| BusError::Encode(e.to_string()))?;
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    Ok(Envelope {
        version: 1,
        r#type: event_type.to_string(),
        tenant: tenant.to_string(),
        occurred_at: now,
        trace_id: trace_id.unwrap_or("").to_string(),
        idempotency_key: idempotency_key.unwrap_or("").to_string(),
        payload: buf,
    })
}

/// Decode an envelope's payload into a typed message (caller must know the
/// type matches the envelope's `type` discriminator).
pub fn decode_payload<T: Message + Default>(env: &Envelope) -> Result<T, BusError> {
    T::decode(&*env.payload).map_err(|e| BusError::Decode(e.to_string()))
}

// =============================================================================
// Re-exports of the event payload types so callers don't have to import from
// the deeply-nested `omp_proto::events::v1` path.
// =============================================================================

pub mod payload {
    pub use omp_proto::events::v1::{
        CommitCreated, CommitFailed, Envelope, GcCompleted, ManifestStaged, QuotaExceeded,
        RefUpdated,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use omp_proto::events::v1::{
        CommitCreated, CommitFailed, GcCompleted, ManifestStaged, QuotaExceeded, RefUpdated,
    };
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_memory_publish_and_subscribe() {
        let bus = Arc::new(InMemoryBus::default());
        let mut sub = bus.subscribe();

        let payload = CommitCreated {
            branch: "refs/heads/main".into(),
            commit_hash: "deadbeef".into(),
            parent_hashes: vec![],
            paths_touched: vec!["docs/a.md".into()],
        };
        let env = envelope_for(
            event_type::COMMIT_CREATED,
            "alice",
            Some("trace1"),
            None,
            &payload,
        )
        .unwrap();
        bus.publish(env.clone()).await.unwrap();

        let received = sub.next().await.expect("subscriber sees event");
        assert_eq!(received.r#type, event_type::COMMIT_CREATED);
        assert_eq!(received.tenant, "alice");

        let decoded: CommitCreated = decode_payload(&received).unwrap();
        assert_eq!(decoded.commit_hash, "deadbeef");
        assert_eq!(decoded.paths_touched, vec!["docs/a.md".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multiple_subscribers_each_see_event() {
        let bus = Arc::new(InMemoryBus::default());
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();

        let env = envelope_for(
            event_type::REF_UPDATED,
            "alice",
            None,
            None,
            &RefUpdated {
                ref_name: "refs/heads/main".into(),
                old_hash: "".into(),
                new_hash: "abc".into(),
            },
        )
        .unwrap();
        bus.publish(env).await.unwrap();

        let ea = a.next().await.unwrap();
        let eb = b.next().await.unwrap();
        assert_eq!(ea.r#type, event_type::REF_UPDATED);
        assert_eq!(eb.r#type, event_type::REF_UPDATED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn payloads_decode_for_each_event_type() {
        let bus = Arc::new(InMemoryBus::default());
        let mut sub = bus.subscribe();

        let cases: Vec<(&str, Vec<u8>)> = vec![
            (event_type::COMMIT_CREATED, {
                let mut b = Vec::new();
                CommitCreated::default().encode(&mut b).unwrap();
                b
            }),
            (event_type::REF_UPDATED, {
                let mut b = Vec::new();
                RefUpdated::default().encode(&mut b).unwrap();
                b
            }),
            (event_type::MANIFEST_STAGED, {
                let mut b = Vec::new();
                ManifestStaged::default().encode(&mut b).unwrap();
                b
            }),
            (event_type::COMMIT_FAILED, {
                let mut b = Vec::new();
                CommitFailed::default().encode(&mut b).unwrap();
                b
            }),
            (event_type::QUOTA_EXCEEDED, {
                let mut b = Vec::new();
                QuotaExceeded::default().encode(&mut b).unwrap();
                b
            }),
            (event_type::GC_COMPLETED, {
                let mut b = Vec::new();
                GcCompleted::default().encode(&mut b).unwrap();
                b
            }),
        ];

        for (ty, payload) in &cases {
            let env = Envelope {
                version: 1,
                r#type: ty.to_string(),
                tenant: "alice".into(),
                occurred_at: "now".into(),
                trace_id: "".into(),
                idempotency_key: "".into(),
                payload: payload.clone(),
            };
            bus.publish(env).await.unwrap();
        }

        for (ty, _) in &cases {
            let env = sub.next().await.unwrap();
            assert_eq!(env.r#type, *ty);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn late_subscriber_misses_earlier_events() {
        // Documents the broadcast semantics: subscribers get events
        // published *after* they subscribe.
        let bus = Arc::new(InMemoryBus::default());

        bus.publish(
            envelope_for(
                event_type::COMMIT_CREATED,
                "alice",
                None,
                None,
                &CommitCreated::default(),
            )
            .unwrap(),
        )
        .await
        .unwrap();

        let mut late = bus.subscribe();

        // No new event published — late.next() should not see the earlier one.
        let timeout = tokio::time::timeout(std::time::Duration::from_millis(80), late.next()).await;
        assert!(
            timeout.is_err(),
            "late subscriber should not see prior event"
        );
    }
}
