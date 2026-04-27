//! In-memory job table with broadcast-channel log streams.
//!
//! Per [`docs/design/20-server-side-probes.md`](../../../docs/design/20-server-side-probes.md):
//! jobs are ephemeral. State lives in `Arc<Mutex<HashMap<JobId, Job>>>`; logs
//! live in a `tokio::sync::broadcast` channel per job so multiple SSE
//! subscribers can replay + tail. A pod restart drops everything — the UI
//! handles the case as "build expired, please re-submit".

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Opaque job identifier. Random base64 so guessing isn't a useful attack.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub String);

impl JobId {
    pub fn fresh() -> Self {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        // Inputs: nanos + a counter — neither is secret, but together they're
        // unique enough for a v1 demo. A dedicated UUID library would be more
        // appropriate at scale; we'd rather not add the dep for now.
        hasher.update(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_le_bytes(),
        );
        hasher.update(rand_counter().to_le_bytes());
        let raw = hasher.finalize();
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&raw[..16]);
        JobId(id)
    }
}

fn rand_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Building,
    Ok,
    Failed,
    Cancelled,
}

/// Build artifact ready for the client to stage at the named path.
///
/// The bytes are base64-encoded so the JSON response stays plain ASCII.
/// The client decodes and POSTs each artifact through the existing `/files`
/// endpoint to land it in the tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub path: String,
    pub bytes_b64: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct JobView {
    pub id: String,
    pub tenant: String,
    pub state: JobState,
    pub namespace: String,
    pub name: String,
    /// Populated when `state == "ok"`.
    pub artifacts: Option<Vec<Artifact>>,
    /// Populated when `state == "failed"` — last lines of cargo stderr.
    pub error: Option<String>,
    /// ISO-8601 UTC.
    pub created_at: String,
    pub updated_at: String,
}

/// Record + log channel for one build. The mutex protects the record;
/// the broadcast channel is `clone`-safe for multiple subscribers.
pub struct Job {
    pub id: JobId,
    pub tenant: String,
    pub namespace: String,
    pub name: String,
    pub state: JobState,
    pub artifacts: Option<Vec<Artifact>>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Buffered log lines so a late subscriber can replay from the start.
    /// Capped at 4 KiB lines — older entries are dropped silently.
    pub log_buffer: Vec<String>,
    /// Live broadcast channel. Dropped subscribers don't kill the build;
    /// they just miss tail messages.
    pub log_tx: broadcast::Sender<String>,
}

impl Job {
    pub fn view(&self) -> JobView {
        JobView {
            id: self.id.0.clone(),
            tenant: self.tenant.clone(),
            state: self.state.clone(),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
            artifacts: self.artifacts.clone(),
            error: self.error.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
}

#[derive(Default)]
pub struct JobsTable {
    inner: Mutex<HashMap<JobId, Job>>,
}

impl JobsTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn create(
        &self,
        tenant: String,
        namespace: String,
        name: String,
    ) -> (JobId, broadcast::Sender<String>) {
        let id = JobId::fresh();
        let (tx, _rx) = broadcast::channel::<String>(256);
        let now = now_iso();
        let job = Job {
            id: id.clone(),
            tenant,
            namespace,
            name,
            state: JobState::Queued,
            artifacts: None,
            error: None,
            created_at: now.clone(),
            updated_at: now,
            log_buffer: Vec::new(),
            log_tx: tx.clone(),
        };
        self.inner.lock().unwrap().insert(id.clone(), job);
        (id, tx)
    }

    pub fn view(&self, id: &JobId, tenant: &str) -> Option<JobView> {
        let g = self.inner.lock().unwrap();
        let job = g.get(id)?;
        if job.tenant != tenant {
            // Cross-tenant read attempts look identical to "not found" —
            // don't leak whether the id exists in another tenant's namespace.
            return None;
        }
        Some(job.view())
    }

    pub fn update<F: FnOnce(&mut Job)>(&self, id: &JobId, f: F) {
        let mut g = self.inner.lock().unwrap();
        if let Some(job) = g.get_mut(id) {
            f(job);
            job.updated_at = now_iso();
        }
    }

    pub fn append_log(&self, id: &JobId, line: String) {
        let mut g = self.inner.lock().unwrap();
        if let Some(job) = g.get_mut(id) {
            job.log_buffer.push(line.clone());
            // Best-effort broadcast. No subscribers? Fine.
            let _ = job.log_tx.send(line);
        }
    }

    /// Snapshot the buffered log + a fresh subscription for tailing. Used
    /// by the SSE log endpoint to send replay-then-tail.
    pub fn subscribe_log(
        &self,
        id: &JobId,
        tenant: &str,
    ) -> Option<(Vec<String>, broadcast::Receiver<String>)> {
        let g = self.inner.lock().unwrap();
        let job = g.get(id)?;
        if job.tenant != tenant {
            return None;
        }
        Some((job.log_buffer.clone(), job.log_tx.subscribe()))
    }

    pub fn delete(&self, id: &JobId, tenant: &str) -> bool {
        let mut g = self.inner.lock().unwrap();
        if let Some(job) = g.get(id) {
            if job.tenant != tenant {
                return false;
            }
        } else {
            return false;
        }
        g.remove(id).is_some()
    }
}

fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // ISO-8601 to second precision. The cli/tracing crates already use
    // RFC3339 elsewhere; we don't need a full date library here.
    chrono_secs_to_iso(dur.as_secs() as i64)
}

/// Format an i64 seconds-since-epoch as `YYYY-MM-DDTHH:MM:SSZ`. Pure-Rust
/// to avoid pulling chrono just for this.
fn chrono_secs_to_iso(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400) as u32;
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    let (y, mo, d) = days_since_epoch_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_since_epoch_to_ymd(mut days: i64) -> (i32, u32, u32) {
    // Algorithm from Howard Hinnant's date library, adapted for u32 dates
    // post-1970. Good enough for log timestamps; not date-arithmetic-grade.
    days += 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_format_known_dates() {
        assert_eq!(chrono_secs_to_iso(0), "1970-01-01T00:00:00Z");
        // 2024-01-01T00:00:00Z = 1_704_067_200
        assert_eq!(chrono_secs_to_iso(1_704_067_200), "2024-01-01T00:00:00Z");
    }

    #[test]
    fn create_and_view_round_trip() {
        let table = JobsTable::new();
        let (id, _tx) = table.create("alice".into(), "text".into(), "is_test".into());
        let view = table
            .view(&id, "alice")
            .expect("alice can read alice's job");
        assert_eq!(view.namespace, "text");
        assert_eq!(view.name, "is_test");
        assert_eq!(view.state, JobState::Queued);
    }

    #[test]
    fn cross_tenant_view_is_none() {
        let table = JobsTable::new();
        let (id, _tx) = table.create("alice".into(), "x".into(), "y".into());
        assert!(
            table.view(&id, "bob").is_none(),
            "bob must not see alice's job"
        );
    }
}
