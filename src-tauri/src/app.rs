use parking_lot::Mutex;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, LogicalPosition, Manager, Position, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_autostart::ManagerExt as _;

use crate::core::{capture, hotkey, mic, paste, settings, sounds, stt};
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

const TRAY_ID: &str = "main";
const OVERLAY_ID: &str = "recording-overlay";
const OVERLAY_W: f64 = 300.0;
const OVERLAY_H: f64 = 64.0;
const OVERLAY_BOTTOM_MARGIN: f64 = 72.0;

enum Job {
    Transcribe(Vec<f32>, Option<String>, String),
    LoadModel(PathBuf),
}

const PENDING_PASTE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

pub struct PendingPaste {
    pub text: String,
    pub expires_at: std::time::Instant,
}

pub struct AppState {
    pub settings: Mutex<settings::Settings>,
    pub hotkeys: Mutex<Option<hotkey::HotkeyController>>,
    pub recorder: Mutex<Option<capture::AudioRecorder>>,
    pub stt: stt::SttEngine,
    pub sounds: sounds::Sounds,
    pub registry: ModelRegistry,
    job_tx: Mutex<Option<mpsc::Sender<Job>>>,
    downloading: Mutex<Option<String>>,
    download_progress: Mutex<Option<(u64, u64)>>,
    overlay: Mutex<Option<tauri::WebviewWindow<tauri::Wry>>>,
    pending_paste: Mutex<Option<PendingPaste>>,
    recent: Mutex<Vec<String>>,
    pub recording: AtomicBool,
    pub busy: AtomicBool,
    pub model_loaded: AtomicBool,
    pub hotkey_ok: AtomicBool,
    pub update: Mutex<Option<UpdateInfo>>,
    pub update_checking: AtomicBool,
    pub update_installing: AtomicBool,
    pub update_progress: Mutex<Option<(u64, u64)>>,
}

const MAX_RECENT: usize = 3;

impl AppState {
    pub fn new() -> Self {
        Self {
            settings: Mutex::new(settings::load()),
            hotkeys: Mutex::new(None),
            recorder: Mutex::new(None),
            stt: stt::SttEngine::new(),
            sounds: sounds::Sounds::new(),
            registry: ModelRegistry::from_embedded(),
            job_tx: Mutex::new(None),
            downloading: Mutex::new(None),
            download_progress: Mutex::new(None),
            overlay: Mutex::new(None),
            pending_paste: Mutex::new(None),
            recent: Mutex::new(Vec::new()),
            recording: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            model_loaded: AtomicBool::new(false),
            hotkey_ok: AtomicBool::new(true),
            update: Mutex::new(None),
            update_checking: AtomicBool::new(false),
            update_installing: AtomicBool::new(false),
            update_progress: Mutex::new(None),
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
    pub accessibility_trusted: bool,
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
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
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
            open_mic_settings,
            get_inference_stats,
            get_input_devices,
            get_recent_transcriptions,
            copy_transcription,
            check_for_updates,
            install_update,
            get_update_status,
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

            let menu = build_menu(&handle, state)?;
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(tray_icon())
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "settings" => open_settings(app),
                    "chimes" => toggle_chimes(app),
                    "paste-last" => paste_last(app),
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
                        open_settings(tray.app_handle());
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
                    debug_log(&format!("hotkey registered: {}", hotkey::display(&hotkey_spec)));
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
            *state.hotkeys.lock() = Some(controller);

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

            // Trigger the mic TCC prompt proactively when it hasn't been
            // asked yet (cpal alone never shows it).
            let mic_state = state.clone();
            std::thread::spawn(move || {
                if mic::mic_permission() == mic::MicPermission::NotDetermined {
                    debug_log("mic: requesting permission (TCC prompt)");
                    let status = mic::request_mic_permission();
                    debug_log(&format!("mic: status after request = {status:?}"));
                    if status == mic::MicPermission::Authorized {
                        rebuild_recorder(&mic_state);
                    }
                }
            });

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
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        open_settings(&h);
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
/// the main thread, otherwise it traps (SIGTRAP). Clipboard + paste keystroke
/// all happen here on the main thread.
fn paste_text_on_main(app: &AppHandle, text: &str) -> Result<paste::PasteOutcome, String> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<paste::PasteOutcome, String>>(1);
    let text = text.to_string();
    let _ = app.run_on_main_thread(move || {
        let _ = tx.send(paste::paste_text(&text));
    });
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .unwrap_or_else(|_| Err("paste timed out".to_string()))
}

/// Keep the last transcript for manual re-paste (tray menu) for ~1 minute,
/// so a paste that went nowhere (no input focused) can be retried.
fn store_pending_paste(app: &AppHandle, state: &Arc<AppState>, text: &str) {
    let expires_at = std::time::Instant::now() + PENDING_PASTE_TTL;
    {
        let mut pp = state.pending_paste.lock();
        *pp = Some(PendingPaste {
            text: text.to_string(),
            expires_at,
        });
    }
    debug_log(&format!(
        "pending paste stored ({} chars, expires in {}s)",
        text.len(),
        PENDING_PASTE_TTL.as_secs()
    ));
    update_tray(app, state);
    spawn_pending_expiry(app.clone(), state.clone());
}

fn spawn_pending_expiry(app: AppHandle, state: Arc<AppState>) {
    std::thread::Builder::new()
        .name("pending-expiry".into())
        .spawn(move || {
            std::thread::sleep(PENDING_PASTE_TTL);
            let expired = {
                let mut pp = state.pending_paste.lock();
                match pp.as_ref() {
                    Some(p) if p.expires_at <= std::time::Instant::now() => {
                        *pp = None;
                        true
                    }
                    _ => false,
                }
            };
            if expired {
                debug_log("pending paste expired");
                update_tray(&app, &state);
            }
        })
        .ok();
}

fn paste_last(app: &AppHandle) {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        return;
    };
    let text = {
        let mut pp = state.pending_paste.lock();
        match pp.as_ref() {
            Some(p) if p.expires_at > std::time::Instant::now() => Some(p.text.clone()),
            Some(_) => {
                *pp = None;
                None
            }
            None => None,
        }
    };
    if let Some(text) = text {
        if !text.is_empty() {
            debug_log("copy-last: attempting");
            match paste_text_on_main(app, &text) {
                Ok(outcome) if outcome.pasted => {
                    state.sounds.play(sounds::Chime::PasteDone);
                    debug_log("copy-last: ok");
                }
                Ok(_) => debug_log("copy-last: skipped"),
                Err(e) => emit_error(app, &e),
            }
        }
    }
    update_tray(app, &state);
}

fn spawn_hotkey_thread(app: AppHandle, state: Arc<AppState>) {
    std::thread::Builder::new()
        .name("hotkey-events".into())
        .spawn(move || {
            let receiver = global_hotkey::GlobalHotKeyEvent::receiver();
            while let Ok(event) = receiver.recv() {
                match event.state() {
                    global_hotkey::HotKeyState::Pressed => on_pressed(&app, &state),
                    global_hotkey::HotKeyState::Released => on_released(&app, &state),
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
                        let id = state.settings.lock().model_id.clone();
                        match state.stt.load_model(&path.to_string_lossy()) {
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
                                    hide_overlay_delayed(&app, std::time::Duration::from_millis(900));
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
                                    store_pending_paste(&app, &state, &transcription.text);
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
                                            hide_overlay_delayed(&app, std::time::Duration::from_millis(900));
                                        }
                                        Ok(_) => {
                                            debug_log("paste: no-op (empty)");
                                            hide_overlay_delayed(&app, std::time::Duration::from_millis(900));
                                        }
                                        Err(e) => {
                                            debug_log(&format!("paste: failed -> {e}"));
                                            emit_error(&app, &e);
                                            hide_overlay_delayed(&app, std::time::Duration::from_millis(900));
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
            match path {
                Some(path) if path.exists() && is_valid_model_file(&path) => {
                    enqueue_load(&state, path)
                }
                Some(path) if path.exists() => {
                    debug_log(&format!(
                        "model: {} is corrupt — deleting and re-downloading",
                        path.display()
                    ));
                    let _ = std::fs::remove_file(&path);
                    start_download(&app, &state, &id);
                }
                Some(_) | None => start_download(&app, &state, &id),
            }
        })
        .expect("failed to spawn model loader");
}

fn enqueue_load(state: &AppState, path: PathBuf) {
    if let Some(tx) = state.job_tx.lock().as_ref() {
        let _ = tx.send(Job::LoadModel(path));
    }
}

fn on_pressed(app: &AppHandle, state: &Arc<AppState>) {
    if state.busy.load(Ordering::SeqCst) || state.recording.load(Ordering::SeqCst) {
        return;
    }
    if app
        .get_webview_window("settings")
        .is_some_and(|w| w.is_focused().unwrap_or(false))
    {
        return;
    }
    state.recording.store(true, Ordering::SeqCst);
    debug_log(&format!(
        "pressed: model_loaded={}",
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
    state.sounds.play(sounds::Chime::RecordStart);
    let _ = app.emit(EVT_RECORDING_STARTED, ());
    show_overlay(app, state);
    update_tray(app, state);
}

fn on_released(app: &AppHandle, state: &Arc<AppState>) {
    if !state.recording.load(Ordering::SeqCst) {
        return;
    }
    state.recording.store(false, Ordering::SeqCst);
    let samples = state
        .recorder
        .lock()
        .as_ref()
        .map(|r| r.stop())
        .unwrap_or_default();
    let peak = samples.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
    let _ = app.emit(EVT_RECORDING_STOPPED, ());
    update_tray(app, state);
    debug_log(&format!(
        "released: raw_samples={} peak={peak:.4}",
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
        hide_overlay_delayed(app, std::time::Duration::from_millis(900));
        update_tray(app, state);
        return;
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
        hide_overlay_delayed(app, std::time::Duration::from_millis(900));
        return;
    }
    debug_log(&format!(
        "transcribe: samples={} trimmed_ms={} peak={peak:.4}",
        trimmed.len(),
        trimmed.len() * 1000 / capture::SAMPLE_RATE as usize
    ));

    state.busy.store(true, Ordering::SeqCst);
    update_tray(app, state);

    let language = match language.as_str() {
        "en" => Some("en".to_string()),
        "pt" => Some("pt".to_string()),
        _ => None,
    };
    if let Some(tx) = state.job_tx.lock().as_ref() {
        let _ = tx.send(Job::Transcribe(trimmed, language, prompt));
    } else {
        state.busy.store(false, Ordering::SeqCst);
        emit_error(app, "internal error: worker not ready");
    }
}

fn update_tray(app: &AppHandle, state: &AppState) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(menu) = build_menu(app, state) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

fn build_menu(app: &AppHandle, state: &AppState) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let status_text = if state.recording.load(Ordering::SeqCst) {
        "Recording…".to_string()
    } else if state.busy.load(Ordering::SeqCst) {
        "Transcribing…".to_string()
    } else if let Some((done, total)) = state.download_progress.lock().as_ref() {
        if *total > 0 {
            format!("Downloading model… {}%", done * 100 / total)
        } else {
            "Downloading model…".to_string()
        }
    } else if !state.model_loaded.load(Ordering::SeqCst) {
        "Model not downloaded — open Settings".to_string()
    } else if !paste::accessibility_trusted() {
        "Accessibility needed — open Settings".to_string()
    } else if !state.hotkey_ok.load(Ordering::SeqCst) {
        "Hotkey failed — open Settings".to_string()
    } else {
        let hotkey = state.settings.lock().hotkey.clone();
        format!("Ready — hold {} to record", hotkey::display(&hotkey))
    };
    let status = MenuItem::with_id(app, "status", status_text, true, None::<&str>)?;
    let paste_last = MenuItem::with_id(
        app,
        "paste-last",
        "Paste last transcript",
        state.pending_paste.lock().as_ref().is_some_and(|p| p.expires_at > std::time::Instant::now()),
        None::<&str>,
    )?;
    let open = MenuItem::with_id(app, "settings", "Open Settings…", true, Some("CmdOrCtrl+,"))?;
    let check_updates = if let Some(update) = state.update.lock().as_ref() {
        MenuItem::with_id(
            app,
            "check-updates",
            format!("Update available — v{}", update.latest_version),
            true,
            None::<&str>,
        )?
    } else {
        MenuItem::with_id(app, "check-updates", "Check for updates…", true, None::<&str>)?
    };
    let chimes = CheckMenuItem::with_id(
        app,
        "chimes",
        "Play chimes",
        true,
        state.sounds.enabled(),
        None::<&str>,
    )?;
    let quit = PredefinedMenuItem::quit(app, Some("Quit VoiceKeyboard"))?;
    Menu::with_items(app, &[&status, &paste_last, &open, &check_updates, &chimes, &quit])
}

fn toggle_chimes(app: &AppHandle) {
    if let Some(state) = app.try_state::<Arc<AppState>>() {
        let enabled = !state.sounds.enabled();
        state.sounds.set_enabled(enabled);
        let mut s = state.settings.lock();
        s.sounds = enabled;
        let _ = settings::save(&s);
        let _ = app.emit(EVT_SETTINGS_CHANGED, ());
        drop(s);
        update_tray(app, &state);
    }
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

fn show_overlay(app: &AppHandle, state: &Arc<AppState>) {
    let handle = app.clone();
    let state = state.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(win) = ensure_overlay(&handle, &state) else {
            return;
        };
        position_overlay_bottom_center(&win);
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

fn ensure_overlay(
    app: &AppHandle,
    state: &AppState,
) -> Option<tauri::WebviewWindow<tauri::Wry>> {
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
    position_overlay_bottom_center(&win);
    debug_log(&format!("overlay: created {}x{}", OVERLAY_W as u32, OVERLAY_H as u32));
    *state.overlay.lock() = Some(win.clone());
    Some(win)
}

fn position_overlay_bottom_center(win: &tauri::WebviewWindow<tauri::Wry>) {
    let (w, h) = main_screen_logical();
    let x = (w - OVERLAY_W) / 2.0;
    let y = h - OVERLAY_H - OVERLAY_BOTTOM_MARGIN;
    let _ = win.set_position(Position::Logical(LogicalPosition::new(x.max(0.0), y.max(0.0))));
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

fn debug_log(msg: &str) {
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
    if mic::mic_permission() == mic::MicPermission::Authorized
        && state.recorder.lock().is_none()
    {
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
    let status = mic::request_mic_permission();
    debug_log(&format!("mic: permission request result = {status:?}"));
    if status == mic::MicPermission::Authorized {
        rebuild_recorder(state.inner());
        let _ = app.emit(EVT_SETTINGS_CHANGED, ());
    } else if status == mic::MicPermission::NotDetermined {
        // Prompt now showing; the settings UI polls get_permissions and will
        // see the answer when the user responds. Also watch for it here so
        // the recorder is rebuilt as soon as it is granted.
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
    }
    match status {
        mic::MicPermission::Authorized => "authorized".into(),
        mic::MicPermission::Denied => "denied".into(),
        mic::MicPermission::NotDetermined => "notDetermined".into(),
    }
}

#[tauri::command]
fn open_mic_settings() {
    mic::open_mic_settings();
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
fn copy_transcription(_app: AppHandle, state: State<'_, Arc<AppState>>, index: usize) -> Result<(), String> {
    let text = {
        let recent = state.recent.lock();
        recent.get(index).cloned().ok_or("invalid index")?
    };
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.set_text(&text).map_err(|e| e.to_string())?;
    state.sounds.play(sounds::Chime::PasteDone);
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

fn apply_settings(
    app: &AppHandle,
    state: &Arc<AppState>,
    s: &settings::Settings,
    rebuild: bool,
) {
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
    }

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
        match active {
            Some(path) if path.exists() && is_valid_model_file(&path) => {
                enqueue_load(state, path)
            }
            Some(path) if path.exists() => {
                let _ = std::fs::remove_file(&path);
                start_download(app, state, &s.model_id);
            }
            _ => start_download(app, state, &s.model_id),
        }
    }
    update_tray(app, state);
}

fn start_download(app: &AppHandle, state: &Arc<AppState>, model_id: &str) {
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
    let Some(dest) = state.registry.resolve_path(model_id) else {
        *state.downloading.lock() = None;
        return;
    };

    let handle = app.clone();
    let state2 = state.clone();
    let mid = model_id.to_string();

    std::thread::Builder::new()
        .name("model-download".into())
        .spawn(move || {
            debug_log(&format!("download: start {} -> {}", mid, dest.display()));
            let downloader = Downloader::new();
            let mut last_pct = u64::MAX;
            let result = downloader.download(&model.url, &dest, &model.checksum_sha256, |done, total| {
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
            });

            *state2.downloading.lock() = None;
            *state2.download_progress.lock() = None;

            match result {
                Ok(()) => {
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
                        enqueue_load(&state2, dest);
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
            let path = registry.resolve_path(&m.id);
            let downloaded = path.as_ref().is_some_and(|p| p.exists());
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
    let _ = app.emit(EVT_UPDATE_STATUS, UpdateStatusDto {
        checking: true,
        installing: false,
        downloaded: 0,
        total: 0,
        current_version: env!("CARGO_PKG_VERSION").into(),
        available: None,
    });

    let result = updater::check();
    state.update_checking.store(false, Ordering::SeqCst);

    match result {
        Ok(Some(info)) => {
            debug_log(&format!(
                "update: v{} available",
                info.latest_version
            ));
            let dto: UpdateInfoDto = info.clone().into();
            *state.update.lock() = Some(info);
            let _ = app.emit(EVT_UPDATE_STATUS, UpdateStatusDto {
                checking: false,
                installing: false,
                downloaded: 0,
                total: 0,
                current_version: env!("CARGO_PKG_VERSION").into(),
                available: Some(dto),
            });
        }
        Ok(None) => {
            debug_log("update: up to date");
            *state.update.lock() = None;
            let _ = app.emit(EVT_UPDATE_STATUS, UpdateStatusDto {
                checking: false,
                installing: false,
                downloaded: 0,
                total: 0,
                current_version: env!("CARGO_PKG_VERSION").into(),
                available: None,
            });
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
        .spawn(move || {            let _ = handle.emit(EVT_UPDATE_STATUS, UpdateStatusDto {
                checking: false,
                installing: true,
                downloaded: 0,
                total: 0,
                current_version: env!("CARGO_PKG_VERSION").into(),
                available: Some(info.clone().into()),
            });
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
                    let _ = handle.emit(EVT_UPDATE_STATUS, UpdateStatusDto {
                        checking: false,
                        installing: false,
                        downloaded: 0,
                        total: 0,
                        current_version: env!("CARGO_PKG_VERSION").into(),
                        available: None,
                    });
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
