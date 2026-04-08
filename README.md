# Claw & Order

Desktop app (Tauri + React) that lists Claude Code sessions from `~/.claude/projects/`, shows a best-effort busy/idle state and timer from each session’s JSONL tail, and mirrors the transcript in the main pane.

## Install the desktop app

Use this path if you only want to **run** Claw & Order. You need Claude Code sessions on disk at `~/.claude/projects/` for the list to show anything (see [Session file locations](#session-file-locations)).

### macOS

**From a release (simplest, when available)**  
Download the latest `.dmg` or packaged `.app` from this repository’s **Releases** page (if the maintainer publishes builds there). Open the DMG and drag **Claw & Order** into **Applications**, or copy the `.app` into **Applications**.

**Build and install from source**  
From a clone of the repo, with [Rust](https://www.rust-lang.org/tools/install), [Tauri prerequisites](https://tauri.app/start/prerequisites/), and Node.js 20+ installed:

```bash
npm install
npm run tauri build
```

After the build finishes, the app bundle is usually at:

`src-tauri/target/release/bundle/macos/Claw & Order.app`

Drag that into **Applications** (or copy it there). If a **`.dmg`** was produced under `src-tauri/target/release/bundle/dmg/`, open it and drag the app into **Applications** the usual way.

**First launch on macOS**  
The first time you open a locally built or unsigned app, macOS may block it until you **right‑click → Open** once, or allow it under **System Settings → Privacy & Security**.

### Windows and Linux

After `npm run tauri build`, installers appear under `src-tauri/target/release/bundle/` (for example `.msi` / NSIS on Windows, `.deb` or AppImage on Linux, depending on your Tauri bundle targets). Install the artifact for your platform like any other app.

## Development (build and run from source)

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) and platform [Tauri prerequisites](https://tauri.app/start/prerequisites/)
- Node.js 20+

### Quickstart

```bash
git clone <your-repo-url>
cd claw-and-order
npm install
npm run tauri dev
```

### If `tauri build` fails with `cargo metadata` / `No such file or directory (os error 2)`

Rust’s `cargo` is missing or not on your `PATH`. Install Rust (macOS/Linux):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then open a **new** terminal (or run `source ~/.cargo/env`) and confirm:

```bash
cargo --version
```

On macOS you may also need Xcode Command Line Tools: `xcode-select --install`.

### Other useful commands

- **Frontend-only dev server**: `npm run dev` (Vite on `http://localhost:1420`; Tauri expects this exact port)
- **Production desktop bundle**: `npm run tauri build`
- **Type-check + bundle**: `npm run build`
- **Rust tests**: `cargo test --manifest-path src-tauri/Cargo.toml`

## Session file locations

- `~/.claude/projects/<encoded-project>/*.jsonl`
- `~/.claude/projects/<encoded-project>/sessions/*.jsonl`

This app is **offline**: it only reads local files written by the Claude Code CLI (no Anthropic API calls).

Status and timers are **approximate** (transcript-based, not the Claude TUI).

## Sidebar “terminal” button (macOS)

The app tries, in order: `lsof` on the session file; any process whose **command line contains the session id** (from the JSONL filename); any **Claude-related process** whose **cwd** matches the session. It then walks the process tree to **activate** Terminal, iTerm, Warp, Cursor, etc.

That only works if the OS exposes command line and cwd to the app (same user is fine). It **cannot** select a specific tab—only bring the host app forward. If nothing matches, it opens a **new** Terminal window at the project folder.

### macOS permissions note

Because this feature uses AppleScript (`osascript`) to activate Terminal/iTerm/etc., macOS may prompt for **Automation** permission the first time (System Settings → Privacy & Security → Automation). If you deny it, the button may fail to focus the terminal and will fall back to opening a new Terminal window.

### Debugging without huge `ps` output

Avoid `ps eww` (it dumps every process environment and is enormous). Use something small:

```bash
# Actual Claude Code CLI (process name is exactly "claude") — small output
pgrep -x -l claude 2>/dev/null

# Wider search (noisy: matches this repo name, Cursor hosts, this app, etc.)
pgrep -fl claude 2>/dev/null | head -25

# Does the session id from the JSONL filename appear in any command line?
# Replace UUID with the stem of your session file (e.g. 7cbbb42c-6fd5-43cf-98ea-b16359a4ce9a)
ps ax -o pid=,comm=,args= 2>/dev/null | grep -F "PASTE-SESSION-UUID-HERE" | head -10
```

## Optional hook sidecar (Phase 2)

Append one JSON object per line to `~/.claude/claw-and-order-hook.jsonl` (latest line per `sessionId` wins):

```json
{"sessionId":"<uuid>","needsInput":true,"ts":"2026-04-01T12:00:00.000Z"}
```

If you previously used `claude-manager-hook.jsonl`, rename that file to `claw-and-order-hook.jsonl` (or merge the contents).

Use a Claude Code hook or script to write this when the user must approve a tool or answer a prompt. The app watches `~/.claude/` and merges `needsInput` into the sidebar.
