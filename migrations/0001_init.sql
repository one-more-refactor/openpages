-- openpages schema. SQLite, one file, WAL. Timestamps are RFC3339 UTC strings.

CREATE TABLE users (
  id          INTEGER PRIMARY KEY,
  gh_login    TEXT    NOT NULL UNIQUE,
  gh_id       INTEGER,
  name        TEXT,
  avatar_url  TEXT,
  role        TEXT    NOT NULL DEFAULT 'owner',
  created_at  TEXT    NOT NULL
);

CREATE TABLE sessions (
  token      TEXT PRIMARY KEY,
  user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL
);

-- A node is a machine that can build and serve. The panel always has one
-- ("local"); more can enroll by dialling out, Portainer-style.
CREATE TABLE nodes (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  runtime     TEXT NOT NULL DEFAULT 'auto',
  edge_url    TEXT,
  version     TEXT,
  os          TEXT,
  is_local    INTEGER NOT NULL DEFAULT 0,
  token_hash  TEXT,
  last_seen   TEXT,
  enrolled_at TEXT NOT NULL
);

CREATE TABLE sites (
  id                 INTEGER PRIMARY KEY,
  slug               TEXT NOT NULL UNIQUE,
  repo               TEXT NOT NULL,
  branch             TEXT NOT NULL DEFAULT 'main',
  node_id            TEXT REFERENCES nodes(id),
  kind               TEXT NOT NULL DEFAULT 'static',   -- static | container
  framework          TEXT,
  root_dir           TEXT NOT NULL DEFAULT '',
  install_cmd        TEXT,
  build_cmd          TEXT,
  output_dir         TEXT,
  build_image        TEXT,
  runtime            TEXT NOT NULL DEFAULT 'auto',     -- docker | podman | auto
  swap_policy        TEXT NOT NULL DEFAULT 'quiet',    -- immediate | quiet
  swap_max_wait_secs INTEGER NOT NULL DEFAULT 1800,
  drain_secs         INTEGER NOT NULL DEFAULT 20,
  heal_mode          TEXT NOT NULL DEFAULT 'inherit',  -- inherit | off | pr | direct
  preview_enabled    INTEGER NOT NULL DEFAULT 1,
  webhook_id         INTEGER,
  poll_interval_secs INTEGER NOT NULL DEFAULT 60,
  last_polled_sha    TEXT,
  created_at         TEXT NOT NULL,
  updated_at         TEXT NOT NULL
);

CREATE TABLE site_env (
  site_id   INTEGER NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  key       TEXT NOT NULL,
  value     TEXT NOT NULL,
  is_secret INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (site_id, key)
);

CREATE TABLE deployments (
  id             INTEGER PRIMARY KEY,
  site_id        INTEGER NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
  commit_sha     TEXT NOT NULL,
  commit_msg     TEXT,
  branch         TEXT NOT NULL,
  kind           TEXT NOT NULL DEFAULT 'production',  -- production | preview
  state          TEXT NOT NULL,
  node_id        TEXT,
  artifact_path  TEXT,
  container_id   TEXT,
  container_port INTEGER,
  error_summary  TEXT,
  claimed_at     TEXT,
  started_at     TEXT,
  ready_at       TEXT,
  promoted_at    TEXT,
  finished_at    TEXT,
  retired_at     TEXT,
  created_at     TEXT NOT NULL
);

CREATE INDEX idx_deployments_site  ON deployments(site_id, created_at DESC);
CREATE INDEX idx_deployments_state ON deployments(state);

CREATE TABLE deploy_logs (
  id            INTEGER PRIMARY KEY,
  deployment_id INTEGER NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
  seq           INTEGER NOT NULL,
  ts            TEXT NOT NULL,
  stream        TEXT NOT NULL,
  line          TEXT NOT NULL
);

CREATE INDEX idx_logs_deployment ON deploy_logs(deployment_id, seq);

-- One row per site per minute. The swap scheduler reads quantiles off this to
-- find the quietest moment to change what is on press.
CREATE TABLE traffic (
  site_id INTEGER NOT NULL,
  bucket  TEXT NOT NULL,          -- 'YYYY-MM-DDTHH:MM'
  hits    INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (site_id, bucket)
);

CREATE INDEX idx_traffic_bucket ON traffic(bucket);

CREATE TABLE heals (
  id            INTEGER PRIMARY KEY,
  kind          TEXT NOT NULL,      -- site_build | self_update | runtime
  site_id       INTEGER,
  deployment_id INTEGER,
  target_repo   TEXT NOT NULL,
  commit_sha    TEXT,
  fingerprint   TEXT NOT NULL,
  state         TEXT NOT NULL,      -- running | opened_pr | pushed | no_fix | error | skipped
  summary       TEXT,
  pr_url        TEXT,
  detail        TEXT,
  created_at    TEXT NOT NULL,
  finished_at   TEXT
);

CREATE INDEX idx_heals_repo ON heals(target_repo, created_at DESC);

CREATE TABLE update_events (
  id           INTEGER PRIMARY KEY,
  from_version TEXT,
  to_version   TEXT,
  channel      TEXT,
  state        TEXT NOT NULL,      -- found | compiling | verifying | staged | applied | rolled_back | failed
  detail       TEXT,
  created_at   TEXT NOT NULL
);

CREATE TABLE settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
