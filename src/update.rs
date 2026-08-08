//! Keeping openpages itself current, on the same terms it gives your sites:
//! build it out of the way, prove it works, and only then take the press.
//!
//! The order matters and it is not the obvious one.
//!
//! 1. **Wait for the machine to be bored.** Compiling is the expensive part and
//!    there is no hurry. On the `source` channel nothing starts until the
//!    one-minute load average per core is under the configured threshold.
//! 2. **Prove it before it is anywhere near the socket.** The staged binary is
//!    run in a throwaway data directory on a throwaway port and asked to serve.
//!    A binary that fails here is recorded and discarded; the running version
//!    never learns about it.
//! 3. **Swap when the traffic says so**, same quiet-window logic the sites get.
//! 4. **Survive being wrong.** A marker file is written before the swap and
//!    cleared only after the new version has run healthily for a while. The
//!    `guard` subcommand runs as `ExecStartPre`, so a version that cannot even
//!    boot is reverted by its own second restart rather than by a human.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

use crate::db;
use crate::state::Shared;
use crate::util::load_per_core;

pub const CURRENT: &str = "current";
const MARKER: &str = "pending-update.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub previous: PathBuf,
    pub target: PathBuf,
    pub version: String,
    pub attempts: u32,
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn asset_name() -> String {
    format!("openpages-{}-linux", std::env::consts::ARCH)
}

// ── the loop ────────────────────────────────────────────────────────────────

pub async fn updater_loop(state: Shared) {
    // Confirm whatever the previous version staged, before considering a new one.
    if let Err(e) = confirm_after_boot(&state).await {
        tracing::warn!("update confirm: {e:#}");
    }
    if !state.cfg.update.enabled {
        tracing::info!("auto-update is off");
        return;
    }
    loop {
        tokio::time::sleep(Duration::from_secs(
            state.cfg.update.check_interval_secs.max(300),
        ))
        .await;
        if let Err(e) = cycle(&state).await {
            tracing::warn!("update: {e:#}");
        }
    }
}

async fn cycle(state: &Shared) -> Result<()> {
    let Some(newer) = check(state).await? else {
        return Ok(());
    };
    tracing::info!("update available: {} → {newer}", version());
    db::record_update(
        &state.db,
        Some(version()),
        Some(&newer),
        &state.cfg.update.channel,
        "found",
        None,
    )
    .await?;

    let staged = match stage(state, &newer).await {
        Ok(p) => p,
        Err(e) => {
            let detail = format!("{e:#}");
            db::record_update(
                &state.db,
                Some(version()),
                Some(&newer),
                &state.cfg.update.channel,
                "failed",
                Some(&detail),
            )
            .await?;
            tracing::error!("staging {newer} failed, staying on {}: {detail}", version());
            let s = state.clone();
            let v = newer.clone();
            tokio::spawn(async move { crate::heal::on_update_failure(s, v, detail).await });
            return Ok(());
        }
    };

    db::record_update(
        &state.db,
        Some(version()),
        Some(&newer),
        &state.cfg.update.channel,
        "verifying",
        None,
    )
    .await?;
    if let Err(e) = verify(&staged, &newer).await {
        let detail = format!("{e:#}");
        db::record_update(
            &state.db,
            Some(version()),
            Some(&newer),
            &state.cfg.update.channel,
            "failed",
            Some(&detail),
        )
        .await?;
        tracing::error!(
            "{newer} did not pass verification, staying on {}: {detail}",
            version()
        );
        let _ = tokio::fs::remove_file(&staged).await;
        let s = state.clone();
        let v = newer.clone();
        tokio::spawn(async move { crate::heal::on_update_failure(s, v, detail).await });
        return Ok(());
    }

    *state.pending_update.lock().unwrap() = Some(newer.clone());
    db::record_update(
        &state.db,
        Some(version()),
        Some(&newer),
        &state.cfg.update.channel,
        "staged",
        None,
    )
    .await?;
    tracing::info!("{newer} is compiled, verified and standing by");

    if !state.cfg.update.auto_apply {
        tracing::info!("auto_apply is off — apply it from the panel when you are ready");
        return Ok(());
    }

    wait_for_quiet(state).await;
    apply(state, &staged, &newer).await
}

// ── check ───────────────────────────────────────────────────────────────────

/// The newest published version, if it is actually newer than this one.
pub async fn check(state: &Shared) -> Result<Option<String>> {
    let Some(release) = state.gh.latest_release(&state.cfg.update.repo).await? else {
        tracing::debug!("{} has published no releases", state.cfg.update.repo);
        return Ok(None);
    };
    if release.prerelease {
        return Ok(None);
    }
    let tag = release.tag.trim_start_matches('v').to_string();
    let (Ok(new), Ok(cur)) = (
        semver::Version::parse(&tag),
        semver::Version::parse(version()),
    ) else {
        bail!("cannot compare versions `{tag}` and `{}`", version());
    };
    Ok((new > cur).then_some(tag))
}

// ── stage ───────────────────────────────────────────────────────────────────

async fn stage(state: &Shared, target: &str) -> Result<PathBuf> {
    let dest = state.cfg.stage_dir().join(format!("openpages-{target}"));
    tokio::fs::create_dir_all(state.cfg.stage_dir()).await?;

    match state.cfg.update.channel.as_str() {
        "source" => compile(state, target, &dest).await?,
        _ => download(state, target, &dest).await?,
    }

    let mut perms = tokio::fs::metadata(&dest).await?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
    }
    tokio::fs::set_permissions(&dest, perms).await?;
    Ok(dest)
}

async fn download(state: &Shared, target: &str, dest: &Path) -> Result<()> {
    let release = state
        .gh
        .latest_release(&state.cfg.update.repo)
        .await?
        .with_context(|| format!("{} has no releases", state.cfg.update.repo))?;
    let want = asset_name();
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == want)
        .with_context(|| format!("release {target} has no asset named `{want}`"))?;

    tracing::info!("downloading {}", asset.name);
    let bytes = reqwest::Client::new()
        .get(&asset.browser_download_url)
        .header("user-agent", "openpages")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    if let Some(sum) = release
        .assets
        .iter()
        .find(|a| a.name == format!("{want}.sha256"))
    {
        let expected = reqwest::get(&sum.browser_download_url)
            .await?
            .text()
            .await?;
        let expected = expected
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_lowercase();
        let actual = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(&bytes);
            hex::encode(h.finalize())
        };
        if !expected.is_empty() && expected != actual {
            bail!("checksum mismatch for {want}: expected {expected}, got {actual}");
        }
    } else {
        tracing::warn!("release {target} publishes no .sha256 — cannot verify the download");
    }

    tokio::fs::write(dest, &bytes)
        .await
        .context("writing the staged binary")?;
    Ok(())
}

/// Compile from source, politely: `nice`d, on a fraction of the cores, and only
/// once the machine has been quiet enough to have spare capacity.
async fn compile(state: &Shared, target: &str, dest: &Path) -> Result<()> {
    let src = state.cfg.stage_dir().join(format!("src-{target}"));
    let _ = tokio::fs::remove_dir_all(&src).await;
    tokio::fs::create_dir_all(&src).await?;

    let url = crate::gh::clone_url(
        &state.cfg.update.repo,
        state.gh.token().await.ok().as_deref(),
    );
    run(
        &src,
        "git",
        &[
            "clone",
            "--depth",
            "1",
            "--branch",
            &format!("v{target}"),
            &url,
            ".",
        ],
        600,
    )
    .await
    .context("cloning openpages")?;

    let threshold = state.cfg.update.load_threshold;
    let mut waited = 0u64;
    while load_per_core() > threshold && waited < 6 * 3600 {
        tracing::debug!(
            "load {:.2} above {threshold}, not compiling yet",
            load_per_core()
        );
        tokio::time::sleep(Duration::from_secs(300)).await;
        waited += 300;
    }

    let jobs = (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        / 2)
    .max(1);
    let jobs = jobs.to_string();

    tracing::info!("compiling {target} on {jobs} jobs");
    run(
        &src,
        "cargo",
        &["test", "--release", "--quiet", "--jobs", &jobs],
        5400,
    )
    .await
    .context("the new version does not pass its own tests")?;
    run(
        &src,
        "cargo",
        &["build", "--release", "--quiet", "--jobs", &jobs],
        5400,
    )
    .await
    .context("the new version does not compile")?;

    tokio::fs::copy(src.join("target/release/openpages"), dest)
        .await
        .context("copying the compiled binary")?;
    let _ = tokio::fs::remove_dir_all(&src).await;
    Ok(())
}

async fn run(dir: &Path, bin: &str, args: &[&str], timeout: u64) -> Result<String> {
    let mut cmd = Command::new("nice");
    cmd.arg("-n").arg("19").arg(bin).args(args);
    let child = cmd
        .current_dir(dir)
        .env("CARGO_TERM_COLOR", "never")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running {bin}"))?;

    let out = tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow::anyhow!("{bin} {} timed out", args.first().copied().unwrap_or(""))
        })??;

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        bail!("{combined}");
    }
    Ok(combined)
}

// ── verify ──────────────────────────────────────────────────────────────────

/// Boot the staged binary against a throwaway database on a throwaway port and
/// make it answer. This is the check that "compiles" is not the same as "works".
///
/// It also insists the binary *is* the version the release claimed. A release
/// tagged v1.2.3 whose asset was built from 1.2.2 will otherwise install
/// cleanly, restart, still report the old version, see the update again, and
/// loop for ever — reinstalling itself every check interval until something
/// else breaks. Trusting the tag is the bug; this is the check.
pub async fn verify(binary: &Path, expected: &str) -> Result<()> {
    let out = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .await
        .context("the staged binary will not run at all")?;
    if !out.status.success() {
        bail!("`--version` exited {}", out.status.code().unwrap_or(-1));
    }

    let reported = parse_version(&String::from_utf8_lossy(&out.stdout))
        .context("could not read a version out of `--version`")?;
    if reported != expected {
        bail!(
            "the release is tagged {expected} but the binary in it reports {reported} — \
             refusing to install it, because it would update to itself for ever"
        );
    }

    let scratch = tempfile::tempdir()?;
    let out = tokio::time::timeout(
        Duration::from_secs(90),
        Command::new(binary)
            .arg("selftest")
            .arg("--data-dir")
            .arg(scratch.path())
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("the staged binary hung during its self-test"))??;

    if !out.status.success() {
        bail!(
            "self-test failed: {}",
            crate::gh::first_line(&String::from_utf8_lossy(&out.stderr))
        );
    }
    Ok(())
}

// ── apply ───────────────────────────────────────────────────────────────────

/// The same idea the sites get: prefer a moment when nobody is looking, but
/// never wait forever.
async fn wait_for_quiet(state: &Shared) {
    let deadline =
        std::time::Instant::now() + Duration::from_secs(state.cfg.swap.max_wait_secs as u64);
    loop {
        let busy: i64 = match db::list_sites(&state.db).await {
            Ok(sites) => sites.iter().map(|s| state.traffic.inflight(s.id)).sum(),
            Err(_) => 0,
        };
        if busy == 0 || std::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

pub async fn apply(state: &Shared, staged: &Path, new_version: &str) -> Result<()> {
    let bin_dir = staged.parent().unwrap_or(Path::new(".")).to_path_buf();
    let link = install_dir(state).join(CURRENT);
    let previous = tokio::fs::read_link(&link)
        .await
        .unwrap_or_else(|_| current_exe());

    let installed = install_dir(state).join(format!("openpages-{new_version}"));
    if installed != staged {
        tokio::fs::create_dir_all(install_dir(state)).await.ok();
        // Copy beside it, then rename over. Writing directly to a path some
        // process is executing fails with ETXTBSY; a rename only unlinks the
        // old inode, which the running process keeps hold of until it exits.
        let tmp = installed.with_extension("incoming");
        let _ = tokio::fs::remove_file(&tmp).await;
        tokio::fs::copy(staged, &tmp)
            .await
            .context("installing the staged binary")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = tokio::fs::metadata(&tmp).await?.permissions();
            p.set_mode(0o755);
            tokio::fs::set_permissions(&tmp, p).await?;
        }
        tokio::fs::rename(&tmp, &installed)
            .await
            .context("putting the new binary in place")?;
    }
    let _ = bin_dir;

    write_marker(
        state,
        &Pending {
            previous: previous.clone(),
            target: installed.clone(),
            version: new_version.to_string(),
            attempts: 0,
        },
    )
    .await?;

    relink(&link, &installed).await?;
    db::record_update(
        &state.db,
        Some(version()),
        Some(new_version),
        &state.cfg.update.channel,
        "applied",
        None,
    )
    .await?;
    tracing::warn!("switching to {new_version} — restarting");

    restart(state).await
}

/// Atomic: write the new symlink beside the old one and rename over it. There
/// is no instant where `current` does not exist.
async fn relink(link: &Path, target: &Path) -> Result<()> {
    let tmp = link.with_extension("next");
    let _ = tokio::fs::remove_file(&tmp).await;
    #[cfg(unix)]
    tokio::fs::symlink(target, &tmp)
        .await
        .context("creating the new symlink")?;
    tokio::fs::rename(&tmp, link)
        .await
        .context("swapping the symlink")?;
    Ok(())
}

async fn restart(state: &Shared) -> Result<()> {
    let unit = &state.cfg.update.service;
    let mut args: Vec<&str> = Vec::new();
    if state.cfg.update.systemctl_scope == "user" {
        args.push("--user");
    }
    args.extend(["--no-block", "restart", unit]);

    let ok = Command::new("systemctl")
        .args(&args)
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        tracing::warn!("could not ask systemd to restart {unit}; exiting so a supervisor can");
        tokio::time::sleep(Duration::from_secs(1)).await;
        std::process::exit(0);
    }
    Ok(())
}

// ── the safety net ──────────────────────────────────────────────────────────

/// `ExecStartPre`. Counts boot attempts against a pending update and reverts
/// once it is clear the new version cannot start. Prints what it did, because
/// this is the one piece of openpages that runs when nothing else does.
pub async fn guard(cfg: &crate::config::Config) -> Result<()> {
    let path = cfg.stage_dir().join(MARKER);
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return Ok(());
    };
    let mut pending: Pending =
        serde_json::from_str(&raw).context("reading the pending-update marker")?;
    pending.attempts += 1;

    if pending.attempts > 2 {
        let link = cfg.update.install_dir.join(CURRENT);
        relink(&link, &pending.previous).await?;
        let _ = tokio::fs::remove_file(&path).await;
        eprintln!(
            "openpages: version {} failed to start {} times — reverted to {}",
            pending.version,
            pending.attempts - 1,
            pending.previous.display()
        );
        return Ok(());
    }

    tokio::fs::write(&path, serde_json::to_vec_pretty(&pending)?).await?;
    Ok(())
}

/// Called at startup by the (possibly new) version. Once it has served for the
/// health window without dying, the update is confirmed and the marker goes.
async fn confirm_after_boot(state: &Shared) -> Result<()> {
    let path = state.cfg.stage_dir().join(MARKER);
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return Ok(());
    };
    let pending: Pending = serde_json::from_str(&raw)?;

    if pending.version != version() {
        // We are not the version that was staged. The guard reverted us, or
        // someone installed by hand. Either way this marker is stale.
        db::record_update(
            &state.db,
            Some(version()),
            Some(&pending.version),
            &state.cfg.update.channel,
            "rolled_back",
            Some("the new version did not start; the previous one is running"),
        )
        .await?;
        let _ = tokio::fs::remove_file(&path).await;
        return Ok(());
    }

    let window = state.cfg.update.health_window_secs;
    let state2 = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(window)).await;
        let path = state2.cfg.stage_dir().join(MARKER);
        let _ = tokio::fs::remove_file(&path).await;
        let _ = db::record_update(
            &state2.db,
            None,
            Some(version()),
            &state2.cfg.update.channel,
            "applied",
            Some("confirmed healthy"),
        )
        .await;
        *state2.pending_update.lock().unwrap() = None;
        tracing::info!("running {} — update confirmed", version());
    });
    Ok(())
}

async fn write_marker(state: &Shared, p: &Pending) -> Result<()> {
    tokio::fs::create_dir_all(state.cfg.stage_dir()).await?;
    tokio::fs::write(
        state.cfg.stage_dir().join(MARKER),
        serde_json::to_vec_pretty(p)?,
    )
    .await?;
    Ok(())
}

/// `openpages 1.2.3` → `1.2.3`.
pub fn parse_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find_map(|t| semver::Version::parse(t.trim_start_matches('v')).ok())
        .map(|v| v.to_string())
}

fn install_dir(state: &Shared) -> PathBuf {
    state.cfg.update.install_dir.clone()
}

fn current_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/opt/openpages/bin/openpages"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_read_out_of_the_binarys_own_output() {
        assert_eq!(parse_version("openpages 0.1.2\n").as_deref(), Some("0.1.2"));
        assert_eq!(
            parse_version("openpages v1.10.0").as_deref(),
            Some("1.10.0")
        );
        assert_eq!(parse_version("no version here").as_deref(), None);
        assert_eq!(parse_version("").as_deref(), None);
    }

    #[tokio::test]
    async fn a_binary_that_is_not_the_version_it_claims_is_refused() {
        // The exact shape of a release tagged v0.1.2 whose asset was built
        // before the version bump: it installs, restarts, still reports the old
        // version, and updates to itself for ever.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("openpages-0.1.2");
        std::fs::write(&fake, "#!/bin/sh\necho 'openpages 0.1.1'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let err = verify(&fake, "0.1.2").await.unwrap_err().to_string();
        assert!(err.contains("reports 0.1.1"), "got: {err}");
        assert!(
            err.contains("for ever"),
            "the reason must be in the message: {err}"
        );
    }

    #[test]
    fn the_asset_name_names_this_architecture() {
        assert!(asset_name().starts_with("openpages-"));
        assert!(asset_name().ends_with("-linux"));
    }

    #[tokio::test]
    async fn the_guard_reverts_only_after_repeated_failures_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.server.data_dir = dir.path().to_path_buf();
        cfg.update.install_dir = dir.path().join("bin");
        cfg.ensure_dirs().unwrap();
        std::fs::create_dir_all(&cfg.update.install_dir).unwrap();

        let old = cfg.update.install_dir.join("openpages-0.1.0");
        let new = cfg.update.install_dir.join("openpages-0.2.0");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        let link = cfg.update.install_dir.join(CURRENT);
        std::os::unix::fs::symlink(&new, &link).unwrap();

        let marker = cfg.stage_dir().join(MARKER);
        std::fs::write(
            &marker,
            serde_json::to_vec(&Pending {
                previous: old.clone(),
                target: new.clone(),
                version: "0.2.0".into(),
                attempts: 0,
            })
            .unwrap(),
        )
        .unwrap();

        guard(&cfg).await.unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            new,
            "one failed start is not enough"
        );
        guard(&cfg).await.unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            new,
            "two is still not enough"
        );
        guard(&cfg).await.unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            old,
            "the third start must revert"
        );
        assert!(
            !marker.exists(),
            "the marker must be cleared after reverting"
        );
    }

    #[tokio::test]
    async fn the_guard_is_a_no_op_when_nothing_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::default();
        cfg.server.data_dir = dir.path().to_path_buf();
        cfg.ensure_dirs().unwrap();
        assert!(guard(&cfg).await.is_ok());
    }

    #[tokio::test]
    async fn relinking_is_atomic_and_repeatable() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let link = dir.path().join(CURRENT);

        relink(&link, &a).await.unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), a);
        relink(&link, &b).await.unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), b);
    }
}
