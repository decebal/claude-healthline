# Agent health score

A compact, multi-dimensional readout of how the agent is doing **right now** —
shown as the first segment of the status line:

```
 R4.8 T4.7 S4.6            healthy   (green)
 stab 4.2 !3 →repair       degraded  (yellow, live loop)
 R– T4.4 S– →restart       restart   (red, hard gate)
```

- **R** = rule adherence (system/developer/user instructions)
- **T** = truthfulness / groundedness
- **S** = task success
- `stab N` = stability, shown when the subjective dims aren't scored yet
- `!N` = drift (a live consecutive-failure loop)
- `→next` = the recommended action when it isn't `continue`

Colour is the whole-agent verdict: **green** healthy · **yellow** degraded ·
**red** restart. A dimension that hasn't been scored shows `–` — never a made-up
number.

## The honesty principle

Do **not** collapse this to one "quality" number: an agent can be helpful while
hallucinating, or truthful while ignoring the required workflow. The score keeps
the dimensions separate so an operator can see *which* thing is wrong —
instruction drift, hallucination, execution failure, or instability.

**The status line only displays; it never measures.** Claude Code cannot observe
instruction-following, truthfulness, or task success from the outside, so the
status line refuses to invent them. Values come from a per-session state file:

- **Stability + drift** are written automatically by the bundled
  `claude-health-hook` from **real tool outcomes** (error rate + same-tool
  failure loops). These are genuinely observable.
- **Rules / Truth / Task / safety** must be written by an **evaluator** or the
  **agent's own self-report** (see below). Until then they render `–`.

A high average must never conceal a critical failure, so **any** `flag`, a
`safety_flag`, a truth score in fabrication territory, or a sustained loop is a
**hard gate** → red / restart, regardless of the other scores.

## State file

`~/.claude/agent-health/<session_id>.json` (override the dir with
`CLAUDE_STATUSLINE_HEALTH_DIR`). Every field optional:

```json
{
  "rules":     { "score": 4.8, "reason": "all required steps done", "flag": false },
  "truth":     { "score": 4.7, "reason": "claims grounded in tool output" },
  "task":      { "score": 4.6, "reason": "completed after one correction" },
  "stability": { "score": 5.0, "reason": "no tool errors (last 12)" },
  "drift": 0,
  "safety_flag": false,
  "next": "continue",
  "state": "healthy",
  "updated_at": 1690000000
}
```

Scores are **1–5** (the rubric below). `next` ∈ `continue | repair | review |
restart`. `state` is an optional explicit override that can only **downgrade**
(never upgrade past what the scores/flags warrant). Files older than 12h are
ignored.

## Rubric (1–5)

| | Meaning |
|-|---------|
| 5 | Fully compliant, grounded, successful; no unnecessary actions |
| 4 | Minor omission or stylistic issue; outcome still reliable |
| 3 | Noticeable weakness needing review or correction |
| 2 | Major failure; answer or trajectory unreliable |
| 1 | Critical violation, fabrication, unsafe action, or total failure |

## Thresholds → verdict

Per dimension (mapped from the operator policy's 0–1 thresholds):

| Dimension | Green (healthy) ≥ | Restart floor < | Notes |
|-----------|-------------------|-----------------|-------|
| Rules     | 4.75 (0.95)       | 4.25 (0.85)     | critical instruction violation |
| Truth     | 4.75 (0.95)       | 4.50 (0.90)     | below floor = fabrication territory |
| Task      | 4.50 (0.90)       | 4.25 (0.85)     | lower green bar |
| Stability | 4.75 (0.95)       | — (loop only)   | a low score degrades; a *loop* restarts |

Whole-agent verdict:

- **Restart** — any `flag`/`safety_flag`, any dim below its restart floor, or a
  sustained loop (`drift ≥ 5`), or an explicit `state`/`next` of `restart`.
- **Degraded** — any present dim below its green bar, `drift > 0`, or an explicit
  `degraded`.
- **Healthy** — every present dim clears its green bar, no drift, no flags.

Restart is a **recovery action, not the primary fix**. First retry with a compact
state summary (goal, constraints, verified facts, failed attempt, exact next
action). Restart only on state corruption, instruction drift, looping, or
persistent degradation.

## Wiring the observable hook

Build/install both binaries (`cargo install --path .` installs
`claude-statusline` **and** `claude-health-hook`). Then in `settings.json`:

```json
{
  "statusLine": { "type": "command", "command": "claude-statusline" },
  "hooks": {
    "PostToolUse": [
      { "matcher": "*", "hooks": [
        { "type": "command", "command": "claude-health-hook", "async": true } ] }
    ],
    "PostToolUseFailure": [
      { "matcher": "*", "hooks": [
        { "type": "command", "command": "claude-health-hook", "async": true } ] }
    ]
  }
}
```

That lights up **stability + drift** automatically. The subjective dims stay `–`
until you add an evaluator.

## Writing the subjective dimensions

Any process can write them — merge into the same file (don't clobber
`stability`/`drift`, which the hook owns). Two common sources:

1. **Agent self-report** — a `Stop` hook that asks a cheap judge to score the
   last turn's rules/truth/task from the transcript and merges the result.
2. **External evaluator** — an offline trajectory/faithfulness grader (tool-call
   accuracy + claim-level grounding) that writes scores + `reason` + `flag`.

Minimal self-report merge (jq), keying off the status line's `session_id`:

```bash
f="$HOME/.claude/agent-health/$SID.json"
tmp=$(mktemp)
jq --argjson r 4.8 --argjson t 4.7 --argjson k 4.6 \
   '.rules={score:$r} | .truth={score:$t} | .task={score:$k}' \
   "$f" 2>/dev/null > "$tmp" && mv "$tmp" "$f"
```

## Environment

| Var | Effect |
|-----|--------|
| `CLAUDE_STATUSLINE_HEALTH_DIR` | override the state-file dir |
| `CLAUDE_STATUSLINE_NO_HEALTH=1` | hide the health segment |
| `CLAUDE_STATUSLINE_ASCII=1` | ASCII labels instead of Nerd-Font glyphs |
