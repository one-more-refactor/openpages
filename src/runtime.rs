//! The container runtime, which is either Docker or rootless Podman and is
//! chosen once per node.
//!
//! The two are not quite interchangeable and pretending otherwise is how you
//! get artifacts owned by root. Under rootless Podman, root inside the
//! container *is* the invoking user outside it, so a build writes files the
//! server can read. Under Docker the container's root is the host's root, so we
//! pin `--user` instead. Both need `HOME` pointed somewhere writable, because
//! package managers will otherwise try to write to `/` and fail in ways whose
//! error messages say nothing about permissions.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Docker,
    Podman,
}

impl Runtime {
    pub fn bin(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }

    pub fn name(self) -> &'static str {
        self.bin()
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "docker" => Some(Self::Docker),
            "podman" => Some(Self::Podman),
            _ => None,
        }
    }

    /// `auto` prefers Podman: rootless by default, no daemon, and the file
    /// ownership story above is the one that does not need a workaround.
    pub async fn detect(preference: &str) -> Result<Self> {
        if let Some(explicit) = Self::parse(preference) {
            if !explicit.works().await {
                bail!(
                    "runtime `{}` was requested but `{} info` failed — is it installed and running?",
                    explicit.bin(),
                    explicit.bin()
                );
            }
            return Ok(explicit);
        }
        for candidate in [Self::Podman, Self::Docker] {
            if candidate.works().await {
                return Ok(candidate);
            }
        }
        bail!("no container runtime found — install podman (recommended) or docker")
    }

    pub async fn works(self) -> bool {
        Command::new(self.bin())
            .args(["info", "--format", "{{.Host.Arch}}"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
            || Command::new(self.bin())
                .arg("info")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false)
    }

    /// Rootless Podman maps container-root to the invoking user; Docker does
    /// not, so it gets an explicit `--user`.
    fn identity_args(self) -> Vec<String> {
        match self {
            Self::Podman => vec![],
            Self::Docker => {
                let uid = unsafe { libc::getuid() };
                let gid = unsafe { libc::getgid() };
                vec!["--user".into(), format!("{uid}:{gid}")]
            }
        }
    }
}

pub struct ExecSpec<'a> {
    pub image: &'a str,
    pub workspace: &'a Path,
    /// Relative to the workspace root.
    pub workdir: &'a str,
    pub script: &'a str,
    pub env: &'a [(String, String)],
    pub name: &'a str,
    pub timeout_secs: u64,
    pub network: bool,
}

#[derive(Debug, Clone)]
pub enum LogEvent {
    Line { stream: &'static str, line: String },
}

/// Run a shell script inside a container over a bind-mounted workspace,
/// streaming every line out as it appears. Returns the exit code.
pub async fn exec(rt: Runtime, spec: ExecSpec<'_>, sink: mpsc::Sender<LogEvent>) -> Result<i32> {
    let ws = spec.workspace.to_string_lossy().to_string();
    // :Z relabels for SELinux hosts. Harmless where SELinux is not enforcing,
    // and the difference between "it works" and a bare EACCES where it is.
    let mount = format!("{ws}:/workspace:rw,Z");
    let workdir = if spec.workdir.is_empty() {
        "/workspace".to_string()
    } else {
        format!("/workspace/{}", spec.workdir.trim_matches('/'))
    };

    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        spec.name.to_string(),
        "-v".into(),
        mount,
        "-w".into(),
        workdir,
    ];
    args.extend(rt.identity_args());
    if !spec.network {
        args.push("--network".into());
        args.push("none".into());
    }

    // Sane defaults for every package manager we might land in.
    for (k, v) in [
        ("HOME", "/workspace/.openpages-home"),
        ("CI", "1"),
        ("NPM_CONFIG_CACHE", "/workspace/.openpages-home/.npm"),
        ("NPM_CONFIG_UPDATE_NOTIFIER", "false"),
        ("NPM_CONFIG_FUND", "false"),
        ("NPM_CONFIG_AUDIT", "false"),
        ("YARN_CACHE_FOLDER", "/workspace/.openpages-home/.yarn"),
        ("XDG_CACHE_HOME", "/workspace/.openpages-home/.cache"),
        ("PNPM_HOME", "/workspace/.openpages-home/.pnpm"),
        ("TERM", "dumb"),
        ("NO_COLOR", "1"),
    ] {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    for (k, v) in spec.env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }

    args.push(spec.image.to_string());
    args.push("sh".into());
    args.push("-c".into());
    args.push(spec.script.to_string());

    let mut child = Command::new(rt.bin())
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {}", rt.bin()))?;

    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    pump(stdout, "stdout", sink.clone());
    pump(stderr, "stderr", sink.clone());

    let deadline = std::time::Duration::from_secs(spec.timeout_secs);
    match tokio::time::timeout(deadline, child.wait()).await {
        Ok(status) => Ok(status?.code().unwrap_or(-1)),
        Err(_) => {
            let _ = sink
                .send(LogEvent::Line {
                    stream: "stderr",
                    line: format!(
                        "openpages: timed out after {}s, killing container",
                        spec.timeout_secs
                    ),
                })
                .await;
            let _ = force_remove(rt, spec.name).await;
            let _ = child.kill().await;
            Ok(124)
        }
    }
}

fn pump<R>(reader: R, stream: &'static str, sink: mpsc::Sender<LogEvent>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if sink.send(LogEvent::Line { stream, line }).await.is_err() {
                break;
            }
        }
    });
}

pub async fn force_remove(rt: Runtime, name: &str) -> Result<()> {
    let _ = Command::new(rt.bin())
        .args(["rm", "-f", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    Ok(())
}

/// Build an image from a repo that carries its own Dockerfile.
pub async fn build_image(
    rt: Runtime,
    context: &Path,
    dockerfile: &str,
    tag: &str,
    sink: mpsc::Sender<LogEvent>,
    timeout_secs: u64,
) -> Result<i32> {
    let mut child = Command::new(rt.bin())
        .args([
            "build",
            "-t",
            tag,
            "-f",
            dockerfile,
            &context.to_string_lossy(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawning image build")?;

    pump(child.stdout.take().expect("piped"), "stdout", sink.clone());
    pump(child.stderr.take().expect("piped"), "stderr", sink.clone());

    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(s) => Ok(s?.code().unwrap_or(-1)),
        Err(_) => {
            let _ = child.kill().await;
            Ok(124)
        }
    }
}

/// Start a long-running container and return its id plus the host port it was
/// published on. The port is chosen by the kernel (`-p 127.0.0.1::<port>`)
/// rather than by us, which removes an entire class of race between two
/// deployments starting at the same time.
pub async fn run_detached(
    rt: Runtime,
    image: &str,
    name: &str,
    container_port: u16,
    env: &[(String, String)],
) -> Result<(String, u16)> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        name.into(),
        "--restart".into(),
        "unless-stopped".into(),
        "-p".into(),
        format!("127.0.0.1::{container_port}"),
    ];
    for (k, v) in env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    args.push("-e".into());
    args.push(format!("PORT={container_port}"));
    args.push(image.into());

    let out = Command::new(rt.bin()).args(&args).output().await?;
    if !out.status.success() {
        bail!(
            "{} run: {}",
            rt.bin(),
            crate::gh::first_line(&String::from_utf8_lossy(&out.stderr))
        );
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let port = published_port(rt, &id, container_port).await?;
    Ok((id, port))
}

pub async fn published_port(rt: Runtime, id: &str, container_port: u16) -> Result<u16> {
    let out = Command::new(rt.bin())
        .args(["port", id, &format!("{container_port}/tcp")])
        .output()
        .await?;
    if !out.status.success() {
        bail!("could not read published port for {id}");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter_map(|l| l.rsplit(':').next())
        .find_map(|p| p.trim().parse::<u16>().ok())
        .context("no published port in runtime output")
}

pub async fn stop(rt: Runtime, id: &str, timeout_secs: u64) -> Result<()> {
    let _ = Command::new(rt.bin())
        .args(["stop", "-t", &timeout_secs.to_string(), id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    let _ = Command::new(rt.bin())
        .args(["rm", "-f", id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    Ok(())
}

pub async fn is_running(rt: Runtime, id: &str) -> bool {
    Command::new(rt.bin())
        .args(["inspect", "-f", "{{.State.Running}}", id])
        .output()
        .await
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_names_match_their_binaries() {
        assert_eq!(Runtime::Podman.bin(), "podman");
        assert_eq!(Runtime::Docker.bin(), "docker");
        assert_eq!(Runtime::parse("podman"), Some(Runtime::Podman));
        assert_eq!(Runtime::parse("auto"), None);
    }

    #[test]
    fn only_docker_needs_an_explicit_user() {
        assert!(Runtime::Podman.identity_args().is_empty());
        assert_eq!(Runtime::Docker.identity_args()[0], "--user");
    }
}
