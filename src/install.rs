//! `openpages install` — pick the pieces you want on this machine.
//!
//! Four independent things live in this one binary, and a given machine rarely
//! wants all four. A build box wants the agent and nothing else. The panel host
//! usually wants everything. Making them separate answers rather than separate
//! downloads means one binary, one version, one update path.

use anyhow::{bail, Context, Result};
use clap::Args as ClapArgs;
use std::io::Write;
use std::path::PathBuf;

use crate::config::Config;

#[derive(ClapArgs, Debug, Clone)]
pub struct Args {
    /// Take the defaults and ask nothing.
    #[arg(long)]
    pub yes: bool,
    /// Comma-separated: panel, agent, updater, healer.
    #[arg(long)]
    pub components: Option<String>,
    #[arg(long)]
    pub base_domain: Option<String>,
    #[arg(long)]
    pub panel_host: Option<String>,
    #[arg(long)]
    pub public_url: Option<String>,
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Unix user the services run as.
    #[arg(long, default_value = "openpages")]
    pub user: String,
    /// For an agent-only install: the panel to dial.
    #[arg(long)]
    pub panel_url: Option<String>,
    #[arg(long)]
    pub node_token: Option<String>,
    /// Print everything that would be written, write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Components {
    pub panel: bool,
    pub agent: bool,
    pub updater: bool,
    pub healer: bool,
}

impl Components {
    pub fn parse(list: &str) -> Self {
        let has = |name: &str| list.split(',').any(|p| p.trim().eq_ignore_ascii_case(name));
        Self {
            panel: has("panel"),
            agent: has("agent"),
            updater: has("updater"),
            healer: has("healer"),
        }
    }
    pub fn any(&self) -> bool {
        self.panel || self.agent || self.updater || self.healer
    }
}

pub async fn run(args: Args, mut cfg: Config) -> Result<()> {
    println!("openpages {} — setup\n", crate::update::version());

    let components = match &args.components {
        Some(list) => Components::parse(list),
        None if args.yes => Components {
            panel: true,
            agent: true,
            updater: true,
            healer: false,
        },
        None => ask_components()?,
    };
    if !components.any() {
        bail!("nothing selected — nothing to install");
    }

    if components.panel {
        cfg.server.base_domain = pick(
            args.base_domain.clone(),
            args.yes,
            "Wildcard domain for published sites",
            &cfg.server.base_domain,
        )?;
        let default_panel = format!("openpages.{}", strip_first_label(&cfg.server.base_domain));
        cfg.server.panel_host = Some(pick(
            args.panel_host.clone(),
            args.yes,
            "Hostname for the panel itself",
            &default_panel,
        )?);
        let default_public = format!(
            "https://{}",
            cfg.server.panel_host.clone().unwrap_or_default()
        );
        let public = pick(
            args.public_url.clone(),
            args.yes,
            "Public URL of the panel (blank if it is not reachable from the internet)",
            &default_public,
        )?;
        cfg.server.public_url = (!public.trim().is_empty()).then_some(public);
    } else {
        cfg.node.panel_url = args.panel_url.clone().or(cfg.node.panel_url);
        cfg.node.token = args.node_token.clone().or(cfg.node.token);
        if cfg.node.panel_url.is_none() && !args.yes {
            cfg.node.panel_url = Some(prompt("Panel URL this node should dial", "")?);
        }
        if cfg.node.token.is_none() && !args.yes {
            cfg.node.token = Some(prompt(
                "Node token (mint one in the panel under Nodes)",
                "",
            )?);
        }
    }

    if let Some(d) = args.data_dir.clone() {
        cfg.server.data_dir = d;
    }
    cfg.update.enabled = components.updater;
    cfg.heal.enabled = components.healer;

    let runtime = match crate::runtime::Runtime::detect(&cfg.build.runtime).await {
        Ok(rt) => rt.name().to_string(),
        Err(e) if components.agent => bail!("{e}"),
        Err(_) => "podman".to_string(),
    };
    cfg.build.runtime = runtime.clone();

    let files = plan(&cfg, &components, &args)?;

    println!("\nThis machine will run:");
    for (name, why) in [
        ("web panel + edge", components.panel),
        ("build agent", components.agent),
        ("automatic updates", components.updater),
        ("self-healing (Claude)", components.healer),
    ]
    .iter()
    .filter(|(_, on)| *on)
    .map(|(n, _)| (*n, ""))
    {
        println!("  · {name}{why}");
    }
    println!("  · container runtime: {runtime}");
    println!("\nFiles:");
    for (path, _) in &files {
        println!("  {}", path.display());
    }

    if args.dry_run {
        println!("\n--dry-run: nothing was written.\n");
        for (path, body) in &files {
            println!("\n───────── {} ─────────\n{body}", path.display());
        }
        return Ok(());
    }

    if unsafe { libc::geteuid() } != 0 {
        bail!("writing to /etc and /opt needs root — re-run with sudo, or use --dry-run to see the files");
    }

    ensure_user(&args.user)?;
    for (path, body) in &files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    std::fs::create_dir_all(&cfg.server.data_dir)?;
    std::fs::create_dir_all(&cfg.update.install_dir)?;
    chown(&cfg.server.data_dir, &args.user)?;

    install_binary(&cfg)?;

    println!("\nEnabling services…");
    let units: Vec<&str> = if components.panel {
        vec!["openpages.socket", "openpages.service"]
    } else {
        vec!["openpages-agent.service"]
    };
    sh("systemctl", &["daemon-reload"])?;
    for u in &units {
        sh("systemctl", &["enable", "--now", u])?;
    }

    println!("\nDone.\n");
    if components.panel {
        println!("  1. Sign in to GitHub on this machine:  openpages login");
        println!("  2. Get a code for the browser:         openpages invite");
        println!(
            "  3. Point *.{} and {} at this host, then open the panel.",
            cfg.server.base_domain,
            cfg.server.panel_host.clone().unwrap_or_default()
        );
        println!("\n  Check everything: openpages doctor");
    } else {
        println!("  This node will appear in the panel under Nodes within a minute.");
    }
    Ok(())
}

/// Everything that will be written, as (path, contents). Pure, so `--dry-run`
/// shows exactly what a real run would do rather than an approximation of it.
pub fn plan(cfg: &Config, c: &Components, args: &Args) -> Result<Vec<(PathBuf, String)>> {
    let mut out = vec![(
        PathBuf::from("/etc/openpages/config.toml"),
        toml::to_string_pretty(cfg).context("rendering the config")?,
    )];

    if c.panel {
        out.push((
            PathBuf::from("/etc/systemd/system/openpages.socket"),
            socket_unit(cfg),
        ));
        out.push((
            PathBuf::from("/etc/systemd/system/openpages.service"),
            panel_unit(cfg, &args.user, c.agent),
        ));
    } else if c.agent {
        out.push((
            PathBuf::from("/etc/systemd/system/openpages-agent.service"),
            agent_unit(cfg, &args.user),
        ));
    }
    Ok(out)
}

fn socket_unit(cfg: &Config) -> String {
    format!(
        "[Unit]\n\
         Description=openpages socket\n\n\
         [Socket]\n\
         ListenStream={listen}\n\
         # The kernel holds connections here while the service restarts, which\n\
         # is what makes an update invisible instead of a five-second outage.\n\
         Accept=no\n\
         Backlog=1024\n\n\
         [Install]\n\
         WantedBy=sockets.target\n",
        listen = cfg.server.listen
    )
}

fn panel_unit(cfg: &Config, user: &str, with_agent: bool) -> String {
    let agent_flag = if with_agent { "" } else { " --no-agent" };
    format!(
        "[Unit]\n\
         Description=openpages\n\
         Requires=openpages.socket\n\
         After=network-online.target openpages.socket\n\n\
         [Service]\n\
         Type=exec\n\
         User={user}\n\
         Group={user}\n\
         # Reverts a version that cannot start, before it gets a third chance.\n\
         ExecStartPre={install}/current update guard\n\
         ExecStart={install}/current serve{agent_flag}\n\
         Restart=always\n\
         RestartSec=2\n\
         Environment=OPENPAGES_LOG=openpages=info\n\
         Environment=XDG_RUNTIME_DIR=/run/user/%U\n\
         WorkingDirectory={data}\n\
         StateDirectory=openpages\n\
         NoNewPrivileges=yes\n\
         ProtectSystem=full\n\
         ProtectHome=read-only\n\
         PrivateTmp=yes\n\
         ReadWritePaths={data} {install}\n\
         LimitNOFILE=65536\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        install = cfg.update.install_dir.display(),
        data = cfg.server.data_dir.display(),
    )
}

fn agent_unit(cfg: &Config, user: &str) -> String {
    format!(
        "[Unit]\n\
         Description=openpages build node\n\
         After=network-online.target\n\n\
         [Service]\n\
         Type=exec\n\
         User={user}\n\
         Group={user}\n\
         ExecStart={install}/current agent\n\
         Restart=always\n\
         RestartSec=5\n\
         Environment=XDG_RUNTIME_DIR=/run/user/%U\n\
         WorkingDirectory={data}\n\
         NoNewPrivileges=yes\n\
         ProtectSystem=full\n\
         PrivateTmp=yes\n\
         ReadWritePaths={data}\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        install = cfg.update.install_dir.display(),
        data = cfg.server.data_dir.display(),
    )
}

fn install_binary(cfg: &Config) -> Result<()> {
    let me = std::env::current_exe().context("finding this binary")?;
    let versioned = cfg
        .update
        .install_dir
        .join(format!("openpages-{}", crate::update::version()));
    let link = cfg.update.install_dir.join(crate::update::CURRENT);

    if me != versioned {
        std::fs::copy(&me, &versioned)
            .with_context(|| format!("copying to {}", versioned.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&versioned)?.permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&versioned, p)?;
    }
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&versioned, &link)?;
    println!("installed {} → {}", link.display(), versioned.display());
    Ok(())
}

fn ensure_user(user: &str) -> Result<()> {
    if std::process::Command::new("id")
        .arg(user)
        .output()?
        .status
        .success()
    {
        return Ok(());
    }
    println!("creating system user {user}");
    sh(
        "useradd",
        &[
            "--system",
            "--create-home",
            "--shell",
            "/usr/sbin/nologin",
            user,
        ],
    )?;
    // Rootless podman under a system unit needs a lingering session so that
    // /run/user/<uid> exists before the service starts.
    let _ = sh("loginctl", &["enable-linger", user]);
    Ok(())
}

fn chown(path: &std::path::Path, user: &str) -> Result<()> {
    sh(
        "chown",
        &["-R", &format!("{user}:{user}"), &path.to_string_lossy()],
    )
}

fn sh(bin: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(bin)
        .args(args)
        .status()
        .with_context(|| format!("running {bin}"))?;
    if !status.success() {
        bail!("{bin} {} failed", args.join(" "));
    }
    Ok(())
}

// ── prompts ─────────────────────────────────────────────────────────────────

fn ask_components() -> Result<Components> {
    println!("What should this machine run?\n");
    let panel = yes_no(
        "Web panel and edge (the thing that serves your sites)",
        true,
    )?;
    let agent = yes_no("Build agent (compile sites on this machine)", true)?;
    let updater = yes_no(
        "Automatic updates (build the new version quietly, verify, then switch)",
        true,
    )?;
    println!(
        "\n  Self-healing runs the Claude CLI on this machine when a build breaks.\n  \
         It gets a short summary of the failure, not the whole log, and it opens a\n  \
         pull request by default. Your site is rolled back first, either way."
    );
    let healer = yes_no("Self-healing", false)?;
    Ok(Components {
        panel,
        agent,
        updater,
        healer,
    })
}

fn yes_no(question: &str, default: bool) -> Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    let answer = prompt(&format!("{question}? [{hint}]"), "")?;
    Ok(match answer.trim().to_ascii_lowercase().as_str() {
        "" => default,
        a => a.starts_with('y'),
    })
}

fn pick(given: Option<String>, quiet: bool, question: &str, default: &str) -> Result<String> {
    match given {
        Some(v) => Ok(v),
        None if quiet => Ok(default.to_string()),
        None => prompt(question, default),
    }
}

fn prompt(question: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{question}: ");
    } else {
        print!("{question} [{default}]: ");
    }
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim().to_string();
    Ok(if line.is_empty() {
        default.to_string()
    } else {
        line
    })
}

fn strip_first_label(domain: &str) -> &str {
    domain
        .split_once('.')
        .map(|(_, rest)| rest)
        .unwrap_or(domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_parse_from_a_flag() {
        let c = Components::parse("panel,updater");
        assert!(c.panel && c.updater);
        assert!(!c.agent && !c.healer);
        assert!(!Components::parse("").any());
        assert!(Components::parse(" Agent , HEALER ").agent);
    }

    #[test]
    fn the_panel_host_default_sits_beside_the_wildcard() {
        assert_eq!(strip_first_label("pages.example.com"), "example.com");
        assert_eq!(strip_first_label("example.com"), "com");
        assert_eq!(strip_first_label("localhost"), "localhost");
    }

    #[test]
    fn an_agent_only_install_writes_no_panel_units() {
        let cfg = Config::default();
        let c = Components {
            panel: false,
            agent: true,
            updater: false,
            healer: false,
        };
        let args = Args {
            yes: true,
            components: None,
            base_domain: None,
            panel_host: None,
            public_url: None,
            data_dir: None,
            user: "openpages".into(),
            panel_url: None,
            node_token: None,
            dry_run: true,
        };
        let files = plan(&cfg, &c, &args).unwrap();
        let names: Vec<String> = files.iter().map(|(p, _)| p.display().to_string()).collect();
        assert!(names.iter().any(|n| n.ends_with("openpages-agent.service")));
        assert!(!names.iter().any(|n| n.contains("openpages.socket")));
    }

    #[test]
    fn a_panel_install_gets_a_socket_and_the_startup_guard() {
        let cfg = Config::default();
        let c = Components {
            panel: true,
            agent: true,
            updater: true,
            healer: false,
        };
        let args = Args {
            yes: true,
            components: None,
            base_domain: None,
            panel_host: None,
            public_url: None,
            data_dir: None,
            user: "openpages".into(),
            panel_url: None,
            node_token: None,
            dry_run: true,
        };
        let files = plan(&cfg, &c, &args).unwrap();
        let service = files
            .iter()
            .find(|(p, _)| p.display().to_string().ends_with("openpages.service"))
            .expect("a panel install writes openpages.service");
        assert!(
            service.1.contains("ExecStartPre"),
            "the rollback guard must run before every start"
        );
        assert!(service.1.contains("Requires=openpages.socket"));
        assert!(!service.1.contains("--no-agent"));
    }

    #[test]
    fn a_panel_without_a_local_agent_says_so_in_its_unit() {
        let cfg = Config::default();
        let c = Components {
            panel: true,
            agent: false,
            updater: true,
            healer: false,
        };
        let args = Args {
            yes: true,
            components: None,
            base_domain: None,
            panel_host: None,
            public_url: None,
            data_dir: None,
            user: "openpages".into(),
            panel_url: None,
            node_token: None,
            dry_run: true,
        };
        let files = plan(&cfg, &c, &args).unwrap();
        let service = files
            .iter()
            .find(|(p, _)| p.display().to_string().ends_with("openpages.service"))
            .unwrap();
        assert!(service.1.contains("serve --no-agent"));
    }

    #[test]
    fn the_written_config_round_trips() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.server.listen, cfg.server.listen);
        assert_eq!(back.update.channel, cfg.update.channel);
    }
}
