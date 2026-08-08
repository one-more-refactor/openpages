use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything openpages needs to know that is not in the database.
///
/// Resolution order: `--config <path>`, `$OPENPAGES_CONFIG`,
/// `./openpages.toml`, `/etc/openpages/config.toml`. A missing file is not an
/// error — the defaults are a working single-node install on localhost.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub build: Build,
    #[serde(default)]
    pub github: Github,
    #[serde(default)]
    pub update: Update,
    #[serde(default)]
    pub heal: Heal,
    #[serde(default)]
    pub swap: Swap,
    #[serde(default)]
    pub node: NodeCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    #[serde(default = "d_listen")]
    pub listen: String,
    /// Sites live at `<slug>.<base_domain>`; previews at `<sha>--<slug>.<base_domain>`.
    #[serde(default = "d_base_domain")]
    pub base_domain: String,
    /// Host header that reaches the panel instead of a site.
    #[serde(default)]
    pub panel_host: Option<String>,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    /// Externally reachable panel URL. Presence of this enables GitHub webhooks;
    /// without it openpages falls back to polling, which needs no inbound access.
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default = "d_true")]
    pub trust_forwarded_host: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Build {
    /// docker | podman | auto
    #[serde(default = "d_auto")]
    pub runtime: String,
    #[serde(default = "d_image")]
    pub default_image: String,
    #[serde(default = "d_build_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "d_concurrency")]
    pub max_concurrent: usize,
    /// Hard cap on a built site's disk footprint, in megabytes.
    #[serde(default = "d_artifact_mb")]
    pub max_artifact_mb: u64,
    #[serde(default = "d_keep")]
    pub keep_deployments: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Github {
    /// openpages shells out to the official GitHub CLI rather than making you
    /// register an OAuth or GitHub App. See docs/GITHUB.md.
    #[serde(default = "d_gh")]
    pub gh_bin: String,
    /// Empty means: the first person to log in becomes owner, then it closes.
    #[serde(default)]
    pub allow_users: Vec<String>,
    #[serde(default = "d_true")]
    pub webhooks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Update {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// release = download the published binary; source = compile it here.
    #[serde(default = "d_channel")]
    pub channel: String,
    #[serde(default = "d_update_repo")]
    pub repo: String,
    #[serde(default = "d_check_interval")]
    pub check_interval_secs: u64,
    /// Only start a compile when 1-minute load average per core is below this.
    #[serde(default = "d_load_threshold")]
    pub load_threshold: f64,
    /// Verified builds still wait for a quiet moment before taking the press.
    #[serde(default = "d_true")]
    pub auto_apply: bool,
    #[serde(default = "d_health_secs")]
    pub health_window_secs: u64,
    /// Where versioned binaries live and where the `current` symlink points.
    #[serde(default = "d_install_dir")]
    pub install_dir: PathBuf,
    /// The systemd unit to restart after a swap.
    #[serde(default = "d_service")]
    pub service: String,
    /// `system` or `user` — which systemd manager owns that unit. A rootless
    /// install in a home directory cannot restart a system unit, and asking it
    /// to would leave a verified update staged and never applied.
    #[serde(default = "d_scope")]
    pub systemctl_scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heal {
    #[serde(default)]
    pub enabled: bool,
    /// off | pr | direct
    #[serde(default = "d_heal_mode")]
    pub mode: String,
    #[serde(default = "d_claude")]
    pub claude_bin: String,
    /// Kept in config on purpose: if the Claude CLI changes a flag, you fix it
    /// here instead of waiting for an openpages release.
    #[serde(default = "d_claude_args")]
    pub claude_args: Vec<String>,
    #[serde(default = "d_heal_cap")]
    pub max_per_repo_per_day: i64,
    #[serde(default = "d_heal_timeout")]
    pub timeout_secs: u64,
    /// Characters of distilled incident report handed to Claude. Not the log.
    #[serde(default = "d_heal_budget")]
    pub report_budget_chars: usize,
    #[serde(default)]
    pub heal_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Swap {
    /// immediate | quiet
    #[serde(default = "d_quiet")]
    pub policy: String,
    #[serde(default = "d_max_wait")]
    pub max_wait_secs: i64,
    #[serde(default = "d_drain")]
    pub drain_secs: i64,
    /// Swap when the last minute's traffic is at or below this quantile of the
    /// site's own history for this hour-of-week.
    #[serde(default = "d_quantile")]
    pub quiet_quantile: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCfg {
    /// Only used by `openpages agent`.
    #[serde(default)]
    pub panel_url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// How the panel reaches this node's edge, e.g. http://10.0.0.5:8789
    #[serde(default)]
    pub edge_url: Option<String>,
    #[serde(default = "d_edge_listen")]
    pub edge_listen: String,
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = explicit
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("OPENPAGES_CONFIG").map(PathBuf::from))
            .or_else(|| some_if_file("openpages.toml"))
            // A rootless `--scope user` install lives here. Without this the
            // service would work (its unit sets OPENPAGES_CONFIG) while every
            // CLI command silently fell through to the system config and its
            // /var/lib data directory — which is unwritable, so `openpages
            // invite` would fail for a reason that names the wrong file.
            .or_else(|| some_if_file(user_config_path()))
            .or_else(|| some_if_file("/etc/openpages/config.toml"));

        let mut cfg: Config = match &path {
            Some(p) => {
                let raw = std::fs::read_to_string(p)
                    .with_context(|| format!("reading config {}", p.display()))?;
                toml::from_str(&raw).with_context(|| format!("parsing config {}", p.display()))?
            }
            None => toml::from_str("").expect("empty config is valid"),
        };
        cfg.apply_env();
        Ok(cfg)
    }

    /// A handful of env overrides, enough to run in a container without a file.
    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("OPENPAGES_LISTEN") {
            self.server.listen = v;
        }
        if let Ok(v) = std::env::var("OPENPAGES_BASE_DOMAIN") {
            self.server.base_domain = v;
        }
        if let Ok(v) = std::env::var("OPENPAGES_PUBLIC_URL") {
            self.server.public_url = Some(v);
        }
        if let Ok(v) = std::env::var("OPENPAGES_DATA_DIR") {
            self.server.data_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("OPENPAGES_PANEL_URL") {
            self.node.panel_url = Some(v);
        }
        if let Ok(v) = std::env::var("OPENPAGES_NODE_TOKEN") {
            self.node.token = Some(v);
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.server.data_dir.join("openpages.db")
    }
    pub fn deployments_dir(&self) -> PathBuf {
        self.server.data_dir.join("deployments")
    }
    pub fn work_dir(&self) -> PathBuf {
        self.server.data_dir.join("work")
    }
    pub fn heal_dir(&self) -> PathBuf {
        self.server.data_dir.join("heal")
    }
    pub fn stage_dir(&self) -> PathBuf {
        self.server.data_dir.join("stage")
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [
            self.server.data_dir.clone(),
            self.deployments_dir(),
            self.work_dir(),
            self.heal_dir(),
            self.stage_dir(),
        ] {
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }

    pub fn site_host(&self, slug: &str) -> String {
        format!("{slug}.{}", self.server.base_domain)
    }
    pub fn preview_host(&self, sha: &str, slug: &str) -> String {
        format!("{}--{slug}.{}", short_sha(sha), self.server.base_domain)
    }
    pub fn site_url(&self, slug: &str) -> String {
        format!("https://{}", self.site_host(slug))
    }
    pub fn preview_url(&self, sha: &str, slug: &str) -> String {
        format!("https://{}", self.preview_host(sha, slug))
    }
}

pub fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

fn some_if_file(p: impl Into<PathBuf>) -> Option<PathBuf> {
    let p = p.into();
    p.is_file().then_some(p)
}

/// `$XDG_CONFIG_HOME/openpages/config.toml`, falling back to `~/.config`.
pub fn user_config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("/nonexistent"))
        .join("openpages/config.toml")
}

impl Default for Server {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}
impl Default for Build {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}
impl Default for Github {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}
impl Default for Update {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}
impl Default for Heal {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}
impl Default for Swap {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}
impl Default for NodeCfg {
    fn default() -> Self {
        toml::from_str("").unwrap()
    }
}

fn d_listen() -> String {
    "0.0.0.0:8788".into()
}
fn d_base_domain() -> String {
    "pages.localhost".into()
}
fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/openpages")
}
fn d_true() -> bool {
    true
}
fn d_auto() -> String {
    "auto".into()
}
fn d_image() -> String {
    "docker.io/library/node:22-alpine".into()
}
fn d_build_timeout() -> u64 {
    1800
}
fn d_concurrency() -> usize {
    2
}
fn d_artifact_mb() -> u64 {
    2048
}
fn d_keep() -> i64 {
    20
}
fn d_gh() -> String {
    "gh".into()
}
fn d_channel() -> String {
    "release".into()
}
fn d_update_repo() -> String {
    "one-more-refactor/openpages".into()
}
fn d_check_interval() -> u64 {
    3600
}
fn d_load_threshold() -> f64 {
    0.5
}
fn d_health_secs() -> u64 {
    120
}
fn d_install_dir() -> PathBuf {
    PathBuf::from("/opt/openpages/bin")
}
fn d_service() -> String {
    "openpages.service".into()
}
fn d_scope() -> String {
    "system".into()
}
fn d_heal_mode() -> String {
    "pr".into()
}
fn d_claude() -> String {
    "claude".into()
}
fn d_claude_args() -> Vec<String> {
    [
        "-p",
        "--output-format",
        "json",
        "--max-turns",
        "14",
        "--permission-mode",
        "acceptEdits",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
fn d_heal_cap() -> i64 {
    3
}
fn d_heal_timeout() -> u64 {
    900
}
fn d_heal_budget() -> usize {
    2400
}
fn d_quiet() -> String {
    "quiet".into()
}
fn d_max_wait() -> i64 {
    1800
}
fn d_drain() -> i64 {
    20
}
fn d_quantile() -> f64 {
    0.25
}
fn d_edge_listen() -> String {
    "0.0.0.0:8789".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_a_working_default() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.server.listen, "0.0.0.0:8788");
        assert_eq!(c.build.runtime, "auto");
        assert!(c.update.enabled);
        assert!(!c.heal.enabled, "healing must be opt-in");
    }

    #[test]
    fn partial_config_keeps_other_defaults() {
        let c: Config = toml::from_str("[server]\nbase_domain = \"pages.example.com\"\n").unwrap();
        assert_eq!(c.server.base_domain, "pages.example.com");
        assert_eq!(c.swap.policy, "quiet");
    }

    #[test]
    fn the_user_config_path_follows_xdg() {
        let path = user_config_path();
        assert!(path.ends_with("openpages/config.toml"));
        assert!(
            path.is_absolute(),
            "a relative config path would resolve against the working directory"
        );
    }

    #[test]
    fn hosts_are_built_the_way_the_edge_parses_them() {
        let mut c = Config::default();
        c.server.base_domain = "pages.example.com".into();
        assert_eq!(c.site_host("blog"), "blog.pages.example.com");
        assert_eq!(
            c.preview_host("0123456789abcdef", "blog"),
            "01234567--blog.pages.example.com"
        );
    }
}
