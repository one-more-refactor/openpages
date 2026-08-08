use anyhow::Result;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::config::Config;
use crate::db::Db;
use crate::gh::Gh;
use crate::runtime::Runtime;
use crate::util::{minute_bucket, random_token};

pub type Shared = Arc<AppState>;

pub struct AppState {
    pub db: Db,
    pub cfg: Arc<Config>,
    pub gh: Gh,
    pub rt: Runtime,
    pub traffic: TrafficMeter,
    pub routes: RwLock<HashMap<String, CachedRoute>>,
    pub webhook_secret: String,
    /// Why the container runtime is unusable, if it is. openpages still starts:
    /// a panel that crash-loops is a panel you cannot open to find out why.
    pub runtime_error: Option<String>,
    pub started: Instant,
    /// Set once a new version has been staged and verified, so the panel can
    /// say so and the swap scheduler knows a restart is pending.
    pub pending_update: Mutex<Option<String>>,
}

#[derive(Clone)]
pub struct CachedRoute {
    pub at: Instant,
    pub route: Option<crate::edge::Route>,
}

impl AppState {
    pub async fn new(
        cfg: Config,
        rt: Runtime,
        runtime_error: Option<String>,
        db: Db,
    ) -> Result<Shared> {
        let secret = match crate::db::setting(&db, "webhook_secret").await? {
            Some(s) => s,
            None => {
                let s = random_token(24);
                crate::db::set_setting(&db, "webhook_secret", &s).await?;
                s
            }
        };
        let gh = Gh::new(cfg.github.gh_bin.clone());
        Ok(Arc::new(AppState {
            db,
            gh,
            rt,
            cfg: Arc::new(cfg),
            traffic: TrafficMeter::default(),
            routes: RwLock::new(HashMap::new()),
            webhook_secret: secret,
            runtime_error,
            started: Instant::now(),
            pending_update: Mutex::new(None),
        }))
    }

    /// Called whenever what a hostname points at may have changed. Cheaper and
    /// far less error-prone than trying to evict the exact affected keys.
    pub async fn invalidate_routes(&self) {
        self.routes.write().await.clear();
    }
}

/// Per-site request counting, at minute resolution, plus a live in-flight
/// gauge. The first feeds the swap scheduler's idea of "quiet"; the second is
/// what lets a drain finish instead of guessing.
#[derive(Default)]
pub struct TrafficMeter {
    pending: Mutex<HashMap<(i64, String), i64>>,
    inflight: Mutex<HashMap<i64, Arc<AtomicI64>>>,
}

impl TrafficMeter {
    pub fn hit(&self, site_id: i64) {
        let bucket = minute_bucket(chrono::Utc::now());
        *self
            .pending
            .lock()
            .unwrap()
            .entry((site_id, bucket))
            .or_insert(0) += 1;
    }

    pub fn take(&self) -> Vec<(i64, String, i64)> {
        let mut p = self.pending.lock().unwrap();
        p.drain().map(|((s, b), n)| (s, b, n)).collect()
    }

    fn gauge(&self, site_id: i64) -> Arc<AtomicI64> {
        self.inflight
            .lock()
            .unwrap()
            .entry(site_id)
            .or_insert_with(|| Arc::new(AtomicI64::new(0)))
            .clone()
    }

    pub fn inflight(&self, site_id: i64) -> i64 {
        self.gauge(site_id).load(Ordering::Relaxed)
    }

    /// RAII: the counter goes back down even if the handler panics or the
    /// client disconnects mid-response.
    pub fn enter(&self, site_id: i64) -> InFlight {
        let g = self.gauge(site_id);
        g.fetch_add(1, Ordering::Relaxed);
        InFlight(g)
    }
}

pub struct InFlight(Arc<AtomicI64>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub const ROUTE_TTL: Duration = Duration::from_secs(3);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traffic_accumulates_and_drains_once() {
        let m = TrafficMeter::default();
        m.hit(1);
        m.hit(1);
        m.hit(2);
        let taken = m.take();
        let total: i64 = taken.iter().map(|(_, _, n)| n).sum();
        assert_eq!(total, 3);
        assert!(
            m.take().is_empty(),
            "a drain must not hand out the same counts twice"
        );
    }

    #[test]
    fn inflight_returns_to_zero_when_the_guard_drops() {
        let m = TrafficMeter::default();
        assert_eq!(m.inflight(7), 0);
        {
            let _a = m.enter(7);
            let _b = m.enter(7);
            assert_eq!(m.inflight(7), 2);
        }
        assert_eq!(m.inflight(7), 0);
    }
}
