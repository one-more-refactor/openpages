//! The panel API, the GitHub webhook, and the endpoints remote nodes call.
//!
//! Host routing happens before anything else: if the Host header names a site,
//! the request goes to the edge and never sees a single API route. That is why
//! a published site cannot reach `/api/sites` no matter what it asks for.

use anyhow::Result;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::time::Duration;

use crate::db;
use crate::edge::{self, Host};
use crate::models::{DeployState, User};
use crate::state::Shared;
use crate::{swap, update, worker};

const COOKIE: &str = "op_session";

pub fn router(state: Shared) -> Router {
    let protected = Router::new()
        .route("/overview", get(overview))
        .route("/repos", get(repos))
        .route("/sites", get(list_sites).post(create_site))
        .route(
            "/sites/{id}",
            get(get_site).patch(update_site).delete(remove_site),
        )
        .route("/sites/{id}/env", get(get_env).put(put_env))
        .route("/sites/{id}/env/{key}", delete(del_env))
        .route("/sites/{id}/deploy", post(deploy_site))
        .route("/sites/{id}/deployments", get(site_deployments))
        .route("/sites/{id}/traffic", get(site_traffic))
        .route("/sites/{id}/rollback", post(rollback_site))
        .route("/deployments/{id}", get(get_deployment))
        .route("/deployments/{id}/logs", get(get_logs))
        .route("/deployments/{id}/log.txt", get(get_full_log))
        .route("/deployments/{id}/stream", get(stream_logs))
        .route("/deployments/{id}/promote", post(promote_now))
        .route("/nodes", get(list_nodes).post(add_node))
        .route("/heals", get(list_heals))
        .route("/system", get(system))
        .route("/system/update/check", post(update_check))
        .route("/system/update/apply", post(update_apply))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));

    let open = Router::new()
        .route("/version", get(version))
        .route("/auth/claim", post(claim))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
        .route("/hooks/github", post(webhook))
        .route("/node/job", get(node_job))
        .route("/node/logs", post(node_logs))
        .route("/node/result", post(node_result))
        .route("/node/heartbeat", post(node_heartbeat));

    Router::new()
        .route("/healthz", get(healthz))
        .nest("/api", open.merge(protected))
        .fallback(panel)
        .layer(middleware::from_fn_with_state(state.clone(), host_gate))
        .with_state(state)
}

/// The first thing every request meets. A site's hostname never reaches the
/// panel router at all.
async fn host_gate(State(state): State<Shared>, req: Request, next: Next) -> Response {
    let host = edge::header_str(req.headers(), header::HOST).unwrap_or_default();
    match edge::classify(
        &host,
        &state.cfg.server.base_domain,
        state.cfg.server.panel_host.as_deref(),
    ) {
        Host::Site { .. } => edge::handle(State(state), req).await,
        _ => {
            if !req.method().is_safe() && cross_origin(req.headers()) {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "cross-origin request refused" })),
                )
                    .into_response();
            }
            let mut resp = next.run(req).await;
            // Every hosted site is usually same-site with the panel, so it
            // could frame it and steer clicks on Delete or Promote.
            let h = resp.headers_mut();
            h.insert(
                header::X_FRAME_OPTIONS,
                header::HeaderValue::from_static("DENY"),
            );
            h.insert(
                header::CONTENT_SECURITY_POLICY,
                header::HeaderValue::from_static("frame-ancestors 'none'"),
            );
            resp
        }
    }
}

/// A browser request that a page on another origin made. Published sites sit
/// on sibling subdomains of the panel, so `SameSite=Lax` lets their requests
/// carry the session cookie; bodyless POSTs (rollback, promote, update) need
/// no preflight. Browsers always send `Origin` on a POST, so comparing it with
/// `Host` is enough; webhooks and nodes send no `Origin` and are unaffected.
fn cross_origin(headers: &HeaderMap) -> bool {
    if let Some(site) = edge::header_str(headers, header::HeaderName::from_static("sec-fetch-site"))
    {
        if site != "same-origin" && site != "none" {
            return true;
        }
    }
    let Some(origin) = edge::header_str(headers, header::ORIGIN) else {
        return false;
    };
    let host = edge::header_str(headers, header::HOST).unwrap_or_default();
    let origin_host = origin
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(&origin)
        .trim_end_matches('/');
    !origin_host.eq_ignore_ascii_case(&host)
}

// ── errors ──────────────────────────────────────────────────────────────────

pub struct Fail(StatusCode, String);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for Fail {
    fn from(e: anyhow::Error) -> Self {
        tracing::warn!("api: {e:#}");
        Fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}
impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Self {
        Fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

fn bad(msg: impl Into<String>) -> Fail {
    Fail(StatusCode::BAD_REQUEST, msg.into())
}
fn missing(what: &str) -> Fail {
    Fail(StatusCode::NOT_FOUND, format!("no such {what}"))
}

type ApiResult<T> = std::result::Result<T, Fail>;

// ── auth ────────────────────────────────────────────────────────────────────

/// One-time codes are minted by `openpages invite` on the server. There is no
/// password to guess and no OAuth app to register; proving you can run a
/// command on the box is the proof. See docs/GITHUB.md for why.
pub async fn mint_claim_code(db: &db::Db, minutes: i64) -> Result<String> {
    let raw = crate::util::random_token(5).to_uppercase();
    let code = format!("{}-{}", &raw[0..5], &raw[5..10]);
    let expires = (chrono::Utc::now() + chrono::Duration::minutes(minutes))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    db::set_setting(db, &format!("claim:{code}"), &expires).await?;
    Ok(code)
}

#[derive(Deserialize)]
struct ClaimBody {
    code: String,
}

async fn claim(State(state): State<Shared>, Json(body): Json<ClaimBody>) -> ApiResult<Response> {
    let code = body.code.trim().to_uppercase();
    let key = format!("claim:{code}");
    let Some(expires) = db::setting(&state.db, &key).await? else {
        return Err(Fail(
            StatusCode::UNAUTHORIZED,
            "that code is not valid".into(),
        ));
    };
    db::set_setting(&state.db, &key, "used").await?;
    if expires == "used"
        || crate::util::parse_ts(&expires)
            .map(|t| t < chrono::Utc::now())
            .unwrap_or(true)
    {
        return Err(Fail(
            StatusCode::UNAUTHORIZED,
            "that code has expired".into(),
        ));
    }

    // Identity comes from whoever `gh` is logged in as on this machine.
    let user = match state.gh.whoami().await {
        Ok(u) => {
            db::upsert_user(
                &state.db,
                &u.login,
                Some(u.id),
                u.name.as_deref(),
                u.avatar_url.as_deref(),
                "owner",
            )
            .await?
        }
        Err(_) => db::upsert_user(&state.db, "operator", None, None, None, "owner").await?,
    };

    let token = db::create_session(&state.db, user.id, 30).await?;
    let cookie = format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        30 * 86400
    );
    Ok(([(header::SET_COOKIE, cookie)], Json(user)).into_response())
}

async fn me(State(state): State<Shared>, headers: HeaderMap) -> ApiResult<Json<serde_json::Value>> {
    match session_of(&state, &headers).await? {
        Some(u) => Ok(Json(json!({ "user": u }))),
        None => Ok(Json(json!({ "user": null }))),
    }
}

async fn logout(State(state): State<Shared>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(t) = cookie(&headers, COOKIE) {
        db::drop_session(&state.db, &t).await?;
    }
    Ok((
        [(
            header::SET_COOKIE,
            format!("{COOKIE}=; Path=/; HttpOnly; Max-Age=0"),
        )],
        Json(json!({ "ok": true })),
    )
        .into_response())
}

async fn session_of(state: &Shared, headers: &HeaderMap) -> Result<Option<User>> {
    match cookie(headers, COOKIE) {
        Some(t) => db::session_user(&state.db, &t).await,
        None => Ok(None),
    }
}

async fn require_session(State(state): State<Shared>, mut req: Request, next: Next) -> Response {
    match session_of(&state, req.headers()).await {
        Ok(Some(user)) => {
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "sign in first — run `openpages invite` on the server for a code" })),
        )
            .into_response(),
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

// ── basics ──────────────────────────────────────────────────────────────────

async fn healthz(State(state): State<Shared>) -> Response {
    match sqlx::query("SELECT 1").fetch_one(&state.db).await {
        Ok(_) => (StatusCode::OK, "ok\n").into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("database: {e}\n")).into_response(),
    }
}

async fn version(State(state): State<Shared>) -> Json<serde_json::Value> {
    Json(json!({
        "version": update::version(),
        "runtime": state.rt.name(),
        "runtime_error": state.runtime_error,
        "base_domain": state.cfg.server.base_domain,
        "uptime_secs": state.started.elapsed().as_secs(),
    }))
}

#[derive(Serialize)]
struct SiteView {
    #[serde(flatten)]
    site: crate::models::Site,
    url: String,
    live: Option<crate::models::Deployment>,
    state: String,
    label: String,
    hits_24h: i64,
}

async fn view(state: &Shared, site: crate::models::Site) -> Result<SiteView> {
    let live = db::live_deployment(&state.db, site.id).await?;
    let latest = db::deployments_for(&state.db, site.id, 1)
        .await?
        .into_iter()
        .next();
    let shown = live.clone().or(latest);
    let s = shown
        .as_ref()
        .map(|d| d.state())
        .unwrap_or(DeployState::Queued);
    Ok(SiteView {
        url: state.cfg.site_url(&site.slug),
        hits_24h: db::hits_in_last_minutes(&state.db, site.id, 1440).await?,
        live,
        state: s.as_str().into(),
        label: s.label().into(),
        site,
    })
}

async fn overview(State(state): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    let mut sites = Vec::new();
    for s in db::list_sites(&state.db).await? {
        sites.push(view(&state, s).await?);
    }
    Ok(Json(json!({
        "sites": sites,
        "recent": db::recent_deployments(&state.db, 12).await?,
        "nodes": db::list_nodes(&state.db).await?,
        "version": update::version(),
        "pending_update": state.pending_update.lock().unwrap().clone(),
        "base_domain": state.cfg.server.base_domain,
        "heal_enabled": state.cfg.heal.enabled,
        "runtime_error": state.runtime_error,
    })))
}

async fn repos(State(state): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    let repos = state
        .gh
        .list_repos(100)
        .await
        .map_err(|e| Fail(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(json!(repos
        .into_iter()
        .map(|r| json!({
            "repo": r.name_with_owner,
            "branch": r.default_branch_ref.map(|b| b.name).unwrap_or_else(|| "main".into()),
            "private": r.is_private,
            "pushed_at": r.pushed_at,
        }))
        .collect::<Vec<_>>())))
}

// ── sites ───────────────────────────────────────────────────────────────────

async fn list_sites(State(state): State<Shared>) -> ApiResult<Json<Vec<SiteView>>> {
    let mut out = Vec::new();
    for s in db::list_sites(&state.db).await? {
        out.push(view(&state, s).await?);
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct NewSite {
    repo: String,
    branch: Option<String>,
    slug: Option<String>,
    node_id: Option<String>,
    root_dir: Option<String>,
    runtime: Option<String>,
}

async fn create_site(
    State(state): State<Shared>,
    Json(b): Json<NewSite>,
) -> ApiResult<Json<SiteView>> {
    let repo = b.repo.trim().trim_end_matches(".git").to_string();
    if !repo.contains('/') || repo.split('/').count() != 2 {
        return Err(bad("a repository looks like `owner/name`"));
    }

    let branch = match b.branch.filter(|s| !s.trim().is_empty()) {
        Some(b) => b,
        None => state
            .gh
            .default_branch(&repo)
            .await
            .unwrap_or_else(|_| "main".into()),
    };
    let slug = crate::util::slugify(
        &b.slug
            .unwrap_or_else(|| repo.split('/').nth(1).unwrap_or("site").to_string()),
    );
    if slug.is_empty() {
        return Err(bad("that name has no letters or digits in it"));
    }
    if db::site_by_slug(&state.db, &slug).await?.is_some() {
        return Err(bad(format!("`{slug}` is taken — pick another name")));
    }

    let node_id = b.node_id.unwrap_or_else(|| "local".into());
    if db::node(&state.db, &node_id).await?.is_none() {
        return Err(bad(format!("no node called `{node_id}`")));
    }

    let site = db::create_site(
        &state.db,
        &slug,
        &repo,
        &branch,
        &node_id,
        &b.runtime.unwrap_or_else(|| "auto".into()),
        b.root_dir.as_deref().unwrap_or(""),
    )
    .await?;

    // A webhook when we are reachable, polling when we are not. Both work; one
    // is just faster.
    if state.cfg.github.webhooks {
        if let Some(public) = state.cfg.server.public_url.clone() {
            let url = format!("{}/api/hooks/github", public.trim_end_matches('/'));
            match state
                .gh
                .ensure_webhook(&repo, &url, &state.webhook_secret)
                .await
            {
                Ok(id) => {
                    sqlx::query("UPDATE sites SET webhook_id = ? WHERE id = ?")
                        .bind(id)
                        .bind(site.id)
                        .execute(&state.db)
                        .await?;
                }
                Err(e) => tracing::warn!(
                    "could not add a webhook to {repo} ({e:#}); falling back to polling"
                ),
            }
        }
    }

    if let Ok(head) = state.gh.head_commit(&repo, &branch).await {
        let _ = worker::deploy(
            &state,
            &site,
            &head.sha,
            Some(&head.message),
            &branch,
            "production",
        )
        .await;
    }

    let site = db::site(&state.db, site.id)
        .await?
        .ok_or_else(|| missing("site"))?;
    Ok(Json(view(&state, site).await?))
}

async fn get_site(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let site = db::site(&state.db, id)
        .await?
        .ok_or_else(|| missing("site"))?;
    let deployments = db::deployments_for(&state.db, id, 30).await?;
    let v = view(&state, site).await?;
    Ok(Json(json!({ "site": v, "deployments": deployments })))
}

#[derive(Deserialize)]
struct SitePatch {
    branch: Option<String>,
    root_dir: Option<String>,
    install_cmd: Option<String>,
    build_cmd: Option<String>,
    output_dir: Option<String>,
    build_image: Option<String>,
    runtime: Option<String>,
    kind: Option<String>,
    swap_policy: Option<String>,
    swap_max_wait_secs: Option<i64>,
    drain_secs: Option<i64>,
    heal_mode: Option<String>,
    preview_enabled: Option<bool>,
}

async fn update_site(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(p): Json<SitePatch>,
) -> ApiResult<Json<SiteView>> {
    db::site(&state.db, id)
        .await?
        .ok_or_else(|| missing("site"))?;

    macro_rules! set {
        ($col:literal, $val:expr) => {
            if let Some(v) = $val {
                sqlx::query(concat!("UPDATE sites SET ", $col, " = ? WHERE id = ?"))
                    .bind(v)
                    .bind(id)
                    .execute(&state.db)
                    .await?;
            }
        };
    }
    set!("branch", p.branch);
    set!("root_dir", p.root_dir);
    set!("install_cmd", p.install_cmd);
    set!("build_cmd", p.build_cmd);
    set!("output_dir", p.output_dir);
    set!("build_image", p.build_image);
    set!("runtime", p.runtime);
    set!("kind", p.kind);
    set!("swap_policy", p.swap_policy);
    set!("swap_max_wait_secs", p.swap_max_wait_secs);
    set!("drain_secs", p.drain_secs);
    set!("heal_mode", p.heal_mode);
    set!("preview_enabled", p.preview_enabled.map(|b| b as i64));
    sqlx::query("UPDATE sites SET updated_at = ? WHERE id = ?")
        .bind(crate::util::now())
        .bind(id)
        .execute(&state.db)
        .await?;

    let site = db::site(&state.db, id)
        .await?
        .ok_or_else(|| missing("site"))?;
    Ok(Json(view(&state, site).await?))
}

async fn remove_site(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let site = db::site(&state.db, id)
        .await?
        .ok_or_else(|| missing("site"))?;

    for d in db::deployments_for(&state.db, id, 1000).await? {
        if let (true, Some(cid)) = (d.is_local(), d.container_id.as_deref()) {
            let _ = crate::runtime::stop(state.rt, cid, 5).await;
        }
    }
    if let (Some(hook), true) = (site.webhook_id, state.cfg.github.webhooks) {
        let _ = state.gh.delete_webhook(&site.repo, hook).await;
    }
    let dir = state.cfg.deployments_dir().join(&site.slug);
    let _ = tokio::fs::remove_dir_all(dir).await;

    db::delete_site(&state.db, id).await?;
    state.invalidate_routes().await;
    Ok(Json(json!({ "ok": true })))
}

async fn get_env(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = db::site_env(&state.db, id).await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(k, v, secret)| json!({
            "key": k,
            "value": if secret { "••••••••".to_string() } else { v },
            "secret": secret
        }))
        .collect::<Vec<_>>())))
}

#[derive(Deserialize)]
struct EnvBody {
    key: String,
    value: String,
    #[serde(default)]
    secret: bool,
}

async fn put_env(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(b): Json<EnvBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let key = b.key.trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(bad(
            "an environment variable name is letters, digits and underscores",
        ));
    }
    db::set_site_env(&state.db, id, key, &b.value, b.secret).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn del_env(
    State(state): State<Shared>,
    Path((id, key)): Path<(i64, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    db::unset_site_env(&state.db, id, &key).await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct DeployBody {
    sha: Option<String>,
    #[serde(default)]
    preview: bool,
}

async fn deploy_site(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Json(b): Json<DeployBody>,
) -> ApiResult<Json<crate::models::Deployment>> {
    let site = db::site(&state.db, id)
        .await?
        .ok_or_else(|| missing("site"))?;
    let (sha, msg) = match b.sha.filter(|s| !s.trim().is_empty()) {
        Some(sha) if crate::buildpipe::is_commit_sha(sha.trim()) => (sha.trim().to_string(), None),
        Some(_) => return Err(bad("a commit is 7 to 40 lowercase hex digits")),
        None => {
            let c = state
                .gh
                .head_commit(&site.repo, &site.branch)
                .await
                .map_err(|e| Fail(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
            (c.sha, Some(c.message))
        }
    };
    let kind = if b.preview { "preview" } else { "production" };
    let d = worker::deploy(&state, &site, &sha, msg.as_deref(), &site.branch, kind).await?;
    Ok(Json(d))
}

async fn site_deployments(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<crate::models::Deployment>>> {
    Ok(Json(db::deployments_for(&state.db, id, 50).await?))
}

async fn site_traffic(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let series = db::traffic_series(&state.db, id, 1440).await?;
    Ok(Json(json!({
        "series": series.iter().map(|(b, h)| json!({ "t": b, "hits": h })).collect::<Vec<_>>(),
        "inflight": state.traffic.inflight(id),
        "last_minute": db::hits_in_last_minutes(&state.db, id, 1).await?,
    })))
}

async fn rollback_site(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    match swap::rollback(&state, id, "asked for from the panel").await? {
        Some(to) => Ok(Json(json!({ "ok": true, "deployment_id": to }))),
        None => Err(bad("there is no earlier working deployment to go back to")),
    }
}

// ── deployments ─────────────────────────────────────────────────────────────

async fn get_deployment(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let d = db::deployment(&state.db, id)
        .await?
        .ok_or_else(|| missing("deployment"))?;
    let site = db::site(&state.db, d.site_id)
        .await?
        .ok_or_else(|| missing("site"))?;
    let url = if d.is_preview() {
        state.cfg.preview_url(&d.commit_sha, &site.slug)
    } else {
        state.cfg.site_url(&site.slug)
    };
    let s = d.state();
    Ok(Json(json!({
        "deployment": d, "site": site, "url": url,
        "label": s.label(), "terminal": s.is_terminal(),
    })))
}

#[derive(Deserialize)]
struct After {
    #[serde(default)]
    after: i64,
}

async fn get_logs(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<After>,
) -> ApiResult<Json<Vec<crate::models::LogLine>>> {
    Ok(Json(db::logs_since(&state.db, id, q.after).await?))
}

/// The whole log, as a file. The panel shows the distilled failure; this is for
/// when you want the rest of it.
async fn get_full_log(State(state): State<Shared>, Path(id): Path<i64>) -> ApiResult<Response> {
    let text = db::full_log(&state.db, id).await?;
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "text/plain; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"openpages-deployment-{id}.log\""),
            ),
        ],
        text,
    )
        .into_response())
}

/// Server-sent events, because a build log is one-way and SSE reconnects by
/// itself. A WebSocket here would be a second protocol for no benefit.
///
/// Every event carries its sequence number as the SSE id, and a reconnect is
/// resumed from `Last-Event-ID`. Without that, the browser's own automatic
/// reconnect replays the whole log from the beginning — which it will do,
/// because the stream deliberately ends once a deployment settles.
async fn stream_logs(
    State(state): State<Shared>,
    Path(id): Path<i64>,
    Query(q): Query<After>,
    headers: HeaderMap,
) -> Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>> {
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());

    let stream = async_stream::stream! {
        let mut seq = resume.unwrap_or(q.after);
        let mut idle = 0u32;
        loop {
            match db::logs_since(&state.db, id, seq).await {
                Ok(lines) if !lines.is_empty() => {
                    idle = 0;
                    for l in lines {
                        seq = l.seq;
                        if let Ok(ev) = Event::default().id(l.seq.to_string()).event("log").json_data(&l) {
                            yield Ok(ev);
                        }
                    }
                }
                _ => idle += 1,
            }
            if let Ok(Some(d)) = db::deployment(&state.db, id).await {
                let settled = d.state().is_terminal()
                    || matches!(d.state(), DeployState::Ready | DeployState::Live);
                if let Ok(ev) = Event::default().event("state").json_data(json!({
                    "state": d.state, "label": d.state().label(),
                    "error": d.error_summary, "settled": settled,
                })) {
                    yield Ok(ev);
                }
                if settled && idle > 1 { break; }
            }
            if idle > 900 { break; }
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
    };
    Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn promote_now(
    State(state): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let d = db::deployment(&state.db, id)
        .await?
        .ok_or_else(|| missing("deployment"))?;
    if !matches!(d.state(), DeployState::Ready | DeployState::Superseded) {
        return Err(bad(format!(
            "deployment {id} is {} — only a proofed or previously-live deployment can be put on press",
            d.state().label().to_lowercase()
        )));
    }
    swap::promote(&state, &d, "asked for from the panel").await?;
    Ok(Json(json!({ "ok": true })))
}

// ── nodes, heals, system ────────────────────────────────────────────────────

async fn list_nodes(State(state): State<Shared>) -> ApiResult<Json<Vec<crate::models::Node>>> {
    Ok(Json(db::list_nodes(&state.db).await?))
}

#[derive(Deserialize)]
struct NewNode {
    name: String,
}

async fn add_node(
    State(state): State<Shared>,
    Json(b): Json<NewNode>,
) -> ApiResult<Json<serde_json::Value>> {
    let (id, token) = db::enroll_node(&state.db, b.name.trim()).await?;
    let panel = state
        .cfg
        .server
        .public_url
        .clone()
        .unwrap_or_else(|| "https://<this-panel>".into());
    Ok(Json(json!({
        "id": id,
        "token": token,
        "command": format!("openpages agent --panel {panel} --token {token}"),
        "note": "This token is shown once. It is stored here only as a hash.",
    })))
}

async fn list_heals(
    State(state): State<Shared>,
) -> ApiResult<Json<Vec<crate::models::HealRecord>>> {
    Ok(Json(db::list_heals(&state.db, 50).await?))
}

async fn system(State(state): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "version": update::version(),
        "runtime": state.rt.name(),
        "runtime_error": state.runtime_error,
        "uptime_secs": state.started.elapsed().as_secs(),
        "load_per_core": crate::util::load_per_core(),
        "channel": state.cfg.update.channel,
        "auto_update": state.cfg.update.enabled,
        "auto_apply": state.cfg.update.auto_apply,
        "pending_update": state.pending_update.lock().unwrap().clone(),
        "heal": {
            "enabled": state.cfg.heal.enabled,
            "mode": state.cfg.heal.mode,
            "budget_per_repo_per_day": state.cfg.heal.max_per_repo_per_day,
        },
        "updates": db::list_updates(&state.db, 20).await?,
        "public_url": state.cfg.server.public_url,
        "base_domain": state.cfg.server.base_domain,
    })))
}

async fn update_check(State(state): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    match update::check(&state).await {
        Ok(Some(v)) => Ok(Json(json!({ "available": v }))),
        Ok(None) => Ok(Json(
            json!({ "available": null, "current": update::version() }),
        )),
        Err(e) => Err(Fail(StatusCode::BAD_GATEWAY, format!("{e:#}"))),
    }
}

async fn update_apply(State(state): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    let Some(version) = state.pending_update.lock().unwrap().clone() else {
        return Err(bad("nothing is staged — check for an update first"));
    };
    let staged = state.cfg.stage_dir().join(format!("openpages-{version}"));
    if !staged.exists() {
        return Err(bad("the staged binary is gone; check again"));
    }
    let s = state.clone();
    let applying = version.clone();
    tokio::spawn(async move {
        if let Err(e) = update::apply(&s, &staged, &applying).await {
            tracing::error!("applying {applying}: {e:#}");
        }
    });
    Ok(Json(json!({ "ok": true, "applying": version })))
}

// ── github webhook ──────────────────────────────────────────────────────────

async fn webhook(
    State(state): State<Shared>,
    headers: HeaderMap,
    body: String,
) -> ApiResult<Json<serde_json::Value>> {
    let sig = edge::header_str(
        &headers,
        header::HeaderName::from_static("x-hub-signature-256"),
    )
    .ok_or_else(|| Fail(StatusCode::UNAUTHORIZED, "unsigned".into()))?;
    if !verify_signature(&state.webhook_secret, &body, &sig) {
        return Err(Fail(StatusCode::UNAUTHORIZED, "bad signature".into()));
    }

    let event = edge::header_str(&headers, header::HeaderName::from_static("x-github-event"))
        .unwrap_or_default();
    if event == "ping" {
        return Ok(Json(json!({ "ok": true, "pong": true })));
    }
    if event != "push" {
        return Ok(Json(json!({ "ok": true, "ignored": event })));
    }

    let push: PushEvent =
        serde_json::from_str(&body).map_err(|e| bad(format!("unreadable push event: {e}")))?;
    let Some(branch) = pushed_branch(&push.git_ref).map(str::to_string) else {
        return Ok(Json(json!({ "ok": true, "ignored": push.git_ref })));
    };
    let mut started = Vec::new();

    for site in db::site_by_repo(&state.db, &push.repository.full_name).await? {
        let is_default = branch == site.branch;
        if !is_default && site.preview_enabled == 0 {
            continue;
        }
        if push.deleted {
            continue;
        }
        let kind = if is_default { "production" } else { "preview" };
        let msg = push.head_commit.as_ref().map(|c| c.message.clone());
        let d = worker::deploy(
            &state,
            &site,
            &push.after,
            msg.as_deref().map(crate::gh::first_line),
            &branch,
            kind,
        )
        .await?;
        started.push(d.id);
    }
    Ok(Json(json!({ "ok": true, "deployments": started })))
}

/// `refs/heads/<branch>` → `<branch>`, whole. Tags are not deployments, and
/// taking only the last path segment made a push to `feature/main` — or a
/// tag called `main` — a production deploy.
pub fn pushed_branch(git_ref: &str) -> Option<&str> {
    git_ref
        .strip_prefix("refs/heads/")
        .filter(|b| !b.is_empty())
}

#[derive(Deserialize)]
struct PushEvent {
    #[serde(rename = "ref")]
    git_ref: String,
    after: String,
    #[serde(default)]
    deleted: bool,
    repository: Repo,
    #[serde(default)]
    head_commit: Option<HeadCommit>,
}
#[derive(Deserialize)]
struct Repo {
    full_name: String,
}
#[derive(Deserialize)]
struct HeadCommit {
    message: String,
}

pub fn verify_signature(secret: &str, body: &str, header: &str) -> bool {
    use hmac::{Hmac, Mac};
    let Some(hex_sig) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body.as_bytes());
    let expected = hex::encode(mac.finalize().into_bytes());
    crate::util::secret_eq(&expected, &hex_sig.to_lowercase())
}

// ── remote nodes ────────────────────────────────────────────────────────────

async fn node_auth(state: &Shared, headers: &HeaderMap) -> ApiResult<crate::models::Node> {
    let token = edge::header_str(headers, header::AUTHORIZATION)
        .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
        .ok_or_else(|| Fail(StatusCode::UNAUTHORIZED, "no node token".into()))?;
    db::authenticate_node(&state.db, &token)
        .await?
        .ok_or_else(|| Fail(StatusCode::UNAUTHORIZED, "unknown node token".into()))
}

#[derive(Deserialize)]
struct Heartbeat {
    runtime: String,
    version: String,
    os: String,
    edge_url: Option<String>,
}

async fn node_heartbeat(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(b): Json<Heartbeat>,
) -> ApiResult<Json<serde_json::Value>> {
    let node = node_auth(&state, &headers).await?;
    db::touch_node(
        &state.db,
        &node.id,
        &b.runtime,
        &b.version,
        &b.os,
        b.edge_url.as_deref(),
    )
    .await?;
    Ok(Json(json!({ "ok": true })))
}

/// Long-poll: a node asks for work and the request simply does not come back
/// until there is some. No inbound connection to the node is ever needed, which
/// is what makes a node behind NAT possible.
async fn node_job(
    State(state): State<Shared>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let node = node_auth(&state, &headers).await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(d) = db::claim_job(&state.db, &node.id).await? {
            let site = db::site(&state.db, d.site_id)
                .await?
                .ok_or_else(|| missing("site"))?;
            let job = worker::job_for(&state, &d, &site).await?;
            return Ok(Json(json!({ "job": job })));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(Json(json!({ "job": null })));
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

#[derive(Deserialize)]
struct NodeLogs {
    deployment_id: i64,
    lines: Vec<(String, String)>,
}

/// A node may speak only for deployments it claimed, and only while it is
/// building them. Without this any node token could rewrite any deployment —
/// including the panel's own — and point it at a path or port on the panel.
fn node_may_report(
    node: &crate::models::Node,
    d: &crate::models::Deployment,
) -> Result<bool, Fail> {
    if d.node_id.as_deref().unwrap_or("local") != node.id {
        return Err(Fail(
            StatusCode::FORBIDDEN,
            "that deployment is not this node's".into(),
        ));
    }
    Ok(d.state() == DeployState::Building)
}

async fn node_logs(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(b): Json<NodeLogs>,
) -> ApiResult<Json<serde_json::Value>> {
    let node = node_auth(&state, &headers).await?;
    let d = db::deployment(&state.db, b.deployment_id)
        .await?
        .ok_or_else(|| missing("deployment"))?;
    if !node_may_report(&node, &d)? {
        return Ok(Json(json!({ "ok": true, "ignored": "not building" })));
    }
    db::append_logs(&state.db, b.deployment_id, &b.lines).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn node_result(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(mut out): Json<crate::models::BuildOutcome>,
) -> ApiResult<Json<serde_json::Value>> {
    let node = node_auth(&state, &headers).await?;
    let d = db::deployment(&state.db, out.deployment_id)
        .await?
        .ok_or_else(|| missing("deployment"))?;
    if !node_may_report(&node, &d)? {
        // A retry of a result that already landed. Nothing to write.
        return Ok(Json(json!({ "ok": true, "ignored": "already reported" })));
    }
    let site = db::site(&state.db, d.site_id)
        .await?
        .ok_or_else(|| missing("site"))?;

    // A remote node proved its own artifact; the panel cannot see that disk.
    // What it says about paths and ports describes the node, not this host,
    // so the path is recorded in the node's terms and the port must be one.
    if out.artifact_path.is_some() {
        out.artifact_path = Some(format!("{}/{}", site.slug, d.id));
    }
    if let Some(p) = out.container_port {
        if !(1..=65535).contains(&p) {
            return Err(bad("container_port is not a port"));
        }
    }
    db::finish_deployment(&state.db, &out).await?;
    if out.ok {
        db::mark_ready(&state.db, d.id).await?;
        state.invalidate_routes().await;
    } else if let Some(report) = out.incident.clone() {
        let s = state.clone();
        let summary = out
            .error_summary
            .clone()
            .unwrap_or_else(|| "build failed".into());
        tokio::spawn(async move {
            crate::heal::on_build_failure(
                s,
                site.id,
                d.id,
                site.repo.clone(),
                site.branch.clone(),
                d.commit_sha.clone(),
                report,
                summary,
                site.heal_mode.clone(),
            )
            .await;
        });
    }
    Ok(Json(json!({ "ok": true })))
}

// ── the panel itself ────────────────────────────────────────────────────────

#[derive(rust_embed::RustEmbed)]
#[folder = "web/dist"]
struct Web;

async fn panel(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = Web::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let cache = if path == "index.html" {
            "no-cache"
        } else {
            "public, max-age=31536000, immutable"
        };
        return (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                (header::CACHE_CONTROL, cache),
            ],
            file.data.into_owned(),
        )
            .into_response();
    }

    // Single-page app: unknown paths are routes, not missing files.
    match Web::get("index.html") {
        Some(index) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            index.data.into_owned(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "the panel was not built into this binary",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "It's a Secret to Everybody";

    #[test]
    fn a_correct_github_signature_is_accepted() {
        // The example pair from GitHub's own webhook documentation.
        let body = "Hello, World!";
        let sig = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(verify_signature(SECRET, body, sig));
    }

    #[test]
    fn a_tampered_body_is_rejected() {
        let sig = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(!verify_signature(SECRET, "Hello, World?", sig));
    }

    #[test]
    fn an_unsigned_or_malformed_header_is_rejected() {
        assert!(!verify_signature(SECRET, "Hello, World!", "757107ea"));
        assert!(!verify_signature(SECRET, "Hello, World!", ""));
        assert!(!verify_signature(SECRET, "Hello, World!", "sha1=abc"));
    }

    async fn test_state() -> Shared {
        let dir = tempfile::tempdir().unwrap();
        let db = db::open(&dir.path().join("t.db")).await.unwrap();
        std::mem::forget(dir);
        db::ensure_local_node(&db, "podman").await.unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.github.gh_bin = "/bin/false".into();
        crate::state::AppState::new(cfg, crate::runtime::Runtime::Podman, None, db)
            .await
            .unwrap()
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    fn outcome(id: i64, path: &str) -> crate::models::BuildOutcome {
        crate::models::BuildOutcome {
            deployment_id: id,
            ok: true,
            framework: None,
            artifact_path: Some(path.into()),
            container_id: None,
            container_port: None,
            error_summary: None,
            incident: None,
        }
    }

    #[tokio::test]
    async fn a_node_token_only_reports_on_its_own_builds() {
        let state = test_state().await;
        let (a, a_token) = db::enroll_node(&state.db, "a").await.unwrap();
        let (_, b_token) = db::enroll_node(&state.db, "b").await.unwrap();

        // A deployment of the panel's own: no node may touch it.
        let local = db::create_site(&state.db, "blog", "o/r", "main", "local", "auto", "")
            .await
            .unwrap();
        let ld = db::create_deployment(&state.db, &local, "aaaaaaa", None, "main", "production")
            .await
            .unwrap();
        db::claim_job(&state.db, "local").await.unwrap();
        let r = node_result(
            State(state.clone()),
            bearer(&a_token),
            Json(outcome(ld.id, "/etc")),
        )
        .await;
        assert!(matches!(r, Err(Fail(StatusCode::FORBIDDEN, _))));
        let r = node_logs(
            State(state.clone()),
            bearer(&a_token),
            Json(NodeLogs {
                deployment_id: ld.id,
                lines: vec![("stdout".into(), "x".into())],
            }),
        )
        .await;
        assert!(matches!(r, Err(Fail(StatusCode::FORBIDDEN, _))));
        let ld = db::deployment(&state.db, ld.id).await.unwrap().unwrap();
        assert_eq!(ld.state, "building");
        assert!(ld.artifact_path.is_none());

        // Node a's own deployment: node b may not report it, node a may.
        let remote = db::create_site(&state.db, "shop", "o/s", "main", &a, "auto", "")
            .await
            .unwrap();
        let rd = db::create_deployment(&state.db, &remote, "bbbbbbb", None, "main", "production")
            .await
            .unwrap();
        db::claim_job(&state.db, &a).await.unwrap();
        let r = node_result(
            State(state.clone()),
            bearer(&b_token),
            Json(outcome(rd.id, "/etc")),
        )
        .await;
        assert!(matches!(r, Err(Fail(StatusCode::FORBIDDEN, _))));
        let first = node_result(
            State(state.clone()),
            bearer(&a_token),
            Json(outcome(rd.id, "/etc")),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(first.0["ok"], true);
        let rd = db::deployment(&state.db, rd.id).await.unwrap().unwrap();
        assert_eq!(rd.state, "ready");
        assert_eq!(
            rd.artifact_path.as_deref(),
            Some(format!("shop/{}", rd.id).as_str()),
            "the node's path is recorded in the node's terms"
        );

        // Reported once; a second result is a no-op.
        let again = node_result(
            State(state.clone()),
            bearer(&a_token),
            Json(outcome(rd.id, "/x")),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(again.0["ignored"], "already reported");

        // Live, but the node has no edge: the panel must not serve it from its
        // own disk.
        db::promote(&state.db, remote.id, rd.id).await.unwrap();
        state.invalidate_routes().await;
        assert!(edge::resolve(&state, "shop.pages.localhost")
            .await
            .is_none());

        db::touch_node(
            &state.db,
            &a,
            "podman",
            "0",
            "linux",
            Some("http://10.0.0.5:8789"),
        )
        .await
        .unwrap();
        state.invalidate_routes().await;
        let route = edge::resolve(&state, "shop.pages.localhost").await.unwrap();
        assert!(matches!(route.target, edge::Target::Remote { .. }));
    }

    #[test]
    fn only_a_whole_branch_ref_is_a_branch() {
        assert_eq!(pushed_branch("refs/heads/main"), Some("main"));
        assert_eq!(
            pushed_branch("refs/heads/feature/main"),
            Some("feature/main")
        );
        assert_eq!(pushed_branch("refs/tags/main"), None);
        assert_eq!(pushed_branch("refs/heads/"), None);
        assert_eq!(pushed_branch("main"), None);
    }

    async fn call(state: &Shared, method: &str, path: &str, hdrs: &[(&str, &str)]) -> Response {
        use tower::ServiceExt;
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in hdrs {
            b = b.header(*k, *v);
        }
        router(state.clone())
            .oneshot(b.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_hosted_site_cannot_drive_or_frame_the_panel() {
        let state = test_state().await;
        let panel = "openpages.localhost:8788";

        // The audit's request: from a site, cookie attached, no preflight.
        let csrf = call(
            &state,
            "POST",
            "/api/auth/logout",
            &[
                ("host", panel),
                ("origin", "http://evil.pages.localhost:8788"),
                ("sec-fetch-site", "same-site"),
                ("content-type", "text/plain"),
            ],
        )
        .await;
        assert_eq!(csrf.status(), StatusCode::FORBIDDEN);

        let without_fetch_metadata = call(
            &state,
            "POST",
            "/api/auth/logout",
            &[
                ("host", panel),
                ("origin", "http://evil.pages.localhost:8788"),
            ],
        )
        .await;
        assert_eq!(without_fetch_metadata.status(), StatusCode::FORBIDDEN);

        let same_origin = call(
            &state,
            "POST",
            "/api/auth/logout",
            &[
                ("host", panel),
                ("origin", "http://openpages.localhost:8788"),
                ("sec-fetch-site", "same-origin"),
            ],
        )
        .await;
        assert_eq!(same_origin.status(), StatusCode::OK);

        let no_browser = call(&state, "POST", "/api/auth/logout", &[("host", panel)]).await;
        assert_eq!(
            no_browser.status(),
            StatusCode::OK,
            "curl, nodes and webhooks"
        );

        let page = call(&state, "GET", "/", &[("host", panel)]).await;
        assert_eq!(page.headers()[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(
            page.headers()[header::CONTENT_SECURITY_POLICY],
            "frame-ancestors 'none'"
        );
    }

    #[tokio::test]
    async fn a_deploy_sha_cannot_be_a_git_option() {
        let state = test_state().await;
        let site = db::create_site(&state.db, "blog", "o/r", "main", "local", "auto", "")
            .await
            .unwrap();
        let r = deploy_site(
            State(state),
            Path(site.id),
            Json(DeployBody {
                sha: Some("--upload-pack=touch PWNED; git-upload-pack".into()),
                preview: false,
            }),
        )
        .await;
        assert!(matches!(r, Err(Fail(StatusCode::BAD_REQUEST, _))));
    }

    #[test]
    fn cookies_are_parsed_out_of_a_crowded_header() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "theme=dark; op_session=abc123; other=1".parse().unwrap(),
        );
        assert_eq!(cookie(&h, COOKIE).as_deref(), Some("abc123"));
        assert_eq!(cookie(&h, "nope"), None);
    }
}
