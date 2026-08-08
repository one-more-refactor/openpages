use chrono::{DateTime, SecondsFormat, Utc};
use rand::Rng;
use sha2::{Digest, Sha256};

/// URL-safe opaque token. Used for sessions and node enrollment.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill(&mut buf[..]);
    hex::encode(buf)
}

pub fn sha256_hex(input: &str) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    hex::encode(h.finalize())
}

/// Constant-time-ish comparison for secrets that arrive over the wire.
pub fn secret_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Minute-resolution key for the traffic table.
pub fn minute_bucket(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M").to_string()
}

/// Hour-of-week, 0..168. The swap scheduler compares a site against its own
/// history for the same slot, so a Sunday 04:00 deploy is not judged against
/// Tuesday lunchtime.
pub fn hour_of_week(t: DateTime<Utc>) -> i64 {
    use chrono::{Datelike, Timelike};
    let dow = t.weekday().num_days_from_monday() as i64;
    dow * 24 + t.hour() as i64
}

/// A DNS label: lowercase, alphanumeric and dashes, no leading/trailing dash.
pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = true; // suppresses a leading dash
    for ch in input.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out.truncate(63);
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Strip ANSI SGR/CSI escapes. Build logs are full of them and they wreck both
/// the log viewer and any attempt to match error text.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('@'..='~').contains(&c2) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: runs until BEL or ST
                chars.next();
                while let Some(c2) = chars.next() {
                    if c2 == '\u{7}' {
                        break;
                    }
                    if c2 == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// True for lines that only exist to animate a terminal: spinners, percentage
/// counters, progress bars. Dropping these is what keeps an incident report
/// small enough to be worth sending anywhere.
pub fn is_noise_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return true;
    }
    if t.chars().all(|c| "=-_.#>|/\\ []%".contains(c)) {
        return true;
    }
    const NOISE: [&str; 10] = [
        "npm notice",
        "npm WARN deprecated",
        "Downloading",
        "Fetching",
        "reused",
        "Compiling ",
        "Progress:",
        "⠋",
        "⠙",
        "warning: unused",
    ];
    NOISE.iter().any(|n| t.starts_with(n) || t.contains(n))
}

pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
    }
}

/// 1-minute load average per core. Used to decide when the machine is idle
/// enough to start compiling a new version.
pub fn load_per_core() -> f64 {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0);
    let Ok(raw) = std::fs::read_to_string("/proc/loadavg") else {
        return 0.0;
    };
    raw.split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|l| l / cores)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_is_a_valid_dns_label() {
        assert_eq!(slugify("My Blog!"), "my-blog");
        assert_eq!(slugify("--weird__name--"), "weird-name");
        assert_eq!(
            slugify("one-more-refactor/flick-web"),
            "one-more-refactor-flick-web"
        );
        assert!(!slugify("!!!").contains('-'));
        assert_eq!(slugify("a".repeat(90).as_str()).len(), 63);
    }

    #[test]
    fn ansi_is_removed_without_eating_text() {
        assert_eq!(strip_ansi("\u{1b}[31merror\u{1b}[0m: nope"), "error: nope");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}after"), "after");
    }

    #[test]
    fn noise_detection_keeps_real_errors() {
        assert!(is_noise_line("========================"));
        assert!(is_noise_line("   "));
        assert!(is_noise_line("npm notice New version"));
        assert!(!is_noise_line("error TS2307: Cannot find module './x'"));
        assert!(!is_noise_line("Error: Build failed with 1 error"));
    }

    #[test]
    fn quantiles_interpolate() {
        let v = vec![0.0, 1.0, 2.0, 3.0, 4.0];
        assert_eq!(quantile(&v, 0.0), 0.0);
        assert_eq!(quantile(&v, 1.0), 4.0);
        assert_eq!(quantile(&v, 0.5), 2.0);
        assert_eq!(quantile(&[], 0.5), 0.0);
    }

    #[test]
    fn secret_eq_matches_only_on_equal() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "abcd"));
    }
}
