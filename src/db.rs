use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use crate::models::*;
use crate::util::{now, sha256_hex};

pub type Db = SqlitePool;

pub async fn open(path: &Path) -> Result<Db> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .context("building sqlite options")?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(10))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(15))
        .connect_with(opts)
        .await
        .with_context(|| format!("opening database {}", path.display()))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("running migrations")?;

    Ok(pool)
}

// ── settings ────────────────────────────────────────────────────────────────

pub async fn setting(db: &Db, key: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get::<String, _>("value")))
}

pub async fn set_setting(db: &Db, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO settings(key, value) VALUES(?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(db)
    .await?;
    Ok(())
}

// ── users & sessions ────────────────────────────────────────────────────────

pub async fn user_count(db: &Db) -> Result<i64> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM users")
        .fetch_one(db)
        .await?;
    Ok(row.get::<i64, _>("n"))
}

pub async fn user_by_login(db: &Db, login: &str) -> Result<Option<User>> {
    Ok(
        sqlx::query_as::<_, User>("SELECT * FROM users WHERE gh_login = ?")
            .bind(login)
            .fetch_optional(db)
            .await?,
    )
}

pub async fn upsert_user(
    db: &Db,
    login: &str,
    gh_id: Option<i64>,
    name: Option<&str>,
    avatar: Option<&str>,
    role: &str,
) -> Result<User> {
    sqlx::query(
        "INSERT INTO users(gh_login, gh_id, name, avatar_url, role, created_at)
         VALUES(?, ?, ?, ?, ?, ?)
         ON CONFLICT(gh_login) DO UPDATE SET
           gh_id = excluded.gh_id, name = excluded.name, avatar_url = excluded.avatar_url",
    )
    .bind(login)
    .bind(gh_id)
    .bind(name)
    .bind(avatar)
    .bind(role)
    .bind(now())
    .execute(db)
    .await?;
    user_by_login(db, login)
        .await?
        .context("user vanished immediately after upsert")
}

pub async fn create_session(db: &Db, user_id: i64, days: i64) -> Result<String> {
    let token = crate::util::random_token(32);
    let expires = (chrono::Utc::now() + chrono::Duration::days(days))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    sqlx::query("INSERT INTO sessions(token, user_id, created_at, expires_at) VALUES(?,?,?,?)")
        .bind(sha256_hex(&token))
        .bind(user_id)
        .bind(now())
        .bind(expires)
        .execute(db)
        .await?;
    Ok(token)
}

pub async fn session_user(db: &Db, token: &str) -> Result<Option<User>> {
    let row = sqlx::query_as::<_, User>(
        "SELECT u.* FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token = ? AND s.expires_at > ?",
    )
    .bind(sha256_hex(token))
    .bind(now())
    .fetch_optional(db)
    .await?;
    Ok(row)
}

pub async fn drop_session(db: &Db, token: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token = ?")
        .bind(sha256_hex(token))
        .execute(db)
        .await?;
    Ok(())
}

// ── nodes ───────────────────────────────────────────────────────────────────

pub async fn ensure_local_node(db: &Db, runtime: &str) -> Result<()> {
    let host = hostname();
    sqlx::query(
        "INSERT INTO nodes(id, name, runtime, is_local, version, os, last_seen, enrolled_at)
         VALUES('local', ?, ?, 1, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
           name = excluded.name, runtime = excluded.runtime,
           version = excluded.version, last_seen = excluded.last_seen",
    )
    .bind(host)
    .bind(runtime)
    .bind(env!("CARGO_PKG_VERSION"))
    .bind(std::env::consts::OS)
    .bind(now())
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn list_nodes(db: &Db) -> Result<Vec<Node>> {
    Ok(
        sqlx::query_as::<_, Node>("SELECT * FROM nodes ORDER BY is_local DESC, name")
            .fetch_all(db)
            .await?,
    )
}

pub async fn node(db: &Db, id: &str) -> Result<Option<Node>> {
    Ok(
        sqlx::query_as::<_, Node>("SELECT * FROM nodes WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

pub async fn enroll_node(db: &Db, name: &str) -> Result<(String, String)> {
    let id = format!("n_{}", crate::util::random_token(6));
    let token = crate::util::random_token(24);
    sqlx::query(
        "INSERT INTO nodes(id, name, runtime, is_local, token_hash, enrolled_at)
         VALUES(?, ?, 'auto', 0, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(sha256_hex(&token))
    .bind(now())
    .execute(db)
    .await?;
    Ok((id.clone(), format!("{id}.{token}")))
}

/// Node credentials arrive as `<node_id>.<secret>` in an Authorization header.
pub async fn authenticate_node(db: &Db, presented: &str) -> Result<Option<Node>> {
    let (id, secret) = presented.split_once('.').unwrap_or(("", ""));
    if id.is_empty() || secret.is_empty() {
        return Ok(None);
    }
    let Some(node) = node(db, id).await? else {
        return Ok(None);
    };
    let Some(hash) = node.token_hash.clone() else {
        return Ok(None);
    };
    Ok(crate::util::secret_eq(&hash, &sha256_hex(secret)).then_some(node))
}

pub async fn touch_node(
    db: &Db,
    id: &str,
    runtime: &str,
    version: &str,
    os: &str,
    edge_url: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE nodes SET last_seen = ?, runtime = ?, version = ?, os = ?,
           edge_url = COALESCE(?, edge_url) WHERE id = ?",
    )
    .bind(now())
    .bind(runtime)
    .bind(version)
    .bind(os)
    .bind(edge_url)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

// ── sites ───────────────────────────────────────────────────────────────────

pub async fn list_sites(db: &Db) -> Result<Vec<Site>> {
    Ok(
        sqlx::query_as::<_, Site>("SELECT * FROM sites ORDER BY slug")
            .fetch_all(db)
            .await?,
    )
}

pub async fn site(db: &Db, id: i64) -> Result<Option<Site>> {
    Ok(
        sqlx::query_as::<_, Site>("SELECT * FROM sites WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

pub async fn site_by_slug(db: &Db, slug: &str) -> Result<Option<Site>> {
    Ok(
        sqlx::query_as::<_, Site>("SELECT * FROM sites WHERE slug = ?")
            .bind(slug)
            .fetch_optional(db)
            .await?,
    )
}

pub async fn site_by_repo(db: &Db, repo: &str) -> Result<Vec<Site>> {
    Ok(
        sqlx::query_as::<_, Site>("SELECT * FROM sites WHERE repo = ?")
            .bind(repo)
            .fetch_all(db)
            .await?,
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn create_site(
    db: &Db,
    slug: &str,
    repo: &str,
    branch: &str,
    node_id: &str,
    runtime: &str,
    root_dir: &str,
) -> Result<Site> {
    sqlx::query(
        "INSERT INTO sites(slug, repo, branch, node_id, runtime, root_dir, created_at, updated_at)
         VALUES(?,?,?,?,?,?,?,?)",
    )
    .bind(slug)
    .bind(repo)
    .bind(branch)
    .bind(node_id)
    .bind(runtime)
    .bind(root_dir)
    .bind(now())
    .bind(now())
    .execute(db)
    .await?;
    site_by_slug(db, slug)
        .await?
        .context("site vanished after insert")
}

pub async fn delete_site(db: &Db, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM sites WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn site_env(db: &Db, site_id: i64) -> Result<Vec<(String, String, bool)>> {
    let rows =
        sqlx::query("SELECT key, value, is_secret FROM site_env WHERE site_id = ? ORDER BY key")
            .bind(site_id)
            .fetch_all(db)
            .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("key"),
                r.get::<String, _>("value"),
                r.get::<i64, _>("is_secret") != 0,
            )
        })
        .collect())
}

pub async fn set_site_env(
    db: &Db,
    site_id: i64,
    key: &str,
    value: &str,
    secret: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO site_env(site_id, key, value, is_secret) VALUES(?,?,?,?)
         ON CONFLICT(site_id, key) DO UPDATE SET value = excluded.value, is_secret = excluded.is_secret",
    )
    .bind(site_id)
    .bind(key)
    .bind(value)
    .bind(secret as i64)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn unset_site_env(db: &Db, site_id: i64, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM site_env WHERE site_id = ? AND key = ?")
        .bind(site_id)
        .bind(key)
        .execute(db)
        .await?;
    Ok(())
}

// ── deployments ─────────────────────────────────────────────────────────────

pub async fn create_deployment(
    db: &Db,
    site: &Site,
    sha: &str,
    msg: Option<&str>,
    branch: &str,
    kind: &str,
) -> Result<Deployment> {
    let res = sqlx::query(
        "INSERT INTO deployments(site_id, commit_sha, commit_msg, branch, kind, state, node_id, created_at)
         VALUES(?,?,?,?,?,'queued',?,?)",
    )
    .bind(site.id)
    .bind(sha)
    .bind(msg)
    .bind(branch)
    .bind(kind)
    .bind(site.node_id.as_deref().unwrap_or("local"))
    .bind(now())
    .execute(db)
    .await?;
    deployment(db, res.last_insert_rowid())
        .await?
        .context("deployment vanished after insert")
}

pub async fn deployment(db: &Db, id: i64) -> Result<Option<Deployment>> {
    Ok(
        sqlx::query_as::<_, Deployment>("SELECT * FROM deployments WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

pub async fn deployments_for(db: &Db, site_id: i64, limit: i64) -> Result<Vec<Deployment>> {
    Ok(sqlx::query_as::<_, Deployment>(
        "SELECT * FROM deployments WHERE site_id = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(site_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

pub async fn recent_deployments(db: &Db, limit: i64) -> Result<Vec<Deployment>> {
    Ok(
        sqlx::query_as::<_, Deployment>("SELECT * FROM deployments ORDER BY id DESC LIMIT ?")
            .bind(limit)
            .fetch_all(db)
            .await?,
    )
}

pub async fn live_deployment(db: &Db, site_id: i64) -> Result<Option<Deployment>> {
    Ok(sqlx::query_as::<_, Deployment>(
        "SELECT * FROM deployments WHERE site_id = ? AND state = 'live' ORDER BY id DESC LIMIT 1",
    )
    .bind(site_id)
    .fetch_optional(db)
    .await?)
}

/// The newest deployment that was live and still has its artifact — this is
/// what a rollback lands on.
pub async fn rollback_target(db: &Db, site_id: i64) -> Result<Option<Deployment>> {
    Ok(sqlx::query_as::<_, Deployment>(
        "SELECT * FROM deployments
         WHERE site_id = ? AND state = 'superseded' AND retired_at IS NULL
           AND promoted_at IS NOT NULL AND artifact_path IS NOT NULL
         ORDER BY id DESC LIMIT 1",
    )
    .bind(site_id)
    .fetch_optional(db)
    .await?)
}

pub async fn preview_deployment(
    db: &Db,
    site_id: i64,
    sha_prefix: &str,
) -> Result<Option<Deployment>> {
    Ok(sqlx::query_as::<_, Deployment>(
        "SELECT * FROM deployments
         WHERE site_id = ? AND commit_sha LIKE ? AND artifact_path IS NOT NULL
           AND state IN ('ready','live','superseded','verifying')
         ORDER BY id DESC LIMIT 1",
    )
    .bind(site_id)
    .bind(format!("{sha_prefix}%"))
    .fetch_optional(db)
    .await?)
}

/// Atomically hand the oldest queued job for this node to exactly one worker.
/// The `state = 'queued'` predicate in the UPDATE is the lock; two workers
/// racing here cannot both get a row.
pub async fn claim_job(db: &Db, node_id: &str) -> Result<Option<Deployment>> {
    let row = sqlx::query(
        "UPDATE deployments SET state = 'building', claimed_at = ?, started_at = ?
         WHERE id = (
           SELECT id FROM deployments
           WHERE state = 'queued' AND COALESCE(node_id,'local') = ?
           ORDER BY id LIMIT 1
         )
         RETURNING id",
    )
    .bind(now())
    .bind(now())
    .bind(node_id)
    .fetch_optional(db)
    .await?;
    match row {
        Some(r) => deployment(db, r.get::<i64, _>("id")).await,
        None => Ok(None),
    }
}

pub async fn set_deploy_state(db: &Db, id: i64, state: DeployState) -> Result<()> {
    sqlx::query("UPDATE deployments SET state = ? WHERE id = ?")
        .bind(state.as_str())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn finish_deployment(db: &Db, out: &BuildOutcome) -> Result<()> {
    if out.ok {
        sqlx::query(
            "UPDATE deployments SET state = 'verifying', artifact_path = ?, container_id = ?,
               container_port = ?, finished_at = ? WHERE id = ?",
        )
        .bind(out.artifact_path.as_deref())
        .bind(out.container_id.as_deref())
        .bind(out.container_port)
        .bind(now())
        .bind(out.deployment_id)
        .execute(db)
        .await?;
    } else {
        sqlx::query(
            "UPDATE deployments SET state = 'failed', error_summary = ?, finished_at = ? WHERE id = ?",
        )
        .bind(out.error_summary.as_deref())
        .bind(now())
        .bind(out.deployment_id)
        .execute(db)
        .await?;
    }
    Ok(())
}

pub async fn mark_ready(db: &Db, id: i64) -> Result<()> {
    sqlx::query("UPDATE deployments SET state = 'ready', ready_at = ? WHERE id = ?")
        .bind(now())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Deployments that passed proofing and are waiting for a quiet moment.
pub async fn awaiting_swap(db: &Db) -> Result<Vec<Deployment>> {
    Ok(sqlx::query_as::<_, Deployment>(
        "SELECT * FROM deployments WHERE state = 'ready' AND kind = 'production' ORDER BY id",
    )
    .fetch_all(db)
    .await?)
}

/// Promote in one transaction: the old live becomes the rollback target and the
/// new one takes the press. There is no moment where a site has no live
/// deployment, and no moment where it has two.
pub async fn promote(db: &Db, site_id: i64, deployment_id: i64) -> Result<Option<i64>> {
    let mut tx = db.begin().await?;

    let previous: Option<i64> = sqlx::query(
        "SELECT id FROM deployments WHERE site_id = ? AND state = 'live' ORDER BY id DESC LIMIT 1",
    )
    .bind(site_id)
    .fetch_optional(&mut *tx)
    .await?
    .map(|r| r.get::<i64, _>("id"));

    sqlx::query("UPDATE deployments SET state = 'superseded' WHERE site_id = ? AND state = 'live'")
        .bind(site_id)
        .execute(&mut *tx)
        .await?;

    sqlx::query("UPDATE deployments SET state = 'live', promoted_at = ? WHERE id = ?")
        .bind(now())
        .bind(deployment_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(previous)
}

pub async fn mark_retired(db: &Db, id: i64) -> Result<()> {
    sqlx::query("UPDATE deployments SET retired_at = ? WHERE id = ?")
        .bind(now())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Old deployments whose artifacts can be deleted, keeping `keep` per site plus
/// whatever is live or is the rollback target.
pub async fn prunable(db: &Db, keep: i64) -> Result<Vec<Deployment>> {
    Ok(sqlx::query_as::<_, Deployment>(
        "SELECT * FROM deployments d
         WHERE d.artifact_path IS NOT NULL
           AND d.state NOT IN ('live','ready','building','verifying','queued')
           AND d.retired_at IS NULL
           AND d.id NOT IN (
             SELECT id FROM deployments d2
             WHERE d2.site_id = d.site_id AND d2.artifact_path IS NOT NULL
             ORDER BY d2.id DESC LIMIT ?
           )",
    )
    .bind(keep)
    .fetch_all(db)
    .await?)
}

// ── logs ────────────────────────────────────────────────────────────────────

pub async fn append_logs(db: &Db, deployment_id: i64, lines: &[(String, String)]) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let start: i64 =
        sqlx::query("SELECT COALESCE(MAX(seq), -1) AS s FROM deploy_logs WHERE deployment_id = ?")
            .bind(deployment_id)
            .fetch_one(db)
            .await?
            .get::<i64, _>("s");

    let mut tx = db.begin().await?;
    for (i, (stream, line)) in lines.iter().enumerate() {
        sqlx::query(
            "INSERT INTO deploy_logs(deployment_id, seq, ts, stream, line) VALUES(?,?,?,?,?)",
        )
        .bind(deployment_id)
        .bind(start + 1 + i as i64)
        .bind(now())
        .bind(stream)
        .bind(line)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn logs_since(db: &Db, deployment_id: i64, after_seq: i64) -> Result<Vec<LogLine>> {
    Ok(sqlx::query_as::<_, LogLine>(
        "SELECT seq, ts, stream, line FROM deploy_logs
         WHERE deployment_id = ? AND seq > ? ORDER BY seq LIMIT 2000",
    )
    .bind(deployment_id)
    .bind(after_seq)
    .fetch_all(db)
    .await?)
}

pub async fn full_log(db: &Db, deployment_id: i64) -> Result<String> {
    let lines = sqlx::query_as::<_, LogLine>(
        "SELECT seq, ts, stream, line FROM deploy_logs WHERE deployment_id = ? ORDER BY seq",
    )
    .bind(deployment_id)
    .fetch_all(db)
    .await?;
    Ok(lines
        .into_iter()
        .map(|l| l.line)
        .collect::<Vec<_>>()
        .join("\n"))
}

// ── traffic ─────────────────────────────────────────────────────────────────

pub async fn bump_traffic(db: &Db, samples: &[(i64, String, i64)]) -> Result<()> {
    if samples.is_empty() {
        return Ok(());
    }
    let mut tx = db.begin().await?;
    for (site_id, bucket, hits) in samples {
        sqlx::query(
            "INSERT INTO traffic(site_id, bucket, hits) VALUES(?,?,?)
             ON CONFLICT(site_id, bucket) DO UPDATE SET hits = hits + excluded.hits",
        )
        .bind(site_id)
        .bind(bucket)
        .bind(hits)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Per-minute hit counts for this site over the last `days`, restricted to the
/// same hour-of-week as `hour_of_week`. Sorted ascending, ready for a quantile.
pub async fn traffic_profile(
    db: &Db,
    site_id: i64,
    hour_of_week: i64,
    days: i64,
) -> Result<Vec<f64>> {
    let since = (chrono::Utc::now() - chrono::Duration::days(days))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    let rows = sqlx::query("SELECT bucket, hits FROM traffic WHERE site_id = ? AND bucket >= ?")
        .bind(site_id)
        .bind(since)
        .fetch_all(db)
        .await?;

    let mut v: Vec<f64> = rows
        .into_iter()
        .filter_map(|r| {
            let bucket: String = r.get("bucket");
            let t = chrono::NaiveDateTime::parse_from_str(&bucket, "%Y-%m-%dT%H:%M").ok()?;
            let t = t.and_utc();
            (crate::util::hour_of_week(t) == hour_of_week).then(|| r.get::<i64, _>("hits") as f64)
        })
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Ok(v)
}

pub async fn hits_in_last_minutes(db: &Db, site_id: i64, minutes: i64) -> Result<i64> {
    let since = (chrono::Utc::now() - chrono::Duration::minutes(minutes))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    let row = sqlx::query(
        "SELECT COALESCE(SUM(hits), 0) AS n FROM traffic WHERE site_id = ? AND bucket >= ?",
    )
    .bind(site_id)
    .bind(since)
    .fetch_one(db)
    .await?;
    Ok(row.get::<i64, _>("n"))
}

pub async fn traffic_series(db: &Db, site_id: i64, minutes: i64) -> Result<Vec<(String, i64)>> {
    let since = (chrono::Utc::now() - chrono::Duration::minutes(minutes))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    let rows = sqlx::query(
        "SELECT bucket, hits FROM traffic WHERE site_id = ? AND bucket >= ? ORDER BY bucket",
    )
    .bind(site_id)
    .bind(since)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("bucket"), r.get::<i64, _>("hits")))
        .collect())
}

pub async fn prune_traffic(db: &Db, keep_days: i64) -> Result<()> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(keep_days))
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    sqlx::query("DELETE FROM traffic WHERE bucket < ?")
        .bind(cutoff)
        .execute(db)
        .await?;
    Ok(())
}

// ── heals & updates ─────────────────────────────────────────────────────────

pub async fn heal_count_today(db: &Db, repo: &str) -> Result<i64> {
    let since = (chrono::Utc::now() - chrono::Duration::hours(24))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let row =
        sqlx::query("SELECT COUNT(*) AS n FROM heals WHERE target_repo = ? AND created_at >= ?")
            .bind(repo)
            .bind(since)
            .fetch_one(db)
            .await?;
    Ok(row.get::<i64, _>("n"))
}

pub async fn heal_seen(db: &Db, fingerprint: &str) -> Result<bool> {
    let row = sqlx::query("SELECT COUNT(*) AS n FROM heals WHERE fingerprint = ?")
        .bind(fingerprint)
        .fetch_one(db)
        .await?;
    Ok(row.get::<i64, _>("n") > 0)
}

#[allow(clippy::too_many_arguments)]
pub async fn start_heal(
    db: &Db,
    kind: &str,
    site_id: Option<i64>,
    deployment_id: Option<i64>,
    repo: &str,
    sha: Option<&str>,
    fingerprint: &str,
    summary: &str,
) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO heals(kind, site_id, deployment_id, target_repo, commit_sha, fingerprint, state, summary, created_at)
         VALUES(?,?,?,?,?,?,'running',?,?)",
    )
    .bind(kind)
    .bind(site_id)
    .bind(deployment_id)
    .bind(repo)
    .bind(sha)
    .bind(fingerprint)
    .bind(summary)
    .bind(now())
    .execute(db)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn finish_heal(
    db: &Db,
    id: i64,
    state: &str,
    pr_url: Option<&str>,
    detail: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE heals SET state = ?, pr_url = ?, detail = ?, finished_at = ? WHERE id = ?")
        .bind(state)
        .bind(pr_url)
        .bind(detail)
        .bind(now())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn list_heals(db: &Db, limit: i64) -> Result<Vec<HealRecord>> {
    Ok(
        sqlx::query_as::<_, HealRecord>("SELECT * FROM heals ORDER BY id DESC LIMIT ?")
            .bind(limit)
            .fetch_all(db)
            .await?,
    )
}

pub async fn record_update(
    db: &Db,
    from: Option<&str>,
    to: Option<&str>,
    channel: &str,
    state: &str,
    detail: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO update_events(from_version, to_version, channel, state, detail, created_at)
         VALUES(?,?,?,?,?,?)",
    )
    .bind(from)
    .bind(to)
    .bind(channel)
    .bind(state)
    .bind(detail)
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn list_updates(db: &Db, limit: i64) -> Result<Vec<UpdateEvent>> {
    Ok(
        sqlx::query_as::<_, UpdateEvent>("SELECT * FROM update_events ORDER BY id DESC LIMIT ?")
            .bind(limit)
            .fetch_all(db)
            .await?,
    )
}

pub fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "openpages".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem() -> Db {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir.path().join("t.db")).await.unwrap();
        std::mem::forget(dir); // keep the file alive for the length of the test
        db
    }

    #[tokio::test]
    async fn a_job_is_claimed_by_exactly_one_worker() {
        let db = mem().await;
        ensure_local_node(&db, "podman").await.unwrap();
        let site = create_site(&db, "s", "o/r", "main", "local", "auto", "")
            .await
            .unwrap();
        create_deployment(&db, &site, "abc", None, "main", "production")
            .await
            .unwrap();

        let a = claim_job(&db, "local").await.unwrap();
        let b = claim_job(&db, "local").await.unwrap();
        assert!(a.is_some());
        assert!(b.is_none(), "a second worker must not get the same job");
    }

    #[tokio::test]
    async fn promotion_leaves_exactly_one_live_and_one_rollback_target() {
        let db = mem().await;
        ensure_local_node(&db, "podman").await.unwrap();
        let site = create_site(&db, "s", "o/r", "main", "local", "auto", "")
            .await
            .unwrap();

        let d1 = create_deployment(&db, &site, "aaa", None, "main", "production")
            .await
            .unwrap();
        finish_deployment(
            &db,
            &BuildOutcome {
                deployment_id: d1.id,
                ok: true,
                framework: None,
                artifact_path: Some("/tmp/a".into()),
                container_id: None,
                container_port: None,
                error_summary: None,
                incident: None,
            },
        )
        .await
        .unwrap();
        promote(&db, site.id, d1.id).await.unwrap();

        let d2 = create_deployment(&db, &site, "bbb", None, "main", "production")
            .await
            .unwrap();
        finish_deployment(
            &db,
            &BuildOutcome {
                deployment_id: d2.id,
                ok: true,
                framework: None,
                artifact_path: Some("/tmp/b".into()),
                container_id: None,
                container_port: None,
                error_summary: None,
                incident: None,
            },
        )
        .await
        .unwrap();
        let prev = promote(&db, site.id, d2.id).await.unwrap();

        assert_eq!(prev, Some(d1.id));
        assert_eq!(
            live_deployment(&db, site.id).await.unwrap().unwrap().id,
            d2.id
        );
        assert_eq!(
            rollback_target(&db, site.id).await.unwrap().unwrap().id,
            d1.id
        );
    }

    #[tokio::test]
    async fn a_node_token_authenticates_only_itself() {
        let db = mem().await;
        let (id, token) = enroll_node(&db, "builder-1").await.unwrap();
        assert_eq!(
            authenticate_node(&db, &token).await.unwrap().unwrap().id,
            id
        );
        assert!(authenticate_node(&db, &format!("{id}.wrong"))
            .await
            .unwrap()
            .is_none());
        assert!(authenticate_node(&db, "garbage").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sessions_expire_and_can_be_dropped() {
        let db = mem().await;
        let u = upsert_user(&db, "philip", Some(1), None, None, "owner")
            .await
            .unwrap();
        let t = create_session(&db, u.id, 30).await.unwrap();
        assert!(session_user(&db, &t).await.unwrap().is_some());
        drop_session(&db, &t).await.unwrap();
        assert!(session_user(&db, &t).await.unwrap().is_none());
    }
}
