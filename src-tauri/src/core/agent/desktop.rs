//! Desktop host implementation for clipboard, paste and open-url tools.

use std::sync::mpsc;
use std::sync::{Arc, OnceLock};

use tauri::AppHandle;

use super::tools::DesktopHost;

pub struct SystemDesktop {
    app: OnceLock<AppHandle>,
}

impl Default for SystemDesktop {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemDesktop {
    pub fn new() -> Self {
        Self {
            app: OnceLock::new(),
        }
    }

    pub fn attach(&self, app: AppHandle) {
        let _ = self.app.set(app);
    }
}

impl DesktopHost for SystemDesktop {
    fn read_clipboard(&self) -> Result<String, String> {
        let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
        clipboard.get_text().map_err(|e| e.to_string())
    }

    fn write_clipboard(&self, text: &str) -> Result<(), String> {
        let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
        clipboard
            .set_text(text.to_string())
            .map_err(|e| e.to_string())
    }

    fn paste_text(&self, text: &str) -> Result<(), String> {
        let text = text.to_string();
        let Some(app) = self.app.get().cloned() else {
            let outcome = crate::core::paste::paste_text(&text)?;
            if outcome.skipped_secure {
                return Err("The focused field is a secure field; nothing was pasted.".into());
            }
            return Ok(());
        };
        let (tx, rx) = mpsc::channel();
        app.run_on_main_thread(move || {
            let _ = tx.send(crate::core::paste::paste_text(&text));
        })
        .map_err(|e| e.to_string())?;
        match rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(Ok(outcome)) if outcome.skipped_secure => {
                Err("The focused field is a secure field; nothing was pasted.".into())
            }
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(format!("paste timed out: {e}")),
        }
    }

    fn open_url(&self, url: &str) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        let mut command = {
            let mut c = std::process::Command::new("open");
            c.arg(url);
            c
        };
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut c = std::process::Command::new("rundll32");
            c.args(["url.dll,FileProtocolHandler", url]);
            c
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = {
            let mut c = std::process::Command::new("xdg-open");
            c.arg(url);
            c
        };
        command.spawn().map(|_| ()).map_err(|e| e.to_string())
    }
}

pub type SharedDesktop = Arc<SystemDesktop>;
