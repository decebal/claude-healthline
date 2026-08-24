# claude-statusline

[![CI](https://github.com/decebal/claude-statusline/actions/workflows/ci.yml/badge.svg)](https://github.com/decebal/claude-statusline/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![For Claude Code](https://img.shields.io/badge/for-Claude%20Code-6f42c1.svg)](https://code.claude.com/docs/en/statusline)

**claude-statusline is a fast, dependency-light Rust status line for [Claude Code](https://code.claude.com/docs/en/statusline).** It turns the JSON that Claude Code streams on stdin into a single powerline row showing your model, git branch, color-coded context-window usage, live spend (session cost, per-hour burn rate, today's total across every session), an optional multi-dimensional **[agent-health](#agent-health-score)** readout, and the **[MCP servers that need your attention](#skills-and-mcp-servers)** — and it **never blanks, hangs, or panics**, whatever the session sends it.

![claude-statusline status bar preview: the same status line in three states — everyday, trouble, and a half-width pane](docs/statusline.svg)

Three states, one line. Notice what the middle row does that the top row doesn't: **problems announce themselves, and nothing else competes for the space.** Context crosses into the danger zone and turns bold red, a server goes down and a red `⚠` appears — while on the healthy top row the MCP segment draws nothing at all. Squeeze the pane and the warning is the thing that *survives*.

> The real status line prefixes each segment with a [Nerd Font](https://www.nerdfonts.com/) powerline glyph, and the image above substitutes lookalikes because GitHub can't render Nerd-Font glyphs. Set `CLAUDE_STATUSLINE_ASCII=1` for the plain-text form — this is the top row above, verbatim:

```text
health R4.9 T4.8 S4.6 | model Opus 5 | dir my-repo | git main | skills 3 proj | ctx 42% | cost $2.40 · ~$1.60/hr · today $12.40 | task t-fa00
```

## What it does

claude-statusline renders up to twelve segments in one line, **each drawn only when it has something to say.** Context usage is color-coded green → yellow → **bold red** as you approach the limit, so the danger zone is visible at a glance; the MCP segment appears only when a server is broken. Segments are listed here in the order they render:

- **Agent health** *(optional, first)* — a multi-dimensional readout (rule-adherence · truthfulness · task-success · stability) with a green/yellow/red verdict, instead of one vague "quality" number. Only shows when a health state file exists. See [Agent health score](#agent-health-score).
- **Model** — the active model's display name.
- **Caveman mode** *(optional)* — the active [caveman](https://github.com/JuliusBrussee/caveman) compression level and the tokens it has saved. Only shows when caveman is active. See [Caveman mode](#caveman-mode).
- **Repo / dir** — repository name, else the working-directory basename.
- **Git branch** — read straight from `.git/HEAD` with **no `git` subprocess**; handles worktrees and detached HEAD.
- **Project skills** *(optional)* — how many skills the current checkout ships in `.claude/skills/`. See [Skills and MCP servers](#skills-and-mcp-servers).
- **Context %** — green `<50`, yellow `50–80`, **bold red `>80`** or when `exceeds_200k_tokens` trips.
- **Cost cluster** — `session · ~$/hr burn · today's total` (see below).
- **MCP trouble** *(optional)* — MCP servers that need re-auth or failed to connect. Draws nothing when the fleet is healthy. See [Skills and MCP servers](#skills-and-mcp-servers).
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

That covers every segment above except two, each of which needs a hook to feed it:

| Want | Add | Cost |
|---|---|---|
| MCP servers that need re-auth | `claude-mcp-probe` on `SessionStart` | ~10 ms per session — [details](#mcp-trouble--mcp-1-down--2-auth) |
| The agent-health readout | `claude-health-hook` + `claude-health-report` | see [agent health](#agent-health-score) |

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

## Agent health score

An optional first segment shows a compact, multi-dimensional **agent health**
readout instead of one vague "quality" number — so you can tell instruction
drift from hallucination from execution failure at a glance:

```
 R4.9 T4.8 S4.6          healthy   (green)
 stab 4.2 !3 →repair     degraded  (yellow — a live tool-failure loop)
 R– T4.4 S– →restart     restart   (red — hard gate)
```

R = rule adherence · T = truthfulness · S = task success · `!N` = drift ·
`→next` = recommended action. Colour is the verdict (green/yellow/red).

The green bars are strict on purpose — rules and truthfulness need **≥ 4.75**,
task ≥ 4.50 — so `T4.7` is already *degraded*, not healthy. Fabrication risk
shouldn't have to clear a low bar to get noticed.

**It only displays; it never invents scores.** The status line reads a
per-session state file (`~/.claude/agent-health/<session_id>.json`), filled by
two bundled hooks:

| Binary | Hook | Fills |
|---|---|---|
| `claude-health-hook` | `PostToolUse` + `PostToolUseFailure` | stability + drift, from real tool outcomes |
| `claude-health-report` | `Stop` | rules / truth / task, from a cheap judge model |

Anything a hook hasn't scored renders `–`, never a guess: the report omits a
dimension the transcript can't support, and an unfinished turn scores no `task`
rather than a bad one. Any safety/critical flag is a **hard gate** that forces
red regardless of the averages.

The report costs roughly **$0.02 per turn** (one `haiku` call), never blocks the
turn — it detaches and grades in the background — and cannot recurse into itself.
Skip it and the segment still works, with the subjective three showing `–`.

Full schema, thresholds, restart policy, prompt design, and settings snippets:
[docs/agent-health.md](docs/agent-health.md).

## Caveman mode

[caveman](https://github.com/JuliusBrussee/caveman) compresses assistant prose to
cut token spend, and ships a badge script for the status line. Running it meant
choosing between that badge and this status line, so claude-statusline renders
the badge itself — no `bash`, no `node`, no wrapper:

```text
model Opus 5 | ⛏ full · 114.9k saved | dir my-repo | git main | ctx 42% | cost $2.40
```

It reads the two files the plugin already writes under `$CLAUDE_CONFIG_DIR`
(default `~/.claude`) — `.caveman-active` for the level, and
`.caveman-statusline-suffix` for the savings figure `/caveman-stats` renders.
Both are absent for everyone not running caveman, so the segment costs two
`stat` calls and draws nothing.

The savings figure only appears once `/caveman-stats` has run at least once —
until then the badge is the level alone, never an invented number. The level
`off` draws nothing: "not compressing" is not news.

Both files are treated as hostile input, because their paths are predictable and
a local attacker could plant one: a symlink is refused, the read is capped at 64
bytes, the level must be on caveman's own whitelist, and the savings figure must
parse as a whole token count (`114.9k`, `1.2M`) — so a planted
`\x1b[31mPWNED\x1b[0m` renders nothing rather than repainting the terminal on
every keystroke. Hide the segment with `CLAUDE_STATUSLINE_NO_CAVEMAN=1`.

## Skills and MCP servers

Claude Code's stdin payload says nothing about skills or MCP servers, so both of
these segments derive their own data — and both are built around one rule: **a
number that never changes is not worth a column.**

### Project skills — `skills 3 proj`

Counts the immediate subdirectories of `$cwd/.claude/skills/` that contain a
`SKILL.md`. That is one `read_dir`, so it needs no cache.

The **global** skill count is deliberately never rendered: it is the same number
in every repo on every keystroke, so it can't inform a decision. What does change
as you move around is the skills a checkout ships — often ones you didn't know
were there. Launching Claude Code from `$HOME` makes `$cwd/.claude/skills` *be*
the global directory; the segment detects that and draws nothing rather than
reporting 60-odd global skills as "project" skills. Hide it with
`CLAUDE_STATUSLINE_NO_SKILLS=1`.

### MCP trouble — `mcp 1 down · 2 auth`

Only failures and expired auth are rendered, in red and yellow respectively. "All
17 connected" is not actionable, so a healthy fleet costs **zero columns** — the
segment's mere presence is the signal. For the same reason it is the one optional
segment that is *never* dropped on a narrow terminal.

The status line can't measure this itself: `claude mcp list` health-checks every
remote connector serially and takes **seconds**. So the bundled
`claude-mcp-probe` binary does it out of band and leaves a small cache behind:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "claude-mcp-probe", "timeout": 5 }] }
    ]
  }
}
```

Claude Code **blocks session startup on its hooks**, so the hook invocation does
almost nothing: it `stat`s the cache, returns immediately if it is younger than
`CLAUDE_STATUSLINE_MCP_REFRESH` (default 4h), and otherwise re-launches itself
detached. Either way the hook returns in ~10 ms and your session starts now, with
the refreshed data landing a few seconds later. Run `claude-mcp-probe
--foreground` to refresh synchronously.

> **The probe stores server names and statuses only — never the command line.**
> `claude mcp list` echoes each stdio server's full argv, and those routinely
> carry secrets (`--api-key eyJhbGci…`, tokens, DSNs). The middle field is
> dropped before anything is written and is never logged. Two unit tests assert
> that no part of a sample argv survives the parse.

A status phrasing the probe doesn't recognize is recorded as `unknown` and
**not** counted as trouble: if a future CLI reword turned into a permanent false
alarm, you'd learn to ignore the segment. Equally, a probe run that parses to
nothing leaves the old cache alone rather than writing `{}`, which would render
as "everything is fine".

## Configuration

Every knob is an environment variable, so it composes cleanly with the `command` string.

| Variable | Effect |
|---|---|
| `CLAUDE_STATUSLINE_ASCII=1` (or `NERD_FONT=0`) | Plain-text labels instead of Nerd-Font glyphs (colors kept) |
| `CLAUDE_STATUSLINE_NO_DAILY=1` | Skip the transcript scan (drops the `today $…` figure) |
| `CLAUDE_STATUSLINE_NO_HEALTH=1` | Hide the agent-health segment |
| `CLAUDE_STATUSLINE_HEALTH_DIR=<path>` | Override the health state-file dir |
| `CLAUDE_STATUSLINE_NO_CAVEMAN=1` | Hide the caveman-mode segment |
| `CLAUDE_STATUSLINE_NO_SKILLS=1` | Hide the project-skills count |
| `CLAUDE_STATUSLINE_NO_MCP=1` | Hide the MCP-trouble segment (and make the probe a no-op) |
| `CLAUDE_STATUSLINE_MCP_CACHE=<path>` | Override the probe cache (default `$CLAUDE_CONFIG_DIR/statusline-cache/mcp.json`) |
| `CLAUDE_STATUSLINE_MCP_REFRESH=<secs>` | How stale the probe cache may get before a refresh (default `14400`) |
| `CLAUDE_STATUSLINE_CLAUDE_BIN=<path>` | Explicit path to the `claude` binary, for the probe |
| `CLAUDE_CONFIG_DIR=<path>` | Where the caveman flag files and probe cache live (default `~/.claude`) |
| `CLAUDE_STATUSLINE_CN_TTL=<secs>` | Chronis task cache TTL (default `8`) |
| `CLAUDE_STATUSLINE_CN_BIN=<path>` | Explicit path to the `cn` binary |

## Why it's safe to run on every keystroke

A status line command runs constantly, so claude-statusline is built to be boring under load: fast, bounded, and impossible to break.

- **Never blank / never panics.** Any stdin — empty, truncated, non-JSON, wrong-typed — still prints one non-empty line and exits `0`. (Claude Code blanks the status line on empty stdout or a non-zero exit, so this is a hard guarantee, verified across ~40 malformed and hostile inputs.)
- **Never hangs.** The `cn` lookup is wall-clock bounded to **≤ 800 ms** and cached per directory; the daily-cost scan is cached for **60 s** and only reads files modified today. Warm renders are **single-digit milliseconds**. The skills count and MCP segment add **0.038 ms** together — one `read_dir` and one small JSON read; the multi-second `claude mcp list` runs in a detached hook, never on a render.
- **Tiny.** **Two** dependencies (`serde`, `serde_json`); a **~525 KB** stripped release binary. No `git`, `jq`, or shell subprocesses on the hot path.

## Narrow and split-screen terminals

claude-statusline adapts to the width Claude Code reports in `COLUMNS`. When space runs out it degrades gracefully — dropping the lowest-value segments first, then compacting the cost cluster — so **model, context %, and cost never fall off screen** on a half-width pane.

Removal order: `project skills` → `caveman` → `lines±` → `rate limit` → cost `burn`/`today` extras → `repo` → `git branch` → `task`. The three essentials are never dropped — and neither is `mcp`, which only appears when a server actually needs attention.

| Terminal width | What stays |
|---|---|
| Full | every segment that has data |
| Wide split | drops project skills, then the caveman badge |
| ~Half | drops lines± and rate limit; cost compacts to session-only |
| Very narrow | model · context % · cost — **plus MCP trouble** |

`COLUMNS` is exported by Claude Code v2.1.153+. If it's unset, the line renders in full (no truncation).

## How it compares

claude-statusline optimizes for a lean, native, never-blank single binary with built-in cost math, chronis task tracking, and health signals you can act on — the agent-health readout and MCP trouble alerts are, as far as I know, unique to it. Other excellent status lines trade that for more widgets or a config UI — pick what fits.

| Project | Runtime | Focus |
|---|---|---|
| **claude-statusline** (this) | Rust (single binary) | Speed, never-blank guarantee, cost + burn + daily, chronis tasks, **agent-health readout**, **MCP trouble alerts**, caveman badge |
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

### Why doesn't it show how many skills or MCP servers I have?

Because those totals are the same on every render, so they can't tell you
anything you don't already know. The status line shows the parts that *change* —
the skills a given checkout ships, and the MCP servers that currently need
fixing. See [Skills and MCP servers](#skills-and-mcp-servers).

### Does the MCP segment slow down my session start?

No. The `claude-mcp-probe` hook returns in about **10 ms**: it either finds a
cache younger than 4 hours and does nothing, or re-launches itself detached and
returns straight away. The multi-second `claude mcp list` never runs inline.

### Does the probe write my MCP API keys to disk?

No — and it is written specifically to avoid that. `claude mcp list` prints each
stdio server's full argv, secrets included; the probe keeps only the server name
and its status, and two unit tests assert no part of a sample argv survives the
parse.

### Does it require chronis?
No. The task segment simply disappears if `cn` (chronis) or an in-progress task isn't found. Every other segment works standalone.

### What happens if Claude Code sends malformed data?
It still prints a valid one-line status and exits `0`. A missing or null field omits only its own segment; garbage input falls back to the model name. This is a design guarantee, not a best effort.

### Is the pricing table going to go stale?
The built-in prices are a snapshot (August 2026). When Anthropic changes prices, edit the table or drop a `~/.claude/statusline-pricing.json` override — no recompile needed.

### Do I have to drop caveman's badge script to use this?
No — this renders it. Point `statusLine.command` at claude-statusline and the caveman level plus its savings figure appear as a segment, read from the same files the plugin already writes. Nothing to install, nothing to wrap, and `CLAUDE_STATUSLINE_NO_CAVEMAN=1` turns it off. See [Caveman mode](#caveman-mode).

### What happens on a small or split-screen terminal?
It adapts to `COLUMNS` and drops the least-important segments first (project skills, then the caveman badge, then lines±, then rate limit, then cost extras, then repo/branch/task), always keeping model, context %, cost — and MCP trouble. So on a half-width pane you still see how full your context is, what the session costs, and whether a server is down. The bottom row of the [preview image](#claude-statusline) is exactly this.

### What is the agent-health segment?
An optional first segment reporting how the agent is doing across *separate* dimensions — rule-adherence, truthfulness, task-success, stability — with a green/yellow/red verdict, so you can tell instruction drift from hallucination from execution failure at a glance (not one vague "quality" number). It **only displays** scores from a per-session state file and **never invents them**: the bundled `claude-health-hook` fills the observable dimensions (stability + drift) from real tool outcomes; the rest render `–` until an evaluator or the agent writes them. Any safety/critical flag is a hard gate → red. Full schema, rubric, and hook wiring: [docs/agent-health.md](docs/agent-health.md).

## Contributing

Issues and PRs welcome. `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo build --release` should all be clean.

## License

MIT © decebal
