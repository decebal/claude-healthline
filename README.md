# claude-statusline

[![CI](https://github.com/decebal/claude-statusline/actions/workflows/ci.yml/badge.svg)](https://github.com/decebal/claude-statusline/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![For Claude Code](https://img.shields.io/badge/for-Claude%20Code-6f42c1.svg)](https://code.claude.com/docs/en/statusline)

**claude-statusline is a fast, dependency-light Rust status line for [Claude Code](https://code.claude.com/docs/en/statusline).** It turns the JSON that Claude Code streams on stdin into a single powerline row showing your model, git branch, color-coded context-window usage, and live spend — session cost, per-hour burn rate, and today's total across every session — and it **never blanks, hangs, or panics**, whatever the session sends it.

```
󰄵 Opus 4.8 ▏ my-repo ▏  main ▏ 󰘦 42% ▏ 󰄗 $2.40 · ~$1.60/hr · today $12.40 ▏ 󰄹 t-fa00 ▏ 5h 24% · 7d 41% ▏ 󰷫 +156 -23
```

## What it does

claude-statusline renders eight segments in one line, each drawn only when its data is present. Context usage is color-coded green → yellow → **bold red** as you approach the limit, so the "danger zone" is visible at a glance.

- **Model** — the active model's display name.
- **Repo / dir** — repository name, else the working-directory basename.
- **Git branch** — read straight from `.git/HEAD` with **no `git` subprocess**; handles worktrees and detached HEAD.
- **Context %** — green `<50`, yellow `50–80`, **bold red `>80`** or when `exceeds_200k_tokens` trips.
- **Cost cluster** — `session · ~$/hr burn · today's total` (see below).
- **Task** — the in-progress [chronis](https://github.com/rtk-ai) (`cn`) task in the current directory. Optional; omitted if `cn` or a task isn't found.
- **Rate limit** — 5-hour and 7-day usage windows (shown on Pro/Max plans).
- **Lines ±** — lines added/removed this session.

## Quick start

Install the binary, then point Claude Code at it. That's the whole setup.

```sh
cargo install --git https://github.com/decebal/claude-statusline
```

Add this to `~/.claude/settings.json` (global — applies to every project):

```json
{
  "statusLine": {
    "type": "command",
    "command": "claude-statusline"
  }
}
```

Use an absolute path (e.g. `~/.cargo/bin/claude-statusline`) if `~/.cargo/bin` isn't on the status line's `PATH`. The status line refreshes on Claude Code's own events; add `"refreshInterval": 5` to the block if you want it to also tick while idle.

## Cost, burn rate, and today's spend

The cost cluster answers "what is this session costing me, and how much have I spent today?" in one place. Session cost and the per-hour burn rate come directly from Claude Code's stdin JSON; today's total is computed the way [ccusage](https://ccusage.com/guide/statusline) does it.

Claude Code transcripts record `costUSD: null` on every line, so **today's total is derived, not read**: claude-statusline sums today's token `usage` across `~/.claude/projects/**/*.jsonl` and multiplies by a per-model price table. Prices are per-million tokens (August 2026); cache rates follow Anthropic's published ratios — cache read `0.10×` input, 5-minute cache write `1.25×`, 1-hour cache write `2.0×`.

**Override the built-in prices without recompiling** by creating `~/.claude/statusline-pricing.json`:

```json
{
  "claude-opus-4-8":   { "input": 5.0, "output": 25.0 },
  "claude-sonnet-4-6": { "input": 3.0, "output": 15.0 }
}
```

## Configuration

Every knob is an environment variable, so it composes cleanly with the `command` string.

| Variable | Effect |
|---|---|
| `CLAUDE_STATUSLINE_ASCII=1` (or `NERD_FONT=0`) | Plain-text labels instead of Nerd-Font glyphs (colors kept) |
| `CLAUDE_STATUSLINE_NO_DAILY=1` | Skip the transcript scan (drops the `today $…` figure) |
| `CLAUDE_STATUSLINE_CN_TTL=<secs>` | Chronis task cache TTL (default `8`) |
| `CLAUDE_STATUSLINE_CN_BIN=<path>` | Explicit path to the `cn` binary |

## Why it's safe to run on every keystroke

A status line command runs constantly, so claude-statusline is built to be boring under load: fast, bounded, and impossible to break.

- **Never blank / never panics.** Any stdin — empty, truncated, non-JSON, wrong-typed — still prints one non-empty line and exits `0`. (Claude Code blanks the status line on empty stdout or a non-zero exit, so this is a hard guarantee, verified across ~40 malformed and hostile inputs.)
- **Never hangs.** The `cn` lookup is wall-clock bounded to **≤ 800 ms** and cached per directory; the daily-cost scan is cached for **60 s** and only reads files modified today. Warm renders are **single-digit milliseconds**.
- **Tiny.** **Two** dependencies (`serde`, `serde_json`); a **~470 KB** stripped release binary. No `git`, `jq`, or shell subprocesses on the hot path.

## How it compares

claude-statusline optimizes for a lean, native, never-blank single binary with built-in cost math and chronis task tracking. Other excellent status lines trade that for more widgets or a config UI — pick what fits.

| Project | Runtime | Focus |
|---|---|---|
| **claude-statusline** (this) | Rust (single binary) | Speed, never-blank guarantee, cost + burn + daily, chronis tasks |
| [ccstatusline](https://github.com/sirmalloc/ccstatusline) | TypeScript / Bun | Many widgets + TUI configurator |
| [claude-powerline](https://github.com/chongdashu/claude-powerline) | Node | Plugin-native powerline themes |
| [CCometixLine](https://github.com/Haleclipse/CCometixLine) | Rust | Powerline segments |
| [ccusage](https://ccusage.com/guide/statusline) | Node | Cost/usage analytics (statusline mode) |

## FAQ

### Does it work without a Nerd Font?
Yes. Set `CLAUDE_STATUSLINE_ASCII=1` (or `NERD_FONT=0`) and it renders plain-text labels (`model`, `ctx`, `cost`, …) with colors intact. The powerline glyphs need a [Nerd Font](https://www.nerdfonts.com/) such as *MesloLGS NF*; without one they show as boxes, which is why the ASCII fallback exists.

### How does it calculate today's cost?
It sums today's token usage from your Claude Code transcripts (`~/.claude/projects/**/*.jsonl`) and applies a per-model price table, because transcripts store `costUSD: null`. Cache tokens are priced at Anthropic's ratios (read `0.10×`, write `1.25×`/`2.0×`). Disable the scan with `CLAUDE_STATUSLINE_NO_DAILY=1`.

### Will it slow down Claude Code?
No. Warm renders are single-digit milliseconds; the only external call (`cn`) is capped at 800 ms and cached, and the daily scan is cached for 60 seconds. There are no `git`/`jq`/shell subprocesses on the hot path.

### Does it require chronis?
No. The task segment simply disappears if `cn` (chronis) or an in-progress task isn't found. Every other segment works standalone.

### What happens if Claude Code sends malformed data?
It still prints a valid one-line status and exits `0`. A missing or null field omits only its own segment; garbage input falls back to the model name. This is a design guarantee, not a best effort.

### Is the pricing table going to go stale?
The built-in prices are a snapshot (August 2026). When Anthropic changes prices, edit the table or drop a `~/.claude/statusline-pricing.json` override — no recompile needed.

## Contributing

Issues and PRs welcome. `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo build --release` should all be clean.

## License

MIT © decebal
