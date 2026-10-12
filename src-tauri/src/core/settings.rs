use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::APP_ID;

pub const DEFAULT_HOTKEY: &str = "RightShift";
pub const DEFAULT_LLM_HOTKEY: &str = "ctrl+ShiftRight";
pub const DEFAULT_SESSION_LIMIT: u64 = 30;
pub const DEFAULT_LANGUAGE: &str = "auto";
pub const DEFAULT_MODEL_ID: &str = "large-v3-turbo";
pub const DEFAULT_INITIAL_PROMPT: &str = "The transcript is a message to be sent as-is.";
pub const DEFAULT_MAX_RECORDING_SECS: u64 = 120;
pub const DEFAULT_MIN_RECORDING_MS: u64 = 300;
pub const DEFAULT_SOUNDS: bool = true;
pub const DEFAULT_LAUNCH_AT_LOGIN: bool = false;
pub const DEFAULT_AUTO_UPDATE: bool = true;
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-3.1-flash-lite";
pub const DEFAULT_TTS_MODEL: &str = "gemini-3.8-flash-tts";
pub const DEFAULT_TTS_VOICE: &str = "Kore";
pub const DEFAULT_LLM_SYSTEM_PROMPT: &str =
    "You are a helpful assistant. Reply concisely and naturally, as if speaking \
     to the user. Keep answers short unless the question requires detail. \
     If the user asks about news, current events, or anything recent that you \
     may not know, use Google Search to find current information.";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub hotkey: String,
    pub language: String,
    pub model_id: String,
    pub sounds: bool,
    pub launch_at_login: bool,
    pub initial_prompt: String,
    pub max_recording_secs: u64,
    pub min_recording_ms: u64,
    pub input_device: String,
    pub auto_update: bool,
    pub llm_hotkey: String,
    pub gemini_api_key: String,
    pub llm_model: String,
    pub tts_model: String,
    pub tts_voice: String,
    pub llm_system_prompt: String,
    pub session_limit: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: DEFAULT_HOTKEY.to_string(),
            language: DEFAULT_LANGUAGE.to_string(),
            model_id: DEFAULT_MODEL_ID.to_string(),
            sounds: DEFAULT_SOUNDS,
            launch_at_login: DEFAULT_LAUNCH_AT_LOGIN,
            initial_prompt: DEFAULT_INITIAL_PROMPT.to_string(),
            max_recording_secs: DEFAULT_MAX_RECORDING_SECS,
            min_recording_ms: DEFAULT_MIN_RECORDING_MS,
            input_device: String::new(),
            auto_update: DEFAULT_AUTO_UPDATE,
            llm_hotkey: DEFAULT_LLM_HOTKEY.to_string(),
            gemini_api_key: String::new(),
            llm_model: DEFAULT_GEMINI_MODEL.to_string(),
            tts_model: DEFAULT_TTS_MODEL.to_string(),
            tts_voice: DEFAULT_TTS_VOICE.to_string(),
            llm_system_prompt: DEFAULT_LLM_SYSTEM_PROMPT.to_string(),
            session_limit: DEFAULT_SESSION_LIMIT,
        }
    }
}

pub fn settings_path() -> PathBuf {
    let dir = crate::app_data_dir();
    dir.join("settings.json")
}

pub fn load() -> Settings {
    let path = settings_path();
    match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save(settings: &Settings) -> anyhow::Result<()> {
    let path = settings_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(settings)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn language_label(code: &str) -> &'static str {
    match code {
        "auto" => "Auto-detect",
        "en" => "English",
        "pt" => "Português (Brasil)",
        _ => "Auto-detect",
    }
}

#[allow(dead_code)]
pub fn ensure_dir(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

pub const _APP_ID_HINT: &str = APP_ID;
