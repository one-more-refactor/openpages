//! Work out how to build a repo nobody has configured.
//!
//! Detection is a convenience, never a constraint: every field it produces is
//! an editable default on the site, and an explicit setting always wins. It
//! also refuses to guess an output directory it cannot justify — a wrong guess
//! deploys an empty site, which is worse than asking.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    pub framework: String,
    /// `static` (build once, serve files) or `container` (repo has a Dockerfile).
    pub kind: String,
    pub image: String,
    pub install: Option<String>,
    pub build: Option<String>,
    pub output: Option<String>,
}

const NODE: &str = "docker.io/library/node:22-alpine";
const BUN: &str = "docker.io/oven/bun:1";
const HUGO: &str = "docker.io/hugomods/hugo:exts";
const RUBY: &str = "docker.io/library/ruby:3.3";
const PYTHON: &str = "docker.io/library/python:3.12-slim";

pub fn detect(dir: &Path) -> Recipe {
    if dir.join("Dockerfile").is_file() {
        return Recipe {
            framework: "dockerfile".into(),
            kind: "container".into(),
            image: String::new(),
            install: None,
            build: None,
            output: None,
        };
    }

    if let Some(r) = detect_node(dir) {
        return r;
    }
    if dir.join("config.toml").is_file() && dir.join("content").is_dir() {
        return statically("hugo", HUGO, None, Some("hugo --minify"), Some("public"));
    }
    if dir.join("hugo.toml").is_file() || dir.join("hugo.yaml").is_file() {
        return statically("hugo", HUGO, None, Some("hugo --minify"), Some("public"));
    }
    if dir.join("config.toml").is_file() && dir.join("templates").is_dir() {
        return statically(
            "zola",
            "docker.io/ghcr.io/getzola/zola:v0.19.2",
            None,
            Some("zola build"),
            Some("public"),
        );
    }
    if dir.join("_config.yml").is_file() || dir.join("Gemfile").is_file() {
        return statically(
            "jekyll",
            RUBY,
            Some("bundle install"),
            Some("bundle exec jekyll build"),
            Some("_site"),
        );
    }
    if dir.join("mkdocs.yml").is_file() {
        return statically(
            "mkdocs",
            PYTHON,
            Some("pip install --no-cache-dir mkdocs mkdocs-material"),
            Some("mkdocs build"),
            Some("site"),
        );
    }
    if dir.join("index.html").is_file() {
        return Recipe {
            framework: "static".into(),
            kind: "static".into(),
            image: NODE.into(),
            install: None,
            build: None,
            output: Some(".".into()),
        };
    }

    Recipe {
        framework: "unknown".into(),
        kind: "static".into(),
        image: NODE.into(),
        install: None,
        build: None,
        output: None,
    }
}

fn statically(
    framework: &str,
    image: &str,
    install: Option<&str>,
    build: Option<&str>,
    output: Option<&str>,
) -> Recipe {
    Recipe {
        framework: framework.into(),
        kind: "static".into(),
        image: image.into(),
        install: install.map(str::to_string),
        build: build.map(str::to_string),
        output: output.map(str::to_string),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pm {
    Bun,
    Pnpm,
    Yarn,
    Npm,
}

impl Pm {
    pub fn detect(dir: &Path) -> Self {
        if dir.join("bun.lockb").is_file() || dir.join("bun.lock").is_file() {
            Self::Bun
        } else if dir.join("pnpm-lock.yaml").is_file() {
            Self::Pnpm
        } else if dir.join("yarn.lock").is_file() {
            Self::Yarn
        } else {
            Self::Npm
        }
    }

    /// Frozen-lockfile installs everywhere. A build that silently resolves a
    /// different dependency tree than the one committed is not reproducible,
    /// and the day it matters is the day it breaks.
    pub fn install(self, has_lock: bool) -> String {
        match (self, has_lock) {
            (Self::Bun, true) => "bun install --frozen-lockfile".into(),
            (Self::Bun, false) => "bun install".into(),
            (Self::Pnpm, true) => "corepack enable && pnpm install --frozen-lockfile".into(),
            (Self::Pnpm, false) => "corepack enable && pnpm install".into(),
            (Self::Yarn, true) => "corepack enable && yarn install --immutable".into(),
            (Self::Yarn, false) => "corepack enable && yarn install".into(),
            (Self::Npm, true) => "npm ci".into(),
            (Self::Npm, false) => "npm install --no-audit --no-fund".into(),
        }
    }

    pub fn run(self, script: &str) -> String {
        match self {
            Self::Bun => format!("bun run {script}"),
            Self::Pnpm => format!("pnpm run {script}"),
            Self::Yarn => format!("yarn run {script}"),
            Self::Npm => format!("npm run {script}"),
        }
    }

    pub fn image(self) -> &'static str {
        match self {
            Self::Bun => BUN,
            _ => NODE,
        }
    }
}

fn detect_node(dir: &Path) -> Option<Recipe> {
    let raw = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let pkg: serde_json::Value = serde_json::from_str(&raw).ok()?;

    let deps: BTreeMap<String, String> = ["dependencies", "devDependencies"]
        .iter()
        .filter_map(|k| pkg.get(*k))
        .filter_map(|v| v.as_object())
        .flat_map(|o| o.iter().map(|(k, v)| (k.clone(), v.to_string())))
        .collect();
    let scripts = pkg
        .get("scripts")
        .and_then(|s| s.as_object())
        .cloned()
        .unwrap_or_default();

    let pm = Pm::detect(dir);
    let has_lock = dir.join("bun.lockb").is_file()
        || dir.join("bun.lock").is_file()
        || dir.join("pnpm-lock.yaml").is_file()
        || dir.join("yarn.lock").is_file()
        || dir.join("package-lock.json").is_file();

    let has = |name: &str| deps.contains_key(name);

    // Ordered most specific first: Next ships with React, Nuxt ships with Vue,
    // and a SvelteKit repo also has vite in devDependencies.
    let (framework, output) = if has("next") {
        ("next", "out")
    } else if has("nuxt") || has("nuxt3") {
        ("nuxt", ".output/public")
    } else if has("@sveltejs/kit") {
        ("sveltekit", "build")
    } else if has("astro") {
        ("astro", "dist")
    } else if has("@docusaurus/core") {
        ("docusaurus", "build")
    } else if has("gatsby") {
        ("gatsby", "public")
    } else if has("@angular/core") {
        ("angular", "dist")
    } else if has("@11ty/eleventy") {
        ("eleventy", "_site")
    } else if has("vitepress") {
        ("vitepress", ".vitepress/dist")
    } else if has("react-scripts") {
        ("create-react-app", "build")
    } else if has("vite") {
        ("vite", "dist")
    } else if has("parcel") {
        ("parcel", "dist")
    } else {
        ("node", "dist")
    };

    let build_script = ["build", "build:prod", "generate", "export"]
        .iter()
        .find(|s| scripts.contains_key(**s))
        .copied();

    // A Next.js project only produces `out/` when it is configured to export.
    // Guessing otherwise deploys nothing, so leave output empty and let the
    // panel ask rather than shipping a blank page.
    let output = if framework == "next" && !next_exports(dir) {
        None
    } else {
        Some(output.to_string())
    };

    Some(Recipe {
        framework: framework.into(),
        kind: "static".into(),
        image: pm.image().into(),
        install: Some(pm.install(has_lock)),
        build: build_script.map(|s| pm.run(s)),
        output,
    })
}

fn next_exports(dir: &Path) -> bool {
    for name in [
        "next.config.js",
        "next.config.mjs",
        "next.config.ts",
        "next.config.cjs",
    ] {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            if squeezed.contains("output:'export'") || squeezed.contains("output:\"export\"") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(files: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = d.path().join(name);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, body).unwrap();
        }
        d
    }

    #[test]
    fn a_dockerfile_wins_over_everything_else() {
        let d = tmp(&[("Dockerfile", "FROM alpine"), ("package.json", "{}")]);
        let r = detect(d.path());
        assert_eq!(r.kind, "container");
        assert_eq!(r.framework, "dockerfile");
    }

    #[test]
    fn astro_with_pnpm_gets_a_frozen_lockfile_install() {
        let d = tmp(&[
            (
                "package.json",
                r#"{"dependencies":{"astro":"^4"},"scripts":{"build":"astro build"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
        ]);
        let r = detect(d.path());
        assert_eq!(r.framework, "astro");
        assert_eq!(r.output.as_deref(), Some("dist"));
        assert!(r.install.unwrap().contains("--frozen-lockfile"));
        assert_eq!(r.build.as_deref(), Some("pnpm run build"));
    }

    #[test]
    fn sveltekit_is_not_mistaken_for_vite() {
        let d = tmp(&[(
            "package.json",
            r#"{"devDependencies":{"@sveltejs/kit":"^2","vite":"^5"},"scripts":{"build":"vite build"}}"#,
        )]);
        assert_eq!(detect(d.path()).framework, "sveltekit");
    }

    #[test]
    fn next_without_export_config_refuses_to_guess_an_output_dir() {
        let d = tmp(&[(
            "package.json",
            r#"{"dependencies":{"next":"14"},"scripts":{"build":"next build"}}"#,
        )]);
        let r = detect(d.path());
        assert_eq!(r.framework, "next");
        assert_eq!(
            r.output, None,
            "guessing out/ here would deploy an empty site"
        );

        let d2 = tmp(&[
            (
                "package.json",
                r#"{"dependencies":{"next":"14"},"scripts":{"build":"next build"}}"#,
            ),
            ("next.config.mjs", "export default { output: 'export' }"),
        ]);
        assert_eq!(detect(d2.path()).output.as_deref(), Some("out"));
    }

    #[test]
    fn bun_projects_get_the_bun_image() {
        let d = tmp(&[
            (
                "package.json",
                r#"{"devDependencies":{"vite":"^5"},"scripts":{"build":"vite build"}}"#,
            ),
            ("bun.lockb", ""),
        ]);
        let r = detect(d.path());
        assert_eq!(r.image, BUN);
        assert_eq!(r.build.as_deref(), Some("bun run build"));
    }

    #[test]
    fn a_bare_html_folder_needs_no_build_at_all() {
        let d = tmp(&[("index.html", "<h1>hi</h1>")]);
        let r = detect(d.path());
        assert_eq!(r.framework, "static");
        assert_eq!(r.build, None);
        assert_eq!(r.output.as_deref(), Some("."));
    }

    #[test]
    fn hugo_and_jekyll_are_recognised_without_a_package_json() {
        let d = tmp(&[("hugo.toml", "title = 'x'")]);
        assert_eq!(detect(d.path()).framework, "hugo");
        let d = tmp(&[("_config.yml", "title: x")]);
        assert_eq!(detect(d.path()).framework, "jekyll");
    }

    #[test]
    fn nothing_recognisable_yields_no_build_command() {
        let d = tmp(&[("readme.txt", "hello")]);
        let r = detect(d.path());
        assert_eq!(r.framework, "unknown");
        assert_eq!(r.output, None);
    }
}
