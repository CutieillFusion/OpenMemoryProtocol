//! Prometheus metrics for the OMP HTTP server.
//!
//! See `docs/design/18-observability.md`. The exporter exposes a snapshot at
//! `GET /metrics`; per-request counters/histograms are populated by the
//! `record_request` middleware.
//!
//! Metric set (small on purpose — see doc 18):
//!   omp_request_total{service, op, tenant, status}    counter
//!   omp_request_duration_seconds{service, op, tenant} histogram
//!   omp_request_in_flight{service, op}                gauge

use std::sync::OnceLock;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

const SERVICE: &str = "omp-server";
const UNKNOWN_ROUTE: &str = "_unmatched";
const UNKNOWN_TENANT: &str = "_unknown";

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Initialize the Prometheus recorder and register a handle. Idempotent.
///
/// Uses `build_recorder()` rather than `build()` so we DON'T spin up an
/// HTTP listener on the default 0.0.0.0:9000 port. Two shards running on
/// the same host would otherwise race on that port at startup (the
/// previous code dropped the exporter future immediately, but the listener
/// teardown isn't synchronous, so a second shard launching milliseconds
/// later would hit "Address already in use" and panic). The
/// `/metrics` HTTP route on the gateway-fronted shard already serves the
/// snapshot via `render()`, so the standalone listener was dead weight.
pub fn init() {
    HANDLE.get_or_init(|| {
        // Same buckets as in doc 18: 5ms .. 60s, 7 entries.
        let buckets = [0.005, 0.025, 0.1, 0.5, 2.0, 10.0, 60.0];
        let builder = PrometheusBuilder::new()
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Suffix("_duration_seconds".into()),
                &buckets,
            )
            .expect("set buckets");
        let recorder = builder.build_recorder();
        let handle = recorder.handle();
        metrics::set_global_recorder(recorder).ok();
        handle
    });
}

/// Render the current snapshot. Used by the `/metrics` route.
pub fn render() -> String {
    HANDLE
        .get()
        .map(|h| h.render())
        .unwrap_or_else(|| "# metrics not initialized\n".into())
}

/// Tower middleware that records per-request counters and durations.
///
/// Uses the matched router pattern (e.g. `/files/*path`) rather than the raw
/// URI to keep Prometheus label cardinality bounded.
pub async fn record_request(req: Request<Body>, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str())
        .unwrap_or(UNKNOWN_ROUTE)
        .to_string();
    let method = req.method().as_str().to_string();
    let op = format!("{method} {route}");

    gauge!("omp_request_in_flight",
        "service" => SERVICE,
        "op" => op.clone()
    )
    .increment(1.0);

    let start = Instant::now();
    let resp = next.run(req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let status = resp.status().as_u16().to_string();

    counter!("omp_request_total",
        "service" => SERVICE,
        "op" => op.clone(),
        "tenant" => UNKNOWN_TENANT,
        "status" => status,
    )
    .increment(1);

    histogram!("omp_request_duration_seconds",
        "service" => SERVICE,
        "op" => op.clone(),
        "tenant" => UNKNOWN_TENANT,
    )
    .record(elapsed);

    gauge!("omp_request_in_flight",
        "service" => SERVICE,
        "op" => op,
    )
    .decrement(1.0);

    resp
}
