//! End-to-end tests that drive the real `openpages` binary, the way an operator
//! does: run a command, read what it printed, start the server, talk HTTP to it.
//!
//! Every test gets its own temporary data directory and config file, a config
//! that points `gh` at a binary that does not exist (so nothing ever reaches
//! GitHub) and turns the updater off (so nothing reaches the network). No
//! container runtime is needed: the server starts without one, by design.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, COOKIE, HOST, LOCATION, SET_COOKIE};
use reqwest::StatusCode;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_openpages");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const BASE: &str = "pages.localhost";

// ── sandbox ─────────────────────────────────────────────────────────────────

struct Sandbox {
    dir: tempfile::TempDir,
    config: PathBuf,
    port: u16,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let port = free_port();
        let config = dir.path().join("openpages.toml");
        let data = dir.path().join("data");
        let toml = format!(
            "[server]\n\
             listen = '127.0.0.1:{port}'\n\
             base_domain = '{BASE}'\n\
             data_dir = '{data}'\n\
             \n\
             [github]\n\
             gh_bin = '/nonexistent/openpages-test-gh'\n\
             webhooks = false\n\
             \n\
             [update]\n\
             enabled = false\n",
            data = data.display()
        );
        std::fs::write(&config, toml).expect("write config");
        std::fs::create_dir_all(dir.path().join("home")).expect("fake home");
        Self { dir, config, port }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn data_dir(&self) -> PathBuf {
        self.path().join("data")
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(self.path())
            .env("OPENPAGES_CONFIG", &self.config)
            .env("OPENPAGES_LOG", "openpages=warn,warn")
            .env("HOME", self.path().join("home"))
            .env_remove("OPENPAGES_LISTEN")
            .env_remove("OPENPAGES_BASE_DOMAIN")
            .env_remove("OPENPAGES_PUBLIC_URL")
            .env_remove("OPENPAGES_DATA_DIR")
            .env_remove("OPENPAGES_PANEL_URL")
            .env_remove("OPENPAGES_NODE_TOKEN")
            .env_remove("LISTEN_FDS")
            .env_remove("LISTEN_PID")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("running openpages")
    }

    /// `openpages invite`, and the code it printed.
    fn invite(&self) -> String {
        let out = self.run(&["invite", "--minutes", "5"]);
        assert_ok(&out, "invite");
        let text = stdout(&out);
        text.lines()
            .find_map(|l| l.strip_prefix("Sign-in code: "))
            .map(|c| c.trim().to_string())
            .unwrap_or_else(|| panic!("invite printed no code:\n{text}"))
    }

    /// `openpages serve --no-agent`, waited on until `/healthz` answers.
    async fn serve(&self) -> Server {
        let log_path = self.path().join("serve.log");
        let log = File::create(&log_path).expect("serve log");
        let child = self
            .cmd()
            .args(["serve", "--no-agent"])
            .stdout(log.try_clone().expect("dup log"))
            .stderr(log)
            .spawn()
            .expect("spawning openpages serve");
        let server = Server {
            child,
            base: format!("http://127.0.0.1:{}", self.port),
            log: log_path,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .expect("http client"),
        };
        server.wait_healthy().await;
        server
    }
}

struct Server {
    child: Child,
    base: String,
    log: PathBuf,
    http: reqwest::Client,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    async fn wait_healthy(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(r) = self.http.get(self.url("/healthz")).send().await {
                if r.status().is_success() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        panic!(
            "openpages serve never became healthy. Its log:\n{}",
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(self.url(path))
    }

    fn post(&self, path: &str, body: Value) -> reqwest::RequestBuilder {
        self.http.post(self.url(path)).json(&body)
    }

    fn authed(&self, path: &str, cookie: &str) -> reqwest::RequestBuilder {
        self.get(path).header(COOKIE, cookie)
    }

    fn authed_post(&self, path: &str, cookie: &str, body: Value) -> reqwest::RequestBuilder {
        self.post(path, body).header(COOKIE, cookie)
    }

    /// A request for a published site: same listener, different Host header.
    fn site(&self, host: &str, path: &str) -> reqwest::RequestBuilder {
        self.http.get(self.url(path)).header(HOST, host)
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("a free port")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn assert_ok(o: &Output, what: &str) {
    assert!(
        o.status.success(),
        "`openpages {what}` failed ({}).\nstdout:\n{}\nstderr:\n{}",
        o.status,
        stdout(o),
        String::from_utf8_lossy(&o.stderr)
    );
}

fn session_cookie(headers: &HeaderMap) -> String {
    let raw = headers
        .get(SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("a Set-Cookie header");
    let pair = raw.split(';').next().unwrap_or_default().trim().to_string();
    assert!(pair.starts_with("op_session="), "unexpected cookie: {raw}");
    pair
}

// ── one-shot commands ───────────────────────────────────────────────────────

#[test]
fn version_is_the_crate_version() {
    let sb = Sandbox::new();
    let out = sb.run(&["--version"]);
    assert_ok(&out, "--version");
    assert_eq!(stdout(&out).trim(), format!("openpages {VERSION}"));
}

#[test]
fn help_lists_every_command() {
    let sb = Sandbox::new();
    let out = sb.run(&["--help"]);
    assert_ok(&out, "--help");
    let text = stdout(&out);
    for cmd in [
        "serve", "agent", "install", "login", "invite", "update", "selftest", "doctor",
    ] {
        assert!(text.contains(cmd), "--help does not mention `{cmd}`:\n{text}");
    }
}

#[test]
fn an_unknown_command_fails_loudly() {
    let sb = Sandbox::new();
    let out = sb.run(&["publish-everything"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("publish-everything"));
}

#[test]
fn selftest_passes_on_a_fresh_machine() {
    let sb = Sandbox::new();
    let out = sb.run(&["selftest"]);
    assert_ok(&out, "selftest");
    assert!(
        stdout(&out).contains(&format!("openpages {VERSION} selftest ok")),
        "{}",
        stdout(&out)
    );
}

#[test]
fn invite_mints_a_code_a_person_can_type() {
    let sb = Sandbox::new();
    let code = sb.invite();
    assert_eq!(code.len(), 11, "{code}");
    assert_eq!(&code[5..6], "-", "{code}");
    assert!(code
        .chars()
        .filter(|c| *c != '-')
        .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase()));
    assert!(
        sb.data_dir().join("openpages.db").is_file(),
        "invite should have created the database"
    );
    assert_ne!(code, sb.invite(), "two invites gave the same code");
}

#[test]
fn doctor_reports_every_check_and_names_what_is_missing() {
    let sb = Sandbox::new();
    let out = sb.run(&["doctor"]);
    assert_ok(&out, "doctor");
    let text = stdout(&out);
    for line in [
        "github cli",
        "container runtime",
        "git",
        "data directory",
        "database",
        "self-healing",
        "public url",
    ] {
        assert!(text.contains(line), "doctor skipped `{line}`:\n{text}");
    }
    // gh points at a path that does not exist, so this one must say FAIL.
    assert!(
        text.lines()
            .any(|l| l.starts_with("FAIL") && l.contains("github cli")),
        "{text}"
    );
    assert!(text.contains(&format!("sites will be served at *.{BASE}")));
}

#[test]
fn update_status_works_on_a_fresh_database() {
    let sb = Sandbox::new();
    let out = sb.run(&["update", "status"]);
    assert_ok(&out, "update status");
    assert!(stdout(&out).contains(&format!("running {VERSION}")));
}

#[test]
fn install_dry_run_prints_the_files_and_writes_none() {
    let sb = Sandbox::new();
    let home = sb.path().join("home");
    let out = sb.run(&[
        "install",
        "--scope",
        "user",
        "--yes",
        "--dry-run",
        "--components",
        "panel,updater",
        "--base-domain",
        BASE,
        "--public-url",
        "",
    ]);
    assert_ok(&out, "install --dry-run");
    let text = stdout(&out);
    assert!(text.contains("--dry-run: nothing was written"), "{text}");
    assert!(
        text.contains(&home.display().to_string()),
        "a user install should only touch the home directory:\n{text}"
    );
    let left: Vec<_> = std::fs::read_dir(&home)
        .expect("read home")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert!(left.is_empty(), "--dry-run wrote files: {left:?}");
}

// ── the server, as a browser and a visitor see it ──────────────────────────

async fn send(req: reqwest::RequestBuilder) -> reqwest::Response {
    req.send().await.expect("the request reached the server")
}

async fn status_of(req: reqwest::RequestBuilder) -> StatusCode {
    send(req).await.status()
}

async fn body_of(req: reqwest::RequestBuilder) -> String {
    send(req).await.text().await.expect("a body")
}

async fn json_of(req: reqwest::RequestBuilder) -> Value {
    let r = send(req).await;
    let status = r.status();
    let body = r.text().await.expect("a body");
    assert!(status.is_success(), "HTTP {status}: {body}");
    serde_json::from_str(&body).expect("a JSON body")
}

/// Claim a fresh invite code and return the session cookie it bought.
async fn sign_in(sb: &Sandbox, srv: &Server) -> String {
    let code = sb.invite();
    let r = send(srv.post("/api/auth/claim", json!({ "code": code }))).await;
    assert_eq!(r.status(), StatusCode::OK);
    session_cookie(r.headers())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_panel_is_closed_until_an_invite_code_opens_it() {
    let sb = Sandbox::new();
    let srv = sb.serve().await;

    let health = send(srv.get("/healthz")).await;
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(health.text().await.unwrap().trim(), "ok");

    let v = json_of(srv.get("/api/version")).await;
    assert_eq!(v["version"], VERSION);
    assert_eq!(v["base_domain"], BASE);

    // The panel is a single-page app: every unknown path is the same page.
    for path in ["/", "/sites/42", "/system"] {
        let r = send(srv.get(path)).await;
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        let html = r.text().await.unwrap();
        assert!(html.contains("<"), "{path} is not HTML");
    }

    // Nothing behind the gate answers without a session.
    let closed = status_of(srv.get("/api/overview")).await;
    assert_eq!(closed, StatusCode::UNAUTHORIZED);
    let me = json_of(srv.get("/api/auth/me")).await;
    assert!(me["user"].is_null());

    // A made-up code is refused.
    let made_up = json!({ "code": "AAAAA-BBBBB" });
    let refused = status_of(srv.post("/api/auth/claim", made_up)).await;
    assert_eq!(refused, StatusCode::UNAUTHORIZED);

    // A real one — typed in lower case, with stray spaces — is accepted.
    let code = sb.invite();
    let typed = format!("  {}  ", code.to_lowercase());
    let r = send(srv.post("/api/auth/claim", json!({ "code": typed }))).await;
    assert_eq!(r.status(), StatusCode::OK);
    let cookie = session_cookie(r.headers());
    let user: Value = r.json().await.unwrap();
    // gh is unavailable, so the identity falls back to the local operator.
    assert_eq!(user["gh_login"], "operator");

    // …exactly once.
    let again = status_of(srv.post("/api/auth/claim", json!({ "code": code }))).await;
    assert_eq!(again, StatusCode::UNAUTHORIZED);

    let me = json_of(srv.authed("/api/auth/me", &cookie)).await;
    assert_eq!(me["user"]["gh_login"], "operator");

    let overview = json_of(srv.authed("/api/overview", &cookie)).await;
    assert_eq!(overview["sites"], json!([]));
    assert_eq!(overview["base_domain"], BASE);

    for path in ["/api/system", "/api/heals"] {
        let open = status_of(srv.authed(path, &cookie)).await;
        assert_eq!(open, StatusCode::OK, "{path}");
    }
    let nodes = json_of(srv.authed("/api/nodes", &cookie)).await;
    let ids: Vec<&str> = nodes
        .as_array()
        .expect("a list of nodes")
        .iter()
        .filter_map(|n| n["id"].as_str())
        .collect();
    assert!(ids.contains(&"local"), "the panel host is a node: {nodes}");

    // Signing out ends the session server-side, not just in the browser.
    let logout = srv.http.post(srv.url("/api/auth/logout"));
    let out = status_of(logout.header(COOKIE, cookie.as_str())).await;
    assert!(out.is_success());
    let after = status_of(srv.authed("/api/overview", &cookie)).await;
    assert_eq!(after, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_linked_site_is_served_on_its_own_hostname_and_cannot_reach_the_api() {
    let sb = Sandbox::new();
    let srv = sb.serve().await;
    let cookie = sign_in(&sb, &srv).await;

    // Link a repository. Without gh there is no first build, but the site exists.
    let nonsense = json!({ "repo": "not-a-repository" });
    let r = status_of(srv.authed_post("/api/sites", &cookie, nonsense)).await;
    assert_eq!(r, StatusCode::BAD_REQUEST);

    let new_site = json!({ "repo": "acme-press/Field-Notes.git", "branch": "main" });
    let site = json_of(srv.authed_post("/api/sites", &cookie, new_site)).await;
    assert_eq!(site["slug"], "field-notes");
    assert_eq!(site["repo"], "acme-press/Field-Notes");
    let url = format!("http://field-notes.{BASE}:{}", sb.port);
    assert_eq!(site["url"], url);
    let site_id = site["id"].as_i64().expect("site id");

    let clash = json!({ "repo": "someone-else/field-notes", "branch": "main" });
    let r = status_of(srv.authed_post("/api/sites", &cookie, clash)).await;
    assert_eq!(r, StatusCode::BAD_REQUEST);

    // A hostname under the base domain that names no site.
    let nobody = format!("nobody.{BASE}");
    let r = send(srv.site(&nobody, "/")).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert!(r.text().await.unwrap().contains("nothing published"));

    // Stand in for a finished build: an artifact on disk and a live row.
    let artifact = sb.data_dir().join("deployments/field-notes/1");
    write(&artifact, "index.html", "<h1>Field notes</h1><p>marker-index</p>");
    write(&artifact, "about.html", "<p>marker-about</p>");
    write(&artifact, "_redirects", "/old /about 301\n");
    write(&artifact, "_headers", "/*\n  X-Printed-By: openpages\n");
    let deployment = insert_live_deployment(&sb.data_dir(), site_id, &artifact).await;

    let host = format!("field-notes.{BASE}");
    let r = send(srv.site(&host, "/")).await;
    assert_eq!(r.status(), StatusCode::OK);
    let answered_by = r.headers()["x-openpages-deployment"].to_str().unwrap();
    assert_eq!(answered_by, deployment.to_string());
    assert_eq!(r.headers()["x-printed-by"], "openpages");
    assert!(r.text().await.unwrap().contains("marker-index"));

    // Pretty URLs: /about finds about.html.
    let about = body_of(srv.site(&host, "/about")).await;
    assert!(about.contains("marker-about"), "{about}");

    // _redirects works like it does on Cloudflare.
    let r = send(srv.site(&host, "/old")).await;
    assert_eq!(r.status(), StatusCode::MOVED_PERMANENTLY);
    assert_eq!(r.headers()[LOCATION], "/about");

    // A published site cannot reach the panel API, even with a valid session.
    let sneaky = srv.site(&host, "/api/overview");
    let r = send(sneaky.header(COOKIE, cookie.as_str())).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert!(!r.text().await.unwrap().contains("\"sites\""));

    // Nor can a path walk out of the artifact directory. (`%2f` survives URL
    // normalisation, so this reaches the edge as `/../../openpages.db`.)
    let escape = srv.site(&host, "/..%2f..%2f..%2f..%2fopenpages.db");
    assert_ne!(status_of(escape).await, StatusCode::OK);

    // The panel now reports the site as on press.
    let sites = json_of(srv.authed("/api/sites", &cookie)).await;
    assert_eq!(sites[0]["state"], "live");
    assert_eq!(sites[0]["label"], "ON PRESS");
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).expect("artifact dir");
    std::fs::write(dir.join(name), body).expect("artifact file");
}

/// What a finished build leaves behind: one deployment row, on press.
async fn insert_live_deployment(data_dir: &Path, site_id: i64, artifact: &Path) -> i64 {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    let opts = SqliteConnectOptions::new()
        .filename(data_dir.join("openpages.db"))
        .busy_timeout(Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("open the server's database");
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let sql = "INSERT INTO deployments
           (site_id, commit_sha, commit_msg, branch, kind, state, node_id,
            artifact_path, ready_at, promoted_at, finished_at, created_at)
         VALUES (?, ?, 'first impression', 'main', 'production', 'live', 'local',
                 ?, ?, ?, ?, ?)";
    let done = sqlx::query(sql)
        .bind(site_id)
        .bind("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678")
        .bind(artifact.display().to_string())
        .bind(now.as_str())
        .bind(now.as_str())
        .bind(now.as_str())
        .bind(now.as_str())
        .execute(&pool)
        .await
        .expect("insert deployment");
    pool.close().await;
    done.last_insert_rowid()
}
