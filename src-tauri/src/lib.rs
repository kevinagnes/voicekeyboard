pub mod core;
mod app;
mod downloader;
mod model_registry;
mod updater;

use std::path::PathBuf;

pub const APP_ID: &str = "com.voicekeyboard.app";

pub fn app_data_dir() -> PathBuf {
    directories::ProjectDirs::from("com", "voicekeyboard", "VoiceKeyboard")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("voicekeyboard"))
}

pub fn run() {
    app::run();
}
