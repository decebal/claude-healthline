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
    pub billing: Billing,
}

/// What the environment says about who pays for the session. The stdin JSON
/// decides first (see `show_cost`); this is the fallback and the override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Billing {
    /// `CLAUDE_HEALTHLINE_COST` = `show` / `hide`, which beats every signal.
    pub forced: Option<bool>,
    /// An API key, an auth token, or a cloud provider bills per token.
    pub per_token: bool,
}

impl Billing {
    pub fn from(env: impl Fn(&str) -> Option<String>) -> Billing {
        let forced = match env("CLAUDE_HEALTHLINE_COST").as_deref().map(str::trim) {
            Some("show") => Some(true),
            Some("hide") => Some(false),
            _ => None,
        };
        let set = |k: &str| env(k).is_some_and(|v| !v.trim().is_empty());
        let on = |k: &str| env(k).is_some_and(|v| matches!(v.trim(), "1" | "true"));
        Billing {
            forced,
            per_token: set("ANTHROPIC_API_KEY")
                || set("ANTHROPIC_AUTH_TOKEN")
                || on("CLAUDE_CODE_USE_BEDROCK")
                || on("CLAUDE_CODE_USE_VERTEX")
                || on("CLAUDE_CODE_USE_FOUNDRY"),
        }
    }
}

impl Config {
    pub fn classic() -> Config {
        Config {
            rows: 1,
            task_line: false,
            step: false,
            worktree: false,
            cost_emphasis: false,
            billing: Billing {
                forced: None,
                per_token: false,
            },
        }
    }

    pub fn from_env() -> Config {
        let mut config = Config::classic();

        config.rows = env_rows();
        config.task_line = env_flag("CLAUDE_HEALTHLINE_TASK_LINE");
        config.step = env_flag("CLAUDE_HEALTHLINE_STEP");
        config.worktree = env_flag("CLAUDE_HEALTHLINE_WORKTREE");
        config.cost_emphasis = env_flag("CLAUDE_HEALTHLINE_COST_EMPHASIS");
        config.billing = Billing::from(|k| std::env::var(k).ok());

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
