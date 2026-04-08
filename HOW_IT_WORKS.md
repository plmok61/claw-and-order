# How Claw & Order works

This document describes how the app obtains and interprets session data. It does **not** call Anthropic’s APIs or integrate with the Claude Desktop app. Everything comes from **files Claude Code writes on your machine**.

## Stack

- **Backend:** Rust (Tauri), in `src-tauri/`
- **UI:** React, in `src/`
- **IPC:** Tauri commands (`invoke`) and a single global event for refresh

## Where the data lives

| Location | Role |
|----------|------|
| `~/.claude/projects/<encoded-project>/*.jsonl` | Session transcripts (JSON lines) |
| `~/.claude/projects/<encoded-project>/sessions/*.jsonl` | Same, nested under `sessions/` |
| `~/.claude/claw-and-order-hook.jsonl` | **Optional** sidecar: latest line per `sessionId` can set `needsInput` |

The app resolves `~/.claude` via `dirs::home_dir()` and scans for `*.jsonl` in the two directory patterns above. Implementation: `claude_dir`, `claude_projects_dir`, and `enumerate_session_files` in `src-tauri/src/session.rs`.

## Session identity

Each session file is treated as one session. The **session id** is the JSONL file’s **stem** (filename without extension), e.g. `abc-123.jsonl` → `abc-123`. See `session_id_from_path` in `session.rs`.

## Two ways the app reads JSONL

1. **Sidebar summaries** — For each file, the backend reads only the **last ~192 KiB** of the file (`TAIL_BYTES`), parses newline-delimited JSON, and derives status from that tail. This keeps listing many sessions cheap.
2. **Main transcript pane** — When you select a session, the UI asks for the **full file** up to a cap (`READ_FULL_MAX_BYTES`, 15 MiB). Oversized files return an error instead of loading.

Parsing is line-by-line JSON (`parse_jsonl_lines`); malformed lines are skipped.

## Busy / idle / completed (heuristic)

Status is **not** read from Claude’s TUI or Desktop. It is inferred from the **last “significant” JSON record** in the parsed tail, after skipping `file-history-snapshot` and meta user lines (`last_significant_record`, then `derive_state`).

Rough rules:

- **`result`** → idle and **completed**
- **`user`** (non-meta) → **busy** (model should respond)
- **`tool_result`** / lines with `toolUseResult` → **busy** (tool cycle)
- **`assistant`** with tool-use blocks in content → **busy**
- **`assistant`** with only text (no tool_use) → **idle**
- **`summary`** → **idle**
- Otherwise → **unknown** when needed

`busy_since` is taken from the driving record’s `timestamp` when status is busy. Timers are therefore **approximate** and based on transcript shape, not a live process handle.

## Optional hook sidecar

If present, `~/.claude/claw-and-order-hook.jsonl` is read for **latest line per `sessionId`**. If that line sets `needsInput: true`, the UI treats that session as needing attention and can force derived status toward “waiting on you.” Intended for hooks or scripts that know when the user must approve a tool or answer a prompt. See `load_hook_hints` and `build_summary` in `session.rs`.

## Live updates

On startup, a background thread watches **`~/.claude/` recursively** with debounced filesystem notifications (`notify` + `notify-debouncer-mini`). When anything under that tree changes, the backend emits a Tauri event **`sessions-changed`**. The React app listens and refetches the session list (and transcript if needed). Implementation: `spawn_watcher` in `session.rs`, wired from `src-tauri/src/lib.rs` `setup`.

## Tauri surface

Defined in `src-tauri/src/lib.rs`:

| Command | Purpose |
|---------|---------|
| `get_projects_root` | Returns `~/.claude/projects` as a string (or `null`) |
| `list_sessions` | Builds `Vec<SessionSummary>` via `list_all_summaries` |
| `read_session_transcript` | Full capped read + `parse_jsonl_lines` for the detail view |

The UI calls these via `@tauri-apps/api/core` `invoke` in `src/App.tsx`.

## Key files

| File | Responsibility |
|------|------------------|
| `src-tauri/src/session.rs` | Discovery, tail/full reads, JSONL parse, heuristics, hook merge, watcher |
| `src-tauri/src/lib.rs` | Tauri commands, watcher startup |
| `src/App.tsx` | Invokes commands, listens for `sessions-changed`, renders sidebar + transcript |

Unit tests for transcript fixtures live at the bottom of `session.rs` and under `src-tauri/tests/fixtures/`.
