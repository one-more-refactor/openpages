//! Everything GitHub, done by shelling out to the official `gh` CLI.
//!
//! The alternative is making every person who self-hosts this register a GitHub
//! App, copy a private key onto the server and configure a callback URL before
//! they can deploy their first page. `gh` already solved authentication, it is
//! packaged everywhere, its token is refreshed and stored by someone else, and
//! `gh auth login` is a flow people have already done.
//!
//! Everything in here goes through `run()`, so swapping in a GitHub App later
//! is one impl away — see docs/GITHUB.md.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Gh {
    pub bin: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GhUser {
    pub login: String,
    pub id: i64,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GhRepo {
    #[serde(rename = "nameWithOwner")]
    pub name_with_owner: String,
    #[serde(rename = "defaultBranchRef")]
    pub default_branch_ref: Option<BranchRef>,
    #[serde(rename = "isPrivate")]
    pub is_private: bool,
    #[serde(rename = "pushedAt")]
    pub pushed_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BranchRef {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub message: String,
}

impl Gh {
    pub fn new(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        self.run_in(None, args).await
    }

    async fn run_in(&self, cwd: Option<&Path>, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("CLICOLOR", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let out = cmd
            .output()
            .await
            .with_context(|| format!("running `{} {}`", self.bin, args.join(" ")))?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            bail!(
                "gh {}: {}",
                args.first().copied().unwrap_or(""),
                first_line(&err)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    async fn api<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let raw = self.run(&["api", path]).await?;
        serde_json::from_str(&raw).with_context(|| format!("parsing response of `gh api {path}`"))
    }

    /// Same, but "GitHub has no such thing" is an answer rather than a failure.
    /// A repository with no releases yet is the normal state of a new project,
    /// and reporting it as an error makes `openpages update check` look broken
    /// on the day it is most likely to be run.
    async fn api_opt<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<Option<T>> {
        match self.run(&["api", path]).await {
            Ok(raw) => {
                Ok(Some(serde_json::from_str(&raw).with_context(|| {
                    format!("parsing response of `gh api {path}`")
                })?))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn installed(&self) -> bool {
        Command::new(&self.bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// The token `gh` currently holds. Short-lived in the sense that it is
    /// never stored by openpages: it is fetched per operation and handed to a
    /// build container only for the duration of a clone.
    pub async fn token(&self) -> Result<String> {
        let t = self.run(&["auth", "token"]).await?.trim().to_string();
        if t.is_empty() {
            bail!("gh has no token — run `openpages login` on this server");
        }
        Ok(t)
    }

    pub async fn whoami(&self) -> Result<GhUser> {
        self.api::<GhUser>("user").await
    }

    /// Repos the authenticated account can push to, newest activity first.
    pub async fn list_repos(&self, limit: usize) -> Result<Vec<GhRepo>> {
        let limit = limit.to_string();
        let raw = self
            .run(&[
                "repo",
                "list",
                "--limit",
                &limit,
                "--source",
                "--json",
                "nameWithOwner,defaultBranchRef,isPrivate,pushedAt",
            ])
            .await?;
        serde_json::from_str(&raw).context("parsing repo list")
    }

    pub async fn default_branch(&self, repo: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct R {
            default_branch: String,
        }
        Ok(self
            .api::<R>(&format!("repos/{repo}"))
            .await?
            .default_branch)
    }

    pub async fn head_commit(&self, repo: &str, branch: &str) -> Result<Commit> {
        #[derive(Deserialize)]
        struct C {
            sha: String,
            commit: Inner,
        }
        #[derive(Deserialize)]
        struct Inner {
            message: String,
        }
        let c: C = self
            .api(&format!(
                "repos/{repo}/commits/{}",
                urlencoding::encode(branch)
            ))
            .await
            .with_context(|| format!("reading {repo}@{branch}"))?;
        Ok(Commit {
            sha: c.sha,
            message: first_line(&c.commit.message).to_string(),
        })
    }

    /// Returns the hook id. Idempotent-ish: an existing hook with the same URL
    /// is reused rather than duplicated, because re-adding a site should not
    /// leave a repo with six webhooks.
    pub async fn ensure_webhook(&self, repo: &str, url: &str, secret: &str) -> Result<i64> {
        #[derive(Deserialize)]
        struct Hook {
            id: i64,
            config: HookConfig,
        }
        #[derive(Deserialize)]
        struct HookConfig {
            url: Option<String>,
        }

        if let Ok(existing) = self.api::<Vec<Hook>>(&format!("repos/{repo}/hooks")).await {
            if let Some(h) = existing
                .iter()
                .find(|h| h.config.url.as_deref() == Some(url))
            {
                return Ok(h.id);
            }
        }

        let body = serde_json::json!({
            "name": "web",
            "active": true,
            "events": ["push"],
            "config": { "url": url, "content_type": "json", "secret": secret, "insecure_ssl": "0" }
        })
        .to_string();

        let raw = self
            .post_json(&format!("repos/{repo}/hooks"), &body)
            .await?;

        #[derive(Deserialize)]
        struct Created {
            id: i64,
        }
        Ok(serde_json::from_str::<Created>(&raw)
            .context("parsing created webhook")?
            .id)
    }

    pub async fn delete_webhook(&self, repo: &str, id: i64) -> Result<()> {
        self.run(&[
            "api",
            "--method",
            "DELETE",
            &format!("repos/{repo}/hooks/{id}"),
        ])
        .await?;
        Ok(())
    }

    async fn post_json(&self, path: &str, body: &str) -> Result<String> {
        use tokio::io::AsyncWriteExt;
        let mut child = Command::new(&self.bin)
            .args(["api", "--method", "POST", path, "--input", "-"])
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning gh api")?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("no stdin on gh"))?
            .write_all(body.as_bytes())
            .await?;
        let out = child.wait_with_output().await?;
        if !out.status.success() {
            bail!(
                "gh api {path}: {}",
                first_line(&String::from_utf8_lossy(&out.stderr))
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// Open a pull request from an already-pushed branch. Returns its URL.
    pub async fn create_pr(
        &self,
        dir: &Path,
        base: &str,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<String> {
        let out = self
            .run_in(
                Some(dir),
                &[
                    "pr", "create", "--base", base, "--head", head, "--title", title, "--body",
                    body,
                ],
            )
            .await?;
        out.lines()
            .rev()
            .find(|l| l.contains("github.com"))
            .map(|s| s.trim().to_string())
            .ok_or_else(|| anyhow!("gh pr create printed no URL"))
    }

    /// `None` means the repository exists but has published nothing yet.
    pub async fn latest_release(&self, repo: &str) -> Result<Option<Release>> {
        #[derive(Deserialize)]
        struct R {
            tag_name: String,
            assets: Vec<Asset>,
            #[serde(default)]
            prerelease: bool,
        }
        Ok(self
            .api_opt::<R>(&format!("repos/{repo}/releases/latest"))
            .await?
            .map(|r| Release {
                tag: r.tag_name,
                prerelease: r.prerelease,
                assets: r.assets,
            }))
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    let text = e.to_string();
    text.contains("HTTP 404") || text.contains("Not Found")
}

#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

/// `https://x-access-token:<token>@github.com/owner/repo.git`
///
/// This string is a credential. It is passed to the build container as an
/// argument to a single `git clone` and never written to a log or the database
/// — see `buildpipe::redact`.
pub fn clone_url(repo: &str, token: Option<&str>) -> String {
    match token {
        Some(t) => format!("https://x-access-token:{t}@github.com/{repo}.git"),
        None => format!("https://github.com/{repo}.git"),
    }
}

pub fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_url_embeds_the_token_only_when_there_is_one() {
        assert_eq!(clone_url("a/b", None), "https://github.com/a/b.git");
        assert!(clone_url("a/b", Some("ghs_x")).contains("x-access-token:ghs_x@"));
    }

    #[test]
    fn first_line_trims_multiline_errors() {
        assert_eq!(first_line("boom\ndetails\nmore"), "boom");
        assert_eq!(first_line(""), "");
    }
}
