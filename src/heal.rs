//! Self-healing: when a deployment breaks, put the site back first, then ask
//! Claude to draft a fix.
//!
//! Three rules make this safe enough to leave switched on:
//!
//! 1. **Recovery never waits for it.** The rollback has already happened by the
//!    time anything here runs. If the model is slow, absent, or wrong, the site
//!    is already back up.
//! 2. **It is handed a report, not a log.** See `incident::distill` — a couple
//!    of kilobytes of the part that matters, capped, with the credentials
//!    stripped. This is a cost decision and an accuracy one.
//! 3. **It cannot loop.** The same failure fingerprint is never healed twice,
//!    and each repository has a daily budget. A broken build that a fix does
//!    not repair costs one attempt, not one per push forever.
//!
//! The default mode opens a pull request. `direct` pushes to the branch, which
//! means openpages builds its own fix and can deploy it — powerful, and off by
//! default for exactly that reason.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;

use crate::db;
use crate::gh;
use crate::incident::{distill, Incident};
use crate::state::Shared;

pub struct Trigger {
    pub kind: &'static str,
    pub site_id: Option<i64>,
    pub deployment_id: Option<i64>,
    pub repo: String,
    pub branch: String,
    pub commit_sha: Option<String>,
    pub incident: Incident,
    /// Site-level override: inherit | off | pr | direct
    pub mode: String,
}

/// A build failed. The site is untouched (a failed build never went live), so
/// there is nothing to roll back — just draft the fix.
#[allow(clippy::too_many_arguments)]
pub async fn on_build_failure(
    state: Shared,
    site_id: i64,
    deployment_id: i64,
    repo: String,
    branch: String,
    sha: String,
    report: String,
    summary: String,
    site_mode: String,
) {
    let incident = Incident {
        summary,
        fingerprint: crate::util::sha256_hex(&report)[..16].to_string(),
        report,
        files: vec![],
    };
    let t = Trigger {
        kind: "site_build",
        site_id: Some(site_id),
        deployment_id: Some(deployment_id),
        repo,
        branch,
        commit_sha: Some(sha),
        incident,
        mode: site_mode,
    };
    if let Err(e) = attempt(&state, t).await {
        tracing::warn!("heal: {e:#}");
    }
}

/// A live container died and we already rolled back.
pub async fn on_runtime_failure(
    state: Shared,
    site_id: i64,
    deployment_id: i64,
    summary: String,
    rolled_to: Option<i64>,
) {
    let Ok(Some(site)) = db::site(&state.db, site_id).await else {
        return;
    };
    let sha = db::deployment(&state.db, deployment_id)
        .await
        .ok()
        .flatten()
        .map(|d| d.commit_sha);

    let context = format!(
        "{summary}\nopenpages rolled the site back to deployment {}.",
        rolled_to
            .map(|i| i.to_string())
            .unwrap_or_else(|| "— (nothing to roll back to)".into())
    );
    let incident = distill("", &context, state.cfg.heal.report_budget_chars);
    let t = Trigger {
        kind: "runtime",
        site_id: Some(site_id),
        deployment_id: Some(deployment_id),
        repo: site.repo.clone(),
        branch: site.branch.clone(),
        commit_sha: sha,
        incident: Incident {
            summary,
            ..incident
        },
        mode: site.heal_mode.clone(),
    };
    if let Err(e) = attempt(&state, t).await {
        tracing::warn!("heal: {e:#}");
    }
}

/// openpages could not build or verify its own next version.
pub async fn on_update_failure(state: Shared, to_version: String, log: String) {
    if !state.cfg.heal.heal_self {
        return;
    }
    let context = format!(
        "openpages could not build and verify its own version {to_version}. \
         The running version is unaffected and stayed on press."
    );
    let incident = distill(&log, &context, state.cfg.heal.report_budget_chars);
    let t = Trigger {
        kind: "self_update",
        site_id: None,
        deployment_id: None,
        repo: state.cfg.update.repo.clone(),
        branch: "main".into(),
        commit_sha: None,
        incident,
        mode: "pr".into(), // never push straight to the tool that runs everything
    };
    if let Err(e) = attempt(&state, t).await {
        tracing::warn!("self-heal: {e:#}");
    }
}

// ── the attempt ─────────────────────────────────────────────────────────────

async fn attempt(state: &Shared, t: Trigger) -> Result<()> {
    let mode = effective_mode(&state.cfg.heal.mode, &t.mode, state.cfg.heal.enabled);
    let Some(mode) = mode else { return Ok(()) };

    if db::heal_seen(&state.db, &t.incident.fingerprint).await? {
        tracing::info!(repo = %t.repo, "heal skipped: this exact failure has been attempted before");
        return Ok(());
    }
    let used = db::heal_count_today(&state.db, &t.repo).await?;
    if used >= state.cfg.heal.max_per_repo_per_day {
        tracing::warn!(repo = %t.repo, "heal skipped: daily budget of {used} reached");
        return Ok(());
    }

    let id = db::start_heal(
        &state.db,
        t.kind,
        t.site_id,
        t.deployment_id,
        &t.repo,
        t.commit_sha.as_deref(),
        &t.incident.fingerprint,
        &t.incident.summary,
    )
    .await?;

    match run(state, &t, mode, id).await {
        Ok(Outcome::Pr { url, note }) => {
            db::finish_heal(&state.db, id, "opened_pr", Some(&url), Some(&note)).await?;
            tracing::info!(repo = %t.repo, "heal opened {url}");
        }
        Ok(Outcome::Pushed { branch, note }) => {
            db::finish_heal(
                &state.db,
                id,
                "pushed",
                None,
                Some(&format!("pushed to {branch}: {note}")),
            )
            .await?;
        }
        Ok(Outcome::NoFix { note }) => {
            db::finish_heal(&state.db, id, "no_fix", None, Some(&note)).await?;
        }
        Err(e) => {
            db::finish_heal(&state.db, id, "error", None, Some(&format!("{e:#}"))).await?;
            tracing::warn!(repo = %t.repo, "heal failed: {e:#}");
        }
    }
    Ok(())
}

/// `inherit` on a site defers to the global setting; anything else overrides
/// it. The global kill switch beats both.
pub fn effective_mode(global: &str, site: &str, enabled: bool) -> Option<&'static str> {
    if !enabled {
        return None;
    }
    let chosen = if site == "inherit" || site.is_empty() {
        global
    } else {
        site
    };
    match chosen {
        "pr" => Some("pr"),
        "direct" => Some("direct"),
        _ => None,
    }
}

enum Outcome {
    Pr { url: String, note: String },
    Pushed { branch: String, note: String },
    NoFix { note: String },
}

async fn run(state: &Shared, t: &Trigger, mode: &'static str, heal_id: i64) -> Result<Outcome> {
    let dir = state.cfg.heal_dir().join(format!("h{heal_id}"));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await?;

    let token = state.gh.token().await.ok();
    let url = gh::clone_url(&t.repo, token.as_deref());

    // Shallow but not depth-1: a fix often needs to see what recently changed,
    // and `git log` on a single commit tells you nothing.
    sh(
        &dir,
        "git",
        &["clone", "--depth", "30", "--branch", &t.branch, &url, "."],
        None,
    )
    .await
    .context("cloning the repository to work in")?;

    if let Some(sha) = &t.commit_sha {
        let _ = sh(&dir, "git", &["checkout", "--quiet", sha], None).await;
    }

    let before = sh(&dir, "git", &["rev-parse", "HEAD"], None)
        .await?
        .trim()
        .to_string();
    let note = ask_claude(state, &dir, &t.incident).await?;

    let dirty = sh(&dir, "git", &["status", "--porcelain"], None).await?;
    if dirty.trim().is_empty() {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Ok(Outcome::NoFix { note });
    }

    let short = crate::config::short_sha(&t.commit_sha.clone().unwrap_or(before));
    let branch = format!("openpages/fix-{short}");
    let title = format!(
        "fix: {}",
        crate::incident::first_sentence(&t.incident.summary)
    );

    sh(&dir, "git", &["checkout", "-b", &branch], None).await?;
    sh(&dir, "git", &["add", "-A"], None).await?;
    sh(
        &dir,
        "git",
        &[
            "-c",
            "user.name=openpages",
            "-c",
            "user.email=openpages@localhost",
            "commit",
            "-m",
            &format!("{title}\n\n{note}"),
        ],
        None,
    )
    .await?;

    let target = if mode == "direct" {
        t.branch.clone()
    } else {
        branch.clone()
    };
    let refspec = format!("HEAD:refs/heads/{target}");
    sh(
        &dir,
        "git",
        &["push", "--force-with-lease", "origin", &refspec],
        None,
    )
    .await
    .context("pushing the fix")?;

    if mode == "direct" {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Ok(Outcome::Pushed {
            branch: target,
            note,
        });
    }

    let body = pr_body(t, &note);
    let url = state
        .gh
        .create_pr(&dir, &t.branch, &branch, &title, &body)
        .await;
    let _ = tokio::fs::remove_dir_all(&dir).await;

    match url {
        Ok(url) => Ok(Outcome::Pr { url, note }),
        Err(e) => Ok(Outcome::Pushed {
            branch,
            note: format!("{note} (branch pushed; opening the PR failed: {e})"),
        }),
    }
}

fn pr_body(t: &Trigger, note: &str) -> String {
    format!(
        "{note}\n\n\
         ---\n\n\
         Opened automatically by **openpages** on `{host}` after a failed deployment.\n\
         The change was drafted by the Claude CLI running on that machine; nothing was sent anywhere else.\n\n\
         <details><summary>What broke</summary>\n\n```\n{report}\n```\n\n</details>\n\n\
         The site was not left broken — openpages kept the last working deployment on press.\n",
        host = crate::db::hostname(),
        report = t.incident.report.replace("```", "``\u{200b}`"),
    )
}

async fn ask_claude(state: &Shared, dir: &Path, inc: &Incident) -> Result<String> {
    let cfg = &state.cfg.heal;
    let prompt = format!(
        "A deployment of this repository failed. You are in a clean checkout of it.\n\n\
         {}\n\n\
         Fix the cause, then stop. Rules:\n\
         - Change as little as possible. Do not refactor, reformat, or tidy anything unrelated.\n\
         - Do not touch CI config, licences, or secrets.\n\
         - A missing dependency belongs in the manifest, not vendored in.\n\
         - If you cannot find a safe fix, change nothing at all.\n\n\
         Answer with one sentence describing what you changed, or why you changed nothing.",
        inc.report
    );

    let mut cmd = Command::new(&cfg.claude_bin);
    cmd.args(&cfg.claude_args)
        .arg(&prompt)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = cmd.spawn().with_context(|| {
        format!(
            "starting `{}` — is the Claude CLI installed and on PATH for the openpages user?",
            cfg.claude_bin
        )
    })?;

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(cfg.timeout_secs),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the Claude CLI did not finish within {}s", cfg.timeout_secs))??;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() && stdout.trim().is_empty() {
        bail!(
            "claude exited {}: {}",
            out.status.code().unwrap_or(-1),
            gh::first_line(&String::from_utf8_lossy(&out.stderr))
        );
    }
    Ok(parse_reply(&stdout))
}

/// `--output-format json` gives an object with a `result` string, but the flag
/// set is configurable, so accept plain text too rather than failing on a
/// perfectly good answer.
pub fn parse_reply(stdout: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        for key in ["result", "text", "content", "message"] {
            if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                return s.trim().to_string();
            }
        }
    }
    let text = stdout.trim();
    if text.is_empty() {
        return "no description given".into();
    }
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(text)
        .trim()
        .to_string()
}

async fn sh(dir: &PathBuf, bin: &str, args: &[&str], env: Option<(&str, &str)>) -> Result<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some((k, v)) = env {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .await
        .with_context(|| format!("running {bin}"))?;
    if !out.status.success() {
        bail!(
            "{bin} {}: {}",
            args.first().copied().unwrap_or(""),
            gh::first_line(&String::from_utf8_lossy(&out.stderr))
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_global_kill_switch_beats_every_site_setting() {
        assert_eq!(effective_mode("pr", "direct", false), None);
        assert_eq!(effective_mode("direct", "direct", false), None);
    }

    #[test]
    fn a_site_can_opt_out_of_a_globally_enabled_healer() {
        assert_eq!(effective_mode("pr", "off", true), None);
    }

    #[test]
    fn inherit_defers_to_the_global_setting() {
        assert_eq!(effective_mode("pr", "inherit", true), Some("pr"));
        assert_eq!(effective_mode("direct", "inherit", true), Some("direct"));
        assert_eq!(effective_mode("off", "inherit", true), None);
    }

    #[test]
    fn a_site_can_choose_direct_when_the_default_is_a_pull_request() {
        assert_eq!(effective_mode("pr", "direct", true), Some("direct"));
    }

    #[test]
    fn claude_json_output_is_unwrapped() {
        let json = r#"{"type":"result","result":"Added the missing dependency.","cost_usd":0.01}"#;
        assert_eq!(parse_reply(json), "Added the missing dependency.");
    }

    #[test]
    fn plain_text_output_still_works() {
        assert_eq!(
            parse_reply("Fixed the import path.\n"),
            "Fixed the import path."
        );
        assert_eq!(parse_reply("  \n"), "no description given");
    }
}
