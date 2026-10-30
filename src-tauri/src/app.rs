use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{
    AppHandle, Emitter, LogicalPosition, Manager, PhysicalPosition, PhysicalSize, Position, State,
    WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_autostart::ManagerExt as _;

use crate::core::agent::{desktop::SystemDesktop, AgentService};
use crate::core::{capture, gemini, hotkey, mic, paste, settings, sounds, stt};
use crate::downloader::{is_valid_model_file, Downloader};
use crate::model_registry::ModelRegistry;
use crate::updater::{self, UpdateInfo};

pub const EVT_RECORDING_STARTED: &str = "recording-started";
pub const EVT_RECORDING_STOPPED: &str = "recording-stopped";
pub const EVT_TRANSCRIBING: &str = "transcribing";
pub const EVT_TRANSCRIPT: &str = "transcript";
pub const EVT_SECURE_SKIPPED: &str = "secure-skipped";
pub const EVT_ERROR: &str = "app-error";
pub const EVT_DOWNLOAD_PROGRESS: &str = "model-download-progress";
pub const EVT_MODEL_LOADED: &str = "model-loaded";
pub const EVT_MODEL_MISSING: &str = "model-missing";
pub const EVT_SETTINGS_CHANGED: &str = "settings-changed";
pub const EVT_UPDATE_STATUS: &str = "update-status";
pub const EVT_UPDATE_PROGRESS: &str = "update-download-progress";
pub const EVT_PASTE_FAILED: &str = "paste-failed";
pub const EVT_PERMISSIONS_GRANTED: &str = "permissions-granted";
pub const EVT_LLM_STATUS: &str = "llm-status";
pub const EVT_LLM_SESSION: &str = "llm-session";
pub const EVT_OPEN_SESSIONS: &str = "open-sessions";

const TRAY_ID: &str = "main";
const OVERLAY_ID: &str = "recording-overlay";
const GATE_ID: &str = "permissions-gate";
const LLM_OVERLAY_ID: &str = "llm-overlay";
const SHOT_ID: &str = "screenshot-overlay";
const OVERLAY_W: f64 = 380.0;
const OVERLAY_H: f64 = 64.0;
const OVERLAY_BOTTOM_MARGIN: f64 = 72.0;
const LLM_OVERLAY_W: f64 = 560.0;
const LLM_OVERLAY_H: f64 = 250.0;
/// Taller panel when the user expands the session history.
const LLM_OVERLAY_H_EXPANDED: f64 = 620.0;
/// How many sessions the tray submenu lists; the rest live in Settings.
const SESSIONS_IN_MENU: usize = 12;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(crate) struct StoredPosition {
    x: i32,
    y: i32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct OverlayPositions {
    recording: Option<StoredPosition>,
    llm: Option<StoredPosition>,
}

fn overlay_positions_path() -> PathBuf {
    crate::app_data_dir().join("overlay_positions.json")
}

fn load_overlay_positions() -> OverlayPositions {
    std::fs::read_to_string(overlay_positions_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_overlay_positions(positions: &OverlayPositions) {
    let path = overlay_positions_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(raw) = serde_json::to_string_pretty(positions) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, raw).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

/// Which action a hotkey press belongs to. Recording and capture are shared;
/// the outcome differs only on release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Record,
    AskLlm,
    Screenshot,
}

enum Job {
    Transcribe(Vec<f32>, Option<String>, String),
    LoadModel(PathBuf),
    /// `session_id` is the session captured when the recording began; `None`
    /// means the question starts a fresh session.
    AskLlm(Vec<f32>, Option<String>, String, Option<String>),
    /// Live transcription already produced the final text.
    RecordedText(String),
    AskText(String, Option<String>),
    /// Screenshot + spoken question, after batch transcription.
    AskScreenshot(
        Vec<f32>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    ),
    /// Screenshot + spoken question, when streaming already produced the text.
    AskScreenshotText(String, Option<String>, Option<String>),
}

pub struct AppState {
    pub settings: Mutex<settings::Settings>,
    pub hotkeys: Mutex<Option<hotkey::HotkeyController>>,
    pub recorder: Mutex<Option<capture::AudioRecorder>>,
    pub stt: Arc<stt::SttEngine>,
    pub sounds: Arc<sounds::Sounds>,
    pub registry: ModelRegistry,
    job_tx: Mutex<Option<mpsc::Sender<Job>>>,
    downloading: Mutex<Option<String>>,
    download_progress: Mutex<Option<(u64, u64)>>,
    overlay: Mutex<Option<tauri::WebviewWindow<tauri::Wry>>>,
    llm_overlay: Mutex<Option<tauri::WebviewWindow<tauri::Wry>>>,
    recent: Mutex<Vec<String>>,
    pub recording: AtomicBool,
    pub busy: AtomicBool,
    pub model_loaded: AtomicBool,
    pub hotkey_ok: AtomicBool,
    pub hotkey_llm_ok: AtomicBool,
    pub hotkey_shot_ok: AtomicBool,
    /// True while the current (or last) hotkey gesture is in Ask-LLM mode.
    pub llm_mode: AtomicBool,
    /// Set by the Cancel button / Esc; the Ask-LLM pipeline checks it between
    /// phases (transcribe → chat → speak) and aborts.
    pub cancel_pending: AtomicBool,
    /// Rebuilt from settings whenever the Gemini key / models change.
    pub gemini: Mutex<Option<gemini::GeminiClient>>,
    /// Active speech output stream (during Ask-Gemini reply playback), so the
    /// Cancel button / Esc can stop it.
    pub speech: Mutex<Option<sounds::SpeechStreamHandle>>,
    /// Recallable AI assistant conversations.
    pub agent: Arc<AgentService>,
    pub update: Mutex<Option<UpdateInfo>>,
    pub update_checking: AtomicBool,
    pub update_installing: AtomicBool,
    pub update_progress: Mutex<Option<(u64, u64)>>,
    /// True while required permissions (mic + accessibility) are missing and
    /// the app is blocked behind the permissions-gate window.
    pub gated: AtomicBool,
    /// User-moved positions for the floating panels (physical pixels).
    pub overlay_positions: Mutex<OverlayPositions>,
    /// Active Gemini Live transcription session while the hotkey is held.
    pub live_stt: Mutex<Option<crate::core::live_stt::LiveSttSession>>,
    /// Incremental local transcription (Whisper/Nemotron) while the hotkey is held.
    pub local_stream: Mutex<Option<crate::core::local_stream::LocalStream>>,
    /// True while the screenshot selection overlay is open.
    pub screenshot_selecting: AtomicBool,
    /// True while the screenshot hotkey is still held.
    pub screenshot_hold: AtomicBool,
    /// Screenshot captured for the current question.
    pub pending_screenshot: Mutex<Option<String>>,
}

const MAX_RECENT: usize = 3;

impl AppState {
    pub fn new() -> Self {
        let settings = settings::load();
        let gemini_client = gemini::GeminiClient::new(
            &settings.gemini_api_key,
            &settings.llm_model,
            &settings.llm_system_prompt,
            &settings.tts_model,
            &settings.tts_voice,
        );
        let sounds = Arc::new(sounds::Sounds::new());
        let desktop = Arc::new(SystemDesktop::new());
        let agent = AgentService::start(
            crate::app_data_dir().join("agent.db"),
            sounds.clone(),
            desktop,
        );
        Self {
            settings: Mutex::new(settings),
            hotkeys: Mutex::new(None),
            recorder: Mutex::new(None),
            stt: Arc::new(stt::SttEngine::new()),
            sounds,
            registry: ModelRegistry::from_embedded(),
            job_tx: Mutex::new(None),
            downloading: Mutex::new(None),
            download_progress: Mutex::new(None),
            overlay: Mutex::new(None),
            llm_overlay: Mutex::new(None),
            recent: Mutex::new(Vec::new()),
            recording: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            model_loaded: AtomicBool::new(false),
            hotkey_ok: AtomicBool::new(true),
            hotkey_llm_ok: AtomicBool::new(true),
            hotkey_shot_ok: AtomicBool::new(true),
            llm_mode: AtomicBool::new(false),
            cancel_pending: AtomicBool::new(false),
            gemini: Mutex::new(Some(gemini_client)),
            speech: Mutex::new(None),
            agent,
            update: Mutex::new(None),
            update_checking: AtomicBool::new(false),
            update_installing: AtomicBool::new(false),
            update_progress: Mutex::new(None),
            gated: AtomicBool::new(false),
            overlay_positions: Mutex::new(load_overlay_positions()),
            live_stt: Mutex::new(None),
            local_stream: Mutex::new(None),
            screenshot_selecting: AtomicBool::new(false),
            screenshot_hold: AtomicBool::new(false),
            pending_screenshot: Mutex::new(None),
        }
    }
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatusDto {
    pub recording: bool,
    pub busy: bool,
    pub model_loaded: bool,
    pub model_id: String,
    pub hotkey: String,
    pub llm_hotkey: String,
    pub gemini_configured: bool,
    pub accessibility_trusted: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LlmStatusDto {
    /// listening | transcribing | thinking | speaking | done | cancelled | cancelling | error
    pub phase: String,
    pub transcript: String,
    pub reply: String,
    pub error: Option<String>,
    /// Titles of the Google Search sources behind the answer, if any.
    pub sources: Vec<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetaDto {
    pub id: String,
    pub title: String,
    pub updated_at: u64,
    pub turns: usize,
    pub active: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatusDto {
    pub id: String,
    pub name: String,
    pub description: String,
    pub filename: String,
    pub size_bytes: u64,
    pub languages: Vec<String>,
    pub downloaded: bool,
    pub path: Option<String>,
    pub active: bool,
    pub engine: String,
    pub cloud: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgressDto {
    pub model_id: String,
    pub downloaded: u64,
    pub total: u64,
    pub done: bool,
    pub error: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptDto {
    pub text: String,
    pub language: String,
    pub inference_ms: u64,
    pub n_segments: i32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsDto {
    pub accessibility_trusted: bool,
    pub has_input_device: bool,
    pub mic_permission: String,
    pub running_from_bundle: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatusDto {
    pub checking: bool,
    pub installing: bool,
    pub downloaded: u64,
    pub total: u64,
    pub current_version: String,
    pub available: Option<UpdateInfoDto>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfoDto {
    pub latest_version: String,
    pub current_version: String,
    pub notes: String,
    pub url: String,
    pub dmg_url: String,
}

impl From<UpdateInfo> for UpdateInfoDto {
    fn from(u: UpdateInfo) -> Self {
        Self {
            latest_version: u.latest_version,
            current_version: u.current_version,
            notes: u.notes,
            url: u.url,
            dmg_url: u.dmg_url,
        }
    }
}

pub fn run() {
    let state = Arc::new(AppState::new());
    let setup_state = state.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(state.clone())
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_settings,
            update_settings,
            get_models,
            download_model,
            get_permissions,
            get_audio_level,
            request_accessibility,
            request_microphone,
            reset_microphone,
            open_mic_settings,
            quit_app,
            get_inference_stats,
            get_input_devices,
            get_recent_transcriptions,
            copy_transcription,
            check_for_updates,
            install_update,
            get_update_status,
            cancel_recording,
            get_gemini_catalog,
            list_sessions,
            get_session,
            get_active_session,
            new_session,
            set_active_session,
            rename_session,
            delete_session,
            save_overlay_position,
            set_llm_overlay_expanded,
            screenshot_region_selected,
            cancel_screenshot,
            crate::agent_commands::respond_permission,
            crate::agent_commands::pending_permission,
            crate::agent_commands::list_runs,
            crate::agent_commands::get_run,
            crate::agent_commands::get_plan,
            crate::agent_commands::clear_plan,
            crate::agent_commands::list_artifacts,
            crate::agent_commands::list_permissions,
            crate::agent_commands::revoke_permission,
            crate::agent_commands::list_jobs,
            crate::agent_commands::cancel_job,
            crate::agent_commands::enqueue_agent_job,
            crate::agent_commands::undo_run,
            crate::agent_commands::agent_metrics,
            crate::agent_commands::agent_status,
            crate::agent_commands::workspace_path,
        ])
        .setup(move |app| {
            let state = &setup_state;
            debug_log(&format!(
                "VoiceKeyboard v{} starting (overlay {}x{})",
                env!("CARGO_PKG_VERSION"),
                OVERLAY_W as u32,
                OVERLAY_H as u32
            ));
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            state.agent.attach(handle.clone());
            state.agent.recover();

            let menu = build_menu(&handle, state)?;
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(tray_icon())
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "settings" => open_settings(app),
                    "about" => open_about(app),
                    "sessions-manage" => {
                        open_settings(app);
                        open_sessions_tab(app);
                    }
                    "session:new" => {
                        if let Some(state) = app.try_state::<Arc<AppState>>() {
                            start_new_session(app, state.inner());
                        }
                    }
                    id if id.starts_with("session:") => {
                        if let Some(state) = app.try_state::<Arc<AppState>>() {
                            activate_session(app, state.inner(), &id["session:".len()..]);
                        }
                    }
                    "check-updates" => {
                        if let Some(state) = app.try_state::<Arc<AppState>>() {
                            if state.update.lock().is_some() {
                                let handle = app.clone();
                                let state2 = state.inner().clone();
                                let _ = tauri::async_runtime::spawn_blocking(move || {
                                    install_update_impl(&handle, &state2)
                                });
                            } else {
                                let handle = app.clone();
                                let state2 = state.inner().clone();
                                let _ = tauri::async_runtime::spawn_blocking(move || {
                                    check_for_updates_impl(&handle, &state2)
                                });
                            }
                        }
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        let gated = app
                            .try_state::<Arc<AppState>>()
                            .is_some_and(|s| s.gated.load(Ordering::SeqCst));
                        if gated {
                            open_gate(app);
                        } else {
                            open_settings(app);
                        }
                    }
                })
                .build(app)?;

            let (job_tx, job_rx) = mpsc::channel::<Job>();
            *state.job_tx.lock() = Some(job_tx.clone());

            let mut controller = hotkey::HotkeyController::new().map_err(|e| e.to_string())?;
            let hotkey_spec = state.settings.lock().hotkey.clone();
            match controller.rebind(&hotkey_spec) {
                Ok(()) => {
                    state.hotkey_ok.store(true, Ordering::SeqCst);
                    debug_log(&format!(
                        "hotkey registered: {}",
                        hotkey::display(&hotkey_spec)
                    ));
                }
                Err(e) => {
                    state.hotkey_ok.store(false, Ordering::SeqCst);
                    debug_log(&format!("hotkey FAILED to register {}: {e}", hotkey_spec));
                    emit_error(
                        &handle,
                        &format!(
                            "Hotkey \"{}\" could not be registered: {e}",
                            hotkey::display(&hotkey_spec)
                        ),
                    );
                }
            }
            let llm_hotkey_spec = state.settings.lock().llm_hotkey.clone();
            match controller.rebind_llm(&llm_hotkey_spec) {
                Ok(()) => {
                    state.hotkey_llm_ok.store(true, Ordering::SeqCst);
                    debug_log(&format!(
                        "ask-llm hotkey registered: {}",
                        hotkey::display(&llm_hotkey_spec)
                    ));
                }
                Err(e) => {
                    state.hotkey_llm_ok.store(false, Ordering::SeqCst);
                    debug_log(&format!(
                        "ask-llm hotkey FAILED to register {}: {e}",
                        llm_hotkey_spec
                    ));
                    emit_error(
                        &handle,
                        &format!(
                            "Ask-LLM hotkey \"{}\" could not be registered: {e}",
                            hotkey::display(&llm_hotkey_spec)
                        ),
                    );
                }
            }
            let screenshot_hotkey_spec = state.settings.lock().screenshot_hotkey.clone();
            match controller.rebind_screenshot(&screenshot_hotkey_spec) {
                Ok(()) => {
                    state.hotkey_shot_ok.store(true, Ordering::SeqCst);
                    debug_log(&format!(
                        "screenshot hotkey registered: {}",
                        hotkey::display(&screenshot_hotkey_spec)
                    ));
                }
                Err(e) => {
                    state.hotkey_shot_ok.store(false, Ordering::SeqCst);
                    debug_log(&format!(
                        "screenshot hotkey FAILED to register {}: {e}",
                        screenshot_hotkey_spec
                    ));
                    emit_error(
                        &handle,
                        &format!(
                            "Screenshot hotkey \"{}\" could not be registered: {e}",
                            hotkey::display(&screenshot_hotkey_spec)
                        ),
                    );
                }
            }
            *state.hotkeys.lock() = Some(controller);

            // Request mic permission BEFORE creating the recorder. cpal's stream setup
            // can hang or fail when TCC is undetermined, and the prompt MUST
            // fire on the main thread (setup runs on it). This blocks setup
            // until the user answers (or 60s) — acceptable on first launch.
            // Identical behavior when running from a bundle or a raw binary.
            let mic_status =
                mic::request_mic_permission_blocking(std::time::Duration::from_secs(60));
            debug_log(&format!("mic: permission = {mic_status:?}"));

            // Accessibility is required to paste text into other apps. Ask for
            // it now if it hasn't been granted yet — the system prompt opens
            // System Settings. Non-blocking, and the mic decision above has
            // already been made (the blocking call waited for the answer).
            if !paste::accessibility_trusted() {
                debug_log("accessibility: not trusted — requesting permission");
                let _ = paste::request_accessibility_permission();
            }

            // Gate: until BOTH permissions are granted the app cannot record
            // or paste, so all other functionality is locked behind the gate
            // window (grant in System Settings, or quit).
            let gated =
                mic_status != mic::MicPermission::Authorized || !paste::accessibility_trusted();
            state.gated.store(gated, Ordering::SeqCst);
            if gated {
                debug_log("permissions: gated — waiting for mic + accessibility");
            }

            let (max_secs, device_name) = {
                let s = state.settings.lock();
                (s.max_recording_secs, s.input_device.clone())
            };
            let recorder = capture::AudioRecorder::new(max_secs, &device_name);
            match recorder {
                Ok(r) => {
                    debug_log("recorder: started");
                    *state.recorder.lock() = Some(r);
                }
                Err(e) => {
                    debug_log(&format!("recorder: failed to start -> {e}"));
                }
            }

            spawn_stt_worker(handle.clone(), state.clone(), job_rx);
            spawn_hotkey_thread(handle.clone(), state.clone());
            spawn_model_loader(handle.clone(), state.clone());

            // Watch for permissions being granted later (user answers the prompt in
            // System Settings). While gated, keep the gate window fresh; once
            // everything is granted, un-gate: rebuild the recorder, close the
            // gate, open Settings and update the tray.
            let perm_handle = handle.clone();
            let perm_state = state.clone();
            std::thread::Builder::new()
                .name("permission-watcher".into())
                .spawn(move || {
                    let mut last_mic = mic::mic_permission();
                    let mut last_acc = paste::accessibility_trusted();
                    for _ in 0..120 {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        let mic = mic::mic_permission();
                        let acc = paste::accessibility_trusted();
                        if mic == mic::MicPermission::Authorized
                            && perm_state.recorder.lock().is_none()
                        {
                            debug_log("mic: granted later — rebuilding recorder");
                            rebuild_recorder(&perm_state);
                        }
                        if mic != last_mic || acc != last_acc {
                            last_mic = mic;
                            last_acc = acc;
                            update_tray(&perm_handle, &perm_state);
                        }
                        if all_permissions_granted()
                            && perm_state.gated.swap(false, Ordering::SeqCst)
                        {
                            debug_log("permissions: all granted — un-gating");
                            update_tray(&perm_handle, &perm_state);
                            let _ = perm_handle.emit(EVT_PERMISSIONS_GRANTED, ());
                            let ph = perm_handle.clone();
                            let ph2 = ph.clone();
                            let _ = ph.run_on_main_thread(move || {
                                close_gate_window(&ph2);
                                open_settings(&ph2);
                            });
                            break;
                        }
                    }
                })
                .ok();

            if gated {
                open_gate(&handle);
            }
            update_tray(&handle, state);

            // Auto-update check shortly after launch.
            if state.settings.lock().auto_update {
                let upd_handle = handle.clone();
                let upd_state = state.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(8));
                    check_for_updates_impl(&upd_handle, &upd_state);
                });
            }

            {
                let marker = crate::app_data_dir().join(".setup_done");
                if !marker.exists() {
                    let h = handle.clone();
                    let st = setup_state.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        if st.gated.load(Ordering::SeqCst) {
                            open_gate(&h);
                        } else {
                            open_settings(&h);
                        }
                        let _ = std::fs::write(&marker, "");
                    });
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// enigo's macOS key injection (Key -> keycode via HIToolbox TSM) must run on
/// the main thread, otherwise it traps (SIGTRAP). Typing + clipboard all
/// happen here on the main thread.
fn paste_text_on_main(app: &AppHandle, text: &str) -> Result<paste::PasteOutcome, String> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<paste::PasteOutcome, String>>(1);
    let text = text.to_string();
    let _ = app.run_on_main_thread(move || {
        let _ = tx.send(paste::paste_text(&text));
    });
    // Typing a long transcript takes longer than a paste; allow up to 60s.
    rx.recv_timeout(std::time::Duration::from_secs(60))
        .unwrap_or_else(|_| Err("paste timed out".to_string()))
}

/// Put text on the system clipboard so the user can paste it manually after a
/// failed auto-paste. Returns true on success.
fn copy_to_clipboard(text: &str) -> bool {
    let mut clipboard = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(e) => {
            debug_log(&format!("clipboard: failed to open -> {e}"));
            return false;
        }
    };
    match clipboard.set_text(text) {
        Ok(()) => {
            debug_log(&format!("clipboard: {} chars copied", text.len()));
            true
        }
        Err(e) => {
            debug_log(&format!("clipboard: set_text failed -> {e}"));
            false
        }
    }
}

/// Handle a paste that failed: put the text on the clipboard, play the error
/// chime, and notify the user that they can paste manually.
fn handle_paste_failure(app: &AppHandle, state: &AppState, text: &str, reason: &str) {
    debug_log(&format!("paste: failed -> {reason}"));
    let copied = copy_to_clipboard(text);
    state.sounds.play(sounds::Chime::Error);
    let message = if copied {
        format!("Couldn't type into the focused app ({reason}) — text is on your clipboard, paste with Cmd/Ctrl+V")
    } else {
        format!("Failed to paste: {reason}")
    };
    let _ = app.emit(EVT_PASTE_FAILED, message.clone());
    // Native notification banner (on the main thread).
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        crate::notify::show("VoiceKeyboard — paste failed", &message);
        let _ = handle;
    });
    hide_overlay_delayed(app, std::time::Duration::from_millis(1500));
}

fn spawn_hotkey_thread(app: AppHandle, state: Arc<AppState>) {
    std::thread::Builder::new()
        .name("hotkey-events".into())
        .spawn(move || {
            let receiver = global_hotkey::GlobalHotKeyEvent::receiver();
            while let Ok(event) = receiver.recv() {
                let mode = {
                    let hotkeys = state.hotkeys.lock();
                    match hotkeys.as_ref() {
                        Some(controller) if Some(event.id()) == controller.record_id() => {
                            Some(Mode::Record)
                        }
                        Some(controller) if Some(event.id()) == controller.llm_id() => {
                            Some(Mode::AskLlm)
                        }
                        Some(controller) if Some(event.id()) == controller.screenshot_id() => {
                            Some(Mode::Screenshot)
                        }
                        _ => None,
                    }
                };
                let Some(mode) = mode else {
                    continue;
                };
                match event.state() {
                    global_hotkey::HotKeyState::Pressed => on_pressed(&app, &state, mode),
                    global_hotkey::HotKeyState::Released => on_released(&app, &state, mode),
                }
            }
        })
        .expect("failed to spawn hotkey thread");
}

fn spawn_stt_worker(app: AppHandle, state: Arc<AppState>, rx: mpsc::Receiver<Job>) {
    std::thread::Builder::new()
        .name("stt-worker".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                match job {
                    Job::LoadModel(path) => {
                        let (id, engine) = {
                            let s = state.settings.lock();
                            let engine = state
                                .registry
                                .find(&s.model_id)
                                .map(|m| m.engine.clone())
                                .unwrap_or_else(|| "whisper".to_string());
                            (s.model_id.clone(), engine)
                        };
                        match state.stt.load_model(&path.to_string_lossy(), &engine) {
                            Ok(()) => {
                                state.model_loaded.store(true, Ordering::SeqCst);
                                let _ = app.emit(EVT_MODEL_LOADED, &id);
                            }
                            Err(e) => emit_error(&app, &e),
                        }
                        update_tray(&app, &state);
                    }
                    Job::Transcribe(samples, language, prompt) => {
                        debug_log("stt: transcribing…");
                        let _ = app.emit(EVT_TRANSCRIBING, ());
                        let result = state.stt.transcribe(&samples, language.as_deref(), &prompt);
                        match result {
                            Ok(transcription) => {
                                if transcription.text.trim().is_empty() {
                                    debug_log("stt: empty transcript");
                                    state.sounds.play(sounds::Chime::Error);
                                    hide_overlay_delayed(
                                        &app,
                                        std::time::Duration::from_millis(900),
                                    );
                                } else {
                                    debug_log(&format!(
                                        "stt: text='{}' lang={}",
                                        transcription.text.trim(),
                                        transcription.language
                                    ));
                                    {
                                        let mut recent = state.recent.lock();
                                        recent.insert(0, transcription.text.clone());
                                        recent.truncate(MAX_RECENT);
                                    }
                                    match paste_text_on_main(&app, &transcription.text) {
                                        Ok(outcome) if outcome.pasted => {
                                            state.sounds.play(sounds::Chime::PasteDone);
                                            let _ = app.emit(
                                                EVT_TRANSCRIPT,
                                                TranscriptDto {
                                                    text: transcription.text,
                                                    language: transcription.language,
                                                    inference_ms: transcription.inference_ms,
                                                    n_segments: transcription.n_segments,
                                                },
                                            );
                                            hide_overlay_delayed(
                                                &app,
                                                std::time::Duration::from_millis(150),
                                            );
                                        }
                                        Ok(outcome) if outcome.skipped_secure => {
                                            debug_log("paste: skipped (secure field)");
                                            let _ = app.emit(EVT_SECURE_SKIPPED, ());
                                            hide_overlay_delayed(
                                                &app,
                                                std::time::Duration::from_millis(900),
                                            );
                                        }
                                        Ok(_) => {
                                            debug_log("paste: no-op (empty)");
                                            hide_overlay_delayed(
                                                &app,
                                                std::time::Duration::from_millis(900),
                                            );
                                        }
                                        Err(e) => {
                                            handle_paste_failure(
                                                &app,
                                                &state,
                                                &transcription.text,
                                                &e,
                                            );
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                debug_log(&format!("stt: error -> {e}"));
                                emit_error(&app, &e);
                                hide_overlay_delayed(&app, std::time::Duration::from_millis(900));
                            }
                        }
                        state.busy.store(false, Ordering::SeqCst);
                        update_tray(&app, &state);
                    }
                    Job::AskLlm(samples, language, prompt, session_id) => {
                        run_ask_llm(&app, &state, samples, language, prompt, session_id);
                        state.llm_mode.store(false, Ordering::SeqCst);
                        state.busy.store(false, Ordering::SeqCst);
                        update_tray(&app, &state);
                    }
                    Job::RecordedText(text) => {
                        let language = state.settings.lock().language.clone();
                        deliver_recorded_text(&app, &state, &text, &language, 0, 0);
                        state.busy.store(false, Ordering::SeqCst);
                        update_tray(&app, &state);
                    }
                    Job::AskText(text, session_id) => {
                        if text.trim().is_empty() {
                            state.sounds.play(sounds::Chime::Error);
                            emit_llm_status(
                                &app,
                                "error",
                                "",
                                "",
                                Some("Nothing was heard — try again".into()),
                                Vec::new(),
                            );
                            hide_llm_overlay_delayed(&app, std::time::Duration::from_millis(1200));
                        } else {
                            {
                                let mut recent = state.recent.lock();
                                recent.insert(0, text.clone());
                                recent.truncate(MAX_RECENT);
                            }
                            let id = state.agent.admit_user_text(
                                session_id.as_deref(),
                                &text,
                                None,
                                "steer",
                                false,
                            );
                            debug_log(&format!("ask-llm: admitted live text to session {id}"));
                            let _ = app.emit(EVT_LLM_SESSION, ());
                        }
                        state.llm_mode.store(false, Ordering::SeqCst);
                        state.busy.store(false, Ordering::SeqCst);
                        update_tray(&app, &state);
                    }
                    Job::AskScreenshot(samples, language, prompt, session_id, image) => {
                        let result = state.stt.transcribe(&samples, language.as_deref(), &prompt);
                        match result {
                            Ok(transcription) if !transcription.text.trim().is_empty() => {
                                let text = transcription.text.trim().to_string();
                                {
                                    let mut recent = state.recent.lock();
                                    recent.insert(0, text.clone());
                                    recent.truncate(MAX_RECENT);
                                }
                                let id = state.agent.admit_user_image(
                                    session_id.as_deref(),
                                    &text,
                                    image.as_deref(),
                                    "steer",
                                    false,
                                );
                                debug_log(&format!("screenshot: admitted to session {id}"));
                                let _ = app.emit(EVT_LLM_SESSION, ());
                            }
                            Ok(_) => {
                                state.sounds.play(sounds::Chime::Error);
                                emit_llm_status(
                                    &app,
                                    "error",
                                    "",
                                    "",
                                    Some("Nothing was heard — try again".into()),
                                    Vec::new(),
                                );
                                hide_llm_overlay_delayed(
                                    &app,
                                    std::time::Duration::from_millis(1200),
                                );
                            }
                            Err(e) => {
                                emit_llm_status(
                                    &app,
                                    "error",
                                    "",
                                    "",
                                    Some(e.to_string()),
                                    Vec::new(),
                                );
                                hide_llm_overlay_delayed(
                                    &app,
                                    std::time::Duration::from_millis(1800),
                                );
                            }
                        }
                        state.llm_mode.store(false, Ordering::SeqCst);
                        state.busy.store(false, Ordering::SeqCst);
                        update_tray(&app, &state);
                    }
                    Job::AskScreenshotText(text, session_id, image) => {
                        if text.trim().is_empty() {
                            state.sounds.play(sounds::Chime::Error);
                            emit_llm_status(
                                &app,
                                "error",
                                "",
                                "",
                                Some("Nothing was heard — try again".into()),
                                Vec::new(),
                            );
                            hide_llm_overlay_delayed(&app, std::time::Duration::from_millis(1200));
                        } else {
                            {
                                let mut recent = state.recent.lock();
                                recent.insert(0, text.clone());
                                recent.truncate(MAX_RECENT);
                            }
                            let id = state.agent.admit_user_image(
                                session_id.as_deref(),
                                &text,
                                image.as_deref(),
                                "steer",
                                false,
                            );
                            debug_log(&format!("screenshot: admitted live text to session {id}"));
                            let _ = app.emit(EVT_LLM_SESSION, ());
                        }
                        state.llm_mode.store(false, Ordering::SeqCst);
                        state.busy.store(false, Ordering::SeqCst);
                        update_tray(&app, &state);
                    }
                }
            }
        })
        .expect("failed to spawn stt worker");
}

fn spawn_model_loader(app: AppHandle, state: Arc<AppState>) {
    std::thread::Builder::new()
        .name("model-loader".into())
        .spawn(move || {
            let (id, path) = {
                let s = state.settings.lock();
                (s.model_id.clone(), state.registry.resolve_path(&s.model_id))
            };
            if model_is_valid(&state, &id) {
                if let Some(path) = path {
                    enqueue_load(&state, path);
                } else {
                    start_download(&app, &state, &id);
                }
            } else {
                debug_log(&format!("model: {} not fully present — downloading", id));
                start_download(&app, &state, &id);
            }
        })
        .expect("failed to spawn model loader");
}

fn enqueue_load(state: &AppState, path: PathBuf) {
    if let Some(tx) = state.job_tx.lock().as_ref() {
        let _ = tx.send(Job::LoadModel(path));
    }
}

fn on_pressed(app: &AppHandle, state: &Arc<AppState>, mode: Mode) {
    if state.gated.load(Ordering::SeqCst) {
        debug_log("pressed: ignored — permissions not granted");
        return;
    }
    if state.screenshot_selecting.load(Ordering::SeqCst) && mode != Mode::Screenshot {
        return;
    }
    if app
        .get_webview_window("settings")
        .is_some_and(|w| w.is_focused().unwrap_or(false))
    {
        return;
    }
    if mode == Mode::Screenshot {
        if state.recording.load(Ordering::SeqCst)
            || state.screenshot_selecting.load(Ordering::SeqCst)
        {
            return;
        }
        if state.agent.is_voice_active() || state.agent.is_speaking() {
            state.agent.notify_barge_in();
        }
        state.screenshot_hold.store(true, Ordering::SeqCst);
        show_screenshot_overlay(app, state);
        return;
    }
    if state.busy.load(Ordering::SeqCst) || state.recording.load(Ordering::SeqCst) {
        if mode == Mode::Record
            || state.recording.load(Ordering::SeqCst)
            || !(state.agent.is_voice_active() || state.agent.is_speaking())
        {
            return;
        }
    }
    if mode == Mode::Record && (state.agent.is_voice_active() || state.agent.is_speaking()) {
        // Do not capture the assistant's own speech with dictation.
        return;
    }
    if mode == Mode::AskLlm && (state.agent.is_voice_active() || state.agent.is_speaking()) {
        // Barge-in: stop the current reply and admit the new utterance as a
        // steer at the next safe boundary.
        debug_log("pressed(AskLlm): barge-in");
        state.agent.notify_barge_in();
        if let Some(recorder) = state.recorder.lock().as_ref() {
            let _ = recorder.stop();
        }
    }
    begin_capture(app, state, mode);
}

fn begin_capture(app: &AppHandle, state: &Arc<AppState>, mode: Mode) {
    if state.recording.load(Ordering::SeqCst) {
        return;
    }
    let assistant = mode != Mode::Record;
    state.recording.store(true, Ordering::SeqCst);
    state.llm_mode.store(assistant, Ordering::SeqCst);
    if assistant {
        state.cancel_pending.store(false, Ordering::SeqCst);
    }
    debug_log(&format!(
        "pressed({mode:?}): model_loaded={}",
        state.model_loaded.load(Ordering::SeqCst)
    ));
    {
        let mut recorder = state.recorder.lock();
        if recorder.is_none() {
            let (max_secs, device_name) = {
                let s = state.settings.lock();
                (s.max_recording_secs, s.input_device.clone())
            };
            *recorder = capture::AudioRecorder::new(max_secs, &device_name).ok();
        }
        if let Some(recorder) = recorder.as_ref() {
            recorder.start();
        }
    }
    // Gemini Transcribe uses the Live API; local engines use incremental
    // segment transcription. Both stream while the hotkey is held.
    {
        let (engine_kind, api_key, language, model_id, prompt) = {
            let s = state.settings.lock();
            let engine = state
                .registry
                .find(&s.model_id)
                .map(|m| m.engine.clone())
                .unwrap_or_else(|| "whisper".to_string());
            (
                engine,
                s.gemini_api_key.clone(),
                if s.language.is_empty() || s.language == "auto" {
                    None
                } else {
                    Some(s.language.clone())
                },
                s.model_id.clone(),
                s.initial_prompt.clone(),
            )
        };
        if engine_kind == "gemini" && !api_key.is_empty() {
            let live = crate::core::live_stt::LiveSttSession::start(
                &api_key,
                &model_id,
                language.as_deref(),
                assistant.then(|| app.clone()),
            );
            let sender = live.audio_sender();
            *state.live_stt.lock() = Some(live);
            if let Some(recorder) = state.recorder.lock().as_ref() {
                recorder.set_chunk_sink(Some(sender));
            }
            debug_log("live-stt: session started");
        } else if engine_kind != "gemini" {
            let stream = crate::core::local_stream::LocalStream::start(
                state.stt.clone(),
                language,
                prompt,
                assistant.then(|| app.clone()),
            );
            let sender = stream.audio_sender();
            *state.local_stream.lock() = Some(stream);
            if let Some(recorder) = state.recorder.lock().as_ref() {
                recorder.set_chunk_sink(Some(sender));
            }
            debug_log("local-stream: session started");
        }
    }
    state.sounds.play(sounds::Chime::RecordStart);
    let _ = app.emit(EVT_RECORDING_STARTED, ());
    if assistant {
        show_llm_overlay(app, state);
        emit_llm_status(app, "listening", "", "", None, Vec::new());
        if mode == Mode::Screenshot {
            let _ = app.emit("screenshot-attached", ());
        }
    } else {
        show_overlay(app, state);
    }
    if mode == Mode::Screenshot && !state.screenshot_hold.load(Ordering::SeqCst) {
        // The hotkey was released between the selection and recording start.
        state.recording.store(false, Ordering::SeqCst);
        if let Some(recorder) = state.recorder.lock().as_ref() {
            let _ = recorder.stop();
        }
        if let Some(session) = state.live_stt.lock().take() {
            session.cancel();
        }
        if let Some(session) = state.local_stream.lock().take() {
            session.cancel();
        }
        hide_llm_overlay_delayed(app, std::time::Duration::from_millis(200));
        return;
    }
    update_tray(app, state);
}

fn on_released(app: &AppHandle, state: &Arc<AppState>, mode: Mode) {
    if mode == Mode::Screenshot {
        state.screenshot_hold.store(false, Ordering::SeqCst);
    }
    if !state.recording.load(Ordering::SeqCst) {
        if mode == Mode::Screenshot {
            cancel_screenshot_overlay(app, state);
        }
        return;
    }
    state.recording.store(false, Ordering::SeqCst);
    let assistant = mode != Mode::Record;
    let samples = state
        .recorder
        .lock()
        .as_ref()
        .map(|r| r.stop())
        .unwrap_or_default();
    let mut live = state.live_stt.lock().take();
    let mut local = state.local_stream.lock().take();
    if let Some(recorder) = state.recorder.lock().as_ref() {
        recorder.set_chunk_sink(None);
    }
    let peak = samples.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
    let _ = app.emit(EVT_RECORDING_STOPPED, ());
    update_tray(app, state);
    debug_log(&format!(
        "released({mode:?}): raw_samples={} peak={peak:.4}",
        samples.len()
    ));

    if peak < 0.001 && state.recorder.lock().is_some() {
        debug_log("recorder: no signal — will recreate stream on next press");
        *state.recorder.lock() = None;
    }

    if !state.model_loaded.load(Ordering::SeqCst) {
        let id = state.settings.lock().model_id.clone();
        debug_log("discard: model not loaded");
        let _ = app.emit(EVT_MODEL_MISSING, &id);
        if assistant {
            emit_llm_status(
                app,
                "error",
                "",
                "",
                Some("Speech model not ready yet — open Settings".into()),
                Vec::new(),
            );
            hide_llm_overlay_delayed(app, std::time::Duration::from_millis(1800));
        } else {
            hide_overlay_delayed(app, std::time::Duration::from_millis(900));
        }
        update_tray(app, state);
        if let Some(session) = live.take() {
            session.cancel();
        }
        if let Some(session) = local.take() {
            session.cancel();
        }
        return;
    }

    if assistant {
        let configured = state.gemini.lock().as_ref().is_some_and(|g| g.configured());
        if !configured {
            debug_log("ask-llm: no Gemini key configured");
            state.sounds.play(sounds::Chime::Error);
            emit_llm_status(
                app,
                "error",
                "",
                "",
                Some("No Gemini API key — add it in Settings".into()),
                Vec::new(),
            );
            hide_llm_overlay_delayed(app, std::time::Duration::from_millis(2200));
            update_tray(app, state);
            if let Some(session) = live.take() {
                session.cancel();
            }
            if let Some(session) = local.take() {
                session.cancel();
            }
            return;
        }
    }

    let trimmed = capture::trim_silence(&samples, 25);
    let (min_ms, language, prompt) = {
        let s = state.settings.lock();
        (
            s.min_recording_ms,
            s.language.clone(),
            s.initial_prompt.clone(),
        )
    };
    let min_len = min_ms as usize * capture::SAMPLE_RATE as usize / 1000;
    if trimmed.len() < min_len {
        let ms = trimmed.len() * 1000 / capture::SAMPLE_RATE as usize;
        debug_log(&format!(
            "discard: too-short trimmed_ms={ms} min_ms={min_ms} peak={peak:.4}"
        ));
        state.sounds.play(sounds::Chime::Error);
        if assistant {
            emit_llm_status(
                app,
                "error",
                "",
                "",
                Some(format!(
                    "Recording too short ({ms} ms) — speak a bit longer"
                )),
                Vec::new(),
            );
            hide_llm_overlay_delayed(app, std::time::Duration::from_millis(1200));
        } else {
            hide_overlay_delayed(app, std::time::Duration::from_millis(900));
        }
        if let Some(session) = live.take() {
            session.cancel();
        }
        if let Some(session) = local.take() {
            session.cancel();
        }
        return;
    }
    debug_log(&format!(
        "transcribe: samples={} trimmed_ms={} peak={peak:.4}",
        trimmed.len(),
        trimmed.len() * 1000 / capture::SAMPLE_RATE as usize
    ));

    if let Some(session) = live.take() {
        state.busy.store(true, Ordering::SeqCst);
        update_tray(app, state);
        let partial = session.partial();
        if assistant {
            emit_llm_status(app, "transcribing", &partial, "", None, Vec::new());
        }
        match session.finish() {
            Ok(text) if !text.trim().is_empty() => {
                let transcript = text.trim().to_string();
                debug_log(&format!(
                    "live-stt: final {} chars for {mode:?}",
                    transcript.chars().count()
                ));
                submit_streamed_transcript(app, state, mode, transcript);
                return;
            }
            Ok(_) => {
                debug_log("live-stt: empty final; falling back to batch STT");
            }
            Err(e) => {
                debug_log(&format!(
                    "live-stt: failed ({e}); falling back to batch STT"
                ));
            }
        }
    }

    if let Some(stream) = local.take() {
        state.busy.store(true, Ordering::SeqCst);
        update_tray(app, state);
        let partial = stream.partial();
        if assistant {
            emit_llm_status(app, "transcribing", &partial, "", None, Vec::new());
        }
        match stream.finish() {
            Ok(text) if !text.trim().is_empty() => {
                let transcript = text.trim().to_string();
                debug_log(&format!(
                    "local-stream: final {} chars for {mode:?}",
                    transcript.chars().count()
                ));
                submit_streamed_transcript(app, state, mode, transcript);
                return;
            }
            Ok(_) => {
                debug_log("local-stream: empty final; falling back to batch STT");
            }
            Err(e) => {
                debug_log(&format!(
                    "local-stream: failed ({e}); falling back to batch STT"
                ));
            }
        }
    }

    state.busy.store(true, Ordering::SeqCst);
    update_tray(app, state);

    // Whisper accepts only a small set of language codes; the Nemotron engine
    // accepts the full dropdown (en, pt, es, fr, ja, …) via its prompt slots.
    let engine_kind = state
        .registry
        .find(&state.settings.lock().model_id)
        .map(|m| m.engine.clone())
        .unwrap_or_else(|| "whisper".to_string());
    let language = match language.as_str() {
        "auto" | "" => None,
        code if engine_kind == "nemotron" || engine_kind == "gemini" => Some(code.to_string()),
        "en" | "pt" => Some(language),
        _ => None,
    };
    if let Some(tx) = state.job_tx.lock().as_ref() {
        let session_id = state.agent.active_session_id();
        let job = if mode == Mode::Screenshot {
            Job::AskScreenshot(
                trimmed,
                language,
                prompt,
                session_id,
                state.pending_screenshot.lock().take(),
            )
        } else if assistant {
            // Snapshot the session now: the user may have clicked "New session"
            // while recording, and that choice must apply to this utterance.
            Job::AskLlm(trimmed, language, prompt, session_id)
        } else {
            Job::Transcribe(trimmed, language, prompt)
        };
        let _ = tx.send(job);
    } else {
        state.busy.store(false, Ordering::SeqCst);
        emit_error(app, "internal error: worker not ready");
    }
}

fn submit_streamed_transcript(
    app: &AppHandle,
    state: &Arc<AppState>,
    mode: Mode,
    transcript: String,
) {
    let session_id = state.agent.active_session_id();
    if mode != Mode::Record && state.cancel_pending.swap(false, Ordering::SeqCst) {
        emit_llm_status(app, "cancelled", &transcript, "", None, Vec::new());
        hide_llm_overlay_delayed(app, std::time::Duration::from_millis(500));
        state.busy.store(false, Ordering::SeqCst);
        return;
    }
    if let Some(tx) = state.job_tx.lock().as_ref() {
        let job = match mode {
            Mode::Screenshot => Job::AskScreenshotText(
                transcript,
                session_id,
                state.pending_screenshot.lock().take(),
            ),
            Mode::AskLlm => Job::AskText(transcript, session_id),
            Mode::Record => Job::RecordedText(transcript),
        };
        let _ = tx.send(job);
    } else {
        state.busy.store(false, Ordering::SeqCst);
        emit_error(app, "internal error: worker not ready");
    }
}

fn deliver_recorded_text(
    app: &AppHandle,
    state: &Arc<AppState>,
    text: &str,
    language: &str,
    inference_ms: u64,
    n_segments: i32,
) {
    if text.trim().is_empty() {
        debug_log("stt: empty transcript");
        state.sounds.play(sounds::Chime::Error);
        hide_overlay_delayed(app, std::time::Duration::from_millis(900));
        return;
    }
    debug_log(&format!("stt: text='{}' lang={}", text.trim(), language));
    {
        let mut recent = state.recent.lock();
        recent.insert(0, text.to_string());
        recent.truncate(MAX_RECENT);
    }
    match paste_text_on_main(app, text) {
        Ok(outcome) if outcome.pasted => {
            state.sounds.play(sounds::Chime::PasteDone);
            let _ = app.emit(
                EVT_TRANSCRIPT,
                TranscriptDto {
                    text: text.to_string(),
                    language: language.to_string(),
                    inference_ms,
                    n_segments,
                },
            );
            hide_overlay_delayed(app, std::time::Duration::from_millis(150));
        }
        Ok(outcome) if outcome.skipped_secure => {
            debug_log("paste: skipped (secure field)");
            let _ = app.emit(EVT_SECURE_SKIPPED, ());
            hide_overlay_delayed(app, std::time::Duration::from_millis(900));
        }
        Ok(_) => {
            debug_log("paste: no-op (empty)");
            hide_overlay_delayed(app, std::time::Duration::from_millis(900));
        }
        Err(e) => {
            handle_paste_failure(app, state, text, &e);
        }
    }
}

/// Ask-LLM pipeline (runs on the STT worker thread): transcribe the recording
/// and admit it to the durable agent inbox. The agent worker performs the
/// model calls, tools, permissions, streaming speech and recovery.
fn run_ask_llm(
    app: &AppHandle,
    state: &Arc<AppState>,
    samples: Vec<f32>,
    language: Option<String>,
    prompt: String,
    session_id: Option<String>,
) {
    emit_llm_status(app, "transcribing", "", "", None, Vec::new());
    let result = state.stt.transcribe(&samples, language.as_deref(), &prompt);
    let transcript = match result {
        Ok(transcription) if transcription.text.trim().is_empty() => {
            debug_log("ask-llm: empty transcript");
            state.sounds.play(sounds::Chime::Error);
            emit_llm_status(
                app,
                "error",
                "",
                "",
                Some("Nothing was heard — try again".into()),
                Vec::new(),
            );
            hide_llm_overlay_delayed(app, std::time::Duration::from_millis(1200));
            return;
        }
        Ok(transcription) => transcription.text.trim().to_string(),
        Err(e) => {
            debug_log(&format!("ask-llm: stt error -> {e}"));
            emit_error(app, &e);
            emit_llm_status(app, "error", "", "", Some(e.to_string()), Vec::new());
            hide_llm_overlay_delayed(app, std::time::Duration::from_millis(1800));
            return;
        }
    };
    if state.cancel_pending.swap(false, Ordering::SeqCst) {
        debug_log("ask-llm: cancelled after transcribe");
        emit_llm_status(app, "cancelled", &transcript, "", None, Vec::new());
        hide_llm_overlay_delayed(app, std::time::Duration::from_millis(500));
        return;
    }
    {
        let mut recent = state.recent.lock();
        recent.insert(0, transcript.clone());
        recent.truncate(MAX_RECENT);
    }
    debug_log(&format!("ask-llm: transcript '{}'", transcript));
    let id = state
        .agent
        .admit_user_text(session_id.as_deref(), &transcript, None, "steer", false);
    debug_log(&format!("ask-llm: admitted to session {id}"));
    let _ = app.emit(EVT_LLM_SESSION, ());
    update_tray(app, state);
}

/// Stop whatever the Ask-Gemini flow is currently doing: an active recording,
/// reply speech playback, or a phase that is still running (transcribe / chat
/// / search). The pipeline is flagged to abort at the next phase boundary,
/// and the panel switches to "Cancelling…" immediately so the cancel is
/// visible even while a blocking network call cannot be interrupted.
fn cancel_ask_flow(app: &AppHandle, state: &Arc<AppState>) {
    if let Some(session) = state.live_stt.lock().take() {
        session.cancel();
    }
    if let Some(session) = state.local_stream.lock().take() {
        session.cancel();
    }
    if let Some(recorder) = state.recorder.lock().as_ref() {
        recorder.set_chunk_sink(None);
    }
    if state.recording.load(Ordering::SeqCst) {
        debug_log("cancel: stopping recording");
        state.recording.store(false, Ordering::SeqCst);
        if let Some(recorder) = state.recorder.lock().as_ref() {
            let _ = recorder.stop(); // discard samples
        }
        state.sounds.play(sounds::Chime::Error);
        let _ = app.emit(EVT_RECORDING_STOPPED, ());
        emit_llm_status(app, "cancelled", "", "", None, Vec::new());
        hide_llm_overlay(app);
        update_tray(app, state);
    }
    state.cancel_pending.store(true, Ordering::SeqCst);
    state.agent.stop();
    if let Some(speech) = state.speech.lock().as_ref() {
        speech.cancel();
    } else if !state.recording.load(Ordering::SeqCst) {
        // Nothing interruptible is running yet (transcribing / thinking /
        // searching): acknowledge the cancel immediately.
        emit_llm_status(app, "cancelling", "", "", None, Vec::new());
    }
}

fn emit_llm_status(
    app: &AppHandle,
    phase: &str,
    transcript: &str,
    reply: &str,
    error: Option<String>,
    sources: Vec<String>,
) {
    let _ = app.emit(
        EVT_LLM_STATUS,
        LlmStatusDto {
            phase: phase.to_string(),
            transcript: transcript.to_string(),
            reply: reply.to_string(),
            error,
            sources,
        },
    );
}

fn update_tray(app: &AppHandle, state: &AppState) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(menu) = build_menu(app, state) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn build_menu(app: &AppHandle, state: &AppState) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    if state.gated.load(Ordering::SeqCst) {
        let missing = if !all_permissions_granted() {
            let mut parts = Vec::new();
            if mic::mic_permission() != mic::MicPermission::Authorized {
                parts.push("Microphone");
            }
            if !paste::accessibility_trusted() {
                parts.push("Accessibility");
            }
            format!(" (missing: {})", parts.join(" + "))
        } else {
            String::new()
        };
        let status_text = format!("Permissions required{missing} — see the window");
        let status = MenuItem::with_id(app, "status", status_text, false, None::<&str>)?;
        let quit = PredefinedMenuItem::quit(app, Some("Quit VoiceKeyboard"))?;
        return Menu::with_items(app, &[&status, &quit]);
    }

    let status_text = if state.recording.load(Ordering::SeqCst) {
        "Recording…".to_string()
    } else if state.agent.is_speaking() {
        "Speaking…".to_string()
    } else if state.agent.is_active() {
        "Agent working…".to_string()
    } else if state.busy.load(Ordering::SeqCst) {
        if state.llm_mode.load(Ordering::SeqCst) {
            "Asking Gemini…".to_string()
        } else {
            "Transcribing…".to_string()
        }
    } else if let Some((done, total)) = state.download_progress.lock().as_ref() {
        if *total > 0 {
            format!("Downloading model… {}%", done * 100 / total)
        } else {
            "Downloading model…".to_string()
        }
    } else if !state.model_loaded.load(Ordering::SeqCst) {
        "Model not downloaded — open Settings".to_string()
    } else if mic::mic_permission() != mic::MicPermission::Authorized {
        "Microphone permission needed — open Settings".to_string()
    } else if !paste::accessibility_trusted() {
        "Accessibility needed — open Settings".to_string()
    } else if !state.hotkey_ok.load(Ordering::SeqCst) {
        "Hotkey failed — open Settings".to_string()
    } else if !state.hotkey_llm_ok.load(Ordering::SeqCst) {
        "Ask-LLM hotkey failed — open Settings".to_string()
    } else {
        let s = state.settings.lock();
        let gemini_hint = if s.gemini_api_key.is_empty() {
            " · no Gemini key".to_string()
        } else {
            String::new()
        };
        format!(
            "Ready — hold {} to record · {} to ask Gemini · {} for a screenshot{gemini_hint}",
            hotkey::display(&s.hotkey),
            hotkey::display(&s.llm_hotkey),
            hotkey::display(&s.screenshot_hotkey)
        )
    };
    let status = MenuItem::with_id(app, "status", status_text, false, None::<&str>)?;
    let sessions_menu = build_sessions_submenu(app, state)?;
    let open = MenuItem::with_id(app, "settings", "Settings…", true, Some("CmdOrCtrl+,"))?;
    let about = MenuItem::with_id(app, "about", "About VoiceKeyboard…", true, None::<&str>)?;
    let quit = PredefinedMenuItem::quit(app, Some("Quit VoiceKeyboard"))?;
    Menu::with_items(app, &[&status, &sessions_menu, &open, &about, &quit])
}

/// The "Sessions" tray submenu: start a clean context, recall any of the most
/// recent conversations (checked = the one the next question continues), or
/// open the full list in Settings.
///
/// Note: session data is snapshotted *before* any menu item is created. Menu
/// item creation dispatches to the main thread, and a webview command running
/// there (e.g. `get_active_session`) may need the sessions lock — holding it
/// here would deadlock the app.
fn build_sessions_submenu(app: &AppHandle, state: &AppState) -> tauri::Result<Submenu<tauri::Wry>> {
    let recent_sessions = state.agent.sessions(SESSIONS_IN_MENU + 1);
    let active_id = recent_sessions
        .iter()
        .find(|s| s.active)
        .map(|s| s.id.clone());
    let recent: Vec<(String, String)> = recent_sessions
        .iter()
        .take(SESSIONS_IN_MENU)
        .map(|s| (s.id.clone(), short_title(&s.title, 48)))
        .collect();
    let submenu = Submenu::with_id(app, "sessions-menu", "Sessions", true)?;
    let new_item = CheckMenuItem::with_id(
        app,
        "session:new",
        "New Session (clean context)",
        true,
        active_id.is_none(),
        None::<&str>,
    )?;
    submenu.append(&new_item)?;
    submenu.append(&PredefinedMenuItem::separator(app)?)?;
    if recent.is_empty() {
        let empty = MenuItem::with_id(app, "session:none", "No sessions yet", false, None::<&str>)?;
        submenu.append(&empty)?;
    } else {
        for (id, title) in &recent {
            let item = CheckMenuItem::with_id(
                app,
                format!("session:{id}"),
                title,
                true,
                active_id.as_deref() == Some(id.as_str()),
                None::<&str>,
            )?;
            submenu.append(&item)?;
        }
    }
    submenu.append(&PredefinedMenuItem::separator(app)?)?;
    let manage = MenuItem::with_id(
        app,
        "sessions-manage",
        "Manage Sessions…",
        true,
        None::<&str>,
    )?;
    submenu.append(&manage)?;
    Ok(submenu)
}

/// Switch to a clean context: the next Ask-LLM question starts a new session.
fn start_new_session(app: &AppHandle, state: &Arc<AppState>) {
    state.agent.new_session();
    debug_log("sessions: new context (next question starts a session)");
    let _ = app.emit(EVT_LLM_SESSION, ());
    update_tray(app, state);
}

/// Make `id` the session the next Ask-LLM question continues.
fn activate_session(app: &AppHandle, state: &Arc<AppState>, id: &str) {
    if state.agent.set_active(id) {
        debug_log(&format!("sessions: active = {id}"));
        let _ = app.emit(EVT_LLM_SESSION, ());
        update_tray(app, state);
    }
}

fn short_title(title: &str, max_chars: usize) -> String {
    let title = title.trim();
    if title.chars().count() <= max_chars {
        return title.to_string();
    }
    let mut short: String = title.chars().take(max_chars.saturating_sub(1)).collect();
    short.push('…');
    short
}

/// Open the Settings window's Sessions tab (the window may have just been
/// created, so give the webview a moment to load before nudging it).
fn open_sessions_tab(app: &AppHandle) {
    let handle = app.clone();
    std::thread::Builder::new()
        .name("open-sessions-tab".into())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(900));
            let _ = handle.emit(EVT_OPEN_SESSIONS, ());
        })
        .ok();
}

fn open_settings(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    let win = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("VoiceKeyboard")
        .inner_size(560.0, 720.0)
        .resizable(true)
        .build();
    if let Ok(win) = win {
        let handle = win.clone();
        win.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = handle.hide();
            }
        });
        let _ = win.set_focus();
    }
}

/// True once every required permission is granted (mic + accessibility).
fn all_permissions_granted() -> bool {
    mic::mic_permission() == mic::MicPermission::Authorized && paste::accessibility_trusted()
}

/// The permissions-gate window: shown until the user grants BOTH required
/// permissions. It only offers to open System Settings or quit — no other
/// functionality is reachable while the app is gated.
fn open_gate(app: &AppHandle) {
    let existing = app.get_webview_window(GATE_ID);
    if let Some(win) = existing {
        let _ = win.set_always_on_top(true);
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    let win = WebviewWindowBuilder::new(app, GATE_ID, WebviewUrl::App("perm.html".into()))
        .title("VoiceKeyboard — Permissions required")
        .inner_size(480.0, 400.0)
        .resizable(false)
        .always_on_top(true)
        .build();
    if let Ok(win) = win {
        let handle = win.clone();
        win.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = handle.hide();
            }
        });
        let _ = win.set_focus();
    }
}

/// Close (destroy) the gate window once permissions are granted.
fn close_gate_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(GATE_ID) {
        let _ = win.destroy();
    }
}

fn open_about(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("about") {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    let win = WebviewWindowBuilder::new(app, "about", WebviewUrl::App("about.html".into()))
        .title("About VoiceKeyboard")
        .inner_size(380.0, 460.0)
        .resizable(false)
        .build();
    if let Ok(win) = win {
        let handle = win.clone();
        win.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = handle.hide();
            }
        });
        let _ = win.set_focus();
    }
}

/// Restore a user-moved panel position if it is still on a connected monitor.
fn apply_saved_overlay_position(
    app: &AppHandle,
    win: &tauri::WebviewWindow<tauri::Wry>,
    state: &AppState,
    llm: bool,
) -> bool {
    let position = {
        let positions = state.overlay_positions.lock();
        if llm {
            positions.llm
        } else {
            positions.recording
        }
    };
    let Some(position) = position else {
        return false;
    };
    let size = win.outer_size().unwrap_or(tauri::PhysicalSize::new(1, 1));
    let visible = app
        .available_monitors()
        .map(|monitors| {
            monitors.iter().any(|monitor| {
                let origin = monitor.position();
                let area = monitor.size();
                position.x + size.width as i32 > origin.x
                    && position.x < origin.x + area.width as i32
                    && position.y + size.height as i32 > origin.y
                    && position.y < origin.y + area.height as i32
            })
        })
        .unwrap_or(false);
    if !visible {
        return false;
    }
    let _ = win.set_position(PhysicalPosition::new(position.x, position.y));
    true
}

fn show_overlay(app: &AppHandle, state: &Arc<AppState>) {
    let handle = app.clone();
    let state = state.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(win) = ensure_overlay(&handle, &state) else {
            return;
        };
        if !apply_saved_overlay_position(&handle, &win, &state, false) {
            position_overlay_bottom_center(&win);
        }
        let _ = win.set_ignore_cursor_events(true);
        let _ = win.show();
    });
}

fn hide_overlay_delayed(app: &AppHandle, delay: std::time::Duration) {
    let handle = app.clone();
    std::thread::Builder::new()
        .name("overlay-hide".into())
        .spawn(move || {
            std::thread::sleep(delay);
            let handle2 = handle.clone();
            let _ = handle.run_on_main_thread(move || {
                if let Some(win) = handle2.get_webview_window(OVERLAY_ID) {
                    let _ = win.hide();
                }
            });
        })
        .ok();
}

fn ensure_overlay(app: &AppHandle, state: &AppState) -> Option<tauri::WebviewWindow<tauri::Wry>> {
    if let Some(win) = state.overlay.lock().as_ref() {
        return Some(win.clone());
    }
    let win = WebviewWindowBuilder::new(app, OVERLAY_ID, WebviewUrl::App("overlay.html".into()))
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .visible(false)
        .build()
        .ok()?;
    let _ = win.set_size(tauri::LogicalSize::new(OVERLAY_W, OVERLAY_H));
    if !apply_saved_overlay_position(app, &win, state, false) {
        position_overlay_bottom_center(&win);
    }
    debug_log(&format!(
        "overlay: created {}x{}",
        OVERLAY_W as u32, OVERLAY_H as u32
    ));
    *state.overlay.lock() = Some(win.clone());
    Some(win)
}

fn position_overlay_bottom_center(win: &tauri::WebviewWindow<tauri::Wry>) {
    let (w, h) = main_screen_logical();
    let x = (w - OVERLAY_W) / 2.0;
    let y = h - OVERLAY_H - OVERLAY_BOTTOM_MARGIN;
    let _ = win.set_position(Position::Logical(LogicalPosition::new(
        x.max(0.0),
        y.max(0.0),
    )));
}

pub(crate) fn show_llm_overlay(app: &AppHandle, state: &Arc<AppState>) {
    let handle = app.clone();
    let state = state.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(win) = ensure_llm_overlay(&handle, &state) else {
            return;
        };
        // Taller panel when there is conversation to recall, so past turns can
        // be shown above the live one.
        let height = LLM_OVERLAY_H;
        let _ = win.set_size(tauri::LogicalSize::new(LLM_OVERLAY_W, height));
        if !apply_saved_overlay_position(&handle, &win, &state, true) {
            position_llm_overlay_bottom_center(&win, height);
        }
        let _ = win.show();
        // Focus the panel so its Cancel button and Esc handling work. The
        // user is speaking, not typing, so stealing focus is acceptable.
        let _ = win.set_focus();
    });
}

fn show_screenshot_overlay(app: &AppHandle, state: &Arc<AppState>) {
    if state.screenshot_selecting.swap(true, Ordering::SeqCst) {
        return;
    }
    *state.pending_screenshot.lock() = None;
    let handle = app.clone();
    let state = state.clone();
    let _ = app.run_on_main_thread(move || {
        let win = if let Some(win) = handle.get_webview_window(SHOT_ID) {
            win
        } else {
            match WebviewWindowBuilder::new(&handle, SHOT_ID, WebviewUrl::App("shot.html".into()))
                .decorations(false)
                .transparent(true)
                .shadow(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .resizable(false)
                .visible(false)
                .build()
            {
                Ok(win) => win,
                Err(e) => {
                    state.screenshot_selecting.store(false, Ordering::SeqCst);
                    debug_log(&format!("screenshot overlay: create failed -> {e}"));
                    emit_error(&handle, &format!("Could not open screenshot overlay: {e}"));
                    return;
                }
            }
        };
        if let Ok(monitors) = handle.available_monitors() {
            let cursor = handle.cursor_position().ok();
            let monitor = cursor
                .and_then(|cursor| {
                    monitors.iter().find(|monitor| {
                        let origin = monitor.position();
                        let size = monitor.size();
                        cursor.x >= origin.x as f64
                            && cursor.y >= origin.y as f64
                            && cursor.x < (origin.x + size.width as i32) as f64
                            && cursor.y < (origin.y + size.height as i32) as f64
                    })
                })
                .or_else(|| monitors.iter().find(|monitor| monitor.position().x == 0))
                .or_else(|| monitors.first());
            if let Some(monitor) = monitor {
                let origin = monitor.position();
                let size = monitor.size();
                let _ = win.set_position(PhysicalPosition::new(origin.x, origin.y));
                let _ = win.set_size(PhysicalSize::new(size.width, size.height));
            }
        }
        let _ = win.set_ignore_cursor_events(false);
        let _ = win.show();
        let _ = win.set_focus();
        debug_log("screenshot overlay: shown");
    });
}

fn cancel_screenshot_overlay(app: &AppHandle, state: &AppState) {
    state.screenshot_selecting.store(false, Ordering::SeqCst);
    state.screenshot_hold.store(false, Ordering::SeqCst);
    if let Some(win) = app.get_webview_window(SHOT_ID) {
        let _ = win.hide();
    }
    debug_log("screenshot overlay: cancelled");
}

#[tauri::command]
fn cancel_screenshot(app: AppHandle, state: State<'_, Arc<AppState>>) {
    cancel_screenshot_overlay(&app, state.inner());
    if let Some(session) = state.live_stt.lock().take() {
        session.cancel();
    }
    if let Some(session) = state.local_stream.lock().take() {
        session.cancel();
    }
}

#[tauri::command]
fn screenshot_region_selected(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<(), String> {
    if !state.screenshot_selecting.swap(false, Ordering::SeqCst) {
        return Err("screenshot selection is no longer active".into());
    }
    let win = app
        .get_webview_window(SHOT_ID)
        .ok_or_else(|| "screenshot overlay is gone".to_string())?;
    let _ = win.hide();
    // Let the compositor remove the overlay before capturing the screen.
    std::thread::sleep(std::time::Duration::from_millis(140));

    let screens = screenshots::Screen::all().map_err(|e| format!("Could not list screens: {e}"))?;
    let scale = win.scale_factor().unwrap_or(1.0);
    let origin = win.outer_position().unwrap_or(PhysicalPosition::new(0, 0));
    let logical_x = (origin.x as f64 / scale).round() as i32;
    let logical_y = (origin.y as f64 / scale).round() as i32;
    let screen = screenshots::Screen::from_point(logical_x, logical_y)
        .ok()
        .or_else(|| {
            screens
                .iter()
                .find(|screen| screen.display_info.is_primary)
                .cloned()
        })
        .or_else(|| screens.first().cloned())
        .ok_or_else(|| "No screen available".to_string())?;
    let image = screen
        .capture_area(
            x.round() as i32,
            y.round() as i32,
            width.round().max(1.0) as u32,
            height.round().max(1.0) as u32,
        )
        .map_err(|e| format!("Could not capture the screen: {e}"))?;
    let dir = crate::app_data_dir().join("screenshots");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("shot_{}.png", crate::core::agent::types::now_ms()));
    image
        .save(&path)
        .map_err(|e| format!("Could not save the screenshot: {e}"))?;
    debug_log(&format!("screenshot: saved {}", path.display()));
    *state.pending_screenshot.lock() = Some(path.to_string_lossy().to_string());

    if !state.screenshot_hold.load(Ordering::SeqCst) {
        // The hotkey was released while the selection was being captured.
        *state.pending_screenshot.lock() = None;
        return Ok(());
    }
    begin_capture(&app, state.inner(), Mode::Screenshot);
    Ok(())
}

fn hide_llm_overlay(app: &AppHandle) {
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(win) = handle.get_webview_window(LLM_OVERLAY_ID) {
            let _ = win.hide();
        }
    });
}

fn hide_llm_overlay_delayed(app: &AppHandle, delay: std::time::Duration) {
    let handle = app.clone();
    std::thread::Builder::new()
        .name("llm-overlay-hide".into())
        .spawn(move || {
            std::thread::sleep(delay);
            hide_llm_overlay(&handle);
        })
        .ok();
}

fn ensure_llm_overlay(
    app: &AppHandle,
    state: &AppState,
) -> Option<tauri::WebviewWindow<tauri::Wry>> {
    if let Some(win) = state.llm_overlay.lock().as_ref() {
        return Some(win.clone());
    }
    let win = WebviewWindowBuilder::new(app, LLM_OVERLAY_ID, WebviewUrl::App("llm.html".into()))
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .visible(false)
        .build()
        .ok()?;
    let _ = win.set_size(tauri::LogicalSize::new(LLM_OVERLAY_W, LLM_OVERLAY_H));
    if !apply_saved_overlay_position(app, &win, state, true) {
        position_llm_overlay_bottom_center(&win, LLM_OVERLAY_H);
    }
    debug_log(&format!(
        "llm-overlay: created {}x{}",
        LLM_OVERLAY_W as u32, LLM_OVERLAY_H as u32
    ));
    *state.llm_overlay.lock() = Some(win.clone());
    Some(win)
}

fn position_llm_overlay_bottom_center(win: &tauri::WebviewWindow<tauri::Wry>, height: f64) {
    let (w, h) = main_screen_logical();
    let x = (w - LLM_OVERLAY_W) / 2.0;
    let y = h - height - OVERLAY_BOTTOM_MARGIN;
    let _ = win.set_position(Position::Logical(LogicalPosition::new(
        x.max(0.0),
        y.max(0.0),
    )));
}

#[cfg(target_os = "macos")]
fn main_screen_logical() -> (f64, f64) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;
    if let Some(mtm) = MainThreadMarker::new() {
        if let Some(screen) = NSScreen::mainScreen(mtm) {
            let frame = screen.frame();
            return (frame.size.width, frame.size.height);
        }
    }
    (1440.0, 900.0)
}

#[cfg(not(target_os = "macos"))]
fn main_screen_logical() -> (f64, f64) {
    (1440.0, 900.0)
}

fn emit_error(app: &AppHandle, message: &str) {
    debug_log(&format!("error: {message}"));
    let _ = app.emit(EVT_ERROR, message);
}

pub(crate) fn debug_log(msg: &str) {
    use std::io::Write;
    let path = crate::app_data_dir().join("voicekeyboard.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(file, "[{ts}] {msg}");
    }
}

fn tray_icon() -> tauri::image::Image<'static> {
    tauri::image::Image::from_bytes(include_bytes!("../icons/icon.png"))
        .expect("embedded tray icon is valid")
}

#[tauri::command]
fn get_status(state: State<'_, Arc<AppState>>) -> StatusDto {
    let s = state.settings.lock();
    StatusDto {
        recording: state.recording.load(Ordering::SeqCst),
        busy: state.busy.load(Ordering::SeqCst),
        model_loaded: state.model_loaded.load(Ordering::SeqCst),
        model_id: s.model_id.clone(),
        hotkey: hotkey::display(&s.hotkey),
        llm_hotkey: hotkey::display(&s.llm_hotkey),
        gemini_configured: !s.gemini_api_key.is_empty(),
        accessibility_trusted: paste::accessibility_trusted(),
    }
}

#[tauri::command]
fn get_settings(state: State<'_, Arc<AppState>>) -> settings::Settings {
    state.settings.lock().clone()
}

#[tauri::command]
fn get_permissions(state: State<'_, Arc<AppState>>) -> PermissionsDto {
    // If the user granted mic access (e.g. fixed it in System Settings after
    // the app was denied/undetermined), rebuild the recorder so the device
    // becomes available.
    if mic::mic_permission() == mic::MicPermission::Authorized && state.recorder.lock().is_none() {
        rebuild_recorder(state.inner());
    }
    PermissionsDto {
        accessibility_trusted: paste::accessibility_trusted(),
        has_input_device: state.recorder.lock().is_some(),
        mic_permission: match mic::mic_permission() {
            mic::MicPermission::Authorized => "authorized".into(),
            mic::MicPermission::Denied => "denied".into(),
            mic::MicPermission::NotDetermined => "notDetermined".into(),
        },
        running_from_bundle: mic::running_from_bundle(),
    }
}

#[tauri::command]
fn request_accessibility(app: AppHandle) -> bool {
    let (tx, rx) = std::sync::mpsc::sync_channel::<bool>(1);
    let _ = app.run_on_main_thread(move || {
        let _ = tx.send(paste::request_accessibility_permission());
    });
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .unwrap_or(false)
}

#[tauri::command]
fn request_microphone(app: AppHandle, state: State<'_, Arc<AppState>>) -> String {
    let status = mic::mic_permission();
    if status == mic::MicPermission::NotDetermined {
        // Must fire on the main thread or macOS will not show the prompt.
        let handle = app.clone();
        let state2 = state.inner().clone();
        let _ = app.run_on_main_thread(move || {
            mic::request_mic_permission();
            let st = mic::mic_permission();
            debug_log(&format!("mic: request result = {st:?}"));
            if st == mic::MicPermission::Authorized {
                rebuild_recorder(&state2);
                let _ = handle.emit(EVT_SETTINGS_CHANGED, ());
            }
        });
        // Poll in the background; once the user answers, rebuild the recorder.
        let handle = app.clone();
        let state2 = state.inner().clone();
        std::thread::spawn(move || {
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if mic::mic_permission() == mic::MicPermission::Authorized {
                    debug_log("mic: granted while waiting");
                    rebuild_recorder(&state2);
                    let _ = handle.emit(EVT_SETTINGS_CHANGED, ());
                    break;
                }
            }
        });
        "requested".into()
    } else {
        match status {
            mic::MicPermission::Authorized => "authorized".into(),
            mic::MicPermission::Denied => "denied".into(),
            mic::MicPermission::NotDetermined => "notDetermined".into(),
        }
    }
}

#[tauri::command]
fn reset_microphone(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<String, String> {
    mic::reset_mic_permission()?;
    debug_log("mic: TCC reset done — requesting fresh prompt");
    let handle = app.clone();
    let state2 = state.inner().clone();
    let _ = app.run_on_main_thread(move || {
        mic::request_mic_permission();
        let st = mic::mic_permission();
        debug_log(&format!("mic: status after reset+request = {st:?}"));
        if st == mic::MicPermission::Authorized {
            rebuild_recorder(&state2);
            let _ = handle.emit(EVT_SETTINGS_CHANGED, ());
        }
    });
    let handle = app.clone();
    let state2 = state.inner().clone();
    std::thread::spawn(move || {
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if mic::mic_permission() == mic::MicPermission::Authorized {
                debug_log("mic: granted after reset");
                rebuild_recorder(&state2);
                let _ = handle.emit(EVT_SETTINGS_CHANGED, ());
                break;
            }
        }
    });
    Ok(match mic::mic_permission() {
        mic::MicPermission::Authorized => "authorized".into(),
        mic::MicPermission::Denied => "denied".into(),
        mic::MicPermission::NotDetermined => "requested".into(),
    })
}

#[tauri::command]
fn open_mic_settings() {
    mic::open_mic_settings();
}

#[tauri::command]
fn set_llm_overlay_expanded(state: State<'_, Arc<AppState>>, expanded: bool) {
    let win = state.llm_overlay.lock().clone();
    let Some(win) = win else {
        return;
    };
    let height = if expanded {
        LLM_OVERLAY_H_EXPANDED
    } else {
        LLM_OVERLAY_H
    };
    let _ = win.set_size(tauri::LogicalSize::new(LLM_OVERLAY_W, height));
    let app = win.app_handle().clone();
    if !apply_saved_overlay_position(&app, &win, state.inner(), true) {
        position_llm_overlay_bottom_center(&win, height);
    }
    clamp_overlay_to_monitor(&win);
}

fn clamp_overlay_to_monitor(win: &tauri::WebviewWindow<tauri::Wry>) {
    let (Ok(position), Ok(size), Ok(Some(monitor))) = (
        win.outer_position(),
        win.outer_size(),
        win.current_monitor(),
    ) else {
        return;
    };
    let origin = monitor.position();
    let area = monitor.size();
    let max_x = origin.x + area.width as i32 - size.width as i32;
    let max_y = origin.y + area.height as i32 - size.height as i32;
    let x = position.x.clamp(origin.x, max_x.max(origin.x));
    let y = position.y.clamp(origin.y, max_y.max(origin.y));
    if x != position.x || y != position.y {
        let _ = win.set_position(PhysicalPosition::new(x, y));
    }
}

#[tauri::command]
fn save_overlay_position(state: State<'_, Arc<AppState>>, panel: String, x: i32, y: i32) {
    let mut positions = state.overlay_positions.lock();
    let position = Some(StoredPosition { x, y });
    match panel.as_str() {
        "llm" => positions.llm = position,
        "recording" => positions.recording = position,
        _ => return,
    }
    save_overlay_positions(&positions);
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

fn rebuild_recorder(state: &AppState) {
    let (max_secs, device_name) = {
        let s = state.settings.lock();
        (s.max_recording_secs, s.input_device.clone())
    };
    let rebuilt = capture::AudioRecorder::new(max_secs, &device_name).ok();
    if rebuilt.is_some() {
        debug_log("recorder: rebuilt");
        *state.recorder.lock() = rebuilt;
    } else {
        debug_log("recorder: rebuild failed (still no device)");
    }
}

#[tauri::command]
fn get_audio_level(state: State<'_, Arc<AppState>>) -> u32 {
    state
        .recorder
        .lock()
        .as_ref()
        .map(|r| r.level())
        .unwrap_or(0)
}

#[tauri::command]
fn get_inference_stats(state: State<'_, Arc<AppState>>) -> stt::InferenceStats {
    state.stt.stats()
}

#[tauri::command]
fn get_input_devices() -> Vec<String> {
    capture::list_input_devices()
}

#[tauri::command]
fn get_recent_transcriptions(state: State<'_, Arc<AppState>>) -> Vec<String> {
    state.recent.lock().clone()
}

#[tauri::command]
fn copy_transcription(
    _app: AppHandle,
    state: State<'_, Arc<AppState>>,
    index: usize,
) -> Result<(), String> {
    let text = {
        let recent = state.recent.lock();
        recent.get(index).cloned().ok_or("invalid index")?
    };
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.set_text(&text).map_err(|e| e.to_string())?;
    state.sounds.play(sounds::Chime::PasteDone);
    Ok(())
}

/// Cancel the active Ask-Gemini gesture: an in-progress recording, or reply
/// speech playback. Called from the LLM panel's Cancel button and Esc.
#[tauri::command]
fn cancel_recording(app: AppHandle, state: State<'_, Arc<AppState>>) {
    cancel_ask_flow(&app, state.inner());
}

/// Fetch the Gemini models/voices catalog for the Settings pickers. Uses the
/// provided key, falling back to the saved one. Voices are filtered by the
/// given app language (falls back to the saved setting; "auto" fetches all).
#[tauri::command]
fn get_gemini_catalog(
    state: State<'_, Arc<AppState>>,
    api_key: Option<String>,
    language: Option<String>,
) -> Result<gemini::GeminiCatalog, String> {
    let key = api_key
        .filter(|k| !k.trim().is_empty())
        .or_else(|| {
            let s = state.settings.lock();
            (!s.gemini_api_key.trim().is_empty()).then(|| s.gemini_api_key.clone())
        })
        .ok_or_else(|| "Enter a Gemini API key first".to_string())?;
    let lang = language.filter(|l| !l.trim().is_empty()).or_else(|| {
        let s = state.settings.lock();
        (!s.language.trim().is_empty()).then(|| s.language.clone())
    });
    let locale = lang.and_then(|l| gemini::gemini_locale(&l));
    gemini::GeminiClient::fetch_catalog(&key, locale.as_deref()).map_err(|e| e.to_string())
}

/// Sessions, most recently updated first, with the active one flagged.
#[tauri::command]
fn list_sessions(state: State<'_, Arc<AppState>>) -> Vec<SessionMetaDto> {
    state
        .agent
        .sessions(500)
        .into_iter()
        .map(|s| SessionMetaDto {
            id: s.id,
            title: s.title,
            updated_at: s.updated_at,
            turns: s.turns,
            active: s.active,
        })
        .collect()
}

#[tauri::command]
fn get_session(state: State<'_, Arc<AppState>>, id: String) -> Option<serde_json::Value> {
    state.agent.session_detail(&id)
}

/// The session the next Ask-LLM question will continue, if any. `None` means
/// the question will start a fresh session (clean context).
#[tauri::command]
fn get_active_session(state: State<'_, Arc<AppState>>) -> Option<serde_json::Value> {
    let active = state.agent.active_session_id()?;
    state.agent.session_detail(&active)
}

/// Start a clean context for the next question.
#[tauri::command]
fn new_session(app: AppHandle, state: State<'_, Arc<AppState>>) {
    start_new_session(&app, state.inner());
}

#[tauri::command]
fn set_active_session(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<(), String> {
    if !state.agent.set_active(&id) {
        return Err("session not found".into());
    }
    let _ = app.emit(EVT_LLM_SESSION, ());
    update_tray(&app, state.inner());
    Ok(())
}

#[tauri::command]
fn rename_session(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    id: String,
    title: String,
) -> Result<(), String> {
    if !state.agent.rename_session(&id, &title) {
        return Err("session not found".into());
    }
    let _ = app.emit(EVT_LLM_SESSION, ());
    update_tray(&app, state.inner());
    Ok(())
}

#[tauri::command]
fn delete_session(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    id: String,
) -> Result<(), String> {
    if state.agent.db.session(&id).is_none() {
        return Err("session not found".into());
    }
    state.agent.delete_session(&id);
    debug_log(&format!("sessions: deleted {id}"));
    let _ = app.emit(EVT_LLM_SESSION, ());
    update_tray(&app, state.inner());
    Ok(())
}

#[tauri::command]
fn update_settings(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    settings: settings::Settings,
) -> Result<(), String> {
    let (changed, max_changed, device_changed) = {
        let mut current = state.settings.lock();
        let max_changed = current.max_recording_secs != settings.max_recording_secs;
        let device_changed = current.input_device != settings.input_device;
        if *current == settings {
            (false, false, false)
        } else {
            *current = settings.clone();
            (true, max_changed, device_changed)
        }
    };
    settings::save(&settings).map_err(|e| e.to_string())?;

    if changed {
        apply_settings(&app, &state, &settings, max_changed || device_changed);
        let _ = app.emit(EVT_SETTINGS_CHANGED, ());
    }
    Ok(())
}

fn apply_settings(app: &AppHandle, state: &Arc<AppState>, s: &settings::Settings, rebuild: bool) {
    state.sounds.set_enabled(s.sounds);

    if let Some(controller) = state.hotkeys.lock().as_mut() {
        match controller.rebind(&s.hotkey) {
            Ok(()) => state.hotkey_ok.store(true, Ordering::SeqCst),
            Err(e) => {
                state.hotkey_ok.store(false, Ordering::SeqCst);
                emit_error(
                    app,
                    &format!(
                        "Hotkey \"{}\" could not be registered: {e}",
                        hotkey::display(&s.hotkey)
                    ),
                );
            }
        }
        match controller.rebind_llm(&s.llm_hotkey) {
            Ok(()) => state.hotkey_llm_ok.store(true, Ordering::SeqCst),
            Err(e) => {
                state.hotkey_llm_ok.store(false, Ordering::SeqCst);
                emit_error(
                    app,
                    &format!(
                        "Ask-LLM hotkey \"{}\" could not be registered: {e}",
                        hotkey::display(&s.llm_hotkey)
                    ),
                );
            }
        }
        match controller.rebind_screenshot(&s.screenshot_hotkey) {
            Ok(()) => state.hotkey_shot_ok.store(true, Ordering::SeqCst),
            Err(e) => {
                state.hotkey_shot_ok.store(false, Ordering::SeqCst);
                emit_error(
                    app,
                    &format!(
                        "Screenshot hotkey \"{}\" could not be registered: {e}",
                        hotkey::display(&s.screenshot_hotkey)
                    ),
                );
            }
        }
    }

    *state.gemini.lock() = Some(gemini::GeminiClient::new(
        &s.gemini_api_key,
        &s.llm_model,
        &s.llm_system_prompt,
        &s.tts_model,
        &s.tts_voice,
    ));

    state.agent.db.prune_sessions(s.session_limit as usize);
    let _ = app.emit(EVT_LLM_SESSION, ());

    let autolaunch = app.autolaunch();
    let _ = match s.launch_at_login {
        true => autolaunch.enable(),
        false => autolaunch.disable(),
    };

    if rebuild {
        rebuild_recorder(state);
    }

    let active = state.registry.resolve_path(&s.model_id);
    if state.stt.model_path() != active {
        if model_is_valid(state, &s.model_id) {
            if let Some(path) = active {
                enqueue_load(state, path);
            }
        } else {
            start_download(app, state, &s.model_id);
        }
    }
    update_tray(app, state);
}

/// True when the model is fully present on disk (single GGML file, or every
/// file of a multi-file ONNX bundle).
fn model_is_valid(state: &AppState, model_id: &str) -> bool {
    let Some(model) = state.registry.find(model_id) else {
        return false;
    };
    if model.engine == "gemini" {
        // Cloud model: nothing to download; the API key is checked when it runs.
        return true;
    }
    if model.files.is_empty() {
        return state
            .registry
            .resolve_path(model_id)
            .is_some_and(|p| is_valid_model_file(&p));
    }
    let dir = state.registry.download_dir();
    model.files.iter().all(|f| {
        use crate::downloader::is_valid_download;
        is_valid_download(&dir.join(&f.filename), f.size_bytes)
    })
}

fn start_download(app: &AppHandle, state: &Arc<AppState>, model_id: &str) {
    if state
        .registry
        .find(model_id)
        .is_some_and(|model| model.engine == "gemini")
    {
        return;
    }
    {
        let mut downloading = state.downloading.lock();
        if downloading.is_some() || downloading.as_deref() == Some(model_id) {
            return;
        }
        *downloading = Some(model_id.to_string());
    }

    let Some(model) = state.registry.find(model_id).cloned() else {
        *state.downloading.lock() = None;
        return;
    };

    let handle = app.clone();
    let state2 = state.clone();
    let mid = model_id.to_string();

    std::thread::Builder::new()
        .name("model-download".into())
        .spawn(move || {
            let downloader = Downloader::new();
            let mut last_pct = u64::MAX;

            let progress = |done: u64, total: u64| {
                let pct = done
                    .checked_mul(100)
                    .and_then(|d| d.checked_div(total))
                    .unwrap_or(0);
                if pct == last_pct {
                    return;
                }
                last_pct = pct;
                *state2.download_progress.lock() = Some((done, total));
                let _ = handle.emit(
                    EVT_DOWNLOAD_PROGRESS,
                    DownloadProgressDto {
                        model_id: mid.clone(),
                        downloaded: done,
                        total,
                        done: false,
                        error: None,
                    },
                );
                update_tray(&handle, &state2);
            };

            let dir = state2.registry.download_dir();
            let result = if model.files.is_empty() {
                debug_log(&format!(
                    "download: start {} -> {}",
                    mid,
                    dir.join(&model.filename).display()
                ));
                downloader
                    .download(
                        &model.url,
                        &dir.join(&model.filename),
                        &model.checksum_sha256,
                        progress,
                    )
                    .map(|_| dir.join(&model.filename))
            } else {
                debug_log(&format!(
                    "download: start bundle {} ({} files)",
                    mid,
                    model.files.len()
                ));
                let files: Vec<(String, String, u64)> = model
                    .files
                    .iter()
                    .map(|f| (f.filename.clone(), f.url.clone(), f.size_bytes))
                    .collect();
                downloader
                    .download_bundle(&dir, &files, progress)
                    .map(|paths| {
                        paths
                            .first()
                            .cloned()
                            .unwrap_or_else(|| dir.join(&model.filename))
                    })
            };

            *state2.downloading.lock() = None;
            *state2.download_progress.lock() = None;

            match result {
                Ok(dest) => {
                    debug_log(&format!("download: done {}", dest.display()));
                    let _ = handle.emit(
                        EVT_DOWNLOAD_PROGRESS,
                        DownloadProgressDto {
                            model_id: mid.clone(),
                            downloaded: 1,
                            total: 1,
                            done: true,
                            error: None,
                        },
                    );
                    let active = state2.settings.lock().model_id.clone();
                    if active == mid {
                        if let Some(path) = state2.registry.resolve_path(&mid) {
                            enqueue_load(&state2, path);
                        }
                    } else {
                        let _ = handle.emit(EVT_MODEL_MISSING, &mid);
                    }
                }
                Err(e) => {
                    debug_log(&format!("download: FAILED -> {e}"));
                    let _ = handle.emit(
                        EVT_DOWNLOAD_PROGRESS,
                        DownloadProgressDto {
                            model_id: mid.clone(),
                            downloaded: 0,
                            total: 0,
                            done: true,
                            error: Some(e.clone()),
                        },
                    );
                    emit_error(&handle, &e);
                }
            }
            update_tray(&handle, &state2);
        })
        .expect("failed to spawn download thread");
}

#[tauri::command]
fn get_models(state: State<'_, Arc<AppState>>) -> Vec<ModelStatusDto> {
    let active = state.settings.lock().model_id.clone();
    let registry = &state.registry;
    registry
        .models()
        .iter()
        .map(|m| {
            let cloud = m.engine == "gemini";
            let path = registry.resolve_path(&m.id);
            let downloaded = cloud
                || if m.files.is_empty() {
                    path.as_ref().is_some_and(|p| p.exists())
                } else {
                    let dir = registry.download_dir();
                    m.files.iter().all(|f| {
                        use crate::downloader::is_valid_download;
                        is_valid_download(&dir.join(&f.filename), f.size_bytes)
                    })
                };
            ModelStatusDto {
                id: m.id.clone(),
                name: m.name.clone(),
                description: m.description.clone(),
                filename: m.filename.clone(),
                size_bytes: m.size_bytes,
                languages: m.languages.clone(),
                downloaded,
                path: path.map(|p| p.to_string_lossy().to_string()),
                active: m.id == active,
                engine: m.engine.clone(),
                cloud,
            }
        })
        .collect()
}

#[tauri::command]
fn download_model(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
    model_id: String,
) -> Result<(), String> {
    start_download(&app, state.inner(), &model_id);
    Ok(())
}

fn check_for_updates_impl(app: &AppHandle, state: &Arc<AppState>) {
    if state.update_checking.swap(true, Ordering::SeqCst) {
        return;
    }
    debug_log("update: checking GitHub releases…");
    let _ = app.emit(
        EVT_UPDATE_STATUS,
        UpdateStatusDto {
            checking: true,
            installing: false,
            downloaded: 0,
            total: 0,
            current_version: env!("CARGO_PKG_VERSION").into(),
            available: None,
        },
    );

    let result = updater::check();
    state.update_checking.store(false, Ordering::SeqCst);

    match result {
        Ok(Some(info)) => {
            debug_log(&format!("update: v{} available", info.latest_version));
            let dto: UpdateInfoDto = info.clone().into();
            *state.update.lock() = Some(info);
            let _ = app.emit(
                EVT_UPDATE_STATUS,
                UpdateStatusDto {
                    checking: false,
                    installing: false,
                    downloaded: 0,
                    total: 0,
                    current_version: env!("CARGO_PKG_VERSION").into(),
                    available: Some(dto),
                },
            );
        }
        Ok(None) => {
            debug_log("update: up to date");
            *state.update.lock() = None;
            let _ = app.emit(
                EVT_UPDATE_STATUS,
                UpdateStatusDto {
                    checking: false,
                    installing: false,
                    downloaded: 0,
                    total: 0,
                    current_version: env!("CARGO_PKG_VERSION").into(),
                    available: None,
                },
            );
        }
        Err(e) => {
            debug_log(&format!("update: check failed -> {e}"));
            *state.update.lock() = None;
            emit_error(app, &e);
        }
    }
    update_tray(app, state);
}

#[tauri::command]
fn check_for_updates(app: AppHandle, state: State<'_, Arc<AppState>>) {
    check_for_updates_impl(&app, state.inner());
}

#[tauri::command]
fn get_update_status(state: State<'_, Arc<AppState>>) -> UpdateStatusDto {
    let (progress, available) = {
        let p = state.update_progress.lock().clone();
        let a = state.update.lock().clone().map(UpdateInfoDto::from);
        (p, a)
    };
    UpdateStatusDto {
        checking: state.update_checking.load(Ordering::SeqCst),
        installing: state.update_installing.load(Ordering::SeqCst),
        downloaded: progress.map(|(d, _)| d).unwrap_or(0),
        total: progress.map(|(_, t)| t).unwrap_or(0),
        current_version: env!("CARGO_PKG_VERSION").into(),
        available,
    }
}

#[tauri::command]
fn install_update(app: AppHandle, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    install_update_impl(&app, state.inner())
}

fn install_update_impl(app: &AppHandle, state: &Arc<AppState>) -> Result<(), String> {
    if state.update_installing.swap(true, Ordering::SeqCst) {
        return Err("update install already in progress".into());
    }
    let Some(info) = state.update.lock().clone() else {
        state.update_installing.store(false, Ordering::SeqCst);
        return Err("no update available".into());
    };
    if updater::is_running_from_mounted_dmg() {
        state.update_installing.store(false, Ordering::SeqCst);
        return Err(
            "you are running VoiceKeyboard from the mounted DMG — move it to /Applications first"
                .into(),
        );
    }

    let handle = app.clone();
    let state2 = state.clone();
    std::thread::Builder::new()
        .name("update-install".into())
        .spawn(move || {
            let _ = handle.emit(
                EVT_UPDATE_STATUS,
                UpdateStatusDto {
                    checking: false,
                    installing: true,
                    downloaded: 0,
                    total: 0,
                    current_version: env!("CARGO_PKG_VERSION").into(),
                    available: Some(info.clone().into()),
                },
            );
            let result = updater::download(&info, {
                let h = handle.clone();
                let s = state2.clone();
                move |done, total| {
                    *s.update_progress.lock() = Some((done, total));
                    let _ = h.emit(
                        EVT_UPDATE_PROGRESS,
                        serde_json::json!({ "downloaded": done, "total": total }),
                    );
                }
            })
            .and_then(|dmg| updater::stage_and_install(&dmg));

            match result {
                Ok(()) => {
                    state2.update_installing.store(false, Ordering::SeqCst);
                    debug_log("update: staged — quitting for swap + relaunch");
                    let _ = handle.emit(
                        EVT_UPDATE_STATUS,
                        UpdateStatusDto {
                            checking: false,
                            installing: false,
                            downloaded: 0,
                            total: 0,
                            current_version: env!("CARGO_PKG_VERSION").into(),
                            available: None,
                        },
                    );
                    // The detached installer swaps bundles and relaunches.
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    handle.exit(0);
                }
                Err(e) => {
                    state2.update_installing.store(false, Ordering::SeqCst);
                    state2.update_progress.lock().take();
                    debug_log(&format!("update: install failed -> {e}"));
                    emit_error(&handle, &format!("Update install failed: {e}"));
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}
