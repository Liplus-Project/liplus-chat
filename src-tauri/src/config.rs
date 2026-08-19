use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::AppHandle;
use tauri::Manager;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabConfig {
    pub id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub cli_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub tabs: Vec<TabConfig>,
}

// ---------------------------------------------------------------------------
// Session persistence types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedChatMessage {
    pub role: String,
    pub content_type: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub id: String,
    pub name: String,
    pub messages: Vec<SavedChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabSessions {
    pub tab_id: String,
    pub active_session_id: Option<String>,
    pub sessions: Vec<SessionData>,
}

/// Legacy config format for migration
#[derive(Debug, Clone, Deserialize)]
struct LegacyPaneConfig {
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyAppConfig {
    left: LegacyPaneConfig,
    right: LegacyPaneConfig,
}

fn cli_kind_from_command(command: &str) -> String {
    if command.to_lowercase().contains("codex") {
        "codex".to_string()
    } else if command.to_lowercase().contains("gemini") {
        "gemini".to_string()
    } else {
        "claude".to_string()
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            tabs: vec![
                TabConfig {
                    id: "tab-1".to_string(),
                    name: "Claude Code".to_string(),
                    command: "claude".to_string(),
                    args: vec![],
                    cwd: None,
                    cli_kind: "claude".to_string(),
                },
                TabConfig {
                    id: "tab-2".to_string(),
                    name: "Codex".to_string(),
                    command: "codex".to_string(),
                    args: vec![],
                    cwd: None,
                    cli_kind: "codex".to_string(),
                },
            ],
        }
    }
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))?;
    Ok(dir.join("config.json"))
}

#[tauri::command]
pub fn load_config(app: AppHandle) -> Result<AppConfig, String> {
    let path = config_path(&app)?;
    if !path.exists() {
        return Ok(AppConfig::default());
    }
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read config: {e}"))?;

    // Try parsing as new format first
    if let Ok(config) = serde_json::from_str::<AppConfig>(&content) {
        return Ok(config);
    }

    // Fall back to legacy left/right format and migrate
    if let Ok(legacy) = serde_json::from_str::<LegacyAppConfig>(&content) {
        let migrated = AppConfig {
            tabs: vec![
                TabConfig {
                    id: "tab-1".to_string(),
                    name: "Claude Code".to_string(),
                    command: legacy.left.command.clone(),
                    args: legacy.left.args,
                    cwd: legacy.left.cwd,
                    cli_kind: cli_kind_from_command(&legacy.left.command),
                },
                TabConfig {
                    id: "tab-2".to_string(),
                    name: "Codex".to_string(),
                    command: legacy.right.command.clone(),
                    args: legacy.right.args,
                    cwd: legacy.right.cwd,
                    cli_kind: cli_kind_from_command(&legacy.right.command),
                },
            ],
        };
        // Save migrated config
        if let Ok(json) = serde_json::to_string_pretty(&migrated) {
            let _ = std::fs::write(&path, json);
        }
        return Ok(migrated);
    }

    Err("Failed to parse config: unrecognized format".to_string())
}

#[tauri::command]
pub fn save_config(app: AppHandle, config: AppConfig) -> Result<(), String> {
    let path = config_path(&app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {e}"))?;
    }
    let content = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize config: {e}"))?;
    std::fs::write(&path, content).map_err(|e| format!("Failed to write config: {e}"))
}

// ---------------------------------------------------------------------------
// Session persistence commands
// ---------------------------------------------------------------------------

fn sessions_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))?;
    Ok(dir.join("sessions.json"))
}

#[tauri::command]
pub fn save_sessions(app: AppHandle, data: Vec<TabSessions>) -> Result<(), String> {
    let path = sessions_path(&app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create sessions dir: {e}"))?;
    }
    let content = serde_json::to_string_pretty(&data)
        .map_err(|e| format!("Failed to serialize sessions: {e}"))?;
    std::fs::write(&path, content).map_err(|e| format!("Failed to write sessions: {e}"))
}

#[tauri::command]
pub fn load_sessions(app: AppHandle) -> Result<Vec<TabSessions>, String> {
    let path = sessions_path(&app)?;
    if !path.exists() {
        return Ok(vec![]);
    }
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read sessions: {e}"))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse sessions: {e}"))
}
