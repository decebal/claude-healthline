//! claude-health-hook — populates the OBSERVABLE agent-health dimensions.
//!
//! Wire this to Claude Code's `PostToolUse` and `PostToolUseFailure` hooks. It
//! reads the hook JSON on STDIN, keeps a small rolling window of tool outcomes
//! per session, and writes the *observable* signals — `stability` (from the
//! recent tool-error rate) and `drift` (a live consecutive-failure loop count) —
//! into `~/.claude/agent-health/<session_id>.json`.
//!
//! It DELIBERATELY never writes `rules` / `truth` / `task` / `safety_flag`: those
//! are not observable from tool telemetry and must come from an evaluator or the
//! agent's own self-report. Existing values in the file are preserved. Always
//! exits 0 so it can never break a turn.
//!
//! Override the state dir with `CLAUDE_HEALTHLINE_HEALTH_DIR`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Rolling window of tool outcomes used for the stability score.
const WINDOW: usize = 12;

fn health_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("CLAUDE_HEALTHLINE_HEALTH_DIR") {
        if !d.trim().is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(Path::new(&home).join(".claude/agent-health"))
}

/// Session ids must be filesystem-safe before we build a path from them.
fn safe_sid(sid: &str) -> Option<&str> {
    let s = sid.trim();
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some(s)
    } else {
        None
    }
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

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

fn main() {
    // Read stdin best-effort; on any problem we still exit 0.
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let ev: Value = serde_json::from_str(raw.trim()).unwrap_or_else(|_| json!({}));

    let sid = match ev
        .get("session_id")
        .and_then(|v| v.as_str())
        .and_then(safe_sid)
    {
        Some(s) => s.to_string(),
        None => return, // nothing to key on
    };
    let event = ev
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tool = ev
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Map the event to an outcome. Only tool-completion events carry a signal.
    let ok = match event {
        "PostToolUse" => true,
        "PostToolUseFailure" => false,
        _ => return, // PreToolUse / Stop / etc: nothing to record
    };

    let dir = match health_dir() {
        Some(d) => d,
        None => return,
    };
    let counters_path = dir.join(format!("{sid}.counters.json"));
    let state_path = dir.join(format!("{sid}.json"));

    // --- update the rolling counters -------------------------------------
    let mut counters = read_json(&counters_path);
    let mut recent: Vec<bool> = counters
        .get("recent")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_bool()).collect())
        .unwrap_or_default();
    recent.push(ok);
    if recent.len() > WINDOW {
        let excess = recent.len() - WINDOW;
        recent.drain(0..excess);
    }

    let prev_consec = counters
        .get("consec_fail")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let prev_fail_tool = counters
        .get("last_fail_tool")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // A loop = the SAME tool failing again and again; a different tool failing
    // resets the "same-tool" streak to 1.
    let consec_fail = if ok {
        0
    } else if prev_fail_tool == tool {
        prev_consec + 1
    } else {
        1
    };

    counters["recent"] = json!(recent);
    counters["consec_fail"] = json!(consec_fail);
    counters["last_fail_tool"] = json!(if ok { "" } else { tool.as_str() });
    write_atomic(&counters_path, &counters.to_string());

    // --- derive the observable signals -----------------------------------
    let n = recent.len().max(1);
    let fails = recent.iter().filter(|&&o| !o).count();
    let ratio = fails as f64 / n as f64;
    // 0 failures -> 5.0 ; all failures -> 1.0 ; linear in between.
    let stability = (5.0 - ratio * 4.0).clamp(1.0, 5.0);
    let stability = (stability * 10.0).round() / 10.0;
    // Drift only counts a *sustained* same-tool loop (>= 3 in a row); a couple of
    // isolated failures are noise, not drift.
    let drift = if consec_fail >= 3 { consec_fail } else { 0 };
    let reason = if fails == 0 {
        format!("no tool errors (last {n})")
    } else {
        format!("{fails}/{n} recent tool calls failed")
    };

    // --- merge into the shared health state (preserve subjective dims) ----
    let mut state = read_json(&state_path);
    state["stability"] = json!({ "score": stability, "reason": reason });
    state["drift"] = json!(drift);
    state["updated_at"] = json!(now_unix());
    // NOTE: rules / truth / task / safety_flag / next / state are left exactly as
    // an evaluator (or the agent) set them — this hook cannot observe them.
    write_atomic(&state_path, &state.to_string());
}
