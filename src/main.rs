//! claude-statusline — a Claude Code status line command.
//!
//! Reads a JSON object on STDIN, prints ONE line to STDOUT with ANSI color +
//! Nerd-Font glyphs, and always exits 0.
//!
//! Hard contract: never panic on external input, always print at least one
//! non-empty line, always exit 0. A missing/null field just omits its segment.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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

/// Glyphs (Nerd Font v3, PUA). Codepoints documented in the report.
struct Glyphs {
    model: &'static str,     // U+F0135 robot
    repo: &'static str,      // U+F07B  folder
    branch: &'static str,    // U+E0A0  powerline branch
    context: &'static str,   // U+F0626 gauge
    cost: &'static str,      // U+F0117 cash
    task: &'static str,      // U+F0139 checklist
    ratelimit: &'static str, // U+F017  clock
    lines: &'static str,     // U+F0DEB plus-minus
    warning: &'static str,   // U+F071  warning triangle
    sep: String,             // U+E0B1 powerline thin separator, dim, padded
}

fn nerd_glyphs() -> Glyphs {
    Glyphs {
        model: "\u{F0135}",
        repo: "\u{F07B}",
        branch: "\u{E0A0}",
        context: "\u{F0626}",
        cost: "\u{F0117}",
        task: "\u{F0139}",
        ratelimit: "\u{F017}",
        lines: "\u{F0DEB}",
        warning: "\u{F071}",
        sep: format!("{DIM} \u{E0B1} {RESET}"),
    }
}

fn ascii_glyphs() -> Glyphs {
    Glyphs {
        model: "model",
        repo: "dir",
        branch: "git",
        context: "ctx",
        cost: "cost",
        task: "task",
        ratelimit: "rate",
        lines: "lines",
        warning: "!",
        sep: format!("{DIM} | {RESET}"),
    }
}

fn ascii_mode() -> bool {
    match std::env::var("CLAUDE_STATUSLINE_ASCII") {
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
    let secs = std::env::var("CLAUDE_STATUSLINE_CN_TTL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_CN_TTL_SECS);
    Duration::from_secs(secs)
}

/// Resolve the chronis (`cn`) binary: an explicit `CLAUDE_STATUSLINE_CN_BIN`
/// override, else the conventional cargo bin under `$HOME` (present on most
/// Rust setups), else bare `cn` on `PATH`. The task segment simply omits itself
/// if none of these can run.
fn cn_bin() -> String {
    if let Ok(b) = std::env::var("CLAUDE_STATUSLINE_CN_BIN") {
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
    std::env::temp_dir().join(format!("claude-statusline-cn-{:016x}", hash_cwd(cwd)))
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
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();
    let cwd_owned = cwd.to_string();
    std::thread::spawn(move || {
        let payload = cn_list_first_hit(&cwd_owned);
        let _ = tx.send(payload);
    });

    // Timeout / disconnect -> None (CnOutcome default): fall back to the stale
    // cache; the detached thread is left to exit on its own.
    rx.recv_timeout(Duration::from_millis(CN_WAIT_MS))
        .unwrap_or_default()
}

/// Run `cn list --status=<filter> --toon` for each candidate filter spelling.
/// Returns `None` if `cn` never ran successfully for any spelling; otherwise
/// `Some(<task id or None>)` from the first successful invocation that had a
/// task (preferring a hit, else the last clean-but-empty response).
fn cn_list_first_hit(cwd: &str) -> CnOutcome {
    let mut ran_clean_empty = false;
    for filter in ["in_progress", "in-progress"] {
        if let Some(out) = cn_list_once(cwd, filter) {
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

fn cn_list_once(cwd: &str, filter: &str) -> Option<String> {
    use std::process::{Command, Stdio};
    let arg = format!("--status={filter}");
    let out = Command::new(cn_bin())
        .args(["list", &arg, "--toon"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
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
// table with ~/.claude/statusline-pricing.json:
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
    let path = Path::new(&home).join(".claude/statusline-pricing.json");
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
/// Disable entirely with `CLAUDE_STATUSLINE_NO_DAILY=1`.
fn daily_cost() -> Option<f64> {
    if matches!(std::env::var("CLAUDE_STATUSLINE_NO_DAILY"), Ok(v) if v == "1") {
        return None;
    }
    let today = today_utc();
    let path = std::env::temp_dir().join(format!("claude-statusline-daily-{today}"));

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
fn fit(mut segs: Vec<Seg>, sep: &str) -> String {
    if let Some(cols) = columns() {
        let width = |s: &[Seg]| {
            s.iter().map(|x| x.plain).sum::<usize>() + SEP_PLAIN * s.len().saturating_sub(1)
        };

        // Each reduction applies only while still over budget, in value order.
        for kind in ["lines", "rate"] {
            if width(&segs) > cols {
                segs.retain(|s| s.kind != kind);
            }
        }
        if width(&segs) > cols {
            for s in segs.iter_mut() {
                if s.kind == "cost" {
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

    // 1. MODEL
    let model_name = input
        .model
        .as_ref()
        .and_then(|m| m.display_name.as_deref())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Claude");
    segs.push(Seg::new("model", DIM_CYAN, g.model, &clean(model_name, 24)));

    // 2. REPO / DIR
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

    // 3. GIT BRANCH
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

    // 4. CONTEXT %
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

    // 5. COST
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

    // 6. TASK
    if let Some(id) = resolve_task_id(input.cwd.as_deref()) {
        segs.push(Seg::new("task", YELLOW, g.task, &id));
    }

    // 7. RATE LIMITS
    if let Some(rl) = input.rate_limits.as_ref() {
        if let Some(five) = rl.five_hour.as_ref().and_then(|b| b.used_percentage) {
            let mut text = format!("5h {}%", round_pct(five));
            if let Some(seven) = rl.seven_day.as_ref().and_then(|b| b.used_percentage) {
                text.push_str(&format!(" \u{00B7} 7d {}%", round_pct(seven)));
            }
            segs.push(Seg::new("rate", DIM, g.ratelimit, &text));
        }
    }

    // 8. LINES ±
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
