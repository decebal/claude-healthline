//! The session's task list, its progress bar, and the tool call in flight.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::{
    clean, safe_session_id, step_dir, Glyphs, Seg, BRIGHT_MAGENTA, BRIGHT_WHITE, GREEN, GREY, RESET,
    WHITE,
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
    saw_events: bool,
}

impl TaskFold {
    fn apply_line(&mut self, line: &str) {
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
            self.start_new_run_if_settled();
            self.pending_creates.extend(creates);
        }
        for (task_id, status, active_form) in updates {
            self.saw_events = true;
            if status.as_deref() == Some("deleted") {
                self.tasks.retain(|(id, _)| *id != task_id);
                continue;
            }
            let Some((_, entry)) = self.tasks.iter_mut().find(|(id, _)| *id == task_id) else {
                continue;
            };

            if let Some(status) = status {
                entry.status = status;
            }
            if active_form.is_some() {
                entry.active_form = active_form;
            }
        }
    }

    /// A create arriving once every task is finished opens a NEW run, so the bar
    /// starts empty rather than inheriting the previous run's fill. A run with
    /// anything still pending or in flight is the same run, and keeps its cells.
    fn start_new_run_if_settled(&mut self) {
        let settled = !self.tasks.is_empty()
            && self
                .tasks
                .iter()
                .all(|(_, entry)| entry.status == "completed");

        if settled {
            self.tasks.clear();
        }
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
        }
    }
}

/// What the bar and the goal label are drawn from. A line merely quoting the
/// tool name yields `None` from the parse, whereas a real call with nothing in
/// progress is authoritative and CLEARS an earlier goal rather than deferring
/// to it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TodoProgress {
    active: Option<String>,
    done: usize,
    total: usize,
}

fn todo_progress_from_line(line: &str) -> Option<TodoProgress> {
    let v: Value = serde_json::from_str(line).ok()?;
    let mut found = None;
    last_todowrite_input(&v, &mut found);
    let todos = match found?.get("todos").and_then(Value::as_array) {
        Some(t) => t,
        None => return Some(TodoProgress::default()),
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
    Some(TodoProgress {
        active,
        done: todos
            .iter()
            .filter(|item| status(item) == "completed")
            .count(),
        total: todos.len(),
    })
}

/// The newest todo list in the transcript's tail window.
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

    let mut latest: Option<TodoProgress> = None;
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
            fold.apply_line(&line);
        }

        if !line.contains("\"TodoWrite\"") {
            continue;
        }
        if let Some(progress) = todo_progress_from_line(&line) {
            latest = Some(progress);
        }
    }

    // Task tools are what modern models get; TodoWrite only appears in a session
    // that opted into it, so a live fold outranks any TodoWrite list in the window.
    if fold.saw_events && !fold.tasks.is_empty() {
        return Some(fold.progress());
    }

    latest
}

const BAR_CELLS: usize = 7;

/// A list with nothing in it draws nothing. A finished one stays on screen,
/// full, so an idle session reads as "that run is done" rather than as a session
/// that never had a list.
fn progress_worth_showing(progress: &TodoProgress) -> bool {
    progress.total > 0
}

/// Rounded UP, so any progress at all lights the first cell.
fn filled_cells(done: usize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    (done * BAR_CELLS).div_ceil(total).min(BAR_CELLS)
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

    let styled = format!(
        "{GREEN}{}{RESET}{GREY}{}{RESET}",
        "\u{25AE}".repeat(filled),
        "\u{25AF}".repeat(BAR_CELLS - filled)
    );

    Seg {
        kind: "progress",
        plain: BAR_CELLS,
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

    fn create_line(call: &str, subject: &str, active: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{call}","name":"TaskCreate","input":{{"subject":"{subject}","activeForm":"{active}"}}}}]}}}}"#
        )
    }

    fn created_line(call: &str, id: &str) -> String {
        format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{call}"}}]}},"toolUseResult":{{"task":{{"id":"{id}"}}}}}}"#
        )
    }

    /// The streamed spelling is whatever the model emitted, hence task_id here.
    fn update_line(id: &str, status: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"tu_u","name":"TaskUpdate","input":{{"task_id":"{id}","status":"{status}"}}}}]}}}}"#
        )
    }

    fn write_transcript(tag: &str, lines: &[String]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "claude-healthline-{tag}-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write transcript");
        path
    }

    #[test]
    fn active_todo_prefers_active_form_of_the_in_progress_item() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"TodoWrite","input":{"todos":[
                {"content":"Ship it","activeForm":"Shipping it","status":"completed"},
                {"content":"Wire the segment","activeForm":"Wiring the segment","status":"in_progress"},
                {"content":"Later","activeForm":"Doing later","status":"pending"}]}}]}}"#;
        assert_eq!(
            todo_progress_from_line(line)
                .and_then(|progress| progress.active)
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
            todo_progress_from_line(line)
                .and_then(|progress| progress.active)
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
        let progress = todo_progress_from_line(line).expect("progress");
        assert_eq!(progress.done, 2);
        assert_eq!(progress.total, 7);
        assert_eq!(progress.active.as_deref(), Some("Doing three"));

        // One of seven must still light a cell rather than reading as untouched.
        assert_eq!(filled_cells(1, 7), 1);
        assert_eq!(filled_cells(1, 2), 4);
        assert_eq!(filled_cells(3, 0), 0);
    }

    #[test]
    fn a_run_starts_empty_and_ends_full_rather_than_clearing() {
        let list = |done: usize, total: usize| TodoProgress {
            active: None,
            done,
            total,
        };

        assert!(!progress_worth_showing(&list(0, 0)));
        assert!(progress_worth_showing(&list(0, 7)));
        assert!(progress_worth_showing(&list(7, 7)));

        assert_eq!(filled_cells(0, 7), 0);
        assert_eq!(filled_cells(7, 7), BAR_CELLS);
    }

    #[test]
    fn task_events_fold_into_progress_and_outrank_todowrite() {
        let path = write_transcript(
            "fold",
            &[
                create_line("tu_1", "Fold task events", "Folding task events"),
                created_line("tu_1", "task-1"),
                create_line("tu_2", "Add the alias", "Adding the alias"),
                created_line("tu_2", "task-2"),
                create_line("tu_3", "Write the tests", "Writing the tests"),
                created_line("tu_3", "task-3"),
                create_line("tu_4", "Drop this one", "Dropping this one"),
                created_line("tu_4", "task-4"),
                update_line("task-1", "completed"),
                update_line("task-2", "completed"),
                update_line("task-3", "in_progress"),
                update_line("task-4", "deleted"),
                // A TodoWrite list in the same window must not win over the fold.
                r#"{"content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"Stale","status":"in_progress"}]}}]}"#.to_string(),
            ],
        );

        let progress = scan_todo_progress(&path, 1 << 20).expect("progress");
        assert_eq!((progress.done, progress.total), (2, 3));
        assert_eq!(progress.active.as_deref(), Some("Writing the tests"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_create_after_a_finished_run_starts_the_bar_over() {
        let path = write_transcript(
            "new-run",
            &[
                create_line("tu_1", "First", "Doing first"),
                created_line("tu_1", "task-1"),
                create_line("tu_2", "Second", "Doing second"),
                created_line("tu_2", "task-2"),
                update_line("task-1", "completed"),
                update_line("task-2", "completed"),
                create_line("tu_3", "Next run", "Starting the next run"),
                created_line("tu_3", "task-3"),
            ],
        );

        let progress = scan_todo_progress(&path, 1 << 20).expect("progress");
        assert_eq!((progress.done, progress.total), (0, 1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_create_while_a_run_is_live_extends_it_instead() {
        let path = write_transcript(
            "same-run",
            &[
                create_line("tu_1", "First", "Doing first"),
                created_line("tu_1", "task-1"),
                create_line("tu_2", "Second", "Doing second"),
                created_line("tu_2", "task-2"),
                update_line("task-1", "completed"),
                create_line("tu_3", "Third", "Doing third"),
                created_line("tu_3", "task-3"),
            ],
        );

        let progress = scan_todo_progress(&path, 1 << 20).expect("progress");
        assert_eq!((progress.done, progress.total), (1, 3));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_rewritten_todo_list_keeps_the_newest_counts() {
        let call = |done: usize, total: usize| {
            let todos: Vec<String> = (0..total)
                .map(|index| {
                    let status = if index < done { "completed" } else { "pending" };
                    format!(r#"{{"content":"Item {index}","status":"{status}"}}"#)
                })
                .collect();
            format!(
                r#"{{"content":[{{"type":"tool_use","name":"TodoWrite","input":{{"todos":[{}]}}}}]}}"#,
                todos.join(",")
            )
        };
        let path = write_transcript(
            "rewritten",
            &[call(0, 4), call(2, 4), call(2, 6), call(3, 6)],
        );

        let progress = scan_todo_progress(&path, 1 << 20).expect("progress");
        assert_eq!((progress.done, progress.total), (3, 6));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_completed_list_is_authoritative_and_a_mere_mention_is_not() {
        let done = r#"{"type":"tool_use","name":"TodoWrite","input":{"todos":[
            {"content":"Done","status":"completed"}]}}"#;
        let progress = todo_progress_from_line(done).expect("a real TodoWrite");
        assert_eq!(progress.active, None);
        assert_eq!((progress.done, progress.total), (1, 1));

        // A line that merely quotes the tool name must not shadow a real block.
        let mention = r#"{"type":"user","content":"grep for \"TodoWrite\" please"}"#;
        assert_eq!(todo_progress_from_line(mention), None);
        assert_eq!(todo_progress_from_line("not json"), None);
        assert_eq!(todo_progress_from_line(r#"{"todos":[]}"#), None);
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
