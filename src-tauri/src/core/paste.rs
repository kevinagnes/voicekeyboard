#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteOutcome {
    pub pasted: bool,
    pub skipped_secure: bool,
}

pub fn sanitize(text: &str) -> String {
    let text = text.trim();
    let mut lines: Vec<String> = Vec::with_capacity(text.lines().count());
    let mut prev_blank = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if prev_blank {
                continue;
            }
            prev_blank = true;
        } else {
            prev_blank = false;
        }
        lines.push(trimmed.to_string());
    }
    lines.join("\n")
}

#[cfg(target_os = "macos")]
pub fn accessibility_trusted() -> bool {
    macos_accessibility_client::accessibility::application_is_trusted()
}

#[cfg(not(target_os = "macos"))]
pub fn accessibility_trusted() -> bool {
    true
}

/// Triggers the macOS accessibility permission prompt (System Settings).
/// Returns true if the app is already trusted.
#[cfg(target_os = "macos")]
pub fn request_accessibility_permission() -> bool {
    macos_accessibility_client::accessibility::application_is_trusted_with_prompt()
}

#[cfg(not(target_os = "macos"))]
pub fn request_accessibility_permission() -> bool {
    true
}

pub const ACCESSIBILITY_GUIDE: &str =
    "VoiceKeyboard needs Accessibility permission to paste text into other apps.\n\n\
     Grant it in System Settings → Privacy & Security → Accessibility, enable VoiceKeyboard, \
     then relaunch the app. Until then, nothing will be pasted.";

pub fn accessibility_missing_error() -> String {
    ACCESSIBILITY_GUIDE.to_string()
}

#[cfg(target_os = "macos")]
pub fn focused_is_secure() -> bool {
    use accessibility::{AXUIElement, AXUIElementAttributes, ElementFinder};
    use objc2_app_kit::NSWorkspace;
    use std::time::Duration as StdDuration;

    let workspace = NSWorkspace::sharedWorkspace();
    let Some(app) = workspace.frontmostApplication() else {
        return false;
    };
    let app_el = AXUIElement::application(app.processIdentifier());
    let Ok(window) = app_el.focused_window() else {
        return false;
    };
    let finder = ElementFinder::new(
        &window,
        |el| el.focused().is_ok_and(bool::from),
        Some(StdDuration::from_millis(200)),
    );
    let Ok(focused) = finder.find() else {
        return false;
    };
    let mut current = Some(focused);
    while let Some(el) = current {
        if let Ok(subrole) = el.subrole() {
            if subrole == "AXSecureTextField" {
                return true;
            }
        }
        current = el.parent().ok();
    }
    false
}

#[cfg(not(target_os = "macos"))]
pub fn focused_is_secure() -> bool {
    false
}

pub fn paste_text(text: &str) -> Result<PasteOutcome, String> {
    let cleaned = sanitize(text);
    if cleaned.is_empty() {
        return Ok(PasteOutcome {
            pasted: false,
            skipped_secure: false,
        });
    }

    if !accessibility_trusted() {
        return Err(accessibility_missing_error());
    }

    if focused_is_secure() {
        return Ok(PasteOutcome {
            pasted: false,
            skipped_secure: true,
        });
    }

    // Always put the text on the clipboard as a fallback.
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.set_text(&cleaned).map_err(|e| e.to_string())?;

    // Simulate typing every keystroke instead of Cmd+V. This works in any
    // app with a focused text input regardless of AX quirks (VS Code,
    // Electron, web views) — no app/input-field detection needed.
    type_text(&cleaned)?;

    Ok(PasteOutcome {
        pasted: true,
        skipped_secure: false,
    })
}

/// Simulate typing `text` as a stream of keyboard events (newlines become
/// Enter). Must be called on the main thread on macOS (enigo requirement).
/// Uses enigo's text() path, which posts the full string through
/// CGEventKeyboardSetUnicodeString — works in any focused text input
/// regardless of app (VS Code, Electron, web views).
pub fn type_text(text: &str) -> Result<(), String> {
    use enigo::{Enigo, Keyboard, Settings};

    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
    enigo
        .text(text)
        .map_err(|e| format!("failed to type text: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_trims_and_collapses_blank_lines() {
        let input = "\n\n  Hello world.  \n\n\nSecond paragraph.\n\n\n";
        let out = sanitize(input);
        assert_eq!(out, "Hello world.\n\nSecond paragraph.");
    }

    #[test]
    fn sanitize_empty() {
        assert_eq!(sanitize("   \n\n  "), "");
    }
}
