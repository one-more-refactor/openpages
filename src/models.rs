use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// The life of a deployment, in press terms.
///
/// `Queued`   — composing: the job exists, no node has taken it yet.
/// `Building` — setting: the container is running install + build.
/// `Verifying`— proofing: the artifact is being served and checked privately.
/// `Ready`    — standing by: it passed, and it is waiting for a quiet moment.
/// `Live`     — on press.
/// `Superseded` — standing: it was live, it still works, it is the rollback.
/// `Failed`   — pied. (Printer's term for a tray of type spilled on the floor.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployState {
    Queued,
    Building,
    Verifying,
    Ready,
    Live,
    Superseded,
    Failed,
    Cancelled,
}

impl DeployState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Building => "building",
            Self::Verifying => "verifying",
            Self::Ready => "ready",
            Self::Live => "live",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "building" => Self::Building,
            "verifying" => Self::Verifying,
            "ready" => Self::Ready,
            "live" => Self::Live,
            "superseded" => Self::Superseded,
            "cancelled" => Self::Cancelled,
            _ => Self::Failed,
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::Cancelled | Self::Superseded)
    }
    /// What the panel prints on the proof strip.
    pub fn label(self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Building => "SETTING",
            Self::Verifying => "PROOFING",
            Self::Ready => "STANDING BY",
            Self::Live => "ON PRESS",
            Self::Superseded => "STANDING",
            Self::Failed => "PIED",
            Self::Cancelled => "KILLED",
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct User {
    pub id: i64,
    pub gh_login: String,
    pub gh_id: Option<i64>,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub role: String,
    pub created_at: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub runtime: String,
    pub edge_url: Option<String>,
    pub version: Option<String>,
    pub os: Option<String>,
    pub is_local: i64,
    #[serde(skip)]
    pub token_hash: Option<String>,
    pub last_seen: Option<String>,
    pub enrolled_at: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Site {
    pub id: i64,
    pub slug: String,
    pub repo: String,
    pub branch: String,
    pub node_id: Option<String>,
    pub kind: String,
    pub framework: Option<String>,
    pub root_dir: String,
    pub install_cmd: Option<String>,
    pub build_cmd: Option<String>,
    pub output_dir: Option<String>,
    pub build_image: Option<String>,
    pub runtime: String,
    pub swap_policy: String,
    pub swap_max_wait_secs: i64,
    pub drain_secs: i64,
    pub heal_mode: String,
    pub preview_enabled: i64,
    pub webhook_id: Option<i64>,
    pub poll_interval_secs: i64,
    pub last_polled_sha: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Deployment {
    pub id: i64,
    pub site_id: i64,
    pub commit_sha: String,
    pub commit_msg: Option<String>,
    pub branch: String,
    pub kind: String,
    pub state: String,
    pub node_id: Option<String>,
    pub artifact_path: Option<String>,
    pub container_id: Option<String>,
    pub container_port: Option<i64>,
    pub error_summary: Option<String>,
    pub claimed_at: Option<String>,
    pub started_at: Option<String>,
    pub ready_at: Option<String>,
    pub promoted_at: Option<String>,
    pub finished_at: Option<String>,
    pub retired_at: Option<String>,
    pub created_at: String,
}

impl Deployment {
    pub fn state(&self) -> DeployState {
        DeployState::parse(&self.state)
    }
    pub fn is_preview(&self) -> bool {
        self.kind == "preview"
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct LogLine {
    pub seq: i64,
    pub ts: String,
    pub stream: String,
    pub line: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct HealRecord {
    pub id: i64,
    pub kind: String,
    pub site_id: Option<i64>,
    pub deployment_id: Option<i64>,
    pub target_repo: String,
    pub commit_sha: Option<String>,
    pub fingerprint: String,
    pub state: String,
    pub summary: Option<String>,
    pub pr_url: Option<String>,
    pub detail: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct UpdateEvent {
    pub id: i64,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub channel: Option<String>,
    pub state: String,
    pub detail: Option<String>,
    pub created_at: String,
}

/// What a node receives when it picks up work. Deliberately self-contained:
/// a remote agent has no database access and must be able to run this job with
/// nothing but the JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildJob {
    pub deployment_id: i64,
    pub site_slug: String,
    pub repo: String,
    pub branch: String,
    pub commit_sha: String,
    pub kind: String,
    pub site_kind: String,
    pub root_dir: String,
    pub install_cmd: Option<String>,
    pub build_cmd: Option<String>,
    pub output_dir: Option<String>,
    pub build_image: Option<String>,
    pub runtime: String,
    pub env: Vec<(String, String)>,
    pub timeout_secs: u64,
    /// Short-lived GitHub token for the clone. Never persisted.
    pub clone_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildOutcome {
    pub deployment_id: i64,
    pub ok: bool,
    pub framework: Option<String>,
    pub artifact_path: Option<String>,
    pub container_id: Option<String>,
    pub container_port: Option<i64>,
    pub error_summary: Option<String>,
    /// Distilled, already trimmed. This is what the healer gets, not the log.
    pub incident: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrips_through_the_database_representation() {
        for s in [
            DeployState::Queued,
            DeployState::Building,
            DeployState::Verifying,
            DeployState::Ready,
            DeployState::Live,
            DeployState::Superseded,
            DeployState::Failed,
            DeployState::Cancelled,
        ] {
            assert_eq!(DeployState::parse(s.as_str()), s);
        }
    }

    #[test]
    fn unknown_state_is_treated_as_failed_not_as_live() {
        assert_eq!(DeployState::parse("nonsense"), DeployState::Failed);
    }
}
