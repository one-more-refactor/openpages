//! The panel's own build worker, plus the two ways a deployment gets started:
//! a push webhook, or polling when there is no inbound route to this machine.

use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Semaphore};

use crate::buildpipe::{self, BuildEnv};
use crate::db;
use crate::models::{BuildJob, BuildOutcome, DeployState, Deployment, Site};
use crate::state::Shared;

pub fn build_env(state: &Shared) -> BuildEnv {
    BuildEnv {
        work_dir: state.cfg.work_dir(),
        deployments_dir: state.cfg.deployments_dir(),
        default_image: state.cfg.build.default_image.clone(),
        max_artifact_mb: state.cfg.build.max_artifact_mb,
        report_budget: state.cfg.heal.report_budget_chars,
    }
}

/// Everything a node needs to run this deployment without a database.
pub async fn job_for(state: &Shared, d: &Deployment, site: &Site) -> Result<BuildJob> {
    let env = db::site_env(&state.db, site.id)
        .await?
        .into_iter()
        .map(|(k, v, _)| (k, v))
        .collect();

    Ok(BuildJob {
        deployment_id: d.id,
        site_slug: site.slug.clone(),
        repo: site.repo.clone(),
        branch: d.branch.clone(),
        commit_sha: d.commit_sha.clone(),
        kind: d.kind.clone(),
        site_kind: site.kind.clone(),
        root_dir: site.root_dir.clone(),
        install_cmd: site.install_cmd.clone(),
        build_cmd: site.build_cmd.clone(),
        output_dir: site.output_dir.clone(),
        build_image: site.build_image.clone(),
        runtime: if site.runtime == "auto" {
            state.cfg.build.runtime.clone()
        } else {
            site.runtime.clone()
        },
        env,
        timeout_secs: state.cfg.build.timeout_secs,
        clone_token: state.gh.token().await.ok(),
    })
}

pub async fn worker_loop(state: Shared) {
    let permits = Arc::new(Semaphore::new(state.cfg.build.max_concurrent.max(1)));
    loop {
        let permit = match permits.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return,
        };
        match db::claim_job(&state.db, "local").await {
            Ok(Some(d)) => {
                let state = state.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = run_one(&state, d).await {
                        tracing::error!("build: {e:#}");
                    }
                });
            }
            Ok(None) => {
                drop(permit);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                drop(permit);
                tracing::warn!("claiming a job: {e:#}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn run_one(state: &Shared, d: Deployment) -> Result<()> {
    let site = db::site(&state.db, d.site_id)
        .await?
        .context("site disappeared")?;
    let job = job_for(state, &d, &site).await?;

    let (tx, rx) = mpsc::channel::<(String, String)>(1024);
    let drain = tokio::spawn(drain_logs(state.db.clone(), d.id, rx));

    let outcome = buildpipe::run(&job, &build_env(state), tx).await;
    let _ = drain.await;

    apply_outcome(state, &site, &d, outcome).await
}

/// Records the result and, when the build worked, runs the proofing step that
/// decides whether it is allowed to become a rollback-worthy deployment.
pub async fn apply_outcome(
    state: &Shared,
    site: &Site,
    d: &Deployment,
    outcome: BuildOutcome,
) -> Result<()> {
    db::finish_deployment(&state.db, &outcome).await?;

    if !outcome.ok {
        let summary = outcome
            .error_summary
            .clone()
            .unwrap_or_else(|| "build failed".into());
        tracing::warn!(site = %site.slug, deployment = d.id, "pied: {summary}");
        if let Some(report) = outcome.incident.clone() {
            let state = state.clone();
            let (site_id, dep_id) = (site.id, d.id);
            let (repo, branch, sha, mode) = (
                site.repo.clone(),
                site.branch.clone(),
                d.commit_sha.clone(),
                site.heal_mode.clone(),
            );
            tokio::spawn(async move {
                crate::heal::on_build_failure(
                    state, site_id, dep_id, repo, branch, sha, report, summary, mode,
                )
                .await;
            });
        }
        return Ok(());
    }

    if let Some(framework) = outcome.framework.as_deref() {
        let _ = sqlx::query("UPDATE sites SET framework = ?, updated_at = ? WHERE id = ?")
            .bind(framework)
            .bind(crate::util::now())
            .bind(site.id)
            .execute(&state.db)
            .await;
    }

    match proof(state, d.id).await {
        Ok(()) => {
            db::mark_ready(&state.db, d.id).await?;
            state.invalidate_routes().await;
            tracing::info!(site = %site.slug, deployment = d.id, "proofed and standing by");
        }
        Err(e) => {
            let msg = format!("{e:#}");
            sqlx::query("UPDATE deployments SET state='failed', error_summary=? WHERE id=?")
                .bind(&msg)
                .bind(d.id)
                .execute(&state.db)
                .await?;
            db::append_logs(
                &state.db,
                d.id,
                &[(
                    "stderr".into(),
                    format!("openpages: proofing failed — {msg}"),
                )],
            )
            .await?;
            tracing::warn!(site = %site.slug, "proofing failed: {msg}");
        }
    }
    Ok(())
}

/// Proofing is the last check before a deployment is allowed to be promoted:
/// the artifact is still there and still serves a page, or the container is
/// still answering. Cheap, and it catches the case where a build succeeded but
/// something ate the result between then and now.
async fn proof(state: &Shared, deployment_id: i64) -> Result<()> {
    let d = db::deployment(&state.db, deployment_id)
        .await?
        .context("deployment vanished")?;

    if let Some(port) = d.container_port {
        buildpipe::wait_healthy(port as u16, 30).await?;
        return Ok(());
    }

    let dir = std::path::PathBuf::from(d.artifact_path.context("deployment has no artifact")?);
    if !dir.is_dir() {
        anyhow::bail!("the built site is no longer on disk");
    }
    let (files, _) = buildpipe::measure(&dir);
    if files == 0 {
        anyhow::bail!("the built site is empty");
    }
    let rules = crate::edge::Rules::load(&dir);
    let servable = dir.join("index.html").is_file()
        || dir.join("200.html").is_file()
        || rules.redirect_for("/").is_some();
    if !servable {
        anyhow::bail!("nothing answers `/` — no index.html and no matching rule in _redirects");
    }
    Ok(())
}

/// Batches log lines into the database. Writing one row per line as it arrives
/// turns a chatty build into thousands of transactions.
async fn drain_logs(db: db::Db, deployment_id: i64, mut rx: mpsc::Receiver<(String, String)>) {
    let mut batch: Vec<(String, String)> = Vec::with_capacity(128);
    let mut ticker = tokio::time::interval(Duration::from_millis(400));
    loop {
        tokio::select! {
            got = rx.recv() => match got {
                Some(line) => {
                    batch.push(line);
                    if batch.len() >= 256 {
                        let _ = db::append_logs(&db, deployment_id, &std::mem::take(&mut batch)).await;
                    }
                }
                None => {
                    if !batch.is_empty() {
                        let _ = db::append_logs(&db, deployment_id, &batch).await;
                    }
                    return;
                }
            },
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    let _ = db::append_logs(&db, deployment_id, &std::mem::take(&mut batch)).await;
                }
            }
        }
    }
}

// ── triggering ──────────────────────────────────────────────────────────────

pub async fn deploy(
    state: &Shared,
    site: &Site,
    sha: &str,
    msg: Option<&str>,
    branch: &str,
    kind: &str,
) -> Result<Deployment> {
    // A queued or building deployment for the same commit is the same work.
    let existing = db::deployments_for(&state.db, site.id, 20).await?;
    if let Some(d) = existing.iter().find(|d| {
        d.commit_sha == sha && matches!(d.state(), DeployState::Queued | DeployState::Building)
    }) {
        return Ok(d.clone());
    }
    let d = db::create_deployment(&state.db, site, sha, msg, branch, kind).await?;
    tracing::info!(site = %site.slug, "queued deployment {} for {}", d.id, crate::config::short_sha(sha));
    Ok(d)
}

/// For installs with no inbound route — the common self-hosted case. A webhook
/// is nicer when it is available; this always works.
pub async fn poller(state: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if let Err(e) = poll_once(&state).await {
            tracing::warn!("poller: {e:#}");
        }
    }
}

async fn poll_once(state: &Shared) -> Result<()> {
    for site in db::list_sites(&state.db).await? {
        if site.webhook_id.is_some() {
            continue;
        }
        let due = site.last_polled_sha.is_none()
            || chrono::Utc::now().timestamp() % site.poll_interval_secs.max(30) < 30;
        if !due {
            continue;
        }
        let head = match state.gh.head_commit(&site.repo, &site.branch).await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(site = %site.slug, "poll: {e:#}");
                continue;
            }
        };
        if site.last_polled_sha.as_deref() == Some(head.sha.as_str()) {
            continue;
        }
        sqlx::query("UPDATE sites SET last_polled_sha = ? WHERE id = ?")
            .bind(&head.sha)
            .bind(site.id)
            .execute(&state.db)
            .await?;

        // The first poll records where the branch is without deploying it —
        // adding a site should not immediately rebuild something already live.
        if site.last_polled_sha.is_none()
            && db::live_deployment(&state.db, site.id).await?.is_some()
        {
            continue;
        }
        deploy(
            state,
            &site,
            &head.sha,
            Some(&head.message),
            &site.branch,
            "production",
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn build_env_follows_the_configured_data_dir() {
        let mut cfg = crate::config::Config::default();
        cfg.server.data_dir = "/srv/op".into();
        assert_eq!(
            cfg.deployments_dir(),
            std::path::PathBuf::from("/srv/op/deployments")
        );
        assert_eq!(cfg.work_dir(), std::path::PathBuf::from("/srv/op/work"));
    }
}
