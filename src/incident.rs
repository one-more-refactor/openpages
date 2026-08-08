//! Turning a build log into something worth reading.
//!
//! A failed `npm run build` can produce 4,000 lines, of which about nine
//! matter. Everything downstream of a failure — the panel's failure banner, the
//! deduplication key, and the report handed to Claude — reads the distilled
//! form, never the log. That is not only about cost: a model given 4,000 lines
//! of progress bars finds the wrong error as reliably as a person does.

use crate::util::{is_noise_line, strip_ansi};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incident {
    /// One line, for the panel and for a PR title.
    pub summary: String,
    /// The distilled report. Bounded by `budget`.
    pub report: String,
    /// Stable across re-runs of the same failure, so the same broken commit is
    /// not healed twice.
    pub fingerprint: String,
    pub files: Vec<String>,
}

/// Markers that mean "the build actually failed here", most specific first.
const ERROR_MARKERS: &[&str] = &[
    "error[E",   // rustc
    "error TS",  // typescript
    "ERROR in ", // webpack
    "Error: ",
    "error: ",
    "ERROR: ",
    "SyntaxError",
    "TypeError",
    "ReferenceError",
    "Cannot find module",
    "Module not found",
    "Failed to compile",
    "Build failed",
    "command not found",
    "No such file or directory",
    "npm ERR!",
    "Permission denied",
    "ELIFECYCLE",
    "exit code",
    "panicked at",
];

pub fn distill(log: &str, context: &str, budget: usize) -> Incident {
    let cleaned: Vec<String> = log
        .lines()
        .map(strip_ansi)
        .map(|l| l.trim_end().to_string())
        .filter(|l| !is_noise_line(l))
        .collect();

    let deduped = dedupe_consecutive(&cleaned);

    let anchor = deduped
        .iter()
        .position(|l| ERROR_MARKERS.iter().any(|m| l.contains(m)));

    // Without a marker the tail is the best evidence we have: whatever the
    // process printed just before it gave up.
    let window: Vec<String> = match anchor {
        Some(i) => {
            let start = i.saturating_sub(4);
            let end = (i + 26).min(deduped.len());
            deduped[start..end].to_vec()
        }
        None => deduped.iter().rev().take(30).rev().cloned().collect(),
    };

    let summary = window
        .iter()
        .find(|l| ERROR_MARKERS.iter().any(|m| l.contains(m)))
        .or_else(|| window.iter().rev().find(|l| !l.trim().is_empty()))
        .map(|l| truncate(l.trim(), 180))
        .unwrap_or_else(|| "build failed with no output".to_string());

    let files = extract_paths(&window);

    let mut report = String::new();
    report.push_str(context);
    report.push_str("\n\n");
    if !files.is_empty() {
        report.push_str(&format!(
            "Files named in the error: {}\n\n",
            files.join(", ")
        ));
    }
    report.push_str("--- distilled build output ---\n");
    for line in &window {
        if report.len() + line.len() + 1 > budget {
            report.push_str("… (truncated)\n");
            break;
        }
        report.push_str(line);
        report.push('\n');
    }

    Incident {
        fingerprint: crate::util::sha256_hex(&normalise_for_fingerprint(&summary))[..16]
            .to_string(),
        summary,
        report,
        files,
    }
}

fn dedupe_consecutive(lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut repeats = 0usize;
    for l in lines {
        if out.last().map(|p| p == l).unwrap_or(false) {
            repeats += 1;
            continue;
        }
        if repeats > 0 {
            out.push(format!("… (previous line repeated {repeats}×)"));
            repeats = 0;
        }
        out.push(l.clone());
    }
    if repeats > 0 {
        out.push(format!("… (previous line repeated {repeats}×)"));
    }
    out
}

/// Numbers, hashes and temp paths change between runs of the same failure.
/// Without this, the same broken build looks new every time and the healer
/// would open a fresh pull request on every push.
fn normalise_for_fingerprint(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_digit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            if !last_was_digit {
                out.push('#');
            }
            last_was_digit = true;
        } else {
            out.push(c);
            last_was_digit = false;
        }
    }
    out
}

fn extract_paths(lines: &[String]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in lines {
        for token in
            line.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '"' || c == '\'')
        {
            let t = token.trim_matches(|c: char| c == ',' || c == ':' || c == ';');
            let t = t.split(':').next().unwrap_or(t); // strip :line:col
            if t.len() < 4 || t.len() > 120 || !t.contains('/') {
                continue;
            }
            if !SOURCE_EXTS.iter().any(|e| t.ends_with(e)) {
                continue;
            }
            if t.contains("node_modules") || t.starts_with("http") {
                continue;
            }
            let t = t.trim_start_matches("./").to_string();
            if !found.contains(&t) {
                found.push(t);
            }
            if found.len() >= 6 {
                return found;
            }
        }
    }
    found
}

const SOURCE_EXTS: &[&str] = &[
    ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".svelte", ".vue", ".astro", ".rs", ".go", ".py",
    ".json", ".yaml", ".yml", ".toml", ".css", ".scss", ".html", ".md", ".mdx",
];

/// First sentence, for a commit subject or a PR title.
pub fn first_sentence(s: &str) -> String {
    let s = s.trim();
    let cut = s.find(". ").map(|i| i + 1).unwrap_or(s.len());
    truncate(s[..cut].trim_end_matches('.').trim(), 68)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
}

/// Remove anything that looks like the credentials we handed to the build.
/// Called on every log line before it is stored or displayed.
pub fn redact(line: &str, secrets: &[String]) -> String {
    let mut out = line.to_string();
    for s in secrets {
        if s.len() >= 8 && out.contains(s.as_str()) {
            out = out.replace(s.as_str(), "«redacted»");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOISY: &str = r#"
npm notice New minor version available
⠋ installing
⠙ installing
========================================
> app@1.0.0 build
> vite build

vite v5.0.0 building for production...
transforming...
\u{1b}[31merror\u{1b}[0m during build:
error TS2307: Cannot find module './lib/missing' or its corresponding type declarations.
  at src/routes/index.ts:12:8
ELIFECYCLE Command failed with exit code 1.
"#;

    #[test]
    fn the_real_error_becomes_the_summary() {
        let i = distill(NOISY, "site: blog", 2000);
        assert!(i.summary.contains("TS2307"), "got: {}", i.summary);
    }

    #[test]
    fn progress_noise_does_not_reach_the_report() {
        let i = distill(NOISY, "site: blog", 2000);
        assert!(!i.report.contains("npm notice"));
        assert!(!i.report.contains("========"));
        assert!(i.report.contains("site: blog"));
    }

    #[test]
    fn the_report_respects_its_budget() {
        let huge = (0..5000)
            .map(|n| format!("Error: {n} {}", "x".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n");
        let i = distill(&huge, "ctx", 600);
        assert!(i.report.len() <= 800, "report was {} bytes", i.report.len());
        assert!(i.report.contains("truncated"));
    }

    #[test]
    fn the_window_stays_small_even_when_the_budget_is_generous() {
        let huge = (0..5000)
            .map(|n| format!("Error: line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let i = distill(&huge, "ctx", 100_000);
        assert!(
            i.report.lines().count() < 40,
            "a report is evidence, not an archive"
        );
    }

    #[test]
    fn the_same_failure_fingerprints_the_same_despite_changing_numbers() {
        let a = distill("Error: failed after 1200ms in build 41", "c", 900);
        let b = distill("Error: failed after 87ms in build 42", "c", 900);
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn different_failures_fingerprint_differently() {
        let a = distill("Error: Cannot find module './a'", "c", 900);
        let b = distill("Error: Cannot find module './b'", "c", 900);
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn source_paths_are_pulled_out_and_node_modules_ignored() {
        let i = distill(
            "Error: boom\n  at src/routes/index.ts:12:8\n  at node_modules/vite/x.js:1:1",
            "c",
            2000,
        );
        assert!(i.files.contains(&"src/routes/index.ts".to_string()));
        assert!(!i.files.iter().any(|f| f.contains("node_modules")));
    }

    #[test]
    fn repeated_lines_collapse_instead_of_filling_the_budget() {
        let spam = std::iter::repeat_n("Error: same", 500)
            .collect::<Vec<_>>()
            .join("\n");
        let i = distill(&spam, "c", 2000);
        assert!(i.report.contains("repeated"));
        assert!(i.report.lines().count() < 20);
    }

    #[test]
    fn an_empty_log_still_produces_a_usable_summary() {
        let i = distill("", "ctx", 500);
        assert_eq!(i.summary, "build failed with no output");
    }

    #[test]
    fn secrets_never_survive_redaction() {
        let token = "ghs_abcdefghijklmnop".to_string();
        let line = format!("cloning https://x-access-token:{token}@github.com/a/b.git");
        assert!(!redact(&line, &[token]).contains("ghs_"));
    }
}
