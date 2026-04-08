# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

- **Dev (desktop app with hot reload):** `npm run tauri dev`
- **Build desktop app:** `npm run tauri build`
- **Frontend-only dev server:** `npm run dev` (Vite on port 1420 — Tauri expects this exact port, see `vite.config.ts` / `tauri.conf.json`)
- **Frontend type-check + bundle:** `npm run build` (runs `tsc` then `vite build`)
- **Rust unit tests:** `cargo test` from `src-tauri/` (fixtures live in `src-tauri/tests/fixtures/*.jsonl`)
- **Run a single Rust test:** `cargo test --manifest-path src-tauri/Cargo.toml <test_name>` (e.g. `busy_after_tool_result`)

If `tauri build` fails with `cargo metadata` / `No such file or directory`, Rust is not on PATH — install via rustup and run `source ~/.cargo/env`. On macOS also ensure `xcode-select --install`.

## Architecture

This is a Tauri 2 desktop app: **Rust backend** in `src-tauri/src/`, **React 19 + Vite + TypeScript frontend** in `src/`. They communicate only via Tauri `invoke` commands and a single Tauri event.

### Data source (critical context)

The app **only reads local files** written by the Claude Code CLI. It never calls Anthropic's API or talks to the Claude Desktop app. All state is derived from:

- `~/.claude/projects/<encoded-project>/*.jsonl` and `~/.claude/projects/<encoded-project>/sessions/*.jsonl` — session transcripts (JSON lines, one record per line)
- `~/.claude/claw-and-order-hook.jsonl` — optional sidecar where external hooks can append `{"sessionId","needsInput","ts"}` lines; latest line per `sessionId` wins

The **session id is the JSONL filename stem** (e.g. `abc-123.jsonl` → `abc-123`). One file = one row in the sidebar.

### Backend layout (`src-tauri/src/`)

- `lib.rs` — registers Tauri commands (`get_projects_root`, `list_sessions`, `read_session_transcript`, `focus_session_terminal`) and spawns the filesystem watcher in `setup`.
- `session.rs` — session discovery, JSONL reading, state derivation, hook merging, and filesystem watcher. Owns all business logic for what the sidebar shows.
- `terminal.rs` — macOS-specific best-effort logic to focus the host terminal/editor that has a given session file open (or open a new Terminal window at the session's cwd).
- `main.rs` — thin entry point calling `claw_and_order_lib::run()`.

### Two read strategies (important when editing `session.rs`)

1. **Sidebar listing** uses `read_tail_text`, which reads only the last `TAIL_BYTES` (192 KiB) of each file so that listing many sessions stays cheap. The first partial line is dropped when the file is larger than the tail.
2. **Detail pane** uses `read_full_capped`, which reads the entire file but errors out above `READ_FULL_MAX_BYTES` (15 MiB) rather than loading it.

Both paths feed `parse_jsonl_lines`, which trims, `serde_json::from_str`s each non-empty line, and **silently skips malformed lines** — important invariant for heuristics that iterate the result.

### Status heuristic (`derive_state` + `last_significant_record`)

Status (`busy` | `idle` | `unknown`) and `completed` are **inferred from the last "significant" record** in the parsed tail, skipping `file-history-snapshot` entries and meta user records (`isMeta: true`). Rough rules:

- `result` → idle + completed
- non-meta `user`, `tool_result`, or any record with `toolUseResult` → busy (`busy_since = record timestamp`)
- `assistant` with a `tool_use` content block → busy; assistant text-only → idle
- `summary` → idle
- anything else → unknown

`busy_since` drives the sidebar timer, so it comes from the transcript and is **not** a real process clock. Timers are approximate and can differ from the Claude TUI.

When the hook sidecar reports `needsInput: true` for a session, `build_summary` overrides status to `idle` and sets `needs_your_attention`. `needs_your_attention` is also true when a session is idle but not completed (i.e. waiting on the user to type).

### Live updates

`spawn_watcher` runs a background thread that watches `~/.claude/` **recursively** via `notify-debouncer-mini` (280ms debounce). On any change it emits the Tauri event `sessions-changed`. `src/App.tsx` listens for that event and re-invokes `list_sessions` (and bumps a counter that re-reads the open transcript). There is no polling of session files.

### Frontend (`src/App.tsx`)

`App.tsx` is a single-file React app with no routing or state library. It holds: the session list, selected path, transcript rows, a 1-second `tick` for timers, and a `transcriptBump` counter incremented on `sessions-changed` to force transcript refetches.

`TranscriptEntry` is a large switch on `row.type` that renders `user`, `assistant` (text / thinking / tool_use blocks), `tool_result`, `system`, `summary`, `result`, and a raw fallback. It is wrapped in `TranscriptRowErrorBoundary` per row because transcript payloads come from arbitrary JSON and individual rows may throw during render — **do not** let render errors bubble up and break the whole transcript.

`contentToDisplayString` exists because tool/user content can be a string, a number, or an array of structured blocks; React cannot render raw objects, so anything non-primitive must be normalized to a string before being placed in JSX.

### Tauri command contract

TypeScript `SessionSummary` in `src/App.tsx` must stay in sync with the Rust `SessionSummary` in `src-tauri/src/session.rs` (serde derives `Serialize` with default snake_case field names, which the TS interface mirrors). When adding a field, update both sides.

The only Tauri commands are:

| Command | Purpose |
|---|---|
| `get_projects_root` | Returns `~/.claude/projects` as a string or `null` |
| `list_sessions` | Scans projects, tail-reads each JSONL, merges hook hints, returns `Vec<SessionSummary>` sorted by mtime desc |
| `read_session_transcript` | Full capped read + parse for the detail pane |
| `focus_session_terminal` | macOS-only: try to activate the terminal/editor hosting the session, else open a new Terminal at cwd |

### `focus_session_terminal` strategy (macOS only)

`terminal.rs` tries, in order: (1) `lsof` on the JSONL file → PIDs → walk ancestors to find a known host app; (2) any process whose **command line contains the session id** (filename stem); (3) process **named exactly `claude`** whose **cwd matches** the session's cwd; (4) wider Claude-like heuristics filtered to exclude this app, Vite, Cursor's **claw-and-order** (or legacy **claude-manager**) workspace; (5) `pgrep` fallback to at least activate Terminal; (6) open a new Terminal window at cwd. It can only **activate** the host app — it cannot focus a specific tab. This only works when macOS exposes cmdline/cwd to the app's user.

Note: `terminal.rs` contains `// #region agent log` blocks that append JSON to a hard-coded debug log path. Those are scaffolding from a previous debugging session — be aware they exist when editing, and prefer not adding new hard-coded absolute paths.

### Test fixtures

Rust unit tests at the bottom of `session.rs` load fixtures from `src-tauri/tests/fixtures/*.jsonl` via `include_str!`. When changing heuristics in `derive_state`/`last_significant_record`/`first_title`, update or add a fixture rather than mocking `Value` trees inline.
