//! openpages — self-hosted Cloudflare Pages.

mod api;
mod buildpipe;
mod config;
mod db;
mod detect;
mod edge;
mod gh;
mod heal;
mod incident;
mod install;
mod models;
mod node;
mod runtime;
mod state;
mod swap;
mod update;
mod util;
mod worker;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use config::Config;
use runtime::Runtime;

#[derive(Parser)]
#[command(name = "openpages", version, about = "Link a repo. Push. It's live.")]
struct Cli {
    /// Config file. Defaults to ./openpages.toml then /etc/openpages/config.toml.
    #[arg(long, short, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the panel, the edge and (unless told otherwise) a local build agent.
    Serve {
        #[arg(long)]
        listen: Option<String>,
        /// Do not build anything here; only accept work from remote nodes.
        #[arg(long)]
        no_agent: bool,
    },
    /// Run as a build node for a panel somewhere else.
    Agent {
        #[arg(long)]
        panel: Option<String>,
        #[arg(long)]
        token: Option<String>,
        /// How the panel reaches this node's edge, e.g. http://10.0.0.5:8789
        #[arg(long)]
        edge_url: Option<String>,
        #[arg(long)]
        edge_listen: Option<String>,
    },
    /// Interactive first-time setup: pick the pieces you want and write the units.
    Install(install::Args),
    /// Log in to GitHub on this machine, by way of the GitHub CLI.
    Login,
    /// Print a one-time code for signing in to the panel from a browser.
    Invite {
        #[arg(long, default_value_t = 15)]
        minutes: i64,
    },
    /// Check, stage or apply an openpages update.
    Update {
        #[command(subcommand)]
        what: UpdateCmd,
    },
    /// Boot against a throwaway database and prove the binary works. Used by
    /// the updater before it lets a new version anywhere near the socket.
    Selftest {
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// What this machine can and cannot do, and why.
    Doctor,
}

#[derive(Subcommand)]
enum UpdateCmd {
    /// Is there a newer version?
    Check,
    /// Revert a version that cannot start. Runs as systemd's ExecStartPre.
    Guard,
    /// The last twenty update events.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();
    let mut cfg = Config::load(cli.config.as_deref())?;

    match cli.cmd.unwrap_or(Cmd::Serve {
        listen: None,
        no_agent: false,
    }) {
        Cmd::Serve { listen, no_agent } => {
            if let Some(l) = listen {
                cfg.server.listen = l;
            }
            serve(cfg, !no_agent).await
        }
        Cmd::Agent {
            panel,
            token,
            edge_url,
            edge_listen,
        } => {
            cfg.node.panel_url = panel.or(cfg.node.panel_url);
            cfg.node.token = token.or(cfg.node.token);
            cfg.node.edge_url = edge_url.or(cfg.node.edge_url);
            if let Some(l) = edge_listen {
                cfg.node.edge_listen = l;
            }
            node::run(cfg).await
        }
        Cmd::Install(args) => install::run(args, cfg).await,
        Cmd::Login => login(&cfg).await,
        Cmd::Invite { minutes } => invite(&cfg, minutes).await,
        Cmd::Update { what } => match what {
            UpdateCmd::Guard => update::guard(&cfg).await,
            UpdateCmd::Check => update_check(cfg).await,
            UpdateCmd::Status => update_status(cfg).await,
        },
        Cmd::Selftest { data_dir } => selftest(cfg, data_dir).await,
        Cmd::Doctor => doctor(cfg).await,
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("OPENPAGES_LOG")
        .unwrap_or_else(|_| EnvFilter::new("openpages=info,tower_http=warn,warn"));
    fmt().with_env_filter(filter).with_target(false).init();
}

// ── serve ───────────────────────────────────────────────────────────────────

async fn serve(cfg: Config, with_agent: bool) -> Result<()> {
    cfg.ensure_dirs()?;
    // A missing or broken container runtime must not stop the panel from
    // starting. Nothing can be built without one, but a crash-looping service
    // is a service you cannot open to find out why — and `podman info` fails
    // for reasons (a sandboxed /tmp, a stopped daemon) that the panel is the
    // right place to report.
    let (rt, runtime_error) = match Runtime::detect(&cfg.build.runtime).await {
        Ok(rt) => (rt, None),
        Err(e) => {
            let why = format!("{e:#}");
            tracing::error!("no usable container runtime: {why}");
            tracing::error!("openpages is starting anyway; builds will fail until this is fixed");
            (Runtime::Podman, Some(why))
        }
    };
    let db = db::open(&cfg.db_path()).await?;
    db::ensure_local_node(&db, rt.name()).await?;

    let listen = cfg.server.listen.clone();
    let state = state::AppState::new(cfg, rt, runtime_error, db).await?;

    if db::user_count(&state.db).await? == 0 {
        let code = api::mint_claim_code(&state.db, 60).await?;
        tracing::warn!("no one has signed in yet. First sign-in code (valid one hour): {code}");
    }

    tokio::spawn(swap::scheduler(state.clone()));
    tokio::spawn(update::updater_loop(state.clone()));
    tokio::spawn(worker::poller(state.clone()));
    if with_agent {
        tokio::spawn(worker::worker_loop(state.clone()));
    } else {
        tracing::info!("local building is off; work goes to remote nodes only");
    }

    let app = api::router(state.clone()).layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = bind(&listen).await?;
    tracing::info!(
        "openpages {} on {} · sites at *.{}",
        update::version(),
        listen,
        state.cfg.server.base_domain
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving")?;
    Ok(())
}

/// Prefer the socket systemd already opened. That is what makes a restart
/// during an update invisible: the kernel holds new connections in the accept
/// queue while the new process starts, instead of refusing them.
async fn bind(addr: &str) -> Result<tokio::net::TcpListener> {
    if let Some(l) = systemd_socket()? {
        tracing::info!("using the socket systemd passed us");
        return Ok(l);
    }
    tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))
}

fn systemd_socket() -> Result<Option<tokio::net::TcpListener>> {
    use std::os::unix::io::FromRawFd;
    let Ok(fds) = std::env::var("LISTEN_FDS") else {
        return Ok(None);
    };
    if std::env::var("LISTEN_PID").ok().as_deref() != Some(&std::process::id().to_string()) {
        return Ok(None);
    }
    if fds.trim() != "1" {
        return Ok(None);
    }
    // SAFETY: fd 3 is the first socket systemd passes, and LISTEN_PID confirms
    // it was passed to this process and not inherited by accident.
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(3) };
    std_listener.set_nonblocking(true)?;
    Ok(Some(tokio::net::TcpListener::from_std(std_listener)?))
}

async fn shutdown() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    let term = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            if let Ok(mut s) = signal(SignalKind::terminate()) {
                s.recv().await;
            }
        }
    };
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    tracing::info!("draining");
}

// ── one-shot commands ───────────────────────────────────────────────────────

async fn login(cfg: &Config) -> Result<()> {
    let gh = gh::Gh::new(cfg.github.gh_bin.clone());
    if !gh.installed().await {
        anyhow::bail!(
            "the GitHub CLI is not installed. openpages uses it so you never have to register \
             a GitHub App — see https://cli.github.com"
        );
    }
    if let Ok(user) = gh.whoami().await {
        println!("Already signed in to GitHub as {}.", user.login);
        println!("To use a different account: gh auth logout && openpages login");
        return Ok(());
    }

    println!("Opening the GitHub CLI login. Choose HTTPS when it asks about git.\n");
    let status = tokio::process::Command::new(&cfg.github.gh_bin)
        .args([
            "auth",
            "login",
            "--hostname",
            "github.com",
            "--git-protocol",
            "https",
            "--scopes",
            "repo,read:org",
        ])
        .status()
        .await
        .context("running `gh auth login`")?;
    if !status.success() {
        anyhow::bail!("gh auth login did not complete");
    }
    let user = gh.whoami().await?;
    println!("\nSigned in as {}.", user.login);
    println!("Now run `openpages invite` for a code to sign in to the panel.");
    Ok(())
}

async fn invite(cfg: &Config, minutes: i64) -> Result<()> {
    let db = db::open(&cfg.db_path()).await?;
    let code = api::mint_claim_code(&db, minutes).await?;
    println!("Sign-in code: {code}");
    println!(
        "Valid for {minutes} minutes. Enter it at {}",
        panel_url(cfg)
    );
    Ok(())
}

/// Where to actually type the code. `public_url` when it is set; otherwise the
/// panel hostname on the port openpages is listening on — which for a localhost
/// install is a working link, and guessing `localhost:8788` was not.
fn panel_url(cfg: &Config) -> String {
    if let Some(url) = &cfg.server.public_url {
        return url.clone();
    }
    let port = cfg
        .server
        .listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8788);
    let host = cfg.server.panel_host.clone().unwrap_or_else(|| {
        let l = &cfg.server.listen;
        if l.starts_with("0.0.0.0") || l.starts_with("[::]") {
            "localhost".into()
        } else {
            l.rsplit_once(':')
                .map(|(h, _)| h.to_string())
                .unwrap_or_else(|| "localhost".into())
        }
    });
    format!("http://{host}:{port}")
}

async fn update_check(cfg: Config) -> Result<()> {
    let rt = Runtime::detect(&cfg.build.runtime)
        .await
        .unwrap_or(Runtime::Podman);
    let db = db::open(&cfg.db_path()).await?;
    let state = state::AppState::new(cfg, rt, None, db).await?;
    match update::check(&state).await? {
        Some(v) => println!("{v} is available (running {})", update::version()),
        None => println!(
            "{} is the newest version {} offers",
            update::version(),
            state.cfg.update.repo
        ),
    }
    Ok(())
}

async fn update_status(cfg: Config) -> Result<()> {
    let db = db::open(&cfg.db_path()).await?;
    println!("running {}", update::version());
    for e in db::list_updates(&db, 20).await? {
        println!(
            "{}  {:<12} {} → {}  {}",
            e.created_at,
            e.state,
            e.from_version.unwrap_or_else(|| "—".into()),
            e.to_version.unwrap_or_else(|| "—".into()),
            e.detail.unwrap_or_default()
        );
    }
    Ok(())
}

/// The verification an update must pass: migrate a fresh database, start the
/// whole server on a throwaway port, and answer a real request.
async fn selftest(mut cfg: Config, data_dir: Option<PathBuf>) -> Result<()> {
    let scratch = tempfile::tempdir()?;
    cfg.server.data_dir = data_dir.unwrap_or_else(|| scratch.path().to_path_buf());
    cfg.server.listen = "127.0.0.1:0".into();
    cfg.update.enabled = false;
    cfg.ensure_dirs()?;

    let rt = Runtime::detect(&cfg.build.runtime)
        .await
        .unwrap_or(Runtime::Podman);
    let db = db::open(&cfg.db_path())
        .await
        .context("selftest: migrations")?;
    db::ensure_local_node(&db, rt.name()).await?;
    let state = state::AppState::new(cfg, rt, None, db).await?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let app = api::router(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let mut last = String::new();
    for _ in 0..40 {
        match client
            .get(format!("http://127.0.0.1:{port}/healthz"))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                let v: serde_json::Value = client
                    .get(format!("http://127.0.0.1:{port}/api/version"))
                    .send()
                    .await?
                    .json()
                    .await?;
                anyhow::ensure!(
                    v["version"] == update::version(),
                    "selftest: /api/version said {:?}, expected {}",
                    v["version"],
                    update::version()
                );
                server.abort();
                println!("openpages {} selftest ok", update::version());
                return Ok(());
            }
            Ok(r) => last = format!("HTTP {}", r.status()),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    server.abort();
    anyhow::bail!("selftest: the server never became healthy ({last})")
}

/// Everything openpages needs, checked one line at a time. The first thing to
/// run when something is not working.
async fn doctor(cfg: Config) -> Result<()> {
    fn line(ok: bool, what: &str, detail: &str) {
        println!("{} {what:<22} {detail}", if ok { "ok  " } else { "FAIL" });
    }
    // A working choice that happens not to be the fast one is not a failure.
    // `doctor` is only useful if FAIL always means something is actually broken.
    fn note(what: &str, detail: &str) {
        println!("note {what:<22} {detail}");
    }

    let gh = gh::Gh::new(cfg.github.gh_bin.clone());
    let has_gh = gh.installed().await;
    line(
        has_gh,
        "github cli",
        if has_gh {
            "installed"
        } else {
            "not found — see https://cli.github.com"
        },
    );
    if has_gh {
        match gh.whoami().await {
            Ok(u) => line(true, "github account", &u.login),
            Err(e) => line(
                false,
                "github account",
                &format!("{} — run `openpages login`", gh::first_line(&e.to_string())),
            ),
        }
    }

    match Runtime::detect(&cfg.build.runtime).await {
        Ok(rt) => line(true, "container runtime", rt.name()),
        Err(e) => line(false, "container runtime", &e.to_string()),
    }

    let git = tokio::process::Command::new("git")
        .arg("--version")
        .output()
        .await;
    line(
        git.as_ref().map(|o| o.status.success()).unwrap_or(false),
        "git",
        "needed to fetch repositories",
    );

    let dirs = cfg.ensure_dirs();
    line(
        dirs.is_ok(),
        "data directory",
        &cfg.server.data_dir.display().to_string(),
    );

    match db::open(&cfg.db_path()).await {
        Ok(_) => line(true, "database", &cfg.db_path().display().to_string()),
        Err(e) => line(false, "database", &format!("{e:#}")),
    }

    if cfg.heal.enabled {
        let claude = tokio::process::Command::new(&cfg.heal.claude_bin)
            .arg("--version")
            .output()
            .await;
        let ok = claude.as_ref().map(|o| o.status.success()).unwrap_or(false);
        line(
            ok,
            "claude cli",
            if ok {
                "installed — self-healing is armed"
            } else {
                "not found; self-healing cannot run"
            },
        );
    } else {
        line(true, "self-healing", "off");
    }

    match cfg.server.public_url.clone() {
        Some(url) => line(true, "public url", &url),
        None => note(
            "public url",
            "not set — GitHub is polled instead of pushing. Works everywhere, just slower.",
        ),
    }
    println!("\nsites will be served at *.{}", cfg.server.base_domain);
    Ok(())
}
