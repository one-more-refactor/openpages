//! Clone → build → verify. No database, no network calls to the panel.
//!
//! This runs identically whether it was started by the panel's own worker or by
//! a remote node that received the job as JSON, which is the only reason a
//! remote node needs nothing but a container runtime and `git`.
//!
//! Nothing here promotes anything. A build that succeeds produces an artifact
//! that is *serveable but unserved*; deciding when it takes over is `swap`'s
//! job, and keeping those two apart is what makes "compile it now, switch later"
//! possible at all.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::detect;
use crate::gh;
use crate::incident::{distill, redact};
use crate::models::{BuildJob, BuildOutcome};
use crate::runtime::{self, ExecSpec, LogEvent, Runtime};

#[derive(Debug, Clone)]
pub struct BuildEnv {
    pub work_dir: PathBuf,
    pub deployments_dir: PathBuf,
    pub default_image: String,
    pub max_artifact_mb: u64,
    pub report_budget: usize,
}

pub type LogSink = mpsc::Sender<(String, String)>;

pub async fn run(job: &BuildJob, env: &BuildEnv, sink: LogSink) -> BuildOutcome {
    let secrets: Vec<String> = job.clone_token.iter().cloned().collect();
    let recorder = Recorder::new(sink, secrets.clone());

    match run_inner(job, env, &recorder).await {
        Ok(out) => out,
        Err(e) => {
            let msg = redact(&format!("{e:#}"), &secrets);
            recorder.say("stderr", &format!("openpages: {msg}")).await;
            fail(job, env, &recorder, &msg).await
        }
    }
}

async fn run_inner(job: &BuildJob, env: &BuildEnv, rec: &Recorder) -> Result<BuildOutcome> {
    let rt = Runtime::detect(&job.runtime).await?;
    rec.say(
        "stdout",
        &format!(
            "openpages: {} · {} · {}@{}",
            rt.name(),
            job.kind,
            job.repo,
            crate::config::short_sha(&job.commit_sha)
        ),
    )
    .await;

    let work = env.work_dir.join(format!("d{}", job.deployment_id));
    let _ = tokio::fs::remove_dir_all(&work).await;
    tokio::fs::create_dir_all(&work)
        .await
        .context("creating workspace")?;

    clone(job, &work, rec).await?;

    let src = if job.root_dir.trim_matches('/').is_empty() {
        work.clone()
    } else {
        work.join(job.root_dir.trim_matches('/'))
    };
    if !src.is_dir() {
        bail!(
            "root directory `{}` does not exist in this repository",
            job.root_dir
        );
    }

    let recipe = detect::detect(&src);
    rec.say(
        "stdout",
        &format!("openpages: detected {}", recipe.framework),
    )
    .await;

    let is_container = job.site_kind == "container" || recipe.kind == "container";
    let outcome = if is_container {
        container_build(job, env, rt, &src, rec).await?
    } else {
        static_build(job, env, rt, &recipe, &src, rec).await?
    };

    // The workspace is a clone plus a node_modules tree. Neither is worth
    // keeping: a heal re-clones, and disk on a small server is not free.
    let _ = tokio::fs::remove_dir_all(&work).await;
    Ok(outcome)
}

// ── clone ───────────────────────────────────────────────────────────────────

/// Fetch exactly one commit. Not the branch tip: between the webhook firing and
/// the build starting, the branch can move, and a deployment labelled `abc1234`
/// that actually contains `def5678` is a debugging session nobody deserves.
async fn clone(job: &BuildJob, work: &Path, rec: &Recorder) -> Result<()> {
    let url = gh::clone_url(&job.repo, job.clone_token.as_deref());
    git(work, &["init", "--quiet"], rec).await?;
    git(work, &["remote", "add", "origin", &url], rec).await?;
    git(
        work,
        &[
            "fetch",
            "--depth",
            "1",
            "--quiet",
            "origin",
            &job.commit_sha,
        ],
        rec,
    )
    .await
    .with_context(|| {
        format!(
            "fetching {} from {}",
            crate::config::short_sha(&job.commit_sha),
            job.repo
        )
    })?;
    git(
        work,
        &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
        rec,
    )
    .await?;

    if work.join(".gitmodules").is_file() {
        rec.say("stdout", "openpages: initialising submodules")
            .await;
        git(
            work,
            &[
                "submodule",
                "update",
                "--init",
                "--depth",
                "1",
                "--recursive",
            ],
            rec,
        )
        .await?;
    }
    Ok(())
}

async fn git(dir: &Path, args: &[&str], rec: &Recorder) -> Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .output()
        .await
        .context("running git — is it installed on this node?")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        rec.say("stderr", &err).await;
        bail!("git {} failed: {}", args[0], gh::first_line(&err));
    }
    Ok(())
}

// ── static ──────────────────────────────────────────────────────────────────

async fn static_build(
    job: &BuildJob,
    env: &BuildEnv,
    rt: Runtime,
    recipe: &detect::Recipe,
    src: &Path,
    rec: &Recorder,
) -> Result<BuildOutcome> {
    let install = job.install_cmd.clone().or_else(|| recipe.install.clone());
    let build = job.build_cmd.clone().or_else(|| recipe.build.clone());
    let image = job
        .build_image
        .clone()
        .unwrap_or_else(|| pick(&recipe.image, &env.default_image));

    let steps: Vec<String> = [install, build]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .collect();

    if steps.is_empty() {
        rec.say(
            "stdout",
            "openpages: no build step, publishing the repository as-is",
        )
        .await;
    } else {
        let script = format!("set -e\n{}", steps.join("\n"));
        rec.say("stdout", &format!("openpages: {image}")).await;
        for s in &steps {
            rec.say("stdout", &format!("$ {s}")).await;
        }

        let code = runtime::exec(
            rt,
            ExecSpec {
                image: &image,
                workspace: src,
                workdir: "",
                script: &script,
                env: &job.env,
                name: &format!("openpages-build-{}", job.deployment_id),
                timeout_secs: job.timeout_secs,
                network: true,
            },
            rec.container_sink(),
        )
        .await?;

        if code != 0 {
            let what = if code == 124 {
                "timed out".into()
            } else {
                format!("exited {code}")
            };
            bail!("build {what}");
        }
    }

    let rel = job
        .output_dir
        .clone()
        .or_else(|| recipe.output.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "could not work out which directory holds the built site — set \"Output directory\" on the site"
            )
        })?;

    let out_dir = src.join(rel.trim_matches('/'));
    verify_static(&out_dir, &rel, env.max_artifact_mb)?;

    let dest = env
        .deployments_dir
        .join(&job.site_slug)
        .join(job.deployment_id.to_string());
    place(&out_dir, &dest).await?;

    let (files, bytes) = measure(&dest);
    rec.say(
        "stdout",
        &format!(
            "openpages: {files} files, {:.1} MB ready to proof",
            bytes as f64 / 1_048_576.0
        ),
    )
    .await;

    Ok(BuildOutcome {
        deployment_id: job.deployment_id,
        ok: true,
        framework: Some(recipe.framework.clone()),
        artifact_path: Some(dest.to_string_lossy().to_string()),
        container_id: None,
        container_port: None,
        error_summary: None,
        incident: None,
    })
}

/// The checks that catch the mistake everybody makes: a build that "succeeded"
/// into the wrong directory. Publishing that produces a live site serving
/// nothing, with a green tick next to it.
fn verify_static(dir: &Path, rel: &str, max_mb: u64) -> Result<()> {
    if !dir.is_dir() {
        bail!("output directory `{rel}` does not exist after the build");
    }
    let (files, bytes) = measure(dir);
    if files == 0 {
        bail!("output directory `{rel}` is empty after the build");
    }
    if bytes > max_mb * 1_048_576 {
        bail!(
            "built site is {:.0} MB, over the {} MB limit for this node",
            bytes as f64 / 1_048_576.0,
            max_mb
        );
    }
    let has_html = walkdir::WalkDir::new(dir)
        .max_depth(4)
        .into_iter()
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().ends_with(".html"));
    if !has_html {
        bail!("output directory `{rel}` contains no HTML — this is usually the wrong directory");
    }
    Ok(())
}

// ── container ───────────────────────────────────────────────────────────────

async fn container_build(
    job: &BuildJob,
    _env: &BuildEnv,
    rt: Runtime,
    src: &Path,
    rec: &Recorder,
) -> Result<BuildOutcome> {
    let tag = format!(
        "openpages/{}:{}",
        job.site_slug,
        crate::config::short_sha(&job.commit_sha)
    );
    rec.say("stdout", &format!("openpages: building image {tag}"))
        .await;

    let code = runtime::build_image(
        rt,
        src,
        "Dockerfile",
        &tag,
        rec.container_sink(),
        job.timeout_secs,
    )
    .await?;
    if code != 0 {
        bail!(if code == 124 {
            "image build timed out".to_string()
        } else {
            format!("image build exited {code}")
        });
    }

    let port: u16 = job
        .env
        .iter()
        .find(|(k, _)| k == "PORT")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(3000);

    let name = format!("openpages-{}-{}", job.site_slug, job.deployment_id);
    let _ = runtime::force_remove(rt, &name).await;
    let (id, host_port) = runtime::run_detached(rt, &tag, &name, port, &job.env).await?;
    rec.say(
        "stdout",
        &format!(
            "openpages: container {} on 127.0.0.1:{host_port}",
            &id[..12.min(id.len())]
        ),
    )
    .await;

    // Proofing, for a container, means: does it actually answer?
    match wait_healthy(host_port, 90).await {
        Ok(status) => {
            rec.say(
                "stdout",
                &format!("openpages: health check returned {status}"),
            )
            .await
        }
        Err(e) => {
            let logs = container_logs(rt, &id).await;
            rec.say("stderr", &logs).await;
            let _ = runtime::stop(rt, &id, 5).await;
            bail!("{e}");
        }
    }

    Ok(BuildOutcome {
        deployment_id: job.deployment_id,
        ok: true,
        framework: Some("dockerfile".into()),
        artifact_path: None,
        container_id: Some(id),
        container_port: Some(host_port as i64),
        error_summary: None,
        incident: None,
    })
}

pub async fn wait_healthy(port: u16, secs: u64) -> Result<u16> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let url = format!("http://127.0.0.1:{port}/");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut last = String::from("no response");
    while std::time::Instant::now() < deadline {
        match client.get(&url).send().await {
            Ok(r) if r.status().as_u16() < 500 => return Ok(r.status().as_u16()),
            Ok(r) => last = format!("HTTP {}", r.status()),
            Err(e) => last = gh::first_line(&e.to_string()).to_string(),
        }
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    }
    bail!("container never became healthy on port {port} ({last})")
}

async fn container_logs(rt: Runtime, id: &str) -> String {
    Command::new(rt.bin())
        .args(["logs", "--tail", "80", id])
        .output()
        .await
        .map(|o| {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            s
        })
        .unwrap_or_default()
}

// ── artifacts ───────────────────────────────────────────────────────────────

/// Move the built directory into place. A rename within the same filesystem is
/// atomic and instant; the copy is the fallback for the case where the data dir
/// and the work dir are on different mounts.
async fn place(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let _ = tokio::fs::remove_dir_all(to).await;
    if tokio::fs::rename(from, to).await.is_ok() {
        return Ok(());
    }
    copy_tree(from, to).context("copying the built site into the deployment store")
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(from) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(from)?;
        let target = to.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(p) = target.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub fn measure(dir: &Path) -> (u64, u64) {
    let mut files = 0;
    let mut bytes = 0;
    for e in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if e.file_type().is_file() {
            files += 1;
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (files, bytes)
}

fn pick(preferred: &str, fallback: &str) -> String {
    if preferred.is_empty() {
        fallback.to_string()
    } else {
        preferred.to_string()
    }
}

// ── failure ─────────────────────────────────────────────────────────────────

async fn fail(job: &BuildJob, env: &BuildEnv, rec: &Recorder, why: &str) -> BuildOutcome {
    let log = rec.captured();
    let context =
        format!(
        "openpages could not build {} at commit {} (branch {}).\nThe build was: {}\nFailure: {}",
        job.repo,
        crate::config::short_sha(&job.commit_sha),
        job.branch,
        job.build_cmd.clone().unwrap_or_else(|| "auto-detected".into()),
        why
    );
    let inc = distill(&log, &context, env.report_budget);
    BuildOutcome {
        deployment_id: job.deployment_id,
        ok: false,
        framework: None,
        artifact_path: None,
        container_id: None,
        container_port: None,
        error_summary: Some(if inc.summary.is_empty() {
            why.to_string()
        } else {
            inc.summary.clone()
        }),
        incident: Some(inc.report),
    }
}

// ── log plumbing ────────────────────────────────────────────────────────────

/// Forwards every line to the caller *and* keeps the tail for distillation.
/// The cap is why a runaway build cannot exhaust memory on the way to failing.
struct Recorder {
    out: LogSink,
    buf: Arc<Mutex<Vec<String>>>,
    secrets: Vec<String>,
}

const KEEP_LINES: usize = 1200;

impl Recorder {
    fn new(out: LogSink, secrets: Vec<String>) -> Self {
        Self {
            out,
            buf: Arc::new(Mutex::new(Vec::new())),
            secrets,
        }
    }

    async fn say(&self, stream: &str, line: &str) {
        for l in line.lines() {
            let clean = redact(l, &self.secrets);
            self.record(&clean);
            let _ = self.out.send((stream.to_string(), clean)).await;
        }
    }

    fn record(&self, line: &str) {
        let mut b = self.buf.lock().unwrap();
        if b.len() >= KEEP_LINES {
            b.drain(0..KEEP_LINES / 4);
        }
        b.push(line.to_string());
    }

    fn captured(&self) -> String {
        self.buf.lock().unwrap().join("\n")
    }

    /// A sender the container runtime can write into.
    fn container_sink(&self) -> mpsc::Sender<LogEvent> {
        let (tx, mut rx) = mpsc::channel::<LogEvent>(512);
        let out = self.out.clone();
        let buf = self.buf.clone();
        let secrets = self.secrets.clone();
        tokio::spawn(async move {
            while let Some(LogEvent::Line { stream, line }) = rx.recv().await {
                let clean = redact(&line, &secrets);
                {
                    let mut b = buf.lock().unwrap();
                    if b.len() >= KEEP_LINES {
                        b.drain(0..KEEP_LINES / 4);
                    }
                    b.push(clean.clone());
                }
                if out.send((stream.to_string(), clean)).await.is_err() {
                    break;
                }
            }
        });
        tx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (n, b) in files {
            let p = d.path().join(n);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b).unwrap();
        }
        d
    }

    #[test]
    fn verification_rejects_a_missing_output_directory() {
        let d = tempfile::tempdir().unwrap();
        let err = verify_static(&d.path().join("dist"), "dist", 100)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn verification_rejects_an_empty_output_directory() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("dist")).unwrap();
        let err = verify_static(&d.path().join("dist"), "dist", 100)
            .unwrap_err()
            .to_string();
        assert!(err.contains("is empty"), "{err}");
    }

    #[test]
    fn verification_rejects_a_directory_with_no_html_at_all() {
        let d = dir_with(&[("dist/bundle.js", "console.log(1)")]);
        let err = verify_static(&d.path().join("dist"), "dist", 100)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no HTML"), "{err}");
    }

    #[test]
    fn verification_accepts_a_real_built_site() {
        let d = dir_with(&[
            ("dist/index.html", "<h1>hi</h1>"),
            ("dist/app.css", "body{}"),
        ]);
        assert!(verify_static(&d.path().join("dist"), "dist", 100).is_ok());
    }

    #[test]
    fn verification_enforces_the_size_cap() {
        let d = dir_with(&[("dist/index.html", &"x".repeat(3_000_000))]);
        let err = verify_static(&d.path().join("dist"), "dist", 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("over the 1 MB limit"), "{err}");
    }

    #[tokio::test]
    async fn placing_an_artifact_moves_the_whole_tree() {
        let src = dir_with(&[("dist/index.html", "hi"), ("dist/a/b.css", "x")]);
        let dest = tempfile::tempdir().unwrap();
        let to = dest.path().join("site/1");
        place(&src.path().join("dist"), &to).await.unwrap();
        assert!(to.join("index.html").is_file());
        assert!(to.join("a/b.css").is_file());
    }

    #[test]
    fn measure_counts_only_files() {
        let d = dir_with(&[("a/index.html", "12345"), ("a/b/c.css", "12")]);
        let (files, bytes) = measure(d.path());
        assert_eq!(files, 2);
        assert_eq!(bytes, 7);
    }
}
