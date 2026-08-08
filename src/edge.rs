//! The edge: what the internet actually talks to.
//!
//! One listener serves every site plus the panel, and decides which is which
//! from the Host header. `<slug>.<base>` is whatever that site currently has on
//! press; `<sha>--<slug>.<base>` is a specific proof.
//!
//! Swapping a deployment is a database transaction plus a cache clear — no
//! file is moved, no process restarts, and requests already in flight keep
//! reading the directory they started with, because they hold an open handle
//! to it. That is the whole reason `promote` never touches the filesystem.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::db;
use crate::state::{CachedRoute, Shared, ROUTE_TTL};

#[derive(Clone, Debug)]
pub enum Target {
    Static {
        dir: PathBuf,
        rules: Arc<Rules>,
    },
    Container {
        port: u16,
    },
    /// The deployment lives on another node; that node's edge serves it. The
    /// node has no database, so we tell it exactly what to serve.
    Remote {
        base: String,
        spec: String,
    },
}

/// The contract between the panel's edge and a node's edge.
pub const TARGET_HEADER: &str = "x-openpages-target";

#[derive(Clone, Debug)]
pub struct Route {
    pub site_id: i64,
    pub deployment_id: i64,
    pub target: Target,
}

// ── host → route ────────────────────────────────────────────────────────────

pub enum Host<'a> {
    Panel,
    Site {
        slug: &'a str,
        preview: Option<&'a str>,
    },
    Unknown,
}

/// Pure, so it can be tested without a database. `panel_host` wins over
/// everything: locking yourself out of the panel by naming a site `openpages`
/// should not be possible.
pub fn classify<'a>(host: &'a str, base_domain: &str, panel_host: Option<&str>) -> Host<'a> {
    let host = host.split(':').next().unwrap_or(host);
    if let Some(p) = panel_host {
        if host.eq_ignore_ascii_case(p) {
            return Host::Panel;
        }
    }
    let suffix = format!(".{base_domain}");
    let Some(label) = host.strip_suffix(&suffix) else {
        return Host::Unknown;
    };
    if label.is_empty() || label.contains('.') {
        return Host::Unknown;
    }
    match label.split_once("--") {
        Some((sha, slug)) if !sha.is_empty() && !slug.is_empty() => Host::Site {
            slug,
            preview: Some(sha),
        },
        _ => Host::Site {
            slug: label,
            preview: None,
        },
    }
}

pub async fn resolve(state: &Shared, host: &str) -> Option<Route> {
    if let Some(hit) = state.routes.read().await.get(host) {
        if hit.at.elapsed() < ROUTE_TTL {
            return hit.route.clone();
        }
    }
    let route = resolve_uncached(state, host).await;
    state.routes.write().await.insert(
        host.to_string(),
        CachedRoute {
            at: std::time::Instant::now(),
            route: route.clone(),
        },
    );
    route
}

async fn resolve_uncached(state: &Shared, host: &str) -> Option<Route> {
    let Host::Site { slug, preview } = classify(
        host,
        &state.cfg.server.base_domain,
        state.cfg.server.panel_host.as_deref(),
    ) else {
        return None;
    };

    let site = db::site_by_slug(&state.db, slug).await.ok().flatten()?;
    let deployment = match preview {
        Some(sha) if site.preview_enabled != 0 => db::preview_deployment(&state.db, site.id, sha)
            .await
            .ok()
            .flatten()?,
        Some(_) => return None,
        None => db::live_deployment(&state.db, site.id)
            .await
            .ok()
            .flatten()?,
    };

    // A deployment built on another node is served by that node.
    let node_id = deployment.node_id.clone().unwrap_or_else(|| "local".into());
    if node_id != "local" {
        if let Ok(Some(node)) = db::node(&state.db, &node_id).await {
            if let Some(base) = node.edge_url {
                let spec = match deployment.container_port {
                    Some(port) => format!("container:{port}"),
                    None => format!("static:{}/{}", site.slug, deployment.id),
                };
                return Some(Route {
                    site_id: site.id,
                    deployment_id: deployment.id,
                    target: Target::Remote { base, spec },
                });
            }
        }
    }

    let target = if let Some(port) = deployment.container_port {
        Target::Container { port: port as u16 }
    } else {
        let dir = PathBuf::from(deployment.artifact_path.as_deref()?);
        let rules = Arc::new(Rules::load(&dir));
        Target::Static { dir, rules }
    };

    Some(Route {
        site_id: site.id,
        deployment_id: deployment.id,
        target,
    })
}

// ── handler ─────────────────────────────────────────────────────────────────

pub async fn handle(State(state): State<Shared>, req: Request) -> Response {
    let host = header_str(req.headers(), header::HOST)
        .or_else(|| req.uri().host().map(str::to_string))
        .unwrap_or_default();

    let Some(route) = resolve(&state, &host).await else {
        return not_a_site(&state, &host);
    };

    state.traffic.hit(route.site_id);
    let _guard = state.traffic.enter(route.site_id);

    let mut response = match route.target.clone() {
        Target::Static { dir, rules } => serve_static(&dir, &rules, req).await,
        Target::Container { port } => proxy(&format!("http://127.0.0.1:{port}"), req, None).await,
        Target::Remote { base, spec } => proxy(&base, req, Some(&spec)).await,
    };

    // Which deployment answered. The difference between "the swap did not
    // happen" and "the swap happened and the page is still wrong" is otherwise
    // a guess.
    if let Ok(v) = HeaderValue::from_str(&route.deployment_id.to_string()) {
        response.headers_mut().insert("x-openpages-deployment", v);
    }
    response
}

fn not_a_site(state: &Shared, host: &str) -> Response {
    let body = format!(
        "openpages has nothing published at {host}.\n\n\
         Sites are served at <name>.{base}, proofs at <commit>--<name>.{base}.\n",
        base = state.cfg.server.base_domain
    );
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body,
    )
        .into_response()
}

// ── static ──────────────────────────────────────────────────────────────────

pub async fn serve_static(dir: &Path, rules: &Rules, req: Request) -> Response {
    let path = req.uri().path().to_string();

    if let Some(rel) = safe_join(&path) {
        if let Some((file, status)) = pick_file(dir, &rel).await {
            return send_file(&file, status, &path, rules, req.headers()).await;
        }
    }

    match rules.redirect_for(&path) {
        // A 200 rule rewrites rather than redirects — this is how `/* /index.html 200`
        // makes a single-page app work.
        Some(Redirect::Found { to, status: 200 }) => {
            if let Some(rel) = safe_join(&to) {
                if let Some((file, _)) = pick_file(dir, &rel).await {
                    return send_file(&file, 200, &path, rules, req.headers()).await;
                }
            }
        }
        Some(Redirect::Found { to, status }) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::FOUND);
            return (code, [(header::LOCATION, to)]).into_response();
        }
        None => {}
    }

    let custom = dir.join("404.html");
    if custom.is_file() {
        return send_file(&custom, 404, &path, rules, req.headers()).await;
    }
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        "<!doctype html><title>404</title><h1>404</h1><p>Not published here.</p>",
    )
        .into_response()
}

/// `/`, `/about`, `/about/` and `/about.html` should all find the same page —
/// static site generators disagree about which one they emit.
async fn pick_file(dir: &Path, rel: &str) -> Option<(PathBuf, u16)> {
    let base = dir.join(rel);
    if base.is_file() {
        return Some((base, 200));
    }
    if base.is_dir() {
        let idx = base.join("index.html");
        if idx.is_file() {
            return Some((idx, 200));
        }
    }
    if !rel.is_empty() {
        let with_html = dir.join(format!("{}.html", rel.trim_end_matches('/')));
        if with_html.is_file() {
            return Some((with_html, 200));
        }
    }
    None
}

async fn send_file(
    file: &Path,
    status: u16,
    request_path: &str,
    rules: &Rules,
    req_headers: &HeaderMap,
) -> Response {
    let Ok(meta) = tokio::fs::metadata(file).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = etag_for(&meta);

    if let Some(inm) = header_str(req_headers, header::IF_NONE_MATCH) {
        if inm.split(',').any(|t| t.trim() == etag) {
            return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
        }
    }

    let Ok(bytes) = tokio::fs::read(file).await else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };

    let mime = mime_guess::from_path(file).first_or_octet_stream();
    // Text without a charset is a decade-old source of mojibake. Say UTF-8.
    let content_type = if mime.type_() == mime_guess::mime::TEXT
        || mime.subtype() == "javascript"
        || mime.subtype() == "json"
    {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    if let Ok(v) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, v);
    }
    if let Ok(v) = HeaderValue::from_str(cache_control(file)) {
        headers.insert(header::CACHE_CONTROL, v);
    }
    headers.insert("x-openpages", HeaderValue::from_static("static"));

    for (k, v) in rules.headers_for(request_path) {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(k.as_str()), HeaderValue::from_str(&v))
        {
            headers.insert(name, value);
        }
    }

    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        headers,
        bytes,
    )
        .into_response()
}

/// Content-hashed assets can be cached forever; HTML never can, or a swap
/// would take a week to become visible.
fn cache_control(file: &Path) -> &'static str {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let hashed = name
        .rsplit_once('.')
        .map(|(stem, _)| {
            stem.rsplit(['-', '.'])
                .next()
                .map(|seg| {
                    seg.len() >= 8
                        && seg.chars().all(|c| c.is_ascii_alphanumeric())
                        && seg.chars().any(|c| c.is_ascii_digit())
                })
                .unwrap_or(false)
        })
        .unwrap_or(false);

    if name.ends_with(".html") {
        "public, max-age=0, must-revalidate"
    } else if hashed {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    }
}

fn etag_for(meta: &std::fs::Metadata) -> String {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", meta.len(), mtime)
}

/// Reject anything that could escape the deployment directory. Percent-decoding
/// happens first, because `%2e%2e%2f` is `../` and a check that runs before
/// decoding sees neither.
pub fn safe_join(path: &str) -> Option<String> {
    let decoded = urlencoding::decode(path).ok()?.into_owned();
    if decoded.contains('\0') {
        return None;
    }
    let mut out: Vec<&str> = Vec::new();
    for part in decoded.split('/') {
        match part {
            "" | "." => {}
            ".." => return None,
            // `.git`, `.env` and friends are never published, but `.well-known`
            // is a real published path (ACME, app association files).
            ".well-known" => out.push(part),
            p if p.starts_with('.') && p.len() > 1 => return None,
            p => out.push(p),
        }
    }
    Some(out.join("/"))
}

// ── _redirects and _headers ─────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct Rules {
    redirects: Vec<Rule>,
    headers: Vec<HeaderRule>,
}

#[derive(Debug)]
struct Rule {
    from: String,
    to: String,
    status: u16,
    splat: bool,
}

#[derive(Debug)]
struct HeaderRule {
    pattern: String,
    splat: bool,
    values: Vec<(String, String)>,
}

pub enum Redirect {
    Found { to: String, status: u16 },
}

impl Rules {
    pub fn load(dir: &Path) -> Self {
        Self {
            redirects: std::fs::read_to_string(dir.join("_redirects"))
                .map(|s| parse_redirects(&s))
                .unwrap_or_default(),
            headers: std::fs::read_to_string(dir.join("_headers"))
                .map(|s| parse_headers(&s))
                .unwrap_or_default(),
        }
    }

    pub fn redirect_for(&self, path: &str) -> Option<Redirect> {
        for r in &self.redirects {
            if r.splat {
                let prefix = r.from.trim_end_matches('*');
                if let Some(rest) = path.strip_prefix(prefix) {
                    return Some(Redirect::Found {
                        to: r.to.replace(":splat", rest),
                        status: r.status,
                    });
                }
            } else if r.from == path {
                return Some(Redirect::Found {
                    to: r.to.clone(),
                    status: r.status,
                });
            }
        }
        None
    }

    pub fn headers_for(&self, path: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for h in &self.headers {
            let hit = if h.splat {
                path.starts_with(h.pattern.trim_end_matches('*'))
            } else {
                h.pattern == path
            };
            if hit {
                out.extend(h.values.iter().cloned());
            }
        }
        out
    }
}

fn parse_redirects(text: &str) -> Vec<Rule> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let from = parts.next()?.to_string();
            let to = parts.next()?.to_string();
            let status = parts
                .next()
                .and_then(|s| s.trim_end_matches('!').parse().ok())
                .unwrap_or(301);
            Some(Rule {
                splat: from.ends_with('*'),
                from,
                to,
                status,
            })
        })
        .collect()
}

fn parse_headers(text: &str) -> Vec<HeaderRule> {
    let mut out: Vec<HeaderRule> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            let pattern = line.trim().to_string();
            out.push(HeaderRule {
                splat: pattern.ends_with('*'),
                pattern,
                values: Vec::new(),
            });
        } else if let Some(rule) = out.last_mut() {
            if let Some((k, v)) = line.trim().split_once(':') {
                rule.values
                    .push((k.trim().to_string(), v.trim().to_string()));
            }
        }
    }
    out
}

// ── proxy ───────────────────────────────────────────────────────────────────

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("building the proxy client")
    })
}

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

pub async fn proxy(base: &str, req: Request, spec: Option<&str>) -> Response {
    let (parts, body) = req.into_parts();
    let path_q = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", base.trim_end_matches('/'), path_q);

    let bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };

    let mut out = client().request(parts.method.clone(), &url);
    for (k, v) in parts.headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&name.as_str()) || name == "host" {
            continue;
        }
        out = out.header(k, v);
    }
    out = out.header("x-forwarded-proto", "https");
    if let Some(h) = parts.headers.get(header::HOST) {
        out = out.header("x-forwarded-host", h);
    }
    if let Some(spec) = spec {
        out = out.header(TARGET_HEADER, spec);
    }

    let resp = match out.body(bytes).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!(
                    "openpages: upstream did not answer ({})\n",
                    crate::gh::first_line(&e.to_string())
                ),
            )
                .into_response()
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = HeaderMap::new();
    for (k, v) in resp.headers().iter() {
        if HOP_BY_HOP.contains(&k.as_str().to_ascii_lowercase().as_str()) {
            continue;
        }
        headers.insert(k.clone(), v.clone());
    }
    headers.insert("x-openpages", HeaderValue::from_static("proxy"));

    let stream = futures_util::TryStreamExt::map_err(resp.bytes_stream(), std::io::Error::other);
    (status, headers, Body::from_stream(stream)).into_response()
}

pub fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panel_host_beats_a_site_with_the_same_name() {
        assert!(matches!(
            classify(
                "openpages.example.com",
                "pages.example.com",
                Some("openpages.example.com")
            ),
            Host::Panel
        ));
    }

    #[test]
    fn production_and_preview_hosts_are_told_apart() {
        match classify("blog.pages.example.com", "pages.example.com", None) {
            Host::Site { slug, preview } => {
                assert_eq!(slug, "blog");
                assert!(preview.is_none());
            }
            _ => panic!("expected a site"),
        }
        match classify(
            "a1b2c3d4--blog.pages.example.com",
            "pages.example.com",
            None,
        ) {
            Host::Site { slug, preview } => {
                assert_eq!(slug, "blog");
                assert_eq!(preview, Some("a1b2c3d4"));
            }
            _ => panic!("expected a preview"),
        }
    }

    #[test]
    fn a_port_in_the_host_header_is_ignored() {
        assert!(matches!(
            classify("blog.pages.example.com:8788", "pages.example.com", None),
            Host::Site { .. }
        ));
    }

    #[test]
    fn unrelated_and_nested_hosts_are_rejected() {
        assert!(matches!(
            classify("example.com", "pages.example.com", None),
            Host::Unknown
        ));
        assert!(matches!(
            classify("pages.example.com", "pages.example.com", None),
            Host::Unknown
        ));
        assert!(matches!(
            classify("a.b.pages.example.com", "pages.example.com", None),
            Host::Unknown
        ));
    }

    #[test]
    fn path_traversal_cannot_escape_the_deployment() {
        assert_eq!(safe_join("/a/b.css").as_deref(), Some("a/b.css"));
        assert_eq!(safe_join("/"), Some(String::new()));
        assert_eq!(safe_join("/../../etc/passwd"), None);
        assert_eq!(safe_join("/a/../../etc/passwd"), None);
        assert_eq!(safe_join("/%2e%2e/%2e%2e/etc/passwd"), None);
        assert_eq!(safe_join("/.git/config"), None);
        assert_eq!(safe_join("/.env"), None);
    }

    #[test]
    fn well_known_survives_the_dotfile_ban() {
        assert_eq!(
            safe_join("/.well-known/acme-challenge/tok").as_deref(),
            Some(".well-known/acme-challenge/tok")
        );
        assert_eq!(safe_join("/.well-known/../.env"), None);
    }

    #[test]
    fn spa_fallback_rules_are_understood() {
        let rules = Rules {
            redirects: parse_redirects("# comment\n/old  /new  301\n/*   /index.html   200\n"),
            headers: vec![],
        };
        match rules.redirect_for("/old") {
            Some(Redirect::Found { to, status }) => {
                assert_eq!(to, "/new");
                assert_eq!(status, 301);
            }
            None => panic!("expected a redirect"),
        }
        match rules.redirect_for("/anything/deep") {
            Some(Redirect::Found { to, status }) => {
                assert_eq!(to, "/index.html");
                assert_eq!(status, 200);
            }
            None => panic!("expected the SPA fallback"),
        }
    }

    #[test]
    fn splat_substitution_carries_the_rest_of_the_path() {
        let rules = Rules {
            redirects: parse_redirects("/blog/* /posts/:splat 301"),
            headers: vec![],
        };
        match rules.redirect_for("/blog/hello") {
            Some(Redirect::Found { to, .. }) => assert_eq!(to, "/posts/hello"),
            None => panic!("expected a redirect"),
        }
    }

    #[test]
    fn headers_file_applies_by_prefix() {
        let rules = Rules {
            redirects: vec![],
            headers: parse_headers(
                "/*\n  X-Frame-Options: DENY\n\n/admin/*\n  X-Robots-Tag: noindex\n",
            ),
        };
        let root = rules.headers_for("/index.html");
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].0, "X-Frame-Options");
        assert_eq!(rules.headers_for("/admin/x").len(), 2);
    }

    #[test]
    fn hashed_assets_are_immutable_and_html_never_is() {
        assert_eq!(
            cache_control(Path::new("/x/index.html")),
            "public, max-age=0, must-revalidate"
        );
        assert_eq!(
            cache_control(Path::new("/x/app.a1b2c3d4.js")),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control(Path::new("/x/logo.svg")),
            "public, max-age=3600"
        );
    }
}
