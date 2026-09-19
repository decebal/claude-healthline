//! The opt-in switches, read once at startup.

/// Every addition to the default status line is opt-in and read here once, so
/// an environment that sets none of these renders what it rendered before any
/// of them existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub rows: usize,
    pub task_line: bool,
    pub step: bool,
    pub worktree: bool,
    pub cost_emphasis: bool,
}

impl Config {
    pub fn classic() -> Config {
        Config {
            rows: 1,
            task_line: false,
            step: false,
            worktree: false,
            cost_emphasis: false,
        }
    }

    pub fn from_env() -> Config {
        let mut config = Config::classic();

        config.rows = env_rows();
        config.task_line = env_flag("CLAUDE_HEALTHLINE_TASK_LINE");
        config.step = env_flag("CLAUDE_HEALTHLINE_STEP");
        config.worktree = env_flag("CLAUDE_HEALTHLINE_WORKTREE");
        config.cost_emphasis = env_flag("CLAUDE_HEALTHLINE_COST_EMPHASIS");

        config
    }
}

fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name), Ok(value) if value == "1")
}

/// Claude Code renders one row per printed line, so extra rows cost vertical
/// space in every session.
fn env_rows() -> usize {
    std::env::var("CLAUDE_HEALTHLINE_ROWS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|&rows| (1..=3).contains(&rows))
        .unwrap_or(1)
}
