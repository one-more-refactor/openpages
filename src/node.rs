//! A build node: a machine that is not the panel.
//!
//! It dials out and never listens for control traffic, so a node behind NAT,
//! on a laptop, or on a different continent works with no port forwarding and
//! no VPN. What it *does* listen for is edge traffic, and only from the panel:
//! the panel proxies requests to it with a header saying exactly which
//! deployment to serve, because a node has no database to look that up in.

use anyhow::{bail, Context, Result};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::buildpipe::{self, BuildEnv};
use crate::config::Config;
use crate::edge;
use crate::models::{BuildJob, BuildOutcome};
use crate::runtime::Runtime;

#[derive(Clone)]
pub struct Agent {
    pub panel: String,
    pub token: String,
    pub cfg: Arc<Config>,
    pub rt: Runtime,
    pub http: reqwest::Client,
}

pub async fn run(cfg: Config) -> Result<()> {
    let panel = cfg
        .node
        .panel_url
        .clone()
        .context("no panel URL — pass --panel or set node.panel_url")?;
    let token = cfg
        .node
        .token
        .clone()
        .context("no node token — pass --token, or mint one in the panel under Nodes")?;

    cfg.ensure_dirs()?;
    let rt = Runtime::detect(&cfg.build.runtime).await?;
    let edge_listen = cfg.node.edge_listen.clone();

    let agent = Agent {
        panel: panel.trim_end_matches('/').to_string(),
        token,
        rt,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(40))
            .build()?,
        cfg: Arc::new(cfg),
    };

    tracing::info!(
        "node starting: panel {} · runtime {}",
        agent.panel,
        rt.name()
    );

    let serving = agent.clone();
    tokio::spawn(async move {
        if let Err(e) = serve_edge(serving, &edge_listen).await {
            tracing::error!("node edge: {e:#}");
        }
    });

    let beating = agent.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = beating.heartbeat().await {
                tracing::warn!("heartbeat: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    agent.work_loop().await
}

impl Agent {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.panel)
    }

    async fn heartbeat(&self) -> Result<()> {
        let body = serde_json::json!({
            "runtime": self.rt.name(),
            "version": crate::update::version(),
            "os": std::env::consts::OS,
            "edge_url": self.cfg.node.edge_url,
        });
        let r = self
            .http
            .post(self.url("/api/node/heartbeat"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        if r.status() == StatusCode::UNAUTHORIZED {
            bail!("the panel does not recognise this node token");
        }
        r.error_for_status()?;
        Ok(())
    }

    async fn work_loop(&self) -> Result<()> {
        let mut backoff = 2u64;
        loop {
            match self.next_job().await {
                Ok(Some(job)) => {
                    backoff = 2;
                    if let Err(e) = self.run_job(job).await {
                        tracing::error!("job: {e:#}");
                    }
                }
                Ok(None) => backoff = 2,
                Err(e) => {
                    tracing::warn!("polling for work: {e:#}");
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    }

    async fn next_job(&self) -> Result<Option<BuildJob>> {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            job: Option<BuildJob>,
        }
        let r = self
            .http
            .get(self.url("/api/node/job"))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(40))
            .send()
            .await?
            .error_for_status()?;
        Ok(r.json::<Wrapper>().await?.job)
    }

    async fn run_job(&self, job: BuildJob) -> Result<()> {
        tracing::info!("building {} for deployment {}", job.repo, job.deployment_id);
        let (tx, rx) = mpsc::channel::<(String, String)>(1024);
        let shipping = self.clone();
        let id = job.deployment_id;
        let shipper = tokio::spawn(async move { shipping.ship_logs(id, rx).await });

        let env = BuildEnv {
            work_dir: self.cfg.work_dir(),
            deployments_dir: self.cfg.deployments_dir(),
            default_image: self.cfg.build.default_image.clone(),
            max_artifact_mb: self.cfg.build.max_artifact_mb,
            report_budget: self.cfg.heal.report_budget_chars,
        };
        let outcome = buildpipe::run(&job, &env, tx).await;
        let _ = shipper.await;

        self.report(&outcome).await
    }

    /// Batched, and never fatal: losing log lines must not lose the build.
    async fn ship_logs(&self, deployment_id: i64, mut rx: mpsc::Receiver<(String, String)>) {
        let mut batch: Vec<(String, String)> = Vec::new();
        let mut ticker = tokio::time::interval(Duration::from_millis(700));
        loop {
            let flush = tokio::select! {
                got = rx.recv() => match got {
                    Some(l) => { batch.push(l); batch.len() >= 200 }
                    None => { self.post_logs(deployment_id, &batch).await; return; }
                },
                _ = ticker.tick() => true,
            };
            if flush && !batch.is_empty() {
                self.post_logs(deployment_id, &std::mem::take(&mut batch))
                    .await;
            }
        }
    }

    async fn post_logs(&self, deployment_id: i64, lines: &[(String, String)]) {
        if lines.is_empty() {
            return;
        }
        let body = serde_json::json!({ "deployment_id": deployment_id, "lines": lines });
        if let Err(e) = self
            .http
            .post(self.url("/api/node/logs"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
        {
            tracing::debug!("shipping logs: {e}");
        }
    }

    /// Retried, because this one *is* fatal to lose: without it the panel has a
    /// deployment stuck in `building` forever.
    async fn report(&self, outcome: &BuildOutcome) -> Result<()> {
        let mut wait = 2u64;
        for attempt in 1..=6 {
            match self
                .http
                .post(self.url("/api/node/result"))
                .bearer_auth(&self.token)
                .json(outcome)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => return Ok(()),
                Ok(r) => {
                    tracing::warn!("reporting result (attempt {attempt}): HTTP {}", r.status())
                }
                Err(e) => tracing::warn!("reporting result (attempt {attempt}): {e}"),
            }
            tokio::time::sleep(Duration::from_secs(wait)).await;
            wait = (wait * 2).min(60);
        }
        bail!(
            "could not report the result of deployment {} to the panel",
            outcome.deployment_id
        )
    }
}

// ── the node's edge ─────────────────────────────────────────────────────────

async fn serve_edge(agent: Agent, listen: &str) -> Result<()> {
    let app = Router::new()
        .fallback(get(serve).post(serve).put(serve).patch(serve).delete(serve))
        .with_state(agent);
    let l = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!("node edge on {listen}");
    axum::serve(l, app).await?;
    Ok(())
}

async fn serve(State(agent): State<Agent>, req: Request) -> Response {
    let Some(spec) = edge::header_str(
        req.headers(),
        axum::http::HeaderName::from_static(edge::TARGET_HEADER),
    ) else {
        return (
            StatusCode::BAD_REQUEST,
            "this is an openpages node edge; it only answers requests proxied by a panel\n",
        )
            .into_response();
    };

    match spec.split_once(':') {
        Some(("container", port)) => match port.parse::<u16>() {
            Ok(p) => edge::proxy(&format!("http://127.0.0.1:{p}"), req, None).await,
            Err(_) => (StatusCode::BAD_REQUEST, "bad container target").into_response(),
        },
        Some(("static", rel)) => {
            let Some(safe) = edge::safe_join(&format!("/{rel}")) else {
                return (StatusCode::BAD_REQUEST, "bad artifact path").into_response();
            };
            let dir: PathBuf = agent.cfg.deployments_dir().join(safe);
            if !dir.is_dir() {
                return (StatusCode::NOT_FOUND, "this node has no such deployment\n")
                    .into_response();
            }
            let rules = edge::Rules::load(&dir);
            edge::serve_static(&dir, &rules, req).await
        }
        _ => (StatusCode::BAD_REQUEST, "unrecognised target").into_response(),
    }
}
