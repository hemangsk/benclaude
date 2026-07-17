# benclaude

A live outcome-analytics side pane for [Claude Code](https://claude.com/claude-code).
Runs in a narrow terminal split beside your session, tails the transcript
read-only, and annotates what the agent is doing with 30-day history from
past sessions of the same project.

```
┌ LIVE TURN ─────────────────────────┐
│ turn 14 · 3m 12s          8.2k tok │
│ └ Bash  flutter analyze — running… │
│ 12:41:56  Edit  profile_tab.dart   │
├ FILES THIS TURN ───────────────────┤
│ ⚠ profile_tab.dart — 4 edits /30d  │
├ SESSION ── HISTORY · 30D ── ATTN ──┤
│ tokens · follow-ups · babysit gap  │
│ sessions · tok/day ▂▄▆▅▇█ · most…  │
│ ⏳ waiting on you — 2m 40s          │
└────────────────────────────────────┘
```

## Install

```sh
cargo install --path .
```

## Use

```sh
# in a split next to Claude Code (tmux example):
tmux split-window -h -l 50 'benclaude watch'

# or from any project directory:
benclaude watch
benclaude watch --project ~/code/my-app

# sanity check without the TUI:
benclaude doctor
```

Keys: `q` quit · `r`/`h`/`s` reserved for the v0.2 report/heatmap/sessions views.

## How it works

Claude Code appends each session's transcript to
`~/.claude/projects/<sanitized-cwd>/<session-id>.jsonl` while it runs.
benclaude polls that file (250 ms), only ever consuming complete lines, and
folds the events into the live view. At startup it scans the project's last
30 days of transcripts once for the history annotations (per-file churn,
follow-ups per session, tokens per day, babysit time).

benclaude never writes to, locks, or moves transcript files.

## Roadmap

- v0.2 — git join: link sessions to commits, line-survival rate, per-file
  friction heatmap, `benclaude report`.
