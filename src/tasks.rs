//! The session's task list, its ETA, and the tool call in flight.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::{
    clean, now_unix, safe_session_id, step_dir, Glyphs, Seg, BRIGHT_MAGENTA, BRIGHT_WHITE, GREEN,
    GREY, RESET, WHITE,
};

/// Bounds the tail scan, so a multi-megabyte session cannot make a render
/// expensive; an older list simply stops showing.
const TODO_TAIL_BYTES: u64 = 4 * 1024 * 1024;

fn todo_tail_bytes() -> u64 {
    std::env::var("CLAUDE_HEALTHLINE_TODO_TAIL")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(TODO_TAIL_BYTES)
}

/// One line may hold several TodoWrite blocks; the newest wins.
fn last_todowrite_input<'a>(v: &'a Value, found: &mut Option<&'a Value>) {
    match v {
        Value::Object(map) => {
            let is_todowrite = map.get("type").and_then(Value::as_str) == Some("tool_use")
                && map.get("name").and_then(Value::as_str) == Some("TodoWrite");
            if is_todowrite {
                if let Some(input) = map.get("input") {
                    *found = Some(input);
                }
            }
            for child in map.values() {
                last_todowrite_input(child, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                last_todowrite_input(item, found);
            }
        }
        _ => {}
    }
}

fn for_each_object(v: &Value, visit: &mut dyn FnMut(&serde_json::Map<String, Value>)) {
    match v {
        Value::Object(map) => {
            visit(map);
            for child in map.values() {
                for_each_object(child, visit);
            }
        }
        Value::Array(items) => {
            for item in items {
                for_each_object(item, visit);
            }
        }
        _ => {}
    }
}

/// First present of `keys`, trimmed and non-empty. Claude Code repairs
/// `id`/`task_id` -> `taskId` and `active_form` -> `activeForm` only AFTER the
/// call is streamed, so a transcript carries whichever spelling the model emitted.
fn first_str<'a>(map: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .filter_map(|key| map.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
}

#[derive(Debug, Clone)]
struct TaskEntry {
    subject: String,
    active_form: Option<String>,
    status: String,
}

/// Task tools report a stream of create/update events rather than a whole list,
/// so progress is a fold: creates add, `TaskUpdate` moves status, and a `deleted`
/// status removes. Task order is kept, so the first `in_progress` item is stable.
#[derive(Debug, Default)]
struct TaskFold {
    pending_creates: Vec<(String, TaskEntry)>,
    tasks: Vec<(String, TaskEntry)>,
    first_create_secs: Option<i64>,
    completion_secs: Vec<i64>,
    saw_events: bool,
}

impl TaskFold {
    fn apply_line(&mut self, line: &str, stamp: Option<i64>) {
        let parsed: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut creates: Vec<(String, TaskEntry)> = Vec::new();
        let mut updates: Vec<(String, Option<String>, Option<String>)> = Vec::new();
        for_each_object(&parsed, &mut |map| {
            let name = if map.get("type").and_then(Value::as_str) == Some("tool_use") {
                map.get("name").and_then(Value::as_str).unwrap_or("")
            } else {
                ""
            };
            if let Some(input) = map.get("input").and_then(Value::as_object) {
                if name == "TaskCreate" {
                    if let Some(call_id) = map.get("id").and_then(Value::as_str) {
                        creates.push((
                            call_id.to_string(),
                            TaskEntry {
                                subject: first_str(input, &["subject", "content"])
                                    .unwrap_or("")
                                    .to_string(),
                                active_form: first_str(input, &["activeForm", "active_form"])
                                    .map(str::to_string),
                                status: "pending".to_string(),
                            },
                        ));
                    }
                } else if name == "TaskUpdate" {
                    if let Some(task_id) = first_str(input, &["taskId", "id", "task_id"]) {
                        updates.push((
                            task_id.to_string(),
                            first_str(input, &["status"]).map(str::to_string),
                            first_str(input, &["activeForm", "active_form"]).map(str::to_string),
                        ));
                    }
                }
            }
            // TaskCreate's assigned id arrives with the tool's structured output,
            // which the CLI transcript spells toolUseResult.
            if let Some(task) = map
                .get("toolUseResult")
                .or_else(|| map.get("tool_use_result"))
                .and_then(|out| out.get("task"))
                .and_then(Value::as_object)
            {
                if let Some(task_id) = first_str(task, &["id"]) {
                    // Results arrive in call order, so the oldest pending create
                    // is the one this id belongs to.
                    if !self.pending_creates.is_empty() {
                        let (_, entry) = self.pending_creates.remove(0);
                        self.saw_events = true;
                        self.tasks.push((task_id.to_string(), entry));
                    }
                }
            }
        });
        if !creates.is_empty() {
            self.saw_events = true;
            if self.first_create_secs.is_none() {
                self.first_create_secs = stamp;
            }
            self.pending_creates.extend(creates);
        }
        let mut newly_completed = Vec::new();
        for (task_id, status, active_form) in updates {
            self.saw_events = true;
            if status.as_deref() == Some("deleted") {
                self.tasks.retain(|(id, _)| *id != task_id);
                continue;
            }
            let Some((_, entry)) = self.tasks.iter_mut().find(|(id, _)| *id == task_id) else {
                continue;
            };

            let newly_done = status.as_deref() == Some("completed") && entry.status != "completed";

            if let Some(status) = status {
                entry.status = status;
            }
            if active_form.is_some() {
                entry.active_form = active_form;
            }

            if let (true, Some(stamp)) = (newly_done, stamp) {
                newly_completed.push(stamp);
            }
        }
        self.completion_secs.extend(newly_completed);
    }

    fn progress(&self) -> TodoProgress {
        let active = self
            .tasks
            .iter()
            .find(|(_, entry)| entry.status == "in_progress")
            .map(|(_, entry)| {
                entry
                    .active_form
                    .clone()
                    .unwrap_or_else(|| entry.subject.clone())
            })
            .filter(|label| !label.is_empty());
        TodoProgress {
            active,
            done: self
                .tasks
                .iter()
                .filter(|(_, entry)| entry.status == "completed")
                .count(),
            total: self.tasks.len(),
            anchor_secs: self.first_create_secs,
            completion_secs: self.completion_secs.clone(),
        }
    }
}

/// One TodoWrite call's list. A line merely quoting the name yields `None` from
/// the parse, whereas a real call with nothing in progress is authoritative and
/// CLEARS an earlier goal rather than deferring to it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TodoSnapshot {
    active: Option<String>,
    done: usize,
    total: usize,
}

/// The gaps between `completion_secs` are the ETA's rate; `anchor_secs` is only
/// the fallback for a window that opens after those completions.
#[derive(Debug, Default, PartialEq, Eq)]
struct TodoProgress {
    active: Option<String>,
    done: usize,
    total: usize,
    anchor_secs: Option<i64>,
    completion_secs: Vec<i64>,
}

fn todo_snapshot_from_line(line: &str) -> Option<TodoSnapshot> {
    let v: Value = serde_json::from_str(line).ok()?;
    let mut found = None;
    last_todowrite_input(&v, &mut found);
    let todos = match found?.get("todos").and_then(Value::as_array) {
        Some(t) => t,
        None => return Some(TodoSnapshot::default()),
    };
    let status = |item: &Value| {
        item.get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let active = todos
        .iter()
        .find(|item| status(item) == "in_progress")
        .and_then(|item| {
            ["activeForm", "content"]
                .iter()
                .filter_map(|key| item.get(*key).and_then(Value::as_str))
                .map(str::trim)
                .find(|s| !s.is_empty())
                .map(str::to_string)
        });
    Some(TodoSnapshot {
        active,
        done: todos
            .iter()
            .filter(|item| status(item) == "completed")
            .count(),
        total: todos.len(),
    })
}

/// Howard Hinnant's civil->days; the inverse of [`crate::utc_ymd`].
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix seconds from a transcript's ISO-8601 `timestamp` (always UTC, `Z`).
fn parse_iso_secs(stamp: &str) -> Option<i64> {
    let bytes = stamp.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let field = |from: usize, to: usize| stamp.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    let (hour, minute, second) = (field(11, 13)?, field(14, 16)?, field(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn line_timestamp(line: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(line).ok()?;
    parse_iso_secs(v.get("timestamp")?.as_str()?)
}

/// The newest todo list in the transcript's tail window, anchored at the oldest
/// consecutive call carrying the same `total` — so a rewritten list (the model
/// adding items mid-flight) re-anchors instead of inheriting a stale rate.
fn scan_todo_progress(path: &Path, tail: u64) -> Option<TodoProgress> {
    let file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut reader = BufReader::new(file);
    if len > tail {
        reader.seek(SeekFrom::Start(len - tail)).ok()?;
        // The window almost certainly opens mid-line; drop that fragment.
        let mut fragment = Vec::new();
        reader.read_until(b'\n', &mut fragment).ok()?;
    }
    let mut calls: Vec<(TodoSnapshot, Option<i64>)> = Vec::new();
    let mut fold = TaskFold::default();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        // Cheap pre-filters before any JSON parse. A create's assigned id lands on
        // a later result line, so those are only worth parsing while one is open.
        let task_event = line.contains("\"TaskCreate\"") || line.contains("\"TaskUpdate\"");
        let resolves_create = !fold.pending_creates.is_empty() && line.contains("\"task\"");
        if task_event || resolves_create {
            fold.apply_line(&line, line_timestamp(&line));
        }
        if !line.contains("\"TodoWrite\"") {
            continue;
        }
        if let Some(snapshot) = todo_snapshot_from_line(&line) {
            let stamp = line_timestamp(&line);
            calls.push((snapshot, stamp));
        }
    }
    // Task tools are what modern models get; TodoWrite only appears in a session
    // that opted into it, so a live fold outranks any TodoWrite list in the window.
    if fold.saw_events && !fold.tasks.is_empty() {
        return Some(fold.progress());
    }
    let (latest, _) = calls.last()?;
    let latest = latest.clone();

    let mut run: Vec<(TodoSnapshot, Option<i64>)> = calls
        .iter()
        .rev()
        .take_while(|(snapshot, _)| snapshot.total == latest.total)
        .cloned()
        .collect();
    run.reverse();

    let anchor = run.iter().filter_map(|(_, stamp)| *stamp).next();

    Some(TodoProgress {
        active: latest.active,
        done: latest.done,
        total: latest.total,
        anchor_secs: anchor,
        completion_secs: completion_secs_from_snapshots(&run),
    })
}

/// A TodoWrite list arrives as whole snapshots, so an item's completion time is
/// the first call whose `done` count reached it.
fn completion_secs_from_snapshots(run: &[(TodoSnapshot, Option<i64>)]) -> Vec<i64> {
    let mut stamps = Vec::new();
    let mut counted = 0;

    for (snapshot, stamp) in run {
        let Some(stamp) = stamp else { continue };

        for _ in counted..snapshot.done {
            stamps.push(*stamp);
        }

        counted = counted.max(snapshot.done);
    }

    stamps
}

const BAR_CELLS: usize = 7;
/// One completed item extrapolates to a wild ETA, so hold it back for a second
/// sample. Straight-line rate over a list the model may still rewrite: a hint.
const ETA_MIN_DONE: usize = 2;

/// A finished list is not news: it would sit at 100% until the next list starts,
/// so it clears instead.
fn progress_worth_showing(progress: &TodoProgress) -> bool {
    progress.total > 0 && progress.done < progress.total
}

/// Rounded UP, so any progress at all lights the first cell.
fn filled_cells(done: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    (done * BAR_CELLS).div_ceil(total).min(BAR_CELLS)
}

fn format_eta(secs: i64) -> String {
    if secs < 60 {
        return "~<1m".to_string();
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("~{minutes}m");
    }
    format!("~{}h{:02}m", minutes / 60, minutes % 60)
}

/// Two completions logged closer together than this are one bookkeeping batch —
/// a backfilled row, or a burst of ticks — not two units of work.
const MIN_WORK_GAP_SECS: i64 = 5;

/// The middle gap between completions. A single slow item then moves the
/// estimate by one rank instead of dragging a mean, which is what made the
/// figure swing between renders.
fn median_completion_gap(completion_secs: &[i64]) -> Option<i64> {
    let mut gaps: Vec<i64> = completion_secs
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .filter(|gap| *gap >= MIN_WORK_GAP_SECS)
        .collect();

    if gaps.is_empty() {
        return None;
    }

    gaps.sort_unstable();

    Some(gaps[gaps.len() / 2])
}

/// Rate comes from the gaps between completions, never from the anchor: the
/// stretch before the first completion holds the session's planning, reads and
/// permission waits, and charging that to every remaining item ran ~5x long.
fn eta_secs(progress: &TodoProgress, now_secs: i64) -> Option<i64> {
    if progress.done < ETA_MIN_DONE || progress.done >= progress.total {
        return None;
    }

    let remaining = (progress.total - progress.done) as i64;

    if let (Some(gap), Some(last)) = (
        median_completion_gap(&progress.completion_secs),
        progress.completion_secs.last(),
    ) {
        let spent_on_current = (now_secs - last).clamp(0, gap);

        return Some((gap * remaining - spent_on_current).max(0));
    }

    let elapsed = now_secs.checked_sub(progress.anchor_secs?)?;
    if elapsed <= 0 {
        return None;
    }

    Some((elapsed * remaining) / progress.done as i64)
}

fn resolve_todo_progress(transcript_path: Option<&str>) -> Option<TodoProgress> {
    let path = transcript_path?.trim();
    if path.is_empty() {
        return None;
    }
    scan_todo_progress(Path::new(path), todo_tail_bytes())
}

fn progress_bar_seg(progress: &TodoProgress) -> Seg {
    let filled = filled_cells(progress.done, progress.total);
    let eta = eta_secs(progress, now_unix() as i64).map(format_eta);

    let mut styled = format!(
        "{GREEN}{}{RESET}{GREY}{}{RESET}",
        "\u{25AE}".repeat(filled),
        "\u{25AF}".repeat(BAR_CELLS - filled)
    );
    let mut plain = BAR_CELLS;

    if let Some(eta) = &eta {
        styled.push_str(&format!("{GREY} {eta}{RESET}"));
        plain += 1 + eta.chars().count();
    }

    Seg {
        kind: "progress",
        plain,
        styled,
        compact: None,
    }
}

/// The bar, then the goal it is measuring.
pub fn task_segs(transcript_path: Option<&str>, g: &Glyphs) -> Vec<Seg> {
    let Some(progress) = resolve_todo_progress(transcript_path) else {
        return Vec::new();
    };

    let mut segs = Vec::new();

    if progress_worth_showing(&progress) {
        segs.push(progress_bar_seg(&progress));
    }
    if let Some(todo) = progress.active.as_deref() {
        segs.push(Seg::new("todo", BRIGHT_MAGENTA, g.todo, &clean(todo, 48)));
    }

    segs
}

/// A tool call still in flight reads brighter than one that has returned, so the
/// row distinguishes "working" from "waiting" without adding a segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepState {
    Running,
    Done,
}

/// State files may carry a `running`/`done` prefix. A bare label — an older or
/// third-party `PreToolUse` hook — counts as running.
fn parse_step(raw: &str) -> Option<(StepState, String)> {
    // Split before trimming: trimming first would eat the tab after a state
    // field whose label is blank, leaving the state word itself as the label.
    let line = raw.trim_matches(|c| c == '\n' || c == '\r');
    let (state, label) = match line.split_once('\t') {
        Some(("running", rest)) => (StepState::Running, rest),
        Some(("done", rest)) => (StepState::Done, rest),
        _ => (StepState::Running, line),
    };
    let label = label.trim();
    if label.is_empty() {
        None
    } else {
        Some((state, label.to_string()))
    }
}

fn resolve_current_step(session_id: Option<&str>) -> Option<(StepState, String)> {
    let sid = safe_session_id(session_id)?;
    let raw = std::fs::read_to_string(step_dir().join(format!("claude-step-{sid}"))).ok()?;
    parse_step(&raw)
}

pub fn step_seg(session_id: Option<&str>, g: &Glyphs) -> Option<Seg> {
    let (state, label) = resolve_current_step(session_id)?;
    let color = match state {
        StepState::Running => BRIGHT_WHITE,
        StepState::Done => WHITE,
    };

    Some(Seg::new("step", color, g.step, &clean(&label, 48)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utc_ymd;

    #[test]
    fn active_todo_prefers_active_form_of_the_in_progress_item() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"TodoWrite","input":{"todos":[
                {"content":"Ship it","activeForm":"Shipping it","status":"completed"},
                {"content":"Wire the segment","activeForm":"Wiring the segment","status":"in_progress"},
                {"content":"Later","activeForm":"Doing later","status":"pending"}]}}]}}"#;
        assert_eq!(
            todo_snapshot_from_line(line)
                .and_then(|snapshot| snapshot.active)
                .as_deref(),
            Some("Wiring the segment")
        );
    }

    #[test]
    fn active_todo_falls_back_to_content_and_takes_the_last_block() {
        let line = r#"{"content":[
            {"type":"tool_use","name":"TodoWrite","input":{"todos":[
                {"content":"Stale goal","status":"in_progress"}]}},
            {"type":"tool_use","name":"TodoWrite","input":{"todos":[
                {"content":"Fresh goal","activeForm":"  ","status":"in_progress"}]}}]}"#;
        assert_eq!(
            todo_snapshot_from_line(line)
                .and_then(|snapshot| snapshot.active)
                .as_deref(),
            Some("Fresh goal")
        );
    }

    #[test]
    fn progress_counts_come_from_the_list_and_the_bar_rounds_up() {
        let line = r#"{"content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[
            {"content":"One","status":"completed"},
            {"content":"Two","status":"completed"},
            {"content":"Three","activeForm":"Doing three","status":"in_progress"},
            {"content":"Four","status":"pending"},
            {"content":"Five","status":"pending"},
            {"content":"Six","status":"pending"},
            {"content":"Seven","status":"pending"}]}}]}"#;
        let snapshot = todo_snapshot_from_line(line).expect("snapshot");
        assert_eq!(snapshot.done, 2);
        assert_eq!(snapshot.total, 7);
        assert_eq!(snapshot.active.as_deref(), Some("Doing three"));

        // One of seven must still light a cell rather than reading as untouched.
        assert_eq!(filled_cells(1, 7), 1);
        assert_eq!(filled_cells(0, 7), 0);
        assert_eq!(filled_cells(7, 7), BAR_CELLS);
        assert_eq!(filled_cells(1, 2), 4);
        assert_eq!(filled_cells(3, 0), 0);
    }

    #[test]
    fn a_finished_list_clears_rather_than_sitting_at_100_percent() {
        let list = |done: usize, total: usize| TodoProgress {
            active: None,
            done,
            total,
            anchor_secs: Some(1_000),
            completion_secs: Vec::new(),
        };
        assert!(progress_worth_showing(&list(3, 7)));
        assert!(progress_worth_showing(&list(6, 7)));
        assert!(!progress_worth_showing(&list(7, 7)));
        assert!(!progress_worth_showing(&list(0, 0)));
    }

    #[test]
    fn task_events_fold_into_progress_and_outrank_todowrite() {
        let path = std::env::temp_dir().join("claude-healthline-tasks-test.jsonl");
        let create = |stamp: &str, call: &str, subject: &str, active: &str| {
            format!(
                r#"{{"timestamp":"{stamp}","type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{call}","name":"TaskCreate","input":{{"subject":"{subject}","activeForm":"{active}"}}}}]}}}}"#
            )
        };
        let created = |call: &str, id: &str, subject: &str| {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{call}"}}]}},"toolUseResult":{{"task":{{"id":"{id}","subject":"{subject}"}}}}}}"#
            )
        };
        // The streamed spelling is whatever the model emitted, hence task_id here.
        let update = |stamp: &str, id: &str, status: &str| {
            format!(
                r#"{{"timestamp":"{stamp}","type":"assistant","message":{{"content":[{{"type":"tool_use","id":"tu_u","name":"TaskUpdate","input":{{"task_id":"{id}","status":"{status}"}}}}]}}}}"#
            )
        };
        let body = [
            create("2026-09-07T10:00:00.000Z", "tu_1", "Fold task events", "Folding task events"),
            created("tu_1", "task-1", "Fold task events"),
            create("2026-09-07T10:00:01.000Z", "tu_2", "Add the alias", "Adding the alias"),
            created("tu_2", "task-2", "Add the alias"),
            create("2026-09-07T10:00:02.000Z", "tu_3", "Write the tests", "Writing the tests"),
            created("tu_3", "task-3", "Write the tests"),
            create("2026-09-07T10:00:03.000Z", "tu_4", "Drop this one", "Dropping this one"),
            created("tu_4", "task-4", "Drop this one"),
            update("2026-09-07T10:10:00.000Z", "task-1", "completed"),
            update("2026-09-07T10:20:00.000Z", "task-2", "completed"),
            update("2026-09-07T10:21:00.000Z", "task-3", "in_progress"),
            update("2026-09-07T10:22:00.000Z", "task-4", "deleted"),
            // A TodoWrite list in the same window must not win over the fold.
            r#"{"timestamp":"2026-09-07T10:23:00.000Z","content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"Stale","status":"in_progress"}]}}]}"#.to_string(),
        ]
        .join("\n");
        std::fs::write(&path, format!("{body}\n")).expect("write transcript");

        let progress = scan_todo_progress(&path, 1 << 20).expect("progress");
        assert_eq!((progress.done, progress.total), (2, 3));
        assert_eq!(progress.active.as_deref(), Some("Writing the tests"));
        assert_eq!(
            progress.anchor_secs,
            parse_iso_secs("2026-09-07T10:00:00.000Z")
        );
        // 2 done in 20 min -> 10 min each -> 10 min for the one that remains.
        assert_eq!(
            eta_secs(&progress, progress.anchor_secs.unwrap() + 1_200),
            Some(600)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_eta_falls_back_to_the_anchor_without_completion_stamps() {
        let anchored = |done: usize, total: usize, anchor: Option<i64>| TodoProgress {
            active: None,
            done,
            total,
            anchor_secs: anchor,
            completion_secs: Vec::new(),
        };
        // 2 of 8 in 10 minutes -> 5 min each -> 30 min for the remaining 6.
        let progress = anchored(2, 8, Some(1_000));
        assert_eq!(eta_secs(&progress, 1_000 + 600), Some(1_800));

        assert_eq!(eta_secs(&anchored(1, 8, Some(1_000)), 1_600), None);
        assert_eq!(eta_secs(&anchored(8, 8, Some(1_000)), 1_600), None);
        assert_eq!(eta_secs(&anchored(2, 8, None), 1_600), None);
        // A clock that has not advanced (or went backwards) yields no rate.
        assert_eq!(eta_secs(&anchored(2, 8, Some(1_000)), 1_000), None);

        assert_eq!(format_eta(30), "~<1m");
        assert_eq!(format_eta(1_800), "~30m");
        assert_eq!(format_eta(3_900), "~1h05m");
    }

    #[test]
    fn an_eta_prefers_the_median_gap_between_completions() {
        let paced = |done: usize, total: usize, completions: Vec<i64>| TodoProgress {
            active: None,
            done,
            total,
            anchor_secs: Some(0),
            completion_secs: completions,
        };
        // 20 minutes of planning, then completions every 60s: the ramp-up must
        // not be charged to the 5 items that remain.
        let progress = paced(3, 8, vec![1_200, 1_260, 1_320]);
        assert_eq!(eta_secs(&progress, 1_320), Some(300));
        // The anchor rate over the same list would have said 36 minutes.
        assert_eq!(eta_secs(&paced(3, 8, Vec::new()), 1_320), Some(2_200));

        // Time already spent on the in-flight item comes off, but only ever one
        // gap's worth, so a stalled item does not drive the figure to zero.
        assert_eq!(eta_secs(&progress, 1_350), Some(270));
        assert_eq!(eta_secs(&progress, 9_000), Some(240));

        // One slow item moves the median by a rank; a mean would run 7x long.
        assert_eq!(
            eta_secs(&paced(4, 8, vec![0, 60, 120, 1_320]), 1_320),
            Some(240)
        );

        // A backfilled batch is bookkeeping, not work, so it yields no rate and
        // the anchor fallback answers instead.
        assert_eq!(eta_secs(&paced(2, 8, vec![600, 601]), 900), Some(2_700));
    }

    #[test]
    fn todowrite_snapshots_date_each_completion_at_the_call_that_showed_it() {
        let snapshot = |done: usize, stamp: i64| {
            (
                TodoSnapshot {
                    active: None,
                    done,
                    total: 5,
                },
                Some(stamp),
            )
        };
        let run = [
            snapshot(0, 100),
            snapshot(1, 160),
            snapshot(1, 200),
            snapshot(3, 280),
        ];
        assert_eq!(completion_secs_from_snapshots(&run), vec![160, 280, 280]);
    }

    #[test]
    fn iso_timestamps_round_trip_against_the_daily_cost_date() {
        let secs = parse_iso_secs("2026-08-21T12:01:00.000Z").expect("parsed");
        assert_eq!(utc_ymd(secs), "2026-08-21");
        assert_eq!(secs % 86_400, 12 * 3_600 + 60);
        assert_eq!(parse_iso_secs("1970-01-01T00:00:00Z"), Some(0));
        assert!(parse_iso_secs("2026-13-01T00:00:00Z").is_none());
        assert!(parse_iso_secs("not-a-timestamp").is_none());
    }

    #[test]
    fn a_rewritten_list_re_anchors_instead_of_keeping_a_stale_rate() {
        let path = std::env::temp_dir().join("claude-healthline-progress-test.jsonl");
        let call = |stamp: &str, done: usize, total: usize| {
            let todos: Vec<String> = (0..total)
                .map(|index| {
                    let status = if index < done { "completed" } else { "pending" };
                    format!(r#"{{"content":"Item {index}","status":"{status}"}}"#)
                })
                .collect();
            format!(
                r#"{{"timestamp":"{stamp}","content":[{{"type":"tool_use","name":"TodoWrite","input":{{"todos":[{}]}}}}]}}"#,
                todos.join(",")
            )
        };
        let body = format!(
            "{}\n{}\n{}\n{}\n",
            call("2026-08-21T12:00:00.000Z", 0, 4),
            call("2026-08-21T12:10:00.000Z", 2, 4),
            // The list grows: the anchor must move to here, not stay at 12:00.
            call("2026-08-21T12:30:00.000Z", 2, 6),
            call("2026-08-21T12:40:00.000Z", 3, 6)
        );
        std::fs::write(&path, &body).expect("write transcript");

        let progress = scan_todo_progress(&path, 1 << 20).expect("progress");
        assert_eq!((progress.done, progress.total), (3, 6));
        assert_eq!(
            progress.anchor_secs,
            parse_iso_secs("2026-08-21T12:30:00.000Z")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_completed_list_is_authoritative_and_a_mere_mention_is_not() {
        let done = r#"{"type":"tool_use","name":"TodoWrite","input":{"todos":[
            {"content":"Done","status":"completed"}]}}"#;
        let snapshot = todo_snapshot_from_line(done).expect("a real TodoWrite");
        assert_eq!(snapshot.active, None);
        assert_eq!((snapshot.done, snapshot.total), (1, 1));

        // A line that merely quotes the tool name must not shadow a real block.
        let mention = r#"{"type":"user","content":"grep for \"TodoWrite\" please"}"#;
        assert_eq!(todo_snapshot_from_line(mention), None);
        assert_eq!(todo_snapshot_from_line("not json"), None);
        assert_eq!(todo_snapshot_from_line(r#"{"todos":[]}"#), None);
    }

    #[test]
    fn the_tail_window_keeps_the_newest_todo_and_drops_partial_lines() {
        let path = std::env::temp_dir().join("claude-healthline-todo-test.jsonl");
        let todo = |goal: &str| {
            format!(
                r#"{{"content":[{{"type":"tool_use","name":"TodoWrite","input":{{"todos":[{{"content":"{goal}","status":"in_progress"}}]}}}}]}}"#
            )
        };
        let body = format!(
            "{}\n{}\n{}\n",
            todo("First goal"),
            r#"{"type":"user","padding":"a line quoting \"TodoWrite\" harmlessly"}"#,
            todo("Newest goal")
        );
        std::fs::write(&path, &body).expect("write transcript");

        let active = |tail: u64| scan_todo_progress(&path, tail).and_then(|p| p.active);
        assert_eq!(active(1 << 20).as_deref(), Some("Newest goal"));

        // A window that opens mid-line must discard that fragment, not parse it.
        let tail = (body.len() / 2) as u64;
        assert_eq!(active(tail).as_deref(), Some("Newest goal"));

        assert!(scan_todo_progress(Path::new("/nonexistent/transcript.jsonl"), 1 << 20).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn step_state_comes_from_the_prefix_and_a_bare_label_is_running() {
        assert_eq!(
            parse_step("running\tBash: cargo test"),
            Some((StepState::Running, "Bash: cargo test".to_string()))
        );
        assert_eq!(
            parse_step("done\tBash: cargo test\n"),
            Some((StepState::Done, "Bash: cargo test".to_string()))
        );
        // A hook that writes only a label (the older format) must still render.
        assert_eq!(
            parse_step("Read: main.rs"),
            Some((StepState::Running, "Read: main.rs".to_string()))
        );
        assert_eq!(parse_step("done\t   "), None);
        assert_eq!(parse_step("  "), None);
    }
}
