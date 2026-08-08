use std::{fs, path::Path};

// The Svelte panel is embedded into the binary. During a plain `cargo build`
// (CI lint, `cargo check`, a fresh clone) `web/dist` may not exist yet and
// rust-embed refuses to compile against a missing folder. Rather than making
// the Rust build depend on bun, drop in a placeholder that says exactly what
// is missing — a binary built this way still serves the whole API.
fn main() {
    let dist = Path::new("web/dist");
    if !dist.join("index.html").exists() {
        let _ = fs::create_dir_all(dist);
        let _ = fs::write(dist.join("index.html"), PLACEHOLDER);
    }
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-changed=migrations");
}

const PLACEHOLDER: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>openpages — panel not built</title>
<style>
  html{background:#12100e;color:#f2ebdd;font:15px/1.6 ui-monospace,SFMono-Regular,Menlo,monospace}
  body{margin:0;display:grid;place-items:center;min-height:100vh;padding:2rem}
  div{max-width:44ch;border-top:2px solid #d4482a;padding-top:1.25rem}
  b{color:#d4482a;letter-spacing:.18em;text-transform:uppercase;font-size:.72rem}
  code{background:#1d1a17;padding:.15em .45em}
</style>
<div>
  <b>No plate mounted</b>
  <p>This binary was compiled without the web panel. The API is running normally.</p>
  <p>Build it: <code>cd web &amp;&amp; bun install &amp;&amp; bun run build</code>, then rebuild the binary.</p>
</div>
"#;
