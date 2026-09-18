//! claude-healthline — a Claude Code status line command.
//!
//! Reads a JSON object on STDIN, prints ONE line to STDOUT with ANSI color +
//! Nerd-Font glyphs, and always exits 0.
//!
//! Hard contract: never panic on external input, always print at least one
//! non-empty line, always exit 0. A missing/null field just omits its segment.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::Value;

// ---------------------------------------------------------------------------
// STDIN schema — every field optional, parsed defensively.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct Input {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    model: Option<Model>,
    #[serde(default)]
    workspace: Option<Workspace>,
    #[serde(default)]
    cost: Option<Cost>,
    #[serde(default)]
    context_window: Option<ContextWindow>,
    #[serde(default)]
    exceeds_200k_tokens: Option<bool>,
    #[serde(default)]
    rate_limits: Option<RateLimits>,
}

#[derive(Debug, Default, Deserialize)]
struct Model {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Workspace {
    #[serde(default)]
    current_dir: Option<String>,
    #[serde(default)]
    git_worktree: Option<String>,
    #[serde(default)]
    repo: Option<Repo>,
}

#[derive(Debug, Default, Deserialize)]
struct Repo {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Cost {
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    total_lines_added: Option<i64>,
    #[serde(default)]
    total_lines_removed: Option<i64>,
    #[serde(default)]
    total_duration_ms: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct ContextWindow {
    #[serde(default)]
    used_percentage: Option<f64>,
    #[serde(default)]
    total_input_tokens: Option<f64>,
    #[serde(default)]
    context_window_size: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct RateLimits {
    #[serde(default)]
    five_hour: Option<Bucket>,
    #[serde(default)]
    seven_day: Option<Bucket>,
}

#[derive(Debug, Default, Deserialize)]
struct Bucket {
    #[serde(default)]
    used_percentage: Option<f64>,
}

// ---------------------------------------------------------------------------
// ANSI / styling
// ---------------------------------------------------------------------------

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const DIM_CYAN: &str = "\x1b[2;36m";
const DIM_MAGENTA: &str = "\x1b[2;35m";
const GREEN: &str = "\x1b[32m";
const DIM_GREEN: &str = "\x1b[2;32m";
const YELLOW: &str = "\x1b[33m";
const BOLD_RED: &str = "\x1b[1;31m";
const RED: &str = "\x1b[31m";
/// The caveman plugin's own badge color, so the segment matches the badge users
/// already know from `caveman-statusline.sh`.
const ORANGE: &str = "\x1b[38;5;172m";

/// Glyphs (Nerd Font v3, PUA). Codepoints documented in the report.
struct Glyphs {
    health: &'static str,    // U+F0565 shield-check (agent health score)
    model: &'static str,     // U+F0135 robot
    caveman: &'static str,   // U+26CF  pickaxe (caveman compression mode)
    repo: &'static str,      // U+F07B  folder
    branch: &'static str,    // U+E0A0  powerline branch
    context: &'static str,   // U+F0626 gauge
    cost: &'static str,      // U+F0117 cash
    task: &'static str,      // U+F0139 checklist
    ratelimit: &'static str, // U+F017  clock
    lines: &'static str,     // U+F0DEB plus-minus
    warning: &'static str,   // U+F071  warning triangle
    mcp: &'static str,       // U+F0A0A server-network (MCP server health)
    skills: &'static str,    // U+F0EDA toolbox (project-local skills)
    sep: String,             // U+E0B1 powerline thin separator, dim, padded
}

fn nerd_glyphs() -> Glyphs {
    Glyphs {
        health: "\u{F0565}",
        model: "\u{F0135}",
        caveman: "\u{26CF}",
        repo: "\u{F07B}",
        branch: "\u{E0A0}",
        context: "\u{F0626}",
        cost: "\u{F0117}",
        task: "\u{F0139}",
        ratelimit: "\u{F017}",
        lines: "\u{F0DEB}",
        warning: "\u{F071}",
        mcp: "\u{F0A0A}",
        skills: "\u{F0EDA}",
        sep: format!("{DIM} \u{E0B1} {RESET}"),
    }
}

fn ascii_glyphs() -> Glyphs {
    Glyphs {
        health: "health",
        model: "model",
        caveman: "cave",
        repo: "dir",
        branch: "git",
        context: "ctx",
        cost: "cost",
        task: "task",
        ratelimit: "rate",
        lines: "lines",
        warning: "!",
        mcp: "mcp",
        skills: "skills",
        sep: format!("{DIM} | {RESET}"),
    }
}

fn ascii_mode() -> bool {
    match std::env::var("CLAUDE_HEALTHLINE_ASCII") {
        Ok(v) if v == "1" => return true,
        _ => {}
    }
    matches!(std::env::var("NERD_FONT"), Ok(v) if v == "0")
}

// ---------------------------------------------------------------------------
// Segment helpers
// ---------------------------------------------------------------------------

/// A colored `glyph text` segment.
fn seg(color: &str, glyph: &str, text: &str) -> String {
    format!("{color}{glyph} {text}{RESET}")
}

/// Strip control chars (newline/CR/tab/ESC — protects the single-line + ANSI
/// invariants against a hostile or malformed field) and bound the width to
/// `max` chars so no one field can blow up the status line. Multibyte-safe
/// (operates on `char`s, never byte indices).
fn clean(s: &str, max: usize) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).collect();
    if out.chars().count() > max {
        out = out.chars().take(max).collect();
        out.push('…');
    }
    out
}

fn basename(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rsplit('/').next() {
        Some(b) if !b.is_empty() => b.to_string(),
        _ => trimmed.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Git branch resolution (no subprocess)
// ---------------------------------------------------------------------------

fn parse_head_contents(contents: &str) -> Option<String> {
    let line = contents.lines().next().unwrap_or("").trim();
    if let Some(rest) = line.strip_prefix("ref:") {
        let refname = rest.trim();
        let name = refname
            .strip_prefix("refs/heads/")
            .unwrap_or(refname)
            .to_string();
        if name.is_empty() {
            return None;
        }
        return Some(name);
    }
    // Detached HEAD: a 40-hex sha (accept >=7 hex chars defensively).
    let is_hex = !line.is_empty() && line.chars().all(|c| c.is_ascii_hexdigit());
    if is_hex && line.len() >= 7 {
        return Some(line[..7].to_string());
    }
    None
}

/// Read `<git_dir>/HEAD` and resolve to a branch name or short sha.
fn head_from_git_dir(git_dir: &Path) -> Option<String> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    parse_head_contents(&head)
}

fn resolve_branch(cwd: Option<&str>, git_worktree: Option<&str>) -> Option<String> {
    if let Some(wt) = git_worktree {
        let wt = wt.trim();
        if !wt.is_empty() {
            return Some(wt.to_string());
        }
    }
    let cwd = cwd?;
    if cwd.trim().is_empty() {
        return None; // never resolve .git relative to the process's own cwd
    }
    let dot_git = Path::new(cwd).join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return head_from_git_dir(&dot_git);
    }
    if meta.is_file() {
        // `gitdir: <path>` — path may be relative to cwd.
        let contents = std::fs::read_to_string(&dot_git).ok()?;
        let line = contents.lines().next().unwrap_or("").trim();
        let target = line.strip_prefix("gitdir:")?.trim();
        let git_dir = {
            let p = Path::new(target);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                Path::new(cwd).join(p)
            }
        };
        return head_from_git_dir(&git_dir);
    }
    None
}

// ---------------------------------------------------------------------------
// Chronis (cn) in-progress task — cached + time-bounded.
// ---------------------------------------------------------------------------

const DEFAULT_CN_TTL_SECS: u64 = 8;
const CN_WAIT_MS: u64 = 800;

/// FNV-1a 64-bit hash of the cwd, for a stable cache filename.
fn hash_cwd(cwd: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in cwd.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn cn_ttl() -> Duration {
    let secs = std::env::var("CLAUDE_HEALTHLINE_CN_TTL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CN_TTL_SECS);
    Duration::from_secs(secs)
}

/// Resolve the chronis (`cn`) binary: an explicit `CLAUDE_HEALTHLINE_CN_BIN`
/// override, else the conventional cargo bin under `$HOME` (present on most
/// Rust setups), else bare `cn` on `PATH`. The task segment simply omits itself
/// if none of these can run.
fn cn_bin() -> String {
    if let Ok(b) = std::env::var("CLAUDE_HEALTHLINE_CN_BIN") {
        if !b.trim().is_empty() {
            return b;
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let cargo = format!("{home}/.cargo/bin/cn");
        if Path::new(&cargo).exists() {
            return cargo;
        }
    }
    "cn".to_string()
}

fn cache_path(cwd: &str) -> PathBuf {
    std::env::temp_dir().join(format!("claude-healthline-cn-{:016x}", hash_cwd(cwd)))
}

/// Extract the first `t-<...>` token from cn's TOON output, skipping the header.
fn parse_task_id(out: &str) -> Option<String> {
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('[') {
            continue; // header line or blank
        }
        // Rows are pipe-delimited; also tolerate whitespace-delimited.
        for tok in line.split(|c: char| c == '|' || c.is_whitespace()) {
            if is_task_id(tok) {
                return Some(tok.to_string());
            }
        }
    }
    None
}

fn is_task_id(tok: &str) -> bool {
    let rest = match tok.strip_prefix("t-") {
        Some(r) => r,
        None => return false,
    };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
}

/// Outcome of a bounded `cn` invocation.
///   `None`               -> timed out or `cn` could not be run at all
///                           (fall back to the stale cache, do not overwrite).
///   `Some(None)`         -> `cn` ran cleanly but there is no in-progress task
///                           (cache an empty marker so we don't respawn).
///   `Some(Some(id))`     -> `cn` ran and reported this task id.
type CnOutcome = Option<Option<String>>;

/// Spawn `cn` with a hard wall-clock cap.
///
/// Chronis builds disagree on the status filter spelling: some accept
/// `in_progress`, others `in-progress` (and silently return zero rows for the
/// wrong one). We try the underscore form first (as documented), and if it
/// yields no task id, retry with the hyphen form — all inside the single wall
/// clock budget. Under no input may this block longer than `CN_WAIT_MS`.
fn run_cn_bounded(cwd: &str) -> CnOutcome {
    // The budget is enforced by whoever owns the child, so the child can be
    // KILLED when it expires. A worker thread cannot do that: it blocks inside
    // `Command::output()`, which waits on the child, and abandoning the thread
    // abandons the process. The statusline host then kills this binary, the
    // thread dies with it, and `cn` is reparented to init and keeps running.
    //
    // Measured 2026-08-31: nine such orphans at PPID 1, the oldest 88s, for a
    // query that takes ~1s. Two per render, because two filter spellings are
    // tried. Nothing reaped them and each render added more.
    cn_list_first_hit(cwd, Instant::now() + Duration::from_millis(CN_WAIT_MS))
}

/// Run `cn list --status=<filter> --toon` for each candidate filter spelling.
/// Returns `None` if `cn` never ran successfully for any spelling; otherwise
/// `Some(<task id or None>)` from the first successful invocation that had a
/// task (preferring a hit, else the last clean-but-empty response).
fn cn_list_first_hit(cwd: &str, deadline: Instant) -> CnOutcome {
    let mut ran_clean_empty = false;
    for filter in ["in_progress", "in-progress"] {
        // One shared deadline across both spellings, not one each: the caller
        // is a statusline render, and two budgets would let a slow `cn` cost
        // twice what the constant promises.
        if let Some(out) = cn_list_once(cwd, filter, deadline) {
            if let Some(id) = parse_task_id(&out) {
                return Some(Some(id));
            }
            ran_clean_empty = true; // cn succeeded, just no task under this filter
        }
    }
    if ran_clean_empty {
        Some(None)
    } else {
        None
    }
}

/// Run one `cn list` and return its stdout, or `None` on failure or timeout.
///
/// Owns the child for its whole life: on expiry it is killed AND reaped, so
/// this function never leaves a process behind. That is the whole point — see
/// [`run_cn_bounded`].
fn cn_list_once(cwd: &str, filter: &str, deadline: Instant) -> Option<String> {
    use std::process::{Command, Stdio};
    let arg = format!("--status={filter}");
    let mut child = Command::new(cn_bin())
        .args(["list", &arg, "--toon"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Kill THEN wait. Without the wait the child becomes a
                    // zombie instead of an orphan — a different leak, not a fix.
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Read cache if fresh; else refresh via cn (bounded), else fall back to stale
/// cache. The cache stores the resolved task id, or an empty file for "none".
fn resolve_task_id(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?;
    if cwd.trim().is_empty() {
        return None; // avoid running cn in the process's own cwd
    }
    let path = cache_path(cwd);

    let fresh = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
        .map(|age| age < cn_ttl())
        .unwrap_or(false);

    if fresh {
        return read_cache(&path);
    }

    // Refresh (bounded). Only persist when cn actually ran (Some(_)); a clean
    // "no task" caches an empty marker so non-chronis / idle dirs don't respawn
    // cn on every render.
    match run_cn_bounded(cwd) {
        Some(id) => {
            let _ = std::fs::write(&path, id.as_deref().unwrap_or(""));
            id
        }
        // Timeout / cn unavailable: fall back to any (possibly stale) cache.
        None => read_cache(&path),
    }
}

fn read_cache(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let id = contents.trim();
    if id.is_empty() {
        None
    } else if is_task_id(id) {
        Some(id.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Cost pricing + today's spend (computed from transcript usage, ccusage-style).
//
// Claude Code transcripts store `costUSD: null` per line, so today's cost is
// derived from token usage x per-model pricing. Prices are per MILLION tokens
// (Aug 2026); cache rates follow Anthropic's ratios (read = 0.10x input,
// 5-min write = 1.25x input, 1-hour write = 2.0x input). Override the built-in
// table with ~/.claude/healthline-pricing.json:
//   { "claude-opus-4-8": { "input": 5.0, "output": 25.0 }, ... }
// ---------------------------------------------------------------------------

const DAILY_TTL_SECS: u64 = 60;

#[derive(Clone, Copy)]
struct Price {
    input_per_m: f64,
    output_per_m: f64,
}

fn builtin_price(model: &str) -> Option<Price> {
    let m = model.to_ascii_lowercase();
    let p = |i, o| Price {
        input_per_m: i,
        output_per_m: o,
    };
    if m.contains("opus") {
        Some(p(5.0, 25.0)) // Opus 4.6/4.7/4.8, Opus 5
    } else if m.contains("sonnet-5") || m.contains("sonnet5") {
        Some(p(2.0, 10.0))
    } else if m.contains("sonnet") {
        Some(p(3.0, 15.0)) // Sonnet 4.6
    } else if m.contains("haiku") {
        Some(p(1.0, 5.0))
    } else if m.contains("fable") {
        Some(p(10.0, 50.0))
    } else {
        None
    }
}

fn price_for(model: &str, overrides: &Option<Value>) -> Option<Price> {
    if let Some(map) = overrides {
        if let Some(e) = map.get(model) {
            let i = e.get("input").and_then(|v| v.as_f64());
            let o = e.get("output").and_then(|v| v.as_f64());
            if let (Some(input_per_m), Some(output_per_m)) = (i, o) {
                return Some(Price {
                    input_per_m,
                    output_per_m,
                });
            }
        }
    }
    builtin_price(model)
}

fn load_price_overrides() -> Option<Value> {
    let home = std::env::var("HOME").ok()?;
    let path = Path::new(&home).join(".claude/healthline-pricing.json");
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
}

/// Cost (USD) of one assistant transcript line from its `usage` object.
fn line_cost(usage: &Value, price: Price) -> f64 {
    let n = |k: &str| usage.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let input = n("input_tokens");
    let output = n("output_tokens");
    let cache_read = n("cache_read_input_tokens");
    // Prefer the 5m/1h breakdown; fall back to the flat total (treated as 5m).
    let (write_5m, write_1h) = match usage.get("cache_creation") {
        Some(cc) => (
            cc.get("ephemeral_5m_input_tokens")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            cc.get("ephemeral_1h_input_tokens")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
        ),
        None => (n("cache_creation_input_tokens"), 0.0),
    };
    let in_t = price.input_per_m / 1_000_000.0;
    let out_t = price.output_per_m / 1_000_000.0;
    input * in_t
        + output * out_t
        + cache_read * (0.10 * in_t)
        + write_5m * (1.25 * in_t)
        + write_1h * (2.0 * in_t)
}

/// Civil date (UTC) "YYYY-MM-DD" from a unix timestamp — no external deps
/// (Howard Hinnant's days->civil). Used to match today's transcript lines by
/// their ISO-8601 `timestamp` prefix.
fn utc_ymd(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn today_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    utc_ymd(secs)
}

/// Sum today's (UTC) cost across every project transcript under
/// ~/.claude/projects/**/*.jsonl. Only files modified in the last ~26h are
/// scanned; lines are pre-filtered before JSON parse. `None` if disabled, no
/// HOME, or nothing could be priced.
fn daily_cost_uncached(today: &str) -> Option<f64> {
    let home = std::env::var("HOME").ok()?;
    let projects = Path::new(&home).join(".claude/projects");
    let overrides = load_price_overrides();
    let cutoff = SystemTime::now().checked_sub(Duration::from_secs(26 * 3600));

    let mut total = 0.0;
    let mut priced_any = false;

    for dir in std::fs::read_dir(&projects).ok()?.flatten() {
        let files = match std::fs::read_dir(dir.path()) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // Skip files not touched recently — they hold no today-lines.
            if let (Some(cut), Ok(meta)) = (cutoff, file.metadata()) {
                if let Ok(mtime) = meta.modified() {
                    if mtime < cut {
                        continue;
                    }
                }
            }
            let f = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                // Cheap pre-filter before the JSON parse.
                if !line.contains("\"usage\"") || !line.contains(today) {
                    continue;
                }
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let ts = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("");
                if !ts.starts_with(today) {
                    continue;
                }
                let usage = match v
                    .get("usage")
                    .or_else(|| v.get("message").and_then(|m| m.get("usage")))
                {
                    Some(u) => u,
                    None => continue,
                };
                let model = v
                    .get("model")
                    .or_else(|| v.get("message").and_then(|m| m.get("model")))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                if let Some(price) = price_for(model, &overrides) {
                    total += line_cost(usage, price);
                    priced_any = true;
                }
            }
        }
    }
    if priced_any {
        Some(total)
    } else {
        None
    }
}

/// Cached wrapper: recompute today's cost at most every `DAILY_TTL_SECS`.
/// Disable entirely with `CLAUDE_HEALTHLINE_NO_DAILY=1`.
fn daily_cost() -> Option<f64> {
    if matches!(std::env::var("CLAUDE_HEALTHLINE_NO_DAILY"), Ok(v) if v == "1") {
        return None;
    }
    let today = today_utc();
    let path = std::env::temp_dir().join(format!("claude-healthline-daily-{today}"));

    let fresh = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|mt| SystemTime::now().duration_since(mt).ok())
        .map(|age| age < Duration::from_secs(DAILY_TTL_SECS))
        .unwrap_or(false);
    if fresh {
        if let Ok(s) = std::fs::read_to_string(&path) {
            return s.trim().parse::<f64>().ok();
        }
    }
    let val = daily_cost_uncached(&today);
    // Persist (empty = "computed, nothing priced") so hot renders don't rescan.
    let _ = std::fs::write(&path, val.map(|v| v.to_string()).unwrap_or_default());
    val
}

// ---------------------------------------------------------------------------
// Caveman mode badge — a DISPLAY over the caveman plugin's flag files.
//
// caveman (github.com/JuliusBrussee/caveman) compresses assistant prose and
// writes two tiny files under $CLAUDE_CONFIG_DIR (default ~/.claude):
//
//   .caveman-active             the active level: lite | full | ultra | …
//   .caveman-statusline-suffix  a pre-rendered savings string ("⛏ 114.9k")
//
// Both are read directly — no node, no subprocess — and both are treated as
// hostile input: a symlink is refused (the flag path is predictable, so a local
// attacker could point it at a secret and have the status line print its bytes
// every keystroke), the read is byte-capped, the level must be on a whitelist,
// and the savings string is reduced to the digits/scale characters it is
// allowed to contain. Anything unexpected renders NOTHING rather than echoing
// planted bytes. Disable the segment with CLAUDE_HEALTHLINE_NO_CAVEMAN=1.
// ---------------------------------------------------------------------------

/// Levels the caveman plugin writes. `off` is deliberately absent: the plugin
/// writes it when compression is INACTIVE, and a badge for "not on" is noise.
const CAVEMAN_LEVELS: [&str; 10] = [
    "lite",
    "full",
    "ultra",
    "wenyan",
    "wenyan-lite",
    "wenyan-full",
    "wenyan-ultra",
    "commit",
    "review",
    "compress",
];

/// Longest flag-file read, in bytes. The files hold a word and a short number;
/// anything longer is a planted file, not caveman's.
const CAVEMAN_FLAG_MAX_BYTES: u64 = 64;

/// `$CLAUDE_CONFIG_DIR`, else `$HOME/.claude` — the same resolution order the
/// caveman hooks use to decide where to write.
fn claude_config_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !d.trim().is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(Path::new(&home).join(".claude"))
}

/// Read a small flag file defensively: never follow a symlink, never read more
/// than `CAVEMAN_FLAG_MAX_BYTES`, never return control characters.
fn read_flag_file(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return None;
    }
    let f = std::fs::File::open(path).ok()?;
    let mut buf = String::new();
    f.take(CAVEMAN_FLAG_MAX_BYTES)
        .read_to_string(&mut buf)
        .ok()?;
    let cleaned: String = buf.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim().to_string();
    Some(cleaned).filter(|s| !s.is_empty())
}

/// The active caveman level, lowercased and whitelisted.
fn caveman_level(dir: &Path) -> Option<String> {
    let raw = read_flag_file(&dir.join(".caveman-active"))?.to_ascii_lowercase();
    let level: String = raw
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect();
    CAVEMAN_LEVELS
        .iter()
        .find(|m| **m == level)
        .map(|m| (*m).to_string())
}

/// A humanized token count: digits, at most one decimal point, an optional
/// `k`/`m`/`b` scale. Deliberately a WHOLE-token match rather than a character
/// filter — filtering `\x1b[31mPWNED\x1b[0m` down to its digits and letters
/// yields "31m0m", which looks like a figure and is not one.
fn is_token_count(tok: &str) -> bool {
    let chars: Vec<char> = tok.chars().collect();
    if chars.is_empty() || chars.len() > 12 {
        return false;
    }
    let digits = match chars.last() {
        Some(c) if c.is_ascii_alphabetic() => {
            if !matches!(c.to_ascii_lowercase(), 'k' | 'm' | 'b') {
                return false;
            }
            &chars[..chars.len() - 1]
        }
        _ => &chars[..],
    };
    if digits.is_empty() || digits.last() == Some(&'.') {
        return false;
    }
    let mut dots = 0;
    for (i, c) in digits.iter().enumerate() {
        if *c == '.' {
            dots += 1;
            if i == 0 || dots > 1 {
                return false;
            }
        } else if !c.is_ascii_digit() {
            return false;
        }
    }
    true
}

/// The savings figure caveman pre-renders ("⛏ 114.9k" -> "114.9k"). The plugin
/// only writes it once `/caveman-stats` has run, so its absence is normal and
/// renders no number rather than a fabricated one. The pickaxe the plugin
/// prepends is dropped: this segment draws its own glyph.
fn caveman_savings(dir: &Path) -> Option<String> {
    let raw = read_flag_file(&dir.join(".caveman-statusline-suffix"))?;
    raw.split_whitespace()
        .find(|t| t.starts_with(|c: char| c.is_ascii_digit()))
        .filter(|t| is_token_count(t))
        .map(|t| t.to_string())
}

/// `full` or `full · 114.9k saved` — `None` when caveman is not installed, not
/// active, or the flag files are not what caveman writes.
fn caveman_badge(dir: &Path) -> Option<String> {
    let level = caveman_level(dir)?;
    Some(match caveman_savings(dir) {
        Some(saved) => format!("{level} \u{00B7} {saved} saved"),
        None => level,
    })
}

// ---------------------------------------------------------------------------
// Agent health score — a DISPLAY over a per-session state file.
//
// The status line CANNOT itself measure instruction-following, truthfulness, or
// task success — so it never invents them. It renders whatever a writer has put
// in ~/.claude/agent-health/<session_id>.json (override the dir with
// CLAUDE_HEALTHLINE_HEALTH_DIR). The bundled `claude-health-hook` binary writes
// only the OBSERVABLE dimensions (stability + drift, from real tool outcomes);
// the subjective dimensions (rules/truth/task) render `–` until an evaluator or
// the agent itself writes them. Disable the segment with
// CLAUDE_HEALTHLINE_NO_HEALTH=1.
//
// State file schema (every field optional):
//   { "rules":{"score":4.8,"reason":"...","flag":false},
//     "truth":{...}, "task":{...}, "stability":{...},
//     "drift":0, "safety_flag":false,
//     "next":"continue|repair|review|restart",
//     "state":"healthy|degraded|restart",   // optional explicit override
//     "updated_at": 1690000000 }
// Scores are 1–5 (the rubric). A single `flag`/`safety_flag` is a HARD GATE:
// it forces RESTART regardless of how high the averages are.
// ---------------------------------------------------------------------------

const HEALTH_MAX_AGE_SECS: u64 = 12 * 3600;
/// A sustained loop (this many consecutive failing tool calls) is a hard gate.
const DRIFT_RESTART: i64 = 5;

/// Per-dimension `(healthy_bar, restart_floor)` on the 1–5 rubric, mapped from
/// the operator policy's 0–1 thresholds. Below `healthy_bar` is Degraded; below
/// `restart_floor` is Restart. Rules/Truth demand ≥0.95 to stay green and Truth
/// falls to Restart under 0.90 (fabrication territory); Task is allowed a lower
/// 0.90 green bar. Stability has no hard restart floor — a *loop* (drift) is the
/// restart trigger, a merely low stability score is only Degraded.
fn dim_thresholds(name: &str) -> (f64, f64) {
    match name {
        "rules" => (4.75, 4.25),   // ≥0.95 green, <0.85 restart
        "truth" => (4.75, 4.50),   // ≥0.95 green, <0.90 restart
        "task" => (4.50, 4.25),    // ≥0.90 green, <0.85 restart
        _ /* stability */ => (4.75, 1.0), // ≥0.95 green, never hard-gates alone
    }
}

#[derive(Debug, Default, Deserialize)]
struct HealthState {
    #[serde(default)]
    rules: Option<HealthDim>,
    #[serde(default)]
    truth: Option<HealthDim>,
    #[serde(default)]
    task: Option<HealthDim>,
    #[serde(default)]
    stability: Option<HealthDim>,
    #[serde(default)]
    drift: Option<i64>,
    #[serde(default)]
    safety_flag: Option<bool>,
    #[serde(default)]
    next: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HealthDim {
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    flag: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthLevel {
    Healthy,
    Degraded,
    Restart,
}

impl HealthDim {
    /// A finite score clamped to the 1–5 rubric, if present.
    fn clamped(&self) -> Option<f64> {
        self.score
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(1.0, 5.0))
    }
    fn is_flagged(&self) -> bool {
        self.flag.unwrap_or(false)
    }
}

fn health_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("CLAUDE_HEALTHLINE_HEALTH_DIR") {
        if !d.trim().is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(Path::new(&home).join(".claude/agent-health"))
}

/// Read + parse the health state for this session, ignoring a file older than
/// `HEALTH_MAX_AGE_SECS` (so a reused/abandoned session id can't show stale data).
fn read_health(session_id: Option<&str>) -> Option<HealthState> {
    if matches!(std::env::var("CLAUDE_HEALTHLINE_NO_HEALTH"), Ok(v) if v == "1") {
        return None;
    }
    let sid = session_id?.trim();
    if sid.is_empty()
        || !sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None; // guard the path join against traversal / junk
    }
    let path = health_dir()?.join(format!("{sid}.json"));
    let fresh = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|mt| SystemTime::now().duration_since(mt).ok())
        .map(|age| age < Duration::from_secs(HEALTH_MAX_AGE_SECS))
        .unwrap_or(false);
    if !fresh {
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str::<HealthState>(&raw).ok()
}

/// Derive the health level. HARD GATE first: any dimension flag or the safety
/// flag forces `Restart` — a high average must never conceal a critical
/// violation. An explicit `state` in the file wins for Restart/Degraded but can
/// never *upgrade* past what the hard gate or the scores warrant.
fn derive_level(h: &HealthState) -> HealthLevel {
    let named = [
        ("rules", &h.rules),
        ("truth", &h.truth),
        ("task", &h.task),
        ("stability", &h.stability),
    ];
    let drift = h.drift.unwrap_or(0);

    // HARD GATE: any flag, the safety flag, a sustained loop, or a per-dimension
    // score under its restart floor forces Restart — a high average must never
    // conceal a critical violation.
    let hard_gate = h.safety_flag.unwrap_or(false)
        || named
            .iter()
            .any(|(_, d)| d.as_ref().is_some_and(|x| x.is_flagged()))
        || drift >= DRIFT_RESTART
        || h.state.as_deref() == Some("restart")
        || h.next.as_deref() == Some("restart")
        || named.iter().any(|(name, d)| {
            d.as_ref()
                .and_then(|x| x.clamped())
                .is_some_and(|s| s < dim_thresholds(name).1)
        });
    if hard_gate {
        return HealthLevel::Restart;
    }

    // Healthy iff every PRESENT dimension clears its green bar, no drift, and no
    // explicit downgrade. An explicit `state` can only downgrade — never upgrade
    // past what the scores/flags warrant.
    let all_green = named.iter().all(|(name, d)| {
        d.as_ref()
            .and_then(|x| x.clamped())
            .is_none_or(|s| s >= dim_thresholds(name).0)
    });
    if all_green && drift == 0 && h.state.as_deref() != Some("degraded") {
        HealthLevel::Healthy
    } else {
        HealthLevel::Degraded
    }
}

fn health_color(level: HealthLevel) -> &'static str {
    match level {
        HealthLevel::Healthy => GREEN,
        HealthLevel::Degraded => YELLOW,
        HealthLevel::Restart => BOLD_RED,
    }
}

fn default_next(level: HealthLevel) -> &'static str {
    match level {
        HealthLevel::Healthy => "continue",
        HealthLevel::Degraded => "repair",
        HealthLevel::Restart => "restart",
    }
}

/// One rubric score as `4.8`, or `–` when absent.
fn fmt_score(d: Option<&HealthDim>) -> String {
    match d.and_then(|x| x.clamped()) {
        Some(v) => format!("{v:.1}"),
        None => "–".to_string(),
    }
}

/// Build the health segment, or `None` when there is nothing to show. `core` is
/// the always-kept part (`R4.8 T4.7 S4.6`); the full form appends drift + next.
fn render_health(h: &HealthState, g: &Glyphs) -> Option<Seg> {
    let level = derive_level(h);
    let color = health_color(level);

    // Core: the three judged dimensions. If none are present but stability is,
    // show stability alone so an observed-only (hook-fed) session still reads.
    let core = if h.rules.is_some() || h.truth.is_some() || h.task.is_some() {
        format!(
            "R{} T{} S{}",
            fmt_score(h.rules.as_ref()),
            fmt_score(h.truth.as_ref()),
            fmt_score(h.task.as_ref())
        )
    } else if let Some(s) = h.stability.as_ref().and_then(|x| x.clamped()) {
        format!("stab {s:.1}")
    } else if level == HealthLevel::Restart {
        // Flag-only state with no scores: still surface the restart cue.
        "check".to_string()
    } else {
        return None;
    };

    // Suffixes (dropped in the compact variant).
    let mut full = core.clone();
    let drift = h.drift.unwrap_or(0);
    if drift > 0 {
        full.push_str(&format!(" {}{drift}", g.warning));
    }
    let next = h
        .next
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default_next(level));
    if next != "continue" {
        full.push_str(&format!(" \u{2192}{}", clean(next, 10)));
    }

    let mut sg = Seg::new("health", color, g.health, &full);
    if full != core {
        sg.compact = Some((
            g.health.chars().count() + 1 + core.chars().count(),
            seg(color, g.health, &core),
        ));
    }
    Some(sg)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn round_pct(v: f64) -> i64 {
    if !v.is_finite() {
        return 0;
    }
    // Percentages are 0..=100; clamp so a garbage/derived f64 can't render as a
    // 20-digit saturated integer.
    v.round().clamp(0.0, 100.0) as i64
}

// ---------------------------------------------------------------------------
// MCP server health — a DISPLAY over the `claude-mcp-probe` cache.
//
// The status line CANNOT check MCP health itself: `claude mcp list` takes
// SECONDS (it health-checks every remote connector serially), and this binary
// runs on every render. So the bundled `claude-mcp-probe` hook does that work
// once at SessionStart and writes {name: status} to
// ~/.claude/healthline-cache/mcp.json (override: CLAUDE_HEALTHLINE_MCP_CACHE).
//
// This segment renders ONLY problems — servers needing auth or failing to
// connect. "All 17 connected" is a fact the user cannot act on, so a healthy
// fleet draws nothing at all. Disable entirely with CLAUDE_HEALTHLINE_NO_MCP=1.
// ---------------------------------------------------------------------------

/// Beyond this the probe data describes a session that is long gone.
const MCP_MAX_AGE_SECS: u64 = 12 * 3600;

/// A parse of the probe cache: how many servers sit in each bad state.
#[derive(Debug, Default, PartialEq)]
struct McpTrouble {
    needs_auth: usize,
    failed: usize,
}

impl McpTrouble {
    fn is_clean(&self) -> bool {
        self.needs_auth == 0 && self.failed == 0
    }
}

fn mcp_cache_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_HEALTHLINE_MCP_CACHE") {
        if !p.trim().is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    Some(claude_config_dir()?.join("healthline-cache/mcp.json"))
}

/// Tally the bad states in a probe payload. `unknown` is deliberately NOT
/// counted as trouble: the probe emits it when the CLI's wording changed, and a
/// permanent false alarm trains the user to ignore the segment.
fn parse_mcp_cache(raw: &str, now: u64) -> Option<McpTrouble> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let updated = v.get("updated_at").and_then(|u| u.as_u64()).unwrap_or(0);
    if updated == 0 || now.saturating_sub(updated) > MCP_MAX_AGE_SECS {
        return None;
    }
    let servers = v.get("servers")?.as_object()?;
    let mut t = McpTrouble::default();
    for status in servers.values().filter_map(|s| s.as_str()) {
        match status {
            "needs_auth" => t.needs_auth += 1,
            "failed" => t.failed += 1,
            _ => {}
        }
    }
    Some(t)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `1 down · 2 auth` — `None` when the probe never ran, the cache is stale, or
/// every server is healthy.
fn render_mcp(t: &McpTrouble) -> Option<(&'static str, String)> {
    if t.is_clean() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    // Failures lead: a dead server breaks calls outright, whereas a
    // needs-auth one is a prompt away from working.
    if t.failed > 0 {
        parts.push(format!("{} down", t.failed));
    }
    if t.needs_auth > 0 {
        parts.push(format!("{} auth", t.needs_auth));
    }
    let color = if t.failed > 0 { RED } else { YELLOW };
    Some((color, parts.join(" \u{00B7} ")))
}

fn mcp_badge(g: &Glyphs) -> Option<Seg> {
    if matches!(std::env::var("CLAUDE_HEALTHLINE_NO_MCP"), Ok(v) if v == "1") {
        return None;
    }
    let raw = std::fs::read_to_string(mcp_cache_path()?).ok()?;
    let trouble = parse_mcp_cache(&raw, now_unix())?;
    let (color, text) = render_mcp(&trouble)?;
    Some(Seg::new("mcp", color, g.mcp, &clean(&text, 20)))
}

// ---------------------------------------------------------------------------
// Project-local skills.
//
// The GLOBAL skill count is deliberately not rendered: it is the same number on
// every repo and every render, so it informs no decision. What changes as you
// move around — and what you may not know a repo ships — is `.claude/skills/`
// inside the project. Disable with CLAUDE_HEALTHLINE_NO_SKILLS=1.
//
// A plain read_dir of one directory is ~microseconds warm, so this needs no
// cache; there is nothing here to amortize.
// ---------------------------------------------------------------------------

/// Don't enumerate an unbounded directory just to print a number.
const SKILLS_MAX_SCAN: usize = 256;

/// Count immediate subdirectories of `dir` that contain a `SKILL.md`.
fn count_skills(dir: &Path) -> usize {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    entries
        .flatten()
        .take(SKILLS_MAX_SCAN)
        .filter(|e| {
            // `is_dir()` follows symlinks on purpose: linking a skill into
            // .claude/skills is a normal way to install one.
            e.path().is_dir() && e.path().join("SKILL.md").is_file()
        })
        .count()
}

/// The project's own skills dir, or `None` when the cwd's `.claude/skills`
/// resolves to the GLOBAL one. That happens whenever Claude Code is launched
/// from `$HOME`, and counting 60-odd global skills as "project" skills would be
/// actively misleading.
fn project_skills_dir(cwd: Option<&str>) -> Option<PathBuf> {
    let dir = Path::new(cwd?).join(".claude/skills");
    let global = claude_config_dir().map(|d| d.join("skills"));
    let same = match (
        dir.canonicalize(),
        global.and_then(|g| g.canonicalize().ok()),
    ) {
        (Ok(a), Some(b)) => a == b,
        _ => false,
    };
    if same {
        None
    } else {
        Some(dir)
    }
}

fn skills_badge(cwd: Option<&str>, g: &Glyphs) -> Option<Seg> {
    if matches!(std::env::var("CLAUDE_HEALTHLINE_NO_SKILLS"), Ok(v) if v == "1") {
        return None;
    }
    let n = count_skills(&project_skills_dir(cwd)?);
    if n == 0 {
        return None;
    }
    Some(Seg::new("skills", DIM, g.skills, &format!("{n} proj")))
}

/// Visible width of the separator (` ▏ ` / ` | ` = 3 columns in both themes).
const SEP_PLAIN: usize = 3;

/// One rendered segment plus the metadata needed to shrink or drop it when the
/// terminal is narrow.
struct Seg {
    kind: &'static str,
    plain: usize,                     // visible width (excludes ANSI)
    styled: String,                   // full, colored form
    compact: Option<(usize, String)>, // optional narrower variant (width, styled)
}

impl Seg {
    fn new(kind: &'static str, color: &str, glyph: &str, text: &str) -> Seg {
        Seg {
            kind,
            plain: glyph.chars().count() + 1 + text.chars().count(),
            styled: seg(color, glyph, text),
            compact: None,
        }
    }
}

/// Terminal width Claude Code exports before running the status line
/// (`COLUMNS`, since CC v2.1.153). `None` -> assume wide, never truncate.
fn columns() -> Option<usize> {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&c| c > 0)
}

/// Join segments to fit `COLUMNS`, degrading gracefully: drop the lowest-value
/// segments first, then strip the cost cluster's burn/today extras. Essentials
/// — model, context %, and the session cost — are never dropped, so they stay
/// in view on half-screen terminals (a wrap beats hiding the danger-zone cue).
fn fit(segs: Vec<Seg>, sep: &str) -> String {
    fit_within(segs, sep, columns())
}

/// `fit` with the width passed in, so the degradation ORDER is testable without
/// mutating the process-global `COLUMNS`.
fn fit_within(mut segs: Vec<Seg>, sep: &str, cols: Option<usize>) -> String {
    if let Some(cols) = cols {
        let width = |s: &[Seg]| {
            s.iter().map(|x| x.plain).sum::<usize>() + SEP_PLAIN * s.len().saturating_sub(1)
        };

        // Each reduction applies only while still over budget, in value order.
        // The project-skills count goes first — it is ambient inventory, not
        // news. Then the caveman badge: it reports a mode the user chose and
        // already knows, so it is the cheapest remaining thing to lose.
        // NOTE: "mcp" is absent from every drop list on purpose. It renders
        // only when a server is down or unauthenticated, and hiding that to
        // save three columns would defeat the point of having it.
        for kind in ["skills", "caveman", "lines", "rate"] {
            if width(&segs) > cols {
                segs.retain(|s| s.kind != kind);
            }
        }
        if width(&segs) > cols {
            for s in segs.iter_mut() {
                if s.kind == "cost" || s.kind == "health" {
                    if let Some((pl, st)) = s.compact.take() {
                        s.plain = pl;
                        s.styled = st;
                    }
                }
            }
        }
        for kind in ["repo", "branch", "task"] {
            if width(&segs) > cols {
                segs.retain(|s| s.kind != kind);
            }
        }
    }
    segs.into_iter()
        .map(|s| s.styled)
        .collect::<Vec<_>>()
        .join(sep)
}

fn render(input: &Input, g: &Glyphs) -> String {
    let mut segs: Vec<Seg> = Vec::new();

    // 0. AGENT HEALTH — headline; highest value, never dropped (only compacted).
    if let Some(h) = read_health(input.session_id.as_deref()) {
        if let Some(hseg) = render_health(&h, g) {
            segs.push(hseg);
        }
    }

    // 1. MODEL
    let model_name = input
        .model
        .as_ref()
        .and_then(|m| m.display_name.as_deref())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Claude");
    segs.push(Seg::new("model", DIM_CYAN, g.model, &clean(model_name, 24)));

    // 2. CAVEMAN MODE — sits beside the model because it describes how that
    // model is answering. Omitted entirely when caveman isn't active.
    if !matches!(std::env::var("CLAUDE_HEALTHLINE_NO_CAVEMAN"), Ok(v) if v == "1") {
        if let Some(badge) = claude_config_dir().as_deref().and_then(caveman_badge) {
            segs.push(Seg::new("caveman", ORANGE, g.caveman, &clean(&badge, 28)));
        }
    }

    // 3. REPO / DIR
    let repo_name = input
        .workspace
        .as_ref()
        .and_then(|w| w.repo.as_ref())
        .and_then(|r| r.name.as_deref())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            let dir = input.cwd.as_deref().or_else(|| {
                input
                    .workspace
                    .as_ref()
                    .and_then(|w| w.current_dir.as_deref())
            });
            dir.filter(|s| !s.trim().is_empty()).map(basename)
        });
    if let Some(name) = repo_name {
        segs.push(Seg::new("repo", DIM, g.repo, &clean(&name, 24)));
    }

    // 4. GIT BRANCH
    let git_worktree = input
        .workspace
        .as_ref()
        .and_then(|w| w.git_worktree.as_deref());
    if let Some(branch) = resolve_branch(input.cwd.as_deref(), git_worktree) {
        segs.push(Seg::new(
            "branch",
            DIM_MAGENTA,
            g.branch,
            &clean(&branch, 40),
        ));
    }

    // 4b. PROJECT SKILLS — sits with the repo/branch cluster because it
    // describes the checkout you are standing in, not the session.
    if let Some(s) = skills_badge(input.cwd.as_deref(), g) {
        segs.push(s);
    }

    // 5. CONTEXT %
    let exceeds = input.exceeds_200k_tokens.unwrap_or(false);
    let pct: Option<i64> = input
        .context_window
        .as_ref()
        .and_then(|c| c.used_percentage)
        .map(round_pct)
        .or_else(|| {
            let c = input.context_window.as_ref()?;
            let total = c.total_input_tokens?;
            let size = c.context_window_size?;
            if size > 0.0 {
                Some(round_pct(total * 100.0 / size))
            } else {
                None
            }
        });
    if let Some(p) = pct {
        let (color, prefix) = if exceeds || p > 80 {
            let pre = if exceeds {
                format!("{} ", g.warning)
            } else {
                String::new()
            };
            (BOLD_RED, pre)
        } else if p >= 50 {
            (YELLOW, String::new())
        } else {
            (GREEN, String::new())
        };
        let text = format!("{prefix}{p}%");
        segs.push(Seg::new("context", color, g.context, &text));
    } else if exceeds {
        // No percentage but we still want the Dumb Zone warning.
        let text = format!("{} 200k+", g.warning);
        segs.push(Seg::new("context", BOLD_RED, g.context, &text));
    }

    // 6. COST
    let mut cost = input
        .cost
        .as_ref()
        .and_then(|c| c.total_cost_usd)
        .unwrap_or(0.0);
    if !cost.is_finite() || cost < 0.0 {
        cost = 0.0;
    }
    let mut parts: Vec<String> = vec![if cost >= 100_000.0 {
        "$99999+".to_string()
    } else {
        format!("${cost:.2}")
    }];
    // Burn rate ($/hr): session cost over elapsed wall time (stdin only).
    let dur_ms = input
        .cost
        .as_ref()
        .and_then(|c| c.total_duration_ms)
        .unwrap_or(0.0);
    if cost > 0.0 && dur_ms >= 60_000.0 {
        let per_hr = cost / (dur_ms / 3_600_000.0);
        if per_hr.is_finite() && per_hr > 0.0 {
            parts.push(if per_hr >= 100.0 {
                format!("~${per_hr:.0}/hr")
            } else {
                format!("~${per_hr:.2}/hr")
            });
        }
    }
    // Today's total across all sessions (computed from transcript usage).
    if let Some(d) = daily_cost() {
        parts.push(if d >= 100_000.0 {
            "today $99999+".to_string()
        } else {
            format!("today ${d:.2}")
        });
    }
    let full_text = parts.join(" \u{00B7} ");
    let mut cost_seg = Seg::new("cost", DIM_GREEN, g.cost, &full_text);
    if parts.len() > 1 {
        // Narrow terminals collapse the cluster to just the session cost.
        cost_seg.compact = Some((
            g.cost.chars().count() + 1 + parts[0].chars().count(),
            seg(DIM_GREEN, g.cost, &parts[0]),
        ));
    }
    segs.push(cost_seg);

    // 6b. MCP TROUBLE — renders nothing when the fleet is healthy, so its mere
    // presence is the signal. Never dropped for width: it only ever appears
    // when something needs fixing.
    if let Some(s) = mcp_badge(g) {
        segs.push(s);
    }

    // 7. TASK
    if let Some(id) = resolve_task_id(input.cwd.as_deref()) {
        segs.push(Seg::new("task", YELLOW, g.task, &id));
    }

    // 8. RATE LIMITS
    if let Some(rl) = input.rate_limits.as_ref() {
        if let Some(five) = rl.five_hour.as_ref().and_then(|b| b.used_percentage) {
            let mut text = format!("5h {}%", round_pct(five));
            if let Some(seven) = rl.seven_day.as_ref().and_then(|b| b.used_percentage) {
                text.push_str(&format!(" \u{00B7} 7d {}%", round_pct(seven)));
            }
            segs.push(Seg::new("rate", DIM, g.ratelimit, &text));
        }
    }

    // 9. LINES ±
    let added = input
        .cost
        .as_ref()
        .and_then(|c| c.total_lines_added)
        .unwrap_or(0);
    let removed = input
        .cost
        .as_ref()
        .and_then(|c| c.total_lines_removed)
        .unwrap_or(0);
    if added != 0 || removed != 0 {
        // green +added  red -removed, glyph dim.
        let plain_text = format!("+{added} -{removed}");
        let styled = format!(
            "{DIM}{}{RESET} {GREEN}+{added}{RESET} {RED}-{removed}{RESET}",
            g.lines
        );
        segs.push(Seg {
            kind: "lines",
            plain: g.lines.chars().count() + 1 + plain_text.chars().count(),
            styled,
            compact: None,
        });
    }

    fit(segs, &g.sep)
}

/// Minimal never-blank fallback used when stdin is empty / not JSON.
fn fallback_line(g: &Glyphs) -> String {
    seg(DIM_CYAN, g.model, "Claude")
}

fn main() {
    let mut raw = String::new();
    // Best-effort read; ignore errors, we still print a fallback.
    let _ = std::io::stdin().read_to_string(&mut raw);

    let g = if ascii_mode() {
        ascii_glyphs()
    } else {
        nerd_glyphs()
    };

    let line = match serde_json::from_str::<Input>(raw.trim()) {
        Ok(input) => {
            let rendered = render(&input, &g);
            if rendered.trim().is_empty() {
                fallback_line(&g)
            } else {
                rendered
            }
        }
        Err(_) => {
            // Attempt a permissive Value parse so a partly-broken object still
            // yields at least the model name if present; else static fallback.
            match serde_json::from_str::<Value>(raw.trim()) {
                Ok(v) => {
                    let name = v
                        .get("model")
                        .and_then(|m| m.get("display_name"))
                        .and_then(|d| d.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or("Claude");
                    seg(DIM_CYAN, g.model, name)
                }
                Err(_) => fallback_line(&g),
            }
        }
    };

    // ALWAYS a non-empty line, ALWAYS exit 0.
    let line = if line.trim().is_empty() {
        fallback_line(&g)
    } else {
        line
    };
    println!("{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dim(score: Option<f64>, flag: bool) -> HealthDim {
        HealthDim {
            score,
            flag: Some(flag),
        }
    }

    #[test]
    fn all_high_scores_are_healthy() {
        let h = HealthState {
            rules: Some(dim(Some(4.8), false)),
            truth: Some(dim(Some(4.9), false)),
            task: Some(dim(Some(4.75), false)),
            ..Default::default()
        };
        assert_eq!(derive_level(&h), HealthLevel::Healthy);
    }

    #[test]
    fn one_dim_below_nominal_is_degraded() {
        let h = HealthState {
            rules: Some(dim(Some(4.9), false)),
            truth: Some(dim(Some(4.6), false)), // < 4.75
            ..Default::default()
        };
        assert_eq!(derive_level(&h), HealthLevel::Degraded);
    }

    #[test]
    fn safety_flag_is_a_hard_gate_over_perfect_scores() {
        let h = HealthState {
            rules: Some(dim(Some(5.0), false)),
            truth: Some(dim(Some(5.0), false)),
            task: Some(dim(Some(5.0), false)),
            safety_flag: Some(true),
            ..Default::default()
        };
        // A perfect average must NEVER conceal a critical flag.
        assert_eq!(derive_level(&h), HealthLevel::Restart);
    }

    #[test]
    fn any_dimension_flag_forces_restart() {
        let h = HealthState {
            rules: Some(dim(Some(5.0), false)),
            truth: Some(dim(Some(5.0), true)), // fabrication flag
            ..Default::default()
        };
        assert_eq!(derive_level(&h), HealthLevel::Restart);
    }

    #[test]
    fn explicit_state_can_downgrade_but_not_upgrade() {
        // File says "degraded" though scores are perfect -> honor the downgrade.
        let down = HealthState {
            rules: Some(dim(Some(5.0), false)),
            state: Some("degraded".into()),
            ..Default::default()
        };
        assert_eq!(derive_level(&down), HealthLevel::Degraded);

        // File says "healthy" but a dim is below its green bar (yet above the
        // restart floor) -> scores still win, capped at Degraded.
        let up = HealthState {
            rules: Some(dim(Some(4.6), false)), // 4.25 <= 4.6 < 4.75
            state: Some("healthy".into()),
            ..Default::default()
        };
        assert_eq!(derive_level(&up), HealthLevel::Degraded);
    }

    #[test]
    fn per_dimension_thresholds_differ() {
        // Task has a lower green bar (4.50): 4.6 task is Healthy.
        let task_ok = HealthState {
            task: Some(dim(Some(4.6), false)),
            ..Default::default()
        };
        assert_eq!(derive_level(&task_ok), HealthLevel::Healthy);
        // The same 4.6 on Truth is only Degraded (green bar 4.75).
        let truth_soft = HealthState {
            truth: Some(dim(Some(4.6), false)),
            ..Default::default()
        };
        assert_eq!(derive_level(&truth_soft), HealthLevel::Degraded);
        // Truth under 4.50 (its restart floor / ~0.90) is fabrication territory.
        let truth_hard = HealthState {
            truth: Some(dim(Some(4.4), false)),
            ..Default::default()
        };
        assert_eq!(derive_level(&truth_hard), HealthLevel::Restart);
    }

    #[test]
    fn a_live_loop_degrades_then_forces_restart() {
        let degraded = HealthState {
            stability: Some(dim(Some(5.0), false)),
            drift: Some(3),
            ..Default::default()
        };
        assert_eq!(derive_level(&degraded), HealthLevel::Degraded);

        let restart = HealthState {
            stability: Some(dim(Some(5.0), false)),
            drift: Some(DRIFT_RESTART),
            ..Default::default()
        };
        assert_eq!(derive_level(&restart), HealthLevel::Restart);
    }

    #[test]
    fn empty_state_with_no_signals_is_healthy_but_renders_nothing() {
        let h = HealthState::default();
        assert_eq!(derive_level(&h), HealthLevel::Healthy);
        assert!(render_health(&h, &ascii_glyphs()).is_none());
    }

    #[test]
    fn scores_are_clamped_to_the_rubric() {
        assert_eq!(fmt_score(Some(&dim(Some(9.9), false))), "5.0");
        assert_eq!(fmt_score(Some(&dim(Some(-3.0), false))), "1.0");
        assert_eq!(fmt_score(Some(&dim(None, false))), "–");
        assert_eq!(fmt_score(None), "–");
        assert_eq!(fmt_score(Some(&dim(Some(f64::NAN), false))), "–");
    }

    #[test]
    fn stability_only_renders_without_the_subjective_dims() {
        let h = HealthState {
            stability: Some(dim(Some(4.2), false)),
            drift: Some(2),
            ..Default::default()
        };
        let sg = render_health(&h, &ascii_glyphs()).expect("segment");
        assert!(sg.styled.contains("stab 4.2"));
        assert!(sg.styled.contains('2')); // drift surfaced
                                          // degraded (4.2 < 4.75) -> yellow
        assert!(sg.styled.contains(YELLOW));
        // compact drops the drift suffix, keeps the core
        let (_, compact) = sg.compact.expect("compact");
        assert!(compact.contains("stab 4.2"));
    }

    #[test]
    fn subjective_dims_render_dashes_when_absent() {
        let h = HealthState {
            rules: Some(dim(Some(4.8), false)),
            ..Default::default()
        };
        let sg = render_health(&h, &ascii_glyphs()).expect("segment");
        assert!(sg.styled.contains("R4.8"));
        assert!(sg.styled.contains("T–"));
        assert!(sg.styled.contains("S–"));
    }

    // -- caveman badge ------------------------------------------------------

    /// A private scratch dir; the caveman readers take a dir, so no env var is
    /// touched and the tests stay parallel-safe.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "claude-healthline-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn write_flag(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).expect("write flag");
    }

    #[test]
    fn no_flag_file_means_no_badge() {
        let dir = scratch("caveman-absent");
        assert_eq!(caveman_badge(&dir), None);
    }

    #[test]
    fn level_alone_renders_without_a_savings_figure() {
        let dir = scratch("caveman-level");
        write_flag(&dir, ".caveman-active", "full");
        assert_eq!(caveman_badge(&dir).as_deref(), Some("full"));
    }

    #[test]
    fn savings_suffix_is_stripped_to_its_figure() {
        let dir = scratch("caveman-savings");
        write_flag(&dir, ".caveman-active", "ultra\n");
        // The plugin prepends its own pickaxe; this segment draws one already.
        write_flag(&dir, ".caveman-statusline-suffix", "⛏ 114.9k");
        assert_eq!(caveman_badge(&dir).as_deref(), Some("ultra · 114.9k saved"));
    }

    #[test]
    fn level_is_normalized_and_whitelisted() {
        let dir = scratch("caveman-normalize");
        write_flag(&dir, ".caveman-active", " WENYAN-ULTRA \n");
        assert_eq!(caveman_badge(&dir).as_deref(), Some("wenyan-ultra"));

        // Off is a real caveman level, but "not compressing" is not news.
        write_flag(&dir, ".caveman-active", "off");
        assert_eq!(caveman_badge(&dir), None);

        // Anything not on the whitelist renders nothing rather than the bytes.
        write_flag(&dir, ".caveman-active", "$(rm -rf ~)");
        assert_eq!(caveman_badge(&dir), None);
    }

    #[test]
    fn planted_escape_sequences_never_reach_the_terminal() {
        let dir = scratch("caveman-escapes");
        // A flag file carrying ANSI/OSC bytes must not paint the status line.
        write_flag(&dir, ".caveman-active", "\x1b]8;;http://evil\x07full");
        assert_eq!(caveman_badge(&dir), None);

        write_flag(&dir, ".caveman-active", "full");
        write_flag(&dir, ".caveman-statusline-suffix", "\x1b[31mPWNED\x1b[0m");
        // Nothing in that string is a token count -> no figure, level survives.
        assert_eq!(caveman_badge(&dir).as_deref(), Some("full"));
    }

    #[test]
    fn only_a_whole_token_counts_as_a_figure() {
        assert!(is_token_count("114.9k"));
        assert!(is_token_count("512"));
        assert!(is_token_count("1.2M"));
        assert!(!is_token_count("31m0m")); // what a char-filter makes of ESC[31m
        assert!(!is_token_count("1.2.3k"));
        assert!(!is_token_count(".9k"));
        assert!(!is_token_count("9."));
        assert!(!is_token_count("114x"));
        assert!(!is_token_count(""));
    }

    #[test]
    fn oversized_flag_contents_are_rejected() {
        let dir = scratch("caveman-oversize");
        write_flag(&dir, ".caveman-active", "full");
        write_flag(&dir, ".caveman-statusline-suffix", &"9".repeat(64));
        assert_eq!(caveman_badge(&dir).as_deref(), Some("full"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_flag_is_refused() {
        let dir = scratch("caveman-symlink");
        let secret = dir.join("secret");
        std::fs::write(&secret, "full").expect("secret");
        std::os::unix::fs::symlink(&secret, dir.join(".caveman-active")).expect("symlink");
        assert_eq!(caveman_badge(&dir), None);
    }

    fn narrow_row(g: &Glyphs) -> Vec<Seg> {
        vec![
            Seg::new("model", DIM_CYAN, g.model, "Opus 5"),
            Seg::new("caveman", ORANGE, g.caveman, "full · 114.9k saved"),
            Seg::new("lines", DIM, g.lines, "+40 -5"),
        ]
    }

    #[test]
    fn a_wide_terminal_keeps_the_caveman_badge() {
        let g = ascii_glyphs();
        let out = fit_within(narrow_row(&g), &g.sep, Some(200));
        assert!(out.contains("cave full"));
        assert!(out.contains("lines"));
    }

    #[test]
    fn a_narrow_terminal_drops_the_caveman_badge_before_lines() {
        let g = ascii_glyphs();
        // Wide enough for model + lines, too narrow for all three.
        let out = fit_within(narrow_row(&g), &g.sep, Some(30));
        assert!(
            !out.contains("cave "),
            "caveman should be dropped first: {out}"
        );
        assert!(out.contains("lines"), "lines outrank the badge: {out}");
        assert!(out.contains("Opus 5"));
    }

    // --- MCP health ------------------------------------------------------

    fn mcp_json(updated: u64, servers: &str) -> String {
        format!("{{\"updated_at\":{updated},\"servers\":{servers}}}")
    }

    #[test]
    fn a_healthy_fleet_renders_nothing() {
        let raw = mcp_json(1_000, "{\"a\":\"connected\",\"b\":\"connected\"}");
        let t = parse_mcp_cache(&raw, 1_010).expect("fresh cache parses");
        assert!(t.is_clean());
        assert_eq!(render_mcp(&t), None);
    }

    #[test]
    fn counts_auth_and_failures_separately() {
        let raw = mcp_json(
            1_000,
            "{\"a\":\"connected\",\"b\":\"needs_auth\",\"c\":\"needs_auth\",\"d\":\"failed\"}",
        );
        let t = parse_mcp_cache(&raw, 1_010).unwrap();
        assert_eq!(
            t,
            McpTrouble {
                needs_auth: 2,
                failed: 1
            }
        );
        let (color, text) = render_mcp(&t).unwrap();
        assert_eq!(text, "1 down \u{00B7} 2 auth");
        assert_eq!(color, RED, "a dead server outranks an unauthenticated one");
    }

    #[test]
    fn auth_only_trouble_is_a_warning_not_an_error() {
        let raw = mcp_json(1_000, "{\"b\":\"needs_auth\"}");
        let t = parse_mcp_cache(&raw, 1_010).unwrap();
        let (color, text) = render_mcp(&t).unwrap();
        assert_eq!(text, "1 auth");
        assert_eq!(color, YELLOW);
    }

    #[test]
    fn unknown_status_is_not_reported_as_trouble() {
        // The probe emits "unknown" when the CLI's wording changed. A standing
        // false alarm would train the user to ignore this segment.
        let raw = mcp_json(1_000, "{\"a\":\"unknown\",\"b\":\"disabled\"}");
        let t = parse_mcp_cache(&raw, 1_010).unwrap();
        assert!(t.is_clean());
    }

    #[test]
    fn a_stale_or_malformed_cache_yields_no_segment() {
        // Older than MCP_MAX_AGE_SECS.
        let raw = mcp_json(1_000, "{\"b\":\"needs_auth\"}");
        assert_eq!(parse_mcp_cache(&raw, 1_000 + MCP_MAX_AGE_SECS + 1), None);
        // No timestamp at all: cannot judge freshness, so render nothing.
        assert_eq!(
            parse_mcp_cache("{\"servers\":{\"b\":\"failed\"}}", 9_999),
            None
        );
        // Not JSON, wrong shape, empty.
        assert_eq!(parse_mcp_cache("not json", 9_999), None);
        assert_eq!(parse_mcp_cache("{\"updated_at\":1000}", 1_010), None);
        assert_eq!(parse_mcp_cache("", 9_999), None);
    }

    #[test]
    fn mcp_survives_the_narrowest_terminal() {
        let g = ascii_glyphs();
        let segs = vec![
            Seg::new("model", DIM_CYAN, g.model, "Opus 5"),
            Seg::new("skills", DIM, g.skills, "3 proj"),
            Seg::new("mcp", RED, g.mcp, "1 down"),
        ];
        let out = fit_within(segs, &g.sep, Some(10));
        assert!(out.contains("1 down"), "mcp must never be dropped: {out}");
        assert!(!out.contains("proj"), "skills drop first: {out}");
    }

    // --- project-local skills --------------------------------------------

    #[test]
    fn counts_only_dirs_that_hold_a_skill_md() {
        let base = std::env::temp_dir().join(format!("cs-skills-{}", std::process::id()));
        let skills = base.join(".claude/skills");
        std::fs::create_dir_all(skills.join("real")).unwrap();
        std::fs::write(skills.join("real/SKILL.md"), "x").unwrap();
        std::fs::create_dir_all(skills.join("empty-dir")).unwrap();
        std::fs::write(skills.join("loose.md"), "x").unwrap();

        assert_eq!(count_skills(&skills), 1);
        assert_eq!(
            count_skills(&base.join("nope")),
            0,
            "missing dir is 0, not a panic"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_global_skills_dir_is_never_counted_as_project_skills() {
        // Launching Claude Code from $HOME makes cwd/.claude/skills the GLOBAL
        // dir; reporting "66 proj" there would be a lie.
        let home = std::env::var("HOME").unwrap();
        let global = claude_config_dir().unwrap().join("skills");
        if global.exists() {
            assert_eq!(project_skills_dir(Some(&home)), None);
        }
        assert!(project_skills_dir(Some("/tmp/some-repo")).is_some());
        assert_eq!(project_skills_dir(None), None);
    }

    /// The regression this pins: 0.1.0 ran `cn` on a worker thread and
    /// abandoned it at the deadline. `recv_timeout` returned, but the thread
    /// was still inside `Command::output()` holding the child, so the child
    /// outlived this process and reparented to init. Measured 2026-08-31: nine
    /// orphans at PPID 1. The same shape, unfixed, cost ~810% CPU on
    /// 2026-09-18 — which is when the fix finally shipped.
    #[cfg(unix)]
    #[test]
    fn a_cn_that_outruns_its_budget_is_killed_and_reaped() {
        use std::os::unix::fs::PermissionsExt;

        // Unique per test process: the assertion below looks for this name in
        // the live process table, and a stale copy would fail it for free.
        let marker = format!("slow-cn-{}", std::process::id());
        let dir = std::env::temp_dir().join(&marker);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join(&marker);
        std::fs::write(&script, "#!/bin/sh\nsleep 10\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        std::env::set_var("CLAUDE_HEALTHLINE_CN_BIN", &script);
        let started = Instant::now();
        let outcome = run_cn_bounded(dir.to_str().unwrap());
        let elapsed = started.elapsed();
        std::env::remove_var("CLAUDE_HEALTHLINE_CN_BIN");

        assert!(outcome.is_none(), "a cn that never answered has no outcome");
        assert!(
            elapsed < Duration::from_millis(CN_WAIT_MS * 3),
            "both filter spellings share ONE budget, not one each: {elapsed:?}"
        );

        let ps = std::process::Command::new("/bin/ps")
            .args(["-Ao", "args"])
            .output()
            .unwrap();
        let table = String::from_utf8_lossy(&ps.stdout);
        assert!(
            !table.contains(&marker),
            "the cn child outlived its owner — the orphan is back"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
