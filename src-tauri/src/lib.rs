mod session;
mod terminal;

use session::{claude_projects_dir, list_all_summaries, read_full_capped, parse_jsonl_lines, SessionSummary};

#[tauri::command]
fn get_projects_root() -> Option<String> {
    claude_projects_dir().map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn list_sessions() -> Vec<SessionSummary> {
    let Some(root) = claude_projects_dir() else {
        return Vec::new();
    };
    list_all_summaries(&root)
}

#[tauri::command]
fn read_session_transcript(path: String) -> Result<Vec<serde_json::Value>, String> {
    let p = std::path::PathBuf::from(path);
    let text = read_full_capped(&p).map_err(|e| e.to_string())?;
    Ok(parse_jsonl_lines(&text))
}

#[tauri::command]
fn focus_session_terminal(
    session_jsonl_path: String,
    cwd: Option<String>,
) -> Result<String, String> {
    terminal::focus_session_terminal(session_jsonl_path, cwd)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            if let Some(root) = session::claude_dir() {
                session::spawn_watcher(app.handle().clone(), root);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_projects_root,
            list_sessions,
            read_session_transcript,
            focus_session_terminal
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
