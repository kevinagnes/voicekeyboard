//! Tauri commands for the agent workspace: runs, plans, artifacts, permissions,
//! jobs, undo and the permission reply channel.

use std::sync::Arc;

use serde_json::Value;
use tauri::State;

use crate::app::AppState;
use crate::core::agent::types::Decision;

#[tauri::command]
pub fn respond_permission(state: State<'_, Arc<AppState>>, id: String, decision: Decision) -> bool {
    state.agent.respond_permission(&id, decision)
}

#[tauri::command]
pub fn pending_permission(
    state: State<'_, Arc<AppState>>,
) -> Option<crate::core::agent::types::PermissionRequest> {
    state.agent.pending_permission()
}

#[tauri::command]
pub fn list_runs(state: State<'_, Arc<AppState>>, limit: Option<usize>) -> Value {
    serde_json::to_value(state.agent.runs(limit.unwrap_or(50).min(500)))
        .unwrap_or(Value::Array(vec![]))
}

#[tauri::command]
pub fn get_run(state: State<'_, Arc<AppState>>, id: String) -> Value {
    state.agent.run_detail(&id)
}

#[tauri::command]
pub fn get_plan(state: State<'_, Arc<AppState>>, session_id: String) -> Value {
    serde_json::to_value(state.agent.plan(&session_id)).unwrap_or(Value::Array(vec![]))
}

#[tauri::command]
pub fn clear_plan(state: State<'_, Arc<AppState>>, session_id: String) {
    state.agent.db.set_plan(&session_id, &[]);
}

#[tauri::command]
pub fn list_artifacts(state: State<'_, Arc<AppState>>, session_id: String) -> Value {
    serde_json::to_value(state.agent.artifacts(&session_id)).unwrap_or(Value::Array(vec![]))
}

#[tauri::command]
pub fn list_permissions(state: State<'_, Arc<AppState>>) -> Value {
    state
        .agent
        .permissions()
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "action": p.action,
                "resource": p.resource,
                "effect": p.effect,
                "scope": p.scope,
                "createdAt": p.created_at,
            })
        })
        .collect()
}

#[tauri::command]
pub fn revoke_permission(state: State<'_, Arc<AppState>>, id: String) -> bool {
    state.agent.revoke_permission(&id)
}

#[tauri::command]
pub fn list_jobs(state: State<'_, Arc<AppState>>, limit: Option<usize>) -> Value {
    serde_json::to_value(state.agent.jobs(limit.unwrap_or(50).min(500)))
        .unwrap_or(Value::Array(vec![]))
}

#[tauri::command]
pub fn cancel_job(state: State<'_, Arc<AppState>>, id: String) -> bool {
    state.agent.cancel_job(&id)
}

#[tauri::command]
pub fn enqueue_agent_job(
    state: State<'_, Arc<AppState>>,
    prompt: String,
    deep: Option<bool>,
) -> Result<String, String> {
    if prompt.trim().is_empty() {
        return Err("prompt is empty".into());
    }
    Ok(state.agent.enqueue_job(&prompt, deep.unwrap_or(true)))
}

#[tauri::command]
pub fn undo_run(state: State<'_, Arc<AppState>>, run_id: String) -> Result<usize, String> {
    state.agent.undo_run(&run_id)
}

#[tauri::command]
pub fn agent_metrics(state: State<'_, Arc<AppState>>) -> Value {
    state.agent.metrics()
}

#[tauri::command]
pub fn agent_status(state: State<'_, Arc<AppState>>) -> Value {
    serde_json::json!({
        "active": state.agent.is_active(),
        "speaking": state.agent.is_speaking(),
        "activeSessionId": state.agent.active_session_id(),
    })
}

#[tauri::command]
pub fn workspace_path(_state: State<'_, Arc<AppState>>) -> String {
    let settings = crate::core::settings::load();
    if settings.workspace_root.trim().is_empty() {
        crate::app_data_dir()
            .join("workspace")
            .to_string_lossy()
            .to_string()
    } else {
        settings.workspace_root
    }
}
