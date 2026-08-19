# claude-statusline

A fast, **never-blank** [Claude Code](https://code.claude.com/docs/en/statusline)
status line written in Rust. It reads the session JSON Claude Code streams on
stdin and renders a single powerline row with Nerd-Font glyphs:

```
󰄵 Opus 4.8 ▏ my-repo ▏  main ▏ 󰘦 42% ▏ 󰄗 $2.40 · ~$1.60/hr · today $12.40 ▏ 󰄹 t-fa00 ▏ 5h 24% · 7d 41% ▏ 󰷫 +156 -23
```

## Segments

| Segment | Source | Notes |
|---|---|---|
| **Model** | `model.display_name` | dim cyan |
| **Repo / dir** | `workspace.repo.name` → else `basename(cwd)` | dim |
| **Git branch** | reads `<cwd>/.git/HEAD` directly — **no `git` subprocess** | handles worktrees + detached HEAD |
| **Context %** | `context_window.used_percentage` (derives from tokens if null) | **green <50 · yellow 50–80 · bold-red >80 or `exceeds_200k_tokens`** — the "danger zone" cue |
| **Cost cluster** | `cost.total_cost_usd` + `total_duration_ms` + transcript scan | `$session · ~$/hr burn · today $total` |
| **Task** | `cn` (chronis) in-progress task in the cwd | optional; omitted if `cn`/task absent |
| **Rate limit** | `rate_limits.five_hour` / `seven_day` | Pro/Max only |
| **Lines ±** | `cost.total_lines_added` / `removed` | green `+` / red `-` |

### Cost, burn rate & today's spend

- **Session cost** and **burn rate** (`$/hr`) come straight from the stdin JSON.
- **Today's total** is computed ccusage-style: Claude Code transcripts record
  `costUSD: null`, so the tool sums today's token `usage` across
  `~/.claude/projects/**/*.jsonl` and multiplies by a per-model price table.
  Prices are per-million-token (Aug 2026); cache rates follow Anthropic's
  ratios (read `0.10×` input, 5-min write `1.25×`, 1-hour write `2.0×`).
  Override the built-in table without recompiling by creating
  `~/.claude/statusline-pricing.json`:

  ```json
  { "claude-opus-4-8": { "input": 5.0, "output": 25.0 },
    "claude-sonnet-4-6": { "input": 3.0, "output": 15.0 } }
  ```

## Install

```sh
cargo install --git https://github.com/decebal/claude-statusline
# or, from a clone:
cargo install --path .
```

Then point Claude Code at the binary in `~/.claude/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "claude-statusline"
  }
}
```

Use an absolute path (e.g. `~/.cargo/bin/claude-statusline`) if `~/.cargo/bin`
is not on the status line's `PATH`.

## Configuration (environment variables)

| Variable | Effect |
|---|---|
| `CLAUDE_STATUSLINE_ASCII=1` (or `NERD_FONT=0`) | ASCII labels instead of Nerd-Font glyphs (colors kept) |
| `CLAUDE_STATUSLINE_NO_DAILY=1` | Skip the transcript scan (drops the `today $…` figure) |
| `CLAUDE_STATUSLINE_CN_TTL=<secs>` | Chronis task cache TTL (default `8`) |
| `CLAUDE_STATUSLINE_CN_BIN=<path>` | Explicit path to the `cn` binary |

## Design guarantees

- **Never blank / never panics.** Any stdin — empty, truncated, non-JSON,
  wrong-typed — still prints one non-empty line and exits `0` (Claude Code
  blanks the status line on empty stdout or a non-zero exit).
- **Never hangs.** The `cn` lookup is wall-clock bounded (≤800 ms) and cached
  per-cwd; the daily-cost scan is cached (60 s) and only reads today's files.
- **No heavy deps.** `serde` + `serde_json` only. Warm renders are single-digit
  milliseconds.

## The Nerd Font

The powerline glyphs need a [Nerd Font](https://www.nerdfonts.com/) in your
terminal (e.g. *MesloLGS NF*). Without one, set `CLAUDE_STATUSLINE_ASCII=1` for
a plain-text fallback.

## License

MIT © decebal
