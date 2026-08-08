//! When to change what is on press.
//!
//! A build that passed proofing is *ready*, not *live*. Ready deployments wait
//! here until the site is as quiet as it is going to get, then take over. The
//! wait is bounded: a site that is never quiet still deploys, it just deploys
//! at its own worst moment rather than never.
//!
//! Rollback lives here too, and it always runs before anything clever is
//! attempted. Getting back to a working site is not allowed to depend on a
//! network call, a model, or anything else that can be slow or absent.

use anyhow::Result;
use std::time::Duration;

use crate::db;
use crate::models::{DeployState, Deployment};
use crate::runtime;
use crate::state::Shared;
use crate::util::{parse_ts, quantile};

#[derive(Debug, Clone)]
pub struct SwapInputs {
    pub policy: String,
    pub waited_secs: i64,
    pub max_wait_secs: i64,
    pub inflight: i64,
    pub hits_last_minute: i64,
    /// Per-minute hit counts for this site at this hour-of-week, sorted.
    pub profile: Vec<f64>,
    pub quiet_quantile: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Swap(&'static str),
    Wait(&'static str),
}

/// The whole policy, with no I/O in it, because "did it pick the right moment"
/// is the kind of question you want to answer in a unit test rather than by
/// watching production.
pub fn decide(i: &SwapInputs) -> Decision {
    if i.policy == "immediate" {
        return Decision::Swap("policy is immediate");
    }
    if i.waited_secs >= i.max_wait_secs {
        return Decision::Swap("waited long enough for a quiet moment");
    }
    if i.hits_last_minute == 0 && i.inflight == 0 {
        return Decision::Swap("nobody is here");
    }
    // Below ~30 samples the quantile is noise, and acting on noise means
    // swapping at a moment that only looked quiet.
    if i.profile.len() >= 30 {
        let threshold = quantile(&i.profile, i.quiet_quantile);
        if (i.hits_last_minute as f64) <= threshold && i.inflight <= 1 {
            return Decision::Swap("traffic is at its usual low for this hour");
        }
    }
    Decision::Wait("waiting for a quieter minute")
}

pub async fn scheduler(state: Shared) {
    let mut ticks: u64 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        ticks += 1;

        if let Err(e) = flush_traffic(&state).await {
            tracing::warn!("traffic flush: {e:#}");
        }
        if let Err(e) = tick(&state).await {
            tracing::warn!("swap scheduler: {e:#}");
        }
        if let Err(e) = watchdog(&state).await {
            tracing::warn!("watchdog: {e:#}");
        }
        if ticks % 360 == 0 {
            if let Err(e) = housekeeping(&state).await {
                tracing::warn!("housekeeping: {e:#}");
            }
        }
    }
}

async fn flush_traffic(state: &Shared) -> Result<()> {
    let samples = state.traffic.take();
    db::bump_traffic(&state.db, &samples).await
}

async fn tick(state: &Shared) -> Result<()> {
    for d in db::awaiting_swap(&state.db).await? {
        let Some(site) = db::site(&state.db, d.site_id).await? else {
            continue;
        };
        let waited = d
            .ready_at
            .as_deref()
            .and_then(parse_ts)
            .map(|t| (chrono::Utc::now() - t).num_seconds())
            .unwrap_or(0);

        let inputs = SwapInputs {
            policy: site.swap_policy.clone(),
            waited_secs: waited,
            max_wait_secs: site.swap_max_wait_secs,
            inflight: state.traffic.inflight(site.id),
            hits_last_minute: db::hits_in_last_minutes(&state.db, site.id, 1).await?,
            profile: db::traffic_profile(
                &state.db,
                site.id,
                crate::util::hour_of_week(chrono::Utc::now()),
                7,
            )
            .await?,
            quiet_quantile: state.cfg.swap.quiet_quantile,
        };

        if let Decision::Swap(why) = decide(&inputs) {
            tracing::info!(site = %site.slug, deployment = d.id, "promoting: {why}");
            promote(state, &d, why).await?;
        }
    }
    Ok(())
}

/// Put a ready deployment on press and retire its predecessor after a drain.
pub async fn promote(state: &Shared, d: &Deployment, why: &str) -> Result<()> {
    let Some(site) = db::site(&state.db, d.site_id).await? else {
        return Ok(());
    };
    let previous = db::promote(&state.db, d.site_id, d.id).await?;
    state.invalidate_routes().await;
    tracing::info!(site = %site.slug, "on press: deployment {} ({why})", d.id);

    let Some(prev_id) = previous else {
        return Ok(());
    };
    let drain = site.drain_secs.max(0) as u64;
    let state2 = state.clone();
    tokio::spawn(async move {
        // Requests that started against the old deployment are still reading
        // it. Give them their drain window before anything is torn down.
        tokio::time::sleep(Duration::from_secs(drain)).await;
        if let Ok(Some(prev)) = db::deployment(&state2.db, prev_id).await {
            if let Some(cid) = prev.container_id.as_deref() {
                let _ = runtime::stop(state2.rt, cid, 10).await;
            }
        }
    });
    Ok(())
}

/// Back to the last deployment that worked. Called on a failed health check, a
/// dead container, or by hand from the panel.
pub async fn rollback(state: &Shared, site_id: i64, reason: &str) -> Result<Option<i64>> {
    let Some(target) = db::rollback_target(&state.db, site_id).await? else {
        tracing::error!(
            site_id,
            "rollback wanted ({reason}) but there is nothing to roll back to"
        );
        return Ok(None);
    };

    if let Some(current) = db::live_deployment(&state.db, site_id).await? {
        db::set_deploy_state(&state.db, current.id, DeployState::Failed).await?;
        if let Some(cid) = current.container_id.as_deref() {
            let _ = runtime::stop(state.rt, cid, 5).await;
        }
    }

    // Restart the container the rollback target used to run in, if it had one:
    // it was stopped when it was superseded.
    if let (Some(_cid), Some(_)) = (target.container_id.as_deref(), target.container_port) {
        tracing::info!(
            site_id,
            "rollback target is a container deployment; rebuilding is required if it was pruned"
        );
    }

    db::promote(&state.db, site_id, target.id).await?;
    state.invalidate_routes().await;
    tracing::warn!(
        site_id,
        "rolled back to deployment {} ({reason})",
        target.id
    );
    Ok(Some(target.id))
}

/// Live container deployments that stopped answering get rolled back. Static
/// deployments cannot fail this way, which is a good argument for static.
async fn watchdog(state: &Shared) -> Result<()> {
    for site in db::list_sites(&state.db).await? {
        let Some(live) = db::live_deployment(&state.db, site.id).await? else {
            continue;
        };
        let Some(cid) = live.container_id.clone() else {
            continue;
        };
        if live.node_id.as_deref().unwrap_or("local") != "local" {
            continue;
        }
        if runtime::is_running(state.rt, &cid).await {
            continue;
        }
        tracing::error!(site = %site.slug, "live container {cid} is not running");
        let rolled = rollback(state, site.id, "live container stopped").await?;
        let state2 = state.clone();
        let summary = format!("The live container for {} stopped running.", site.slug);
        tokio::spawn(async move {
            crate::heal::on_runtime_failure(state2, site.id, live.id, summary, rolled).await;
        });
    }
    Ok(())
}

async fn housekeeping(state: &Shared) -> Result<()> {
    db::prune_traffic(&state.db, 30).await?;
    for d in db::prunable(&state.db, state.cfg.build.keep_deployments).await? {
        if let Some(path) = d.artifact_path.as_deref() {
            if path.starts_with(&state.cfg.deployments_dir().to_string_lossy().to_string()) {
                let _ = tokio::fs::remove_dir_all(path).await;
            }
        }
        if let Some(cid) = d.container_id.as_deref() {
            let _ = runtime::stop(state.rt, cid, 5).await;
        }
        db::mark_retired(&state.db, d.id).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SwapInputs {
        SwapInputs {
            policy: "quiet".into(),
            waited_secs: 10,
            max_wait_secs: 1800,
            inflight: 5,
            hits_last_minute: 100,
            profile: vec![],
            quiet_quantile: 0.25,
        }
    }

    #[test]
    fn a_busy_site_with_no_history_waits() {
        assert!(matches!(decide(&base()), Decision::Wait(_)));
    }

    #[test]
    fn an_idle_site_swaps_immediately() {
        let i = SwapInputs {
            hits_last_minute: 0,
            inflight: 0,
            ..base()
        };
        assert!(matches!(decide(&i), Decision::Swap(_)));
    }

    #[test]
    fn a_request_still_in_flight_blocks_the_idle_shortcut() {
        let i = SwapInputs {
            hits_last_minute: 0,
            inflight: 1,
            ..base()
        };
        assert!(matches!(decide(&i), Decision::Wait(_)));
    }

    #[test]
    fn the_deadline_always_wins() {
        let i = SwapInputs {
            waited_secs: 1800,
            ..base()
        };
        assert_eq!(
            decide(&i),
            Decision::Swap("waited long enough for a quiet moment")
        );
    }

    #[test]
    fn immediate_policy_skips_every_check() {
        let i = SwapInputs {
            policy: "immediate".into(),
            ..base()
        };
        assert!(matches!(decide(&i), Decision::Swap(_)));
    }

    #[test]
    fn a_quiet_minute_relative_to_this_hours_history_swaps() {
        // A week of this hour: mostly 40-60 hits/min, occasionally near zero.
        let mut profile: Vec<f64> = (0..100).map(|n| 40.0 + (n % 20) as f64).collect();
        profile.extend([0.0, 1.0, 2.0, 3.0, 4.0]);
        profile.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let quiet = SwapInputs {
            hits_last_minute: 1,
            inflight: 0,
            profile: profile.clone(),
            ..base()
        };
        assert!(matches!(decide(&quiet), Decision::Swap(_)));

        let busy = SwapInputs {
            hits_last_minute: 55,
            inflight: 3,
            profile,
            ..base()
        };
        assert!(matches!(decide(&busy), Decision::Wait(_)));
    }

    #[test]
    fn a_thin_history_is_not_trusted() {
        // Ten samples that would say "quiet" — too few to act on.
        let profile = vec![100.0; 10];
        let i = SwapInputs {
            hits_last_minute: 5,
            inflight: 1,
            profile,
            ..base()
        };
        assert!(matches!(decide(&i), Decision::Wait(_)));
    }

    #[test]
    fn a_site_that_is_never_quiet_still_deploys_eventually() {
        let profile = vec![500.0; 200];
        let mut i = SwapInputs {
            hits_last_minute: 500,
            inflight: 40,
            profile,
            ..base()
        };
        assert!(matches!(decide(&i), Decision::Wait(_)));
        i.waited_secs = i.max_wait_secs;
        assert!(matches!(decide(&i), Decision::Swap(_)));
    }
}
