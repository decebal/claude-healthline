//! claude-mcp-probe — records MCP server health for the status line to render.
//!
//! Wire this to Claude Code's `SessionStart` hook. It runs `claude mcp list`,
//! reduces the output to `{name: status}` pairs, and writes them to
//! `~/.claude/healthline-cache/mcp.json` (override with
//! `CLAUDE_HEALTHLINE_MCP_CACHE`). The status line only ever READS that file,
//! because `claude mcp list` takes seconds — far too slow to run on a render.
//!
//! Claude Code BLOCKS session startup on its hooks, so the hook invocation
//! itself does almost nothing: it stats the cache, returns immediately if it is
//! younger than `CLAUDE_HEALTHLINE_MCP_REFRESH` (default 4h), and otherwise
//! re-launches itself DETACHED to do the slow part. Run with `--foreground` to
//! probe synchronously (useful for testing or a manual refresh).
//!
//! SECURITY: `claude mcp list` echoes each stdio server's full argv, and those
//! frequently carry secrets (`--api-key eyJhbGci...`, tokens, DSNs). This probe
//! keeps ONLY the server name and its connection status; the command/URL field
//! is dropped before anything is written and is never logged. Do not "improve"
//! this by caching the raw output.
//!
//! Always exits 0 so it can never break session startup.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Hard bound on the probe. `claude mcp list` health-checks every remote
/// connector serially, so a slow connector can drag it out; past this we keep
/// whatever the previous run cached rather than stalling startup.
const PROBE_TIMEOUT_SECS: u64 = 20;

/// Longest server name we record. Names come from config files, so they are
/// semi-trusted at best.
const MAX_NAME_CHARS: usize = 48;

/// Refuse to record an absurd number of servers (a malformed parse, not a real
/// fleet).
const MAX_SERVERS: usize = 128;

fn cache_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_HEALTHLINE_MCP_CACHE") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let base = match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => Path::new(&std::env::var("HOME").ok()?).join(".claude"),
    };
    Some(base.join("healthline-cache/mcp.json"))
}

fn claude_bin() -> String {
    if let Ok(b) = std::env::var("CLAUDE_HEALTHLINE_CLAUDE_BIN") {
        if !b.trim().is_empty() {
            return b;
        }
    }
    "claude".to_string()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Atomic write: temp file in the same dir, then rename.
fn write_atomic(path: &Path, contents: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), now_unix()));
        if std::fs::write(&tmp, contents).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Normalize one status phrase from `claude mcp list` into a stable token.
/// Unknown phrasings degrade to `"unknown"` rather than being guessed at, so a
/// future CLI wording change can never turn a broken server into a green one.
fn classify(status: &str) -> &'static str {
    let s = status.to_ascii_lowercase();
    if s.contains("connected") {
        "connected"
    } else if s.contains("needs authentication") || s.contains("authenticat") {
        "needs_auth"
    } else if s.contains("fail") || s.contains("error") || s.contains("timed out") {
        "failed"
    } else if s.contains("disabled") {
        "disabled"
    } else {
        "unknown"
    }
}

/// Strip control characters and bound the length. Server names land in a status
/// line, so a name carrying an ANSI escape must not survive this far.
fn clean_name(raw: &str) -> Option<String> {
    let mut out: String = raw.chars().filter(|c| !c.is_control()).collect();
    out = out.trim().to_string();
    if out.is_empty() {
        return None;
    }
    if out.chars().count() > MAX_NAME_CHARS {
        out = out.chars().take(MAX_NAME_CHARS).collect();
    }
    Some(out)
}

/// Parse `claude mcp list` output into `(name, status)` pairs.
///
/// Each server line looks like:
///   `<name>: <command-or-url> - <status>`
/// The middle field is DISCARDED (it carries secrets — see the module note).
/// Splitting the status off uses the LAST ` - ` on the line, because an argv can
/// legitimately contain ` - ` while the status never does.
fn parse_mcp_list(out: &str) -> Vec<(String, &'static str)> {
    let mut servers = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        // Skip the "Checking MCP server health…" preamble and blank lines.
        if line.is_empty() || !line.contains(": ") {
            continue;
        }
        let (name, rest) = match line.split_once(": ") {
            Some(pair) => pair,
            None => continue,
        };
        // No trailing status => not a server row (e.g. an advisory line).
        let status = match rest.rsplit_once(" - ") {
            Some((_dropped_command_or_url, status)) => status,
            None => continue,
        };
        if let Some(name) = clean_name(name) {
            servers.push((name, classify(status)));
        }
        if servers.len() >= MAX_SERVERS {
            break;
        }
    }
    servers
}

/// Run `claude mcp list` under a hard timeout. `None` on timeout or any spawn
/// failure — the caller then leaves the existing cache untouched.
fn run_mcp_list() -> Option<String> {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        use std::process::{Command, Stdio};
        let res = Command::new(claude_bin())
            .args(["mcp", "list"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
        let _ = tx.send(res);
    });

    rx.recv_timeout(Duration::from_secs(PROBE_TIMEOUT_SECS))
        .ok()
        .flatten()
}

/// Age of the cache, in seconds. `None` when there is no readable cache yet.
fn cache_age_secs(path: &Path) -> Option<u64> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(mtime)
        .ok()
        .map(|d| d.as_secs())
}

/// How long a probe result is considered good enough. Re-auth state changes on
/// the order of days, so refreshing every session start would spend seconds of
/// CPU to learn nothing.
fn refresh_interval_secs() -> u64 {
    std::env::var("CLAUDE_HEALTHLINE_MCP_REFRESH")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(4 * 3600)
}

/// Set in the detached child so it knows to do the actual work.
const CHILD_ENV: &str = "CLAUDE_HEALTHLINE_MCP_PROBE_CHILD";

/// Re-launch this same binary detached, so the SessionStart hook returns
/// immediately. Claude Code BLOCKS session startup on its hooks, and
/// `claude mcp list` takes seconds — running it inline would make every launch
/// feel broken. Returns false if the child could not be spawned.
fn spawn_detached() -> bool {
    use std::process::{Command, Stdio};
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return false,
    };
    Command::new(exe)
        .env(CHILD_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

fn main() {
    // Drain stdin so the hook never blocks on an unread pipe. The payload is
    // irrelevant: this probe reports machine-wide MCP state, not session state.
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);

    if matches!(std::env::var("CLAUDE_HEALTHLINE_NO_MCP"), Ok(v) if v == "1") {
        return;
    }

    let path = match cache_path() {
        Some(p) => p,
        None => return,
    };

    let is_child = matches!(std::env::var(CHILD_ENV), Ok(v) if v == "1");
    let forced = std::env::args().any(|a| a == "--foreground" || a == "-f");

    if !is_child && !forced {
        // Still fresh: do nothing at all. This is the common case, and it costs
        // one stat().
        if let Some(age) = cache_age_secs(&path) {
            if age < refresh_interval_secs() {
                return;
            }
        }
        // Stale or absent: hand the slow work to a detached child and let the
        // session start immediately.
        spawn_detached();
        return;
    }

    let out = match run_mcp_list() {
        Some(o) => o,
        // Timed out or `claude` unavailable: keep the previous cache rather
        // than replacing real data with an empty result.
        None => return,
    };

    let servers = parse_mcp_list(&out);
    // A successful run that parsed to nothing means the format changed. Writing
    // `{}` would render "all healthy", which is a lie — leave the cache alone.
    if servers.is_empty() {
        return;
    }

    let mut map = serde_json::Map::new();
    for (name, status) in servers {
        map.insert(name, Value::String(status.to_string()));
    }

    let payload = json!({
        "updated_at": now_unix(),
        "servers": Value::Object(map),
    });
    write_atomic(&path, &payload.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Checking MCP server health…\n\n\
claude.ai Google Calendar: https://calendarmcp.googleapis.com/mcp/v1 - ✔ Connected\n\
claude.ai Gamma: https://mcp.gamma.app/mcp - ! Needs authentication\n\
prime: allsource-prime --data-dir /home/u/.prime --api-key eyJhbGciOiJIUzI1NiJ9.secret - ✔ Connected\n";

    #[test]
    fn parses_names_and_statuses() {
        let got = parse_mcp_list(SAMPLE);
        assert_eq!(
            got,
            vec![
                ("claude.ai Google Calendar".to_string(), "connected"),
                ("claude.ai Gamma".to_string(), "needs_auth"),
                ("prime".to_string(), "connected"),
            ]
        );
    }

    #[test]
    fn never_retains_the_command_field() {
        // The whole point of the probe: secrets in argv must not survive.
        let rendered = format!("{:?}", parse_mcp_list(SAMPLE));
        assert!(!rendered.contains("api-key"));
        assert!(!rendered.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(!rendered.contains("allsource-prime"));
        assert!(!rendered.contains("calendarmcp"));
    }

    #[test]
    fn splits_status_on_the_last_dash_so_argv_dashes_are_safe() {
        let line = "weird: some-cmd --flag a - b --other - ✔ Connected";
        assert_eq!(
            parse_mcp_list(line),
            vec![("weird".to_string(), "connected")]
        );
    }

    #[test]
    fn skips_preamble_and_non_server_lines() {
        assert!(parse_mcp_list("Checking MCP server health…").is_empty());
        assert!(parse_mcp_list("").is_empty());
        assert!(parse_mcp_list("no colon here - ✔ Connected").is_empty());
        // Has a colon but no trailing status: not a server row.
        assert!(parse_mcp_list("Note: something happened").is_empty());
    }

    #[test]
    fn unknown_wording_is_not_optimistic() {
        assert_eq!(classify("✔ Connected"), "connected");
        assert_eq!(classify("! Needs authentication"), "needs_auth");
        assert_eq!(classify("✗ Failed to connect"), "failed");
        assert_eq!(classify("connection timed out"), "failed");
        // A phrasing we've never seen must NOT be reported as healthy.
        assert_eq!(classify("✽ Reticulating splines"), "unknown");
    }

    #[test]
    fn names_are_sanitized_and_bounded() {
        let hostile = "a\x1b[31mb: cmd - ✔ Connected";
        let got = parse_mcp_list(hostile);
        assert_eq!(got, vec![("a[31mb".to_string(), "connected")]);

        let long = format!("{}: cmd - ✔ Connected", "x".repeat(200));
        let got = parse_mcp_list(&long);
        assert_eq!(got[0].0.chars().count(), MAX_NAME_CHARS);
    }
}
