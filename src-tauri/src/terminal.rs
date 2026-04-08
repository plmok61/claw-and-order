//! Best-effort: focus the host app for an active Claude session (macOS), else open Terminal at cwd.
//!
//! Strategy: (1) `lsof` on the session file, (2) command line contains session id (filename stem),
//! (3) process **named** `claude` with matching cwd (real CLI), (4) other Claude-like heuristics
//! with noise filtered (excludes Cursor **claw-and-order** / legacy **claude-manager** workspace, this app, Vite, etc.),
//! (5) open a new Terminal window.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use sysinfo::{Pid, System};

/// Best-effort "is this session currently active?" check.
///
/// On macOS, we consider a session active if:
/// - any process currently has the session JSONL file open (`lsof -t <file>`), OR
/// - a running process command line contains the session id, OR
/// - a `claude` process is running in the session cwd.
///
/// This is intentionally approximate; OS visibility restrictions can cause false negatives.
pub fn session_seems_active(session_file: &Path, cwd: Option<&str>) -> bool {
    #[cfg(target_os = "macos")]
    {
        if !session_file.is_file() {
            return false;
        }

        if !lsof_pids(session_file).is_empty() {
            return true;
        }

        let mut sys = System::new();
        sys.refresh_all();

        if let Some(sid) = session_file.file_stem().and_then(|s| s.to_str()) {
            if sid.len() >= 8 && !pids_whose_cmdline_contains(&sys, sid).is_empty() {
                return true;
            }
        }

        if let Some(cwd) = cwd {
            let cwd_path = Path::new(cwd);
            if !pids_named_claude_in_cwd(&sys, cwd_path).is_empty() {
                return true;
            }
        }

        false
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = session_file;
        let _ = cwd;
        false
    }
}

pub fn focus_session_terminal(
    session_jsonl_path: String,
    cwd: Option<String>,
) -> Result<String, String> {
    let path = PathBuf::from(&session_jsonl_path);
    if !path.is_file() {
        return Err(format!("Session file not found: {}", session_jsonl_path));
    }

    let cwd_resolved = cwd.or_else(|| guess_cwd_from_session_path(&path));
    let cwd_str = cwd_resolved.as_deref().unwrap_or(".");

    #[cfg(target_os = "macos")]
    {
        let mut sys = System::new();
        sys.refresh_all();

        let lsof_n = lsof_pids(&path).len();

        if let Some(msg) = try_activate_via_lsof_macos(&path, &sys) {
            return Ok(msg);
        }

        if let Some(sid) = path.file_stem().and_then(|s| s.to_str()) {
            if sid.len() >= 8 {
                let pids = pids_whose_cmdline_contains(&sys, sid);
                if let Some(msg) = try_activate_from_pids_macos(&sys, pids.iter().copied()) {
                    return Ok(format!(
                        "{msg} Matched the session id in a running process command line."
                    ));
                }
            }
        }

        if let Some(ref cwd_path) = cwd_resolved {
            let cwd_path = Path::new(cwd_path);
            let pids = pids_named_claude_in_cwd(&sys, cwd_path);
            if let Some(msg) = try_activate_from_pids_macos(&sys, pids.iter().copied()) {
                return Ok(format!(
                    "{msg} Matched a `claude` process in the session working directory."
                ));
            }
            let pids = pids_single_claude_named(&sys);
            if let Some(msg) = try_activate_from_pids_macos(&sys, pids.iter().copied()) {
                return Ok(format!(
                    "{msg} Matched the only running `claude` process on this Mac (cwd could not be matched to session)."
                ));
            }
            let pids = pids_claude_like_in_cwd(&sys, cwd_path);
            if let Some(msg) = try_activate_from_pids_macos(&sys, pids.iter().copied()) {
                return Ok(format!(
                    "{msg} Matched a Claude-related process in the session working directory."
                ));
            }
        }

        // If sysinfo can't see processes/cwd/cmdline on this macOS build, fall back to `pgrep`
        // and then try to focus the correct Terminal window by TTY.
        let pgrep_pids = pgrep_claude_pids_macos();
        if !pgrep_pids.is_empty() {
            let chosen = choose_best_claude_pid_for_cwd(&pgrep_pids, &cwd_resolved);
            if let Some(pid) = chosen {
                let tty = tty_for_pid_macos(pid);
                if let Some(tty) = tty {
                    if focus_terminal_window_by_tty_macos(&tty) {
                        return Ok(format!(
                            "Focused Terminal window for {} (pgrep+tty).",
                            tty
                        ));
                    }
                }
            }
            if activate_macos_app("Terminal") {
                return Ok("Activated Terminal (pgrep fallback).".to_string());
            }
        }
        let _ = lsof_n;
    }

    open_terminal_at_cwd(cwd_str)
}

#[cfg(target_os = "macos")]
fn pgrep_claude_pids_macos() -> Vec<u32> {
    let Some(out) = Command::new("pgrep")
        .args(["-x", "claude"])
        .output()
        .ok()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

#[cfg(target_os = "macos")]
fn canonicalize_string_path(s: &str) -> Option<String> {
    fs::canonicalize(Path::new(s))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
fn cwd_for_pid_macos(pid: u32) -> Option<String> {
    cwd_from_lsof_macos(pid).map(|p| p.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
fn choose_best_claude_pid_for_cwd(pids: &[u32], cwd_resolved: &Option<String>) -> Option<u32> {
    if pids.is_empty() {
        return None;
    }
    if pids.len() == 1 {
        return Some(pids[0]);
    }
    let Some(target) = cwd_resolved.as_deref() else {
        return Some(pids[0]);
    };
    let target_canon = canonicalize_string_path(target).unwrap_or_else(|| target.to_string());
    for pid in pids {
        if let Some(cwd) = cwd_for_pid_macos(*pid) {
            let cwd_canon = canonicalize_string_path(&cwd).unwrap_or(cwd);
            if cwd_canon == target_canon {
                return Some(*pid);
            }
        }
    }
    Some(pids[0])
}

#[cfg(target_os = "macos")]
fn tty_for_pid_macos(pid: u32) -> Option<String> {
    let Some(out) = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "tty="])
        .output()
        .ok()
    else {
        return None;
    };
    if !out.status.success() {
        return None;
    }
    let tty = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if tty.is_empty() || tty == "??" {
        return None;
    }
    // Terminal AppleScript uses /dev/ttysXXX
    if tty.starts_with("/dev/") {
        Some(tty)
    } else {
        Some(format!("/dev/{}", tty))
    }
}

#[cfg(target_os = "macos")]
fn focus_terminal_window_by_tty_macos(tty: &str) -> bool {
    // First try native Terminal scripting (index=1 brings window to front within Terminal).
    let script = format!(
        "tell application \"Terminal\"\n\
            activate\n\
            set targetTTY to \"{tty}\"\n\
            repeat with w in windows\n\
                repeat with t in tabs of w\n\
                    try\n\
                        if (tty of t) is equal to targetTTY then\n\
                            set selected tab of w to t\n\
                            set index of w to 1\n\
                            return \"ok\"\n\
                        end if\n\
                    end try\n\
                end repeat\n\
            end repeat\n\
            return \"no\"\n\
        end tell",
        tty = tty.replace('\"', "")
    );
    let out = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .ok();
    if let Some(out) = out {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            return s == "ok";
        }
    }
    false
}

fn guess_cwd_from_session_path(session_file: &Path) -> Option<String> {
    let parent = session_file.parent()?;
    if parent.file_name().is_some_and(|n| n == "sessions") {
        return parent
            .parent()
            .map(|p| p.to_string_lossy().into_owned());
    }
    Some(parent.to_string_lossy().into_owned())
}

#[cfg(target_os = "macos")]
fn canonicalize_or_same(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(target_os = "macos")]
fn paths_same_dir(a: &Path, b: &Path) -> bool {
    canonicalize_or_same(a) == canonicalize_or_same(b)
}

/// `sysinfo::Process::cwd()` is often `None` on macOS for other processes; `lsof` still reports cwd.
#[cfg(target_os = "macos")]
fn cwd_from_lsof_macos(pid: u32) -> Option<PathBuf> {
    let output = Command::new("lsof")
        .args([
            "-a",
            "-p",
            &pid.to_string(),
            "-d",
            "cwd",
            "-Fn",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('n') {
            let p = rest.trim();
            if p.starts_with('/') {
                return Some(PathBuf::from(p));
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn process_cwd_macos(pid: Pid, proc: &sysinfo::Process) -> Option<PathBuf> {
    proc.cwd()
        .map(|p| p.to_path_buf())
        .or_else(|| cwd_from_lsof_macos(pid.as_u32()))
}

#[cfg(target_os = "macos")]
fn cmdline_string(proc: &sysinfo::Process) -> String {
    proc.cmd()
        .iter()
        .map(|s| s.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(target_os = "macos")]
fn pids_whose_cmdline_contains(sys: &System, needle: &str) -> Vec<Pid> {
    let mut out = Vec::new();
    for (pid, proc) in sys.processes() {
        if cmdline_string(proc).contains(needle) && !is_claude_code_noise_process(proc) {
            out.push(*pid);
        }
    }
    out
}

/// Cursor, this app, Vite, etc. match substring "claude" (e.g. workspace folder **claw-and-order**).
#[cfg(target_os = "macos")]
fn is_claude_code_noise_process(proc: &sysinfo::Process) -> bool {
    let name = proc.name().to_string_lossy().to_lowercase();
    let cmd = cmdline_string(proc);
    let c = cmd.to_lowercase();

    if c.contains("claw-and-order") && (c.contains("/target/") || c.contains("\\target\\")) {
        return true;
    }
    if c.contains("claw-and-order/node_modules") || c.contains("claw-and-order\\node_modules") {
        return true;
    }
    if c.contains("tauri dev") || c.contains("tauri build") || c.contains(".bin/tauri") {
        return true;
    }
    if c.contains("@esbuild") || c.contains("esbuild --service") || c.contains("esbuild--service") {
        return true;
    }
    if c.contains("node_modules/.bin/vite") || c.contains("/vite ") || c.ends_with(" vite") {
        return true;
    }
    if name.contains("cursor helper") || name.contains("cursorhelper") {
        return true;
    }
    if name.contains("tsserver") {
        return true;
    }
    if name == "esbuild" {
        return true;
    }
    // Extension host: workspace path often contains the repo folder name
    if name == "node"
        && c.contains("extension-host")
        && (c.contains("claw-and-order") || c.contains("claude-manager"))
    {
        return true;
    }

    false
}

/// True Claude Code CLI on macOS is usually a process whose **name** is `claude` (see `pgrep -x claude`).
#[cfg(target_os = "macos")]
fn pids_named_claude_in_cwd(sys: &System, cwd: &Path) -> Vec<Pid> {
    let mut out = Vec::new();
    if !cwd.is_dir() {
        return out;
    }
    for (pid, proc) in sys.processes() {
        let name = proc.name().to_string_lossy().to_lowercase();
        if name != "claude" {
            continue;
        }
        if is_claude_code_noise_process(proc) {
            continue;
        }
        let Some(pcwd) = process_cwd_macos(*pid, proc) else {
            continue;
        };
        if paths_same_dir(cwd, &pcwd) {
            out.push(*pid);
        }
    }
    out
}

/// When exactly one non-noise `claude` process exists, use it (sysinfo cwd is often missing).
#[cfg(target_os = "macos")]
fn pids_single_claude_named(sys: &System) -> Vec<Pid> {
    let mut found = Vec::new();
    for (pid, proc) in sys.processes() {
        let name = proc.name().to_string_lossy().to_lowercase();
        if name != "claude" {
            continue;
        }
        if is_claude_code_noise_process(proc) {
            continue;
        }
        found.push(*pid);
    }
    if found.len() == 1 {
        found
    } else {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
fn looks_like_claude_code_process(proc: &sysinfo::Process) -> bool {
    if is_claude_code_noise_process(proc) {
        return false;
    }
    let cmd = cmdline_string(proc).to_lowercase();
    let name = proc.name().to_string_lossy().to_lowercase();

    if name == "claude" {
        return true;
    }

    if let Some(exe) = proc.exe() {
        let e = exe.to_string_lossy().to_lowercase();
        if e.ends_with("/claude") || e.ends_with("\\claude") {
            return !e.contains("claude-manager") && !e.contains("claw-and-order");
        }
        if e.contains("anthropic") && e.contains("claude") {
            return true;
        }
    }

    cmd.contains("claude-code")
        || cmd.contains("@anthropic/claude")
        || cmd.contains("anthropic-claude")
        || cmd.contains("bin/claude")
        || (cmd.contains("node") && cmd.contains("anthropic") && cmd.contains("claude"))
        || (cmd.contains("bun") && cmd.contains("claude") && cmd.contains("anthropic"))
}

#[cfg(target_os = "macos")]
fn pids_claude_like_in_cwd(sys: &System, cwd: &Path) -> Vec<Pid> {
    let mut out = Vec::new();
    if !cwd.is_dir() {
        return out;
    }
    for (pid, proc) in sys.processes() {
        let Some(pcwd) = process_cwd_macos(*pid, proc) else {
            continue;
        };
        if !paths_same_dir(cwd, &pcwd) {
            continue;
        }
        if looks_like_claude_code_process(proc) {
            out.push(*pid);
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn lsof_pids(path: &Path) -> Vec<u32> {
    let Some(out) = Command::new("lsof").arg("-t").arg(path).output().ok() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            if t.is_empty() {
                None
            } else {
                t.parse().ok()
            }
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn try_activate_via_lsof_macos(session_file: &Path, sys: &System) -> Option<String> {
    let pids = lsof_pids(session_file)
        .into_iter()
        .map(Pid::from_u32);
    try_activate_from_pids_macos(sys, pids).map(|msg| {
        format!("{msg} (a process had this session file open).")
    })
}

#[cfg(target_os = "macos")]
fn try_activate_from_pids_macos(
    sys: &System,
    pids: impl Iterator<Item = Pid>,
) -> Option<String> {
    for pid in pids {
        let app = terminal_app_for_pid(sys, pid);
        if let Some(app) = app {
            if activate_macos_app(&app) {
                return Some(format!("Brought {app} to the front."));
            }
        }
    }
    None
}

/// Walk parents to find a terminal app or IDE that likely hosts the CLI.
#[cfg(target_os = "macos")]
fn terminal_app_for_pid(sys: &System, start: Pid) -> Option<String> {
    let mut current = Some(start);
    for _ in 0..64 {
        let pid = current?;
        let proc = sys.process(pid)?;
        let name = proc.name().to_string_lossy().to_lowercase();

        if matches!(
            name.as_str(),
            "terminal" | "terminal.app" | "com.apple.terminal"
        ) {
            return Some("Terminal".to_string());
        }
        if name.contains("iterm") {
            return Some("iTerm".to_string());
        }
        if name.contains("warp") {
            return Some("Warp".to_string());
        }
        if name.contains("ghostty") {
            return Some("Ghostty".to_string());
        }
        if name.contains("alacritty") {
            return Some("Alacritty".to_string());
        }
        if name.contains("kitty") {
            return Some("kitty".to_string());
        }
        if name.contains("wezterm") {
            return Some("WezTerm".to_string());
        }
        if name.contains("cursor") {
            return Some("Cursor".to_string());
        }
        if name == "code" || name.contains("visual studio code") {
            return Some("Visual Studio Code".to_string());
        }

        current = proc.parent();
    }
    None
}

#[cfg(target_os = "macos")]
fn activate_macos_app(name: &str) -> bool {
    let names: &[&str] = match name {
        "iTerm" => &["iTerm", "iTerm2"],
        n => &[n],
    };
    for app in names {
        let script = format!("tell application \"{}\" to activate", app.replace('\"', ""));
        let ok = Command::new("osascript")
            .args(["-e", &script])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn open_terminal_at_cwd(cwd: &str) -> Result<String, String> {
    let path = Path::new(cwd);
    if !path.is_dir() {
        return Err(format!("Not a directory: {}", cwd));
    }
    let status = Command::new("open")
        .args(["-a", "Terminal", cwd])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("Failed to run `open -a Terminal`.".into());
    }
    Ok(format!(
        "Opened a new Terminal window at {}. No matching Claude process was found for this session (try running `claude` from a normal Terminal tab so cwd / command line match).",
        cwd
    ))
}

#[cfg(target_os = "linux")]
fn open_terminal_at_cwd(cwd: &str) -> Result<String, String> {
    let path = Path::new(cwd);
    if !path.is_dir() {
        return Err(format!("Not a directory: {}", cwd));
    }
    let try_cmds: &[(&str, &[&str])] = &[
        ("gnome-terminal", &["--working-directory", cwd]),
        ("konsole", &["--workdir", cwd]),
        ("xfce4-terminal", &["--working-directory", cwd]),
        ("alacritty", &["--working-directory", cwd]),
        ("kitty", &["--directory", cwd]),
    ];
    for (cmd, args) in try_cmds {
        if Command::new(cmd).args(*args).spawn().is_ok() {
            return Ok(format!("Opened {} at {}.", cmd, cwd));
        }
    }
    Err(
        "Could not launch a terminal. Install gnome-terminal, konsole, or alacritty.".into(),
    )
}

#[cfg(target_os = "windows")]
fn open_terminal_at_cwd(cwd: &str) -> Result<String, String> {
    let path = Path::new(cwd);
    if !path.is_dir() {
        return Err(format!("Not a directory: {}", cwd));
    }
    if Command::new("wt").args(["-d", cwd]).spawn().is_ok() {
        return Ok(format!("Opened Windows Terminal at {}.", cwd));
    }
    Command::new("cmd")
        .args(["/C", "start", "cmd", "/K", &format!("cd /d {}", cwd)])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(format!("Opened Command Prompt at {}.", cwd))
}

#[cfg(all(
    not(target_os = "macos"),
    not(target_os = "linux"),
    not(target_os = "windows")
))]
fn open_terminal_at_cwd(_cwd: &str) -> Result<String, String> {
    Err("Unsupported platform".into())
}
