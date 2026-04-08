//! Discover Claude Code JSONL sessions, read tails, derive busy/idle heuristics.

use serde::Serialize;
use serde_json::Value;
use tauri::Emitter;
use std::collections::HashMap;
use std::fs::{metadata, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use crate::terminal;
const TAIL_BYTES: u64 = 192 * 1024;
const READ_FULL_MAX_BYTES: u64 = 15 * 1024 * 1024;
/// Fallback: if process detection fails (common on macOS permissions),
/// consider a session active if its JSONL was modified recently.
const ACTIVE_RECENT_FALLBACK_MS: i64 = 6 * 60 * 60 * 1000; // 6 hours
/// Extra fallback for sessions whose status parses as `unknown` but are truly active.
const ACTIVE_VERY_RECENT_MS: i64 = 30 * 60 * 1000; // 30 minutes

pub fn claude_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude"))
}

pub fn claude_projects_dir() -> Option<PathBuf> {
    claude_dir().map(|p| p.join("projects"))
}

/// Optional hook sidecar: append JSON lines `{ "sessionId", "needsInput", "ts" }`.
pub fn hook_sidecar_path() -> Option<PathBuf> {
    claude_dir().map(|p| p.join("claw-and-order-hook.jsonl"))
}

/// All `*.jsonl` under `projects/<key>/*.jsonl` and `projects/<key>/sessions/*.jsonl`.
pub fn enumerate_session_files(projects_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !projects_root.is_dir() {
        return files;
    }
    let Ok(entries) = std::fs::read_dir(projects_root) else {
        return files;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }
        push_jsonl_in_dir(&project_dir, &mut files);
        let sessions = project_dir.join("sessions");
        if sessions.is_dir() {
            push_jsonl_in_dir(&sessions, &mut files);
        }
    }
    files.sort();
    files.dedup();
    files
}

fn push_jsonl_in_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

pub fn read_tail_text(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        if let Some(i) = s.find('\n') {
            s.drain(..=i);
        }
    }
    Ok(s)
}

pub fn read_full_capped(path: &Path) -> std::io::Result<String> {
    let len = metadata(path)?.len();
    if len > READ_FULL_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("session file too large ({} bytes)", len),
        ));
    }
    let mut f = File::open(path)?;
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    Ok(s)
}

pub fn parse_jsonl_lines(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            out.push(v);
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Busy,
    Idle,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct DerivedState {
    pub status: SessionStatus,
    pub busy_since: Option<String>,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub path: String,
    pub session_id: String,
    pub project_label: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub status: SessionStatus,
    pub busy_since: Option<String>,
    pub completed: bool,
    pub mtime_ms: i64,
    /// True if optional hook sidecar says this session needs user action.
    pub needs_input_hook: bool,
    /// Best-effort: waiting on you (idle turn) or hook-reported prompt/permission.
    pub needs_your_attention: bool,
}

fn record_type(v: &Value) -> Option<&str> {
    v.get("type").and_then(|t| t.as_str())
}

fn is_snapshot(v: &Value) -> bool {
    record_type(v) == Some("file-history-snapshot")
}

fn is_meta_user(v: &Value) -> bool {
    v.get("isMeta").and_then(|x| x.as_bool()) == Some(true)
}

fn is_tool_result_like(v: &Value) -> bool {
    if record_type(v) == Some("tool_result") {
        return true;
    }
    v.get("toolUseResult").is_some()
}

fn message_content_array(v: &Value) -> Option<&Value> {
    v.pointer("/message/content").or_else(|| v.get("content"))
}

fn assistant_has_tool_use(v: &Value) -> bool {
    let Some(content) = message_content_array(v) else {
        return false;
    };
    if let Some(arr) = content.as_array() {
        return arr.iter().any(|b| {
            b.get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t == "tool_use")
        });
    }
    false
}

fn timestamp_str(v: &Value) -> Option<String> {
    v.get("timestamp")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
}

/// Last record that drives busy/idle (skips snapshots and meta user lines).
pub fn last_significant_record(values: &[Value]) -> Option<&Value> {
    for v in values.iter().rev() {
        if is_snapshot(v) {
            continue;
        }
        let Some(t) = record_type(v) else {
            continue;
        };
        if t == "user" && is_meta_user(v) {
            continue;
        }
        let _ = t;
        return Some(v);
    }
    None
}

pub fn derive_state(values: &[Value]) -> DerivedState {
    let Some(last) = last_significant_record(values) else {
        return DerivedState {
            status: SessionStatus::Unknown,
            busy_since: None,
            completed: false,
        };
    };
    let t = record_type(last).unwrap_or("");

    if t == "result" {
        return DerivedState {
            status: SessionStatus::Idle,
            busy_since: None,
            completed: true,
        };
    }

    if t == "tool_result" || is_tool_result_like(last) {
        return DerivedState {
            status: SessionStatus::Busy,
            busy_since: timestamp_str(last),
            completed: false,
        };
    }

    if t == "user" {
        return DerivedState {
            status: SessionStatus::Busy,
            busy_since: timestamp_str(last),
            completed: false,
        };
    }

    if t == "assistant" {
        if assistant_has_tool_use(last) {
            // Model finished this turn with a tool_use block; the transcript pauses until the
            // tool runs (often after user permission). Treat as idle so we don't show "working"
            // while the CLI is waiting on approval or execution.
            return DerivedState {
                status: SessionStatus::Idle,
                busy_since: None,
                completed: false,
            };
        }
        return DerivedState {
            status: SessionStatus::Idle,
            busy_since: None,
            completed: false,
        };
    }

    if t == "summary" {
        return DerivedState {
            status: SessionStatus::Idle,
            busy_since: None,
            completed: false,
        };
    }

    DerivedState {
        status: SessionStatus::Unknown,
        busy_since: timestamp_str(last),
        completed: false,
    }
}

fn extract_user_text(v: &Value) -> Option<String> {
    let content = v.pointer("/message/content")?;
    if let Some(s) = content.as_str() {
        return Some(s.trim().to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                    out.push_str(t);
                }
            }
        }
        let out = out.trim().to_string();
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

pub fn first_title(values: &[Value]) -> Option<String> {
    for v in values {
        if is_snapshot(v) {
            continue;
        }
        if record_type(v) != Some("user") {
            continue;
        }
        if is_meta_user(v) || is_tool_result_like(v) {
            continue;
        }
        if let Some(text) = extract_user_text(v) {
            let title: String = text.chars().take(80).collect();
            if !title.is_empty() {
                return Some(title);
            }
        }
    }
    None
}

fn session_id_from_path(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn project_label_for_path(projects_root: &Path, path: &Path) -> String {
    let mut cur = path.parent();
    if cur.and_then(|p| p.file_name()) == Some(std::ffi::OsStr::new("sessions")) {
        cur = cur.and_then(|p| p.parent());
    }
    if let Some(parent) = cur {
        if parent.starts_with(projects_root) {
            return parent
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("project")
                .to_string();
        }
    }
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string()
}

fn cwd_from_values(values: &[Value]) -> Option<String> {
    for v in values.iter().rev() {
        if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
            return Some(c.to_string());
        }
    }
    None
}

/// Latest hook hint per session id (last line wins).
pub fn load_hook_hints(path: &Option<PathBuf>) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    let Some(p) = path else {
        return map;
    };
    if !p.is_file() {
        return map;
    }
    let Ok(text) = read_tail_text(p) else {
        return map;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(sid) = v.get("sessionId").and_then(|s| s.as_str()) else {
            continue;
        };
        let needs = v
            .get("needsInput")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        map.insert(sid.to_string(), needs);
    }
    map
}

pub fn build_summary(
    projects_root: &Path,
    path: &Path,
    hook_needs_input: Option<bool>,
) -> Option<SessionSummary> {
    let meta = metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let mtime_ms = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let tail = read_tail_text(path).ok()?;
    let values = parse_jsonl_lines(&tail);
    let mut derived = derive_state(&values);
    let needs_input_hook = hook_needs_input.unwrap_or(false);
    if needs_input_hook {
        derived.status = SessionStatus::Idle;
    }
    let needs_your_attention = needs_input_hook
        || (matches!(derived.status, SessionStatus::Idle) && !derived.completed);

    Some(SessionSummary {
        path: path.to_string_lossy().to_string(),
        session_id: session_id_from_path(path),
        project_label: project_label_for_path(projects_root, path),
        cwd: cwd_from_values(&values),
        title: first_title(&values),
        status: derived.status,
        busy_since: derived.busy_since,
        completed: derived.completed,
        mtime_ms,
        needs_input_hook,
        needs_your_attention,
    })
}

pub fn list_all_summaries(projects_root: &Path) -> Vec<SessionSummary> {
    let hook_path = hook_sidecar_path();
    let hints = load_hook_hints(&hook_path);
    let mut out = Vec::new();
    for path in enumerate_session_files(projects_root) {
        let sid = session_id_from_path(&path);
        let hook = hints.get(&sid).copied();
        if let Some(s) = build_summary(projects_root, &path, hook) {
            out.push(s);
        }
    }
    // Only show "active" sessions in the sidebar:
    // - always include sessions explicitly flagged by the optional hook
    // - otherwise include sessions that are not completed, are plausibly in-progress
    //   (busy or waiting on you), and appear to have a live backing process
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    out.retain(|s| {
        if s.needs_input_hook {
            return true;
        }
        if s.completed {
            return false;
        }
        let seems_active = terminal::session_seems_active(Path::new(&s.path), s.cwd.as_deref());
        if seems_active {
            return true;
        }

        let age_ms = now_ms.saturating_sub(s.mtime_ms);
        if age_ms >= ACTIVE_RECENT_FALLBACK_MS {
            return false;
        }

        // If status parsing is confident (busy / waiting), accept with the recency fallback.
        if matches!(s.status, SessionStatus::Busy) || s.needs_your_attention {
            return true;
        }

        // If status parsing is *not* confident (unknown), only accept if it's very recent.
        age_ms < ACTIVE_VERY_RECENT_MS
    });
    // Dedupe by (project_label, cwd) to avoid showing multiple JSONLs for the
    // same working session/project; keep the most recently modified.
    let mut by_key: HashMap<String, SessionSummary> = HashMap::new();
    for s in out.into_iter() {
        let key = format!("{}|{}", s.project_label, s.cwd.as_deref().unwrap_or(""));
        match by_key.get(&key) {
            Some(prev) if prev.mtime_ms >= s.mtime_ms => {}
            _ => {
                by_key.insert(key, s);
            }
        }
    }
    let mut out: Vec<SessionSummary> = by_key.into_values().collect();
    out.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
    out
}

pub fn spawn_watcher(app_handle: tauri::AppHandle, watch_root: PathBuf) {
    std::thread::spawn(move || {
        let (tx, rx) = mpsc::channel();
        let mut debouncer = match notify_debouncer_mini::new_debouncer(
            Duration::from_millis(280),
            move |res: notify_debouncer_mini::DebounceEventResult| {
                if matches!(&res, Ok(events) if !events.is_empty()) {
                    let _ = tx.send(());
                }
            },
        ) {
            Ok(d) => d,
            Err(_) => return,
        };
        if debouncer
            .watcher()
            .watch(&watch_root, notify::RecursiveMode::Recursive)
            .is_err()
        {
            return;
        }
        loop {
            if rx.recv().is_err() {
                break;
            }
            let _ = app_handle.emit("sessions-changed", ());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(name: &str) -> Vec<Value> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        parse_jsonl_lines(&text)
    }

    #[test]
    fn idle_after_assistant_text_only() {
        let v = fixture("idle_after_assistant.jsonl");
        let d = derive_state(&v);
        assert_eq!(d.status, SessionStatus::Idle);
        assert!(!d.completed);
    }

    #[test]
    fn busy_after_user_prompt() {
        let v = fixture("busy_after_user.jsonl");
        let d = derive_state(&v);
        assert_eq!(d.status, SessionStatus::Busy);
    }

    #[test]
    fn busy_after_tool_result() {
        let v = fixture("busy_after_tool_result.jsonl");
        let d = derive_state(&v);
        assert_eq!(d.status, SessionStatus::Busy);
    }

    #[test]
    fn idle_when_last_is_assistant_tool_use_pending() {
        let v = fixture("idle_pending_tool_use.jsonl");
        let d = derive_state(&v);
        assert_eq!(d.status, SessionStatus::Idle);
        assert!(!d.completed);
    }

    #[test]
    fn completed_on_result() {
        let v = fixture("session_completed.jsonl");
        let d = derive_state(&v);
        assert_eq!(d.status, SessionStatus::Idle);
        assert!(d.completed);
    }

    #[test]
    fn title_from_first_user() {
        let v = fixture("idle_after_assistant.jsonl");
        assert!(first_title(&v).unwrap().contains("validation"));
    }
}
