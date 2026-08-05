#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteOutcome {
    pub pasted: bool,
    pub skipped_secure: bool,
    pub no_focused_input: bool,
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

/// Whether the currently focused element accepts text input (text field,
/// text area, combo box, search field, or a web/text container). Used to
/// detect a paste that would go nowhere.
#[cfg(target_os = "macos")]
pub fn focused_is_text_input() -> bool {
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

    let text_roles = [
        "AXTextField",
        "AXTextArea",
        "AXComboBox",
        "AXSearchField",
        "AXWebArea",
        "AXTextAreaContainer",
    ];
    let mut current = Some(focused);
    while let Some(el) = current {
        if let Ok(role) = el.role() {
            if text_roles.iter().any(|r| role == *r) {
                return true;
            }
        }
        current = el.parent().ok();
    }
    false
}

#[cfg(not(target_os = "macos"))]
pub fn focused_is_text_input() -> bool {
    true
}

pub fn paste_text(text: &str) -> Result<PasteOutcome, String> {
    let cleaned = sanitize(text);
    if cleaned.is_empty() {
        return Ok(PasteOutcome {
            pasted: false,
            skipped_secure: false,
            no_focused_input: false,
        });
    }

    if !accessibility_trusted() {
        return Err(accessibility_missing_error());
    }

    if focused_is_secure() {
        return Ok(PasteOutcome {
            pasted: false,
            skipped_secure: true,
            no_focused_input: false,
        });
    }

    // The text must be left in the clipboard regardless of paste outcome, so
    // the user can always paste manually with Cmd/Ctrl+V.
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.set_text(&cleaned).map_err(|e| e.to_string())?;

    if !focused_is_text_input() {
        return Ok(PasteOutcome {
            pasted: false,
            skipped_secure: false,
            no_focused_input: true,
        });
    }

    send_paste_keystroke()?;

    Ok(PasteOutcome {
        pasted: true,
        skipped_secure: false,
        no_focused_input: false,
    })
}

pub fn send_paste_keystroke() -> Result<(), String> {
    use enigo::{Direction, Enigo, Keyboard, Key, Settings};

    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
    let modifier = paste_modifier();
    enigo
        .key(modifier, Direction::Press)
        .map_err(|e| e.to_string())?;
    enigo
        .key(Key::Unicode('v'), Direction::Click)
        .map_err(|e| e.to_string())?;
    enigo
        .key(modifier, Direction::Release)
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn paste_modifier() -> enigo::Key {
    #[cfg(target_os = "macos")]
    {
        enigo::Key::Meta
    }
    #[cfg(not(target_os = "macos"))]
    {
        enigo::Key::Control
    }
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
