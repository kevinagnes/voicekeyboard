use std::fmt;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};

#[derive(Debug, Clone)]
pub enum HotkeyError {
    EmptyKey,
    UnsupportedKey(String),
    Register(String),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyKey => write!(f, "no key given"),
            Self::UnsupportedKey(k) => write!(f, "unsupported key \"{k}\""),
            Self::Register(e) => write!(f, "failed to register hotkey: {e}"),
        }
    }
}

impl std::error::Error for HotkeyError {}

pub fn code_from_str(s: &str) -> Result<Code, HotkeyError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "fn" => Err(HotkeyError::UnsupportedKey(
            "Fn is not exposed to the global hotkey API on macOS".to_string(),
        )),
        _ => s.trim().parse().map_err(|_| HotkeyError::UnsupportedKey(s.trim().to_string())),
    }
}

pub fn code_to_str(code: Code) -> String {
    format!("{code:?}")
}

pub fn parse(spec: &str) -> Result<HotKey, HotkeyError> {
    let mut mods = Modifiers::empty();
    let mut key = None;
    for raw in spec.split('+') {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        match lower.as_str() {
            "alt" | "option" => mods.insert(Modifiers::ALT),
            "ctrl" | "control" => mods.insert(Modifiers::CONTROL),
            "cmd" | "command" | "super" => mods.insert(Modifiers::SUPER),
            "shift" => mods.insert(Modifiers::SHIFT),
            _ => key = Some(code_from_str(token)?),
        }
    }
    let key = key.ok_or(HotkeyError::EmptyKey)?;
    Ok(HotKey::new(Some(mods), key))
}

pub fn display(spec: &str) -> String {
    parse(spec)
        .map(|hk| format!("{hk}"))
        .unwrap_or_else(|_| spec.to_string())
}

pub fn is_combo(spec: &str) -> bool {
    spec.split('+').count() > 1
}

pub struct HotkeyController {
    manager: global_hotkey::GlobalHotKeyManager,
    current: Option<HotKey>,
}

// On Windows GlobalHotKeyManager holds a raw HWND whose register/unregister/drop
// are thread-safe Win32 calls; global-hotkey declares the manager Send on macOS
// (same rationale), but omits the impl on Windows, which breaks Arc<AppState>.
unsafe impl Send for HotkeyController {}

impl HotkeyController {
    pub fn new() -> Result<Self, HotkeyError> {
        let manager =
            global_hotkey::GlobalHotKeyManager::new().map_err(|e| HotkeyError::Register(e.to_string()))?;
        Ok(Self {
            manager,
            current: None,
        })
    }

    pub fn current_spec(&self) -> Option<String> {
        self.current.map(|hk| hk.to_string())
    }

    pub fn rebind(&mut self, spec: &str) -> Result<(), HotkeyError> {
        let hotkey = parse(spec)?;
        if let Some(prev) = self.current.take() {
            let _ = self.manager.unregister(prev);
        }
        self.manager
            .register(hotkey)
            .map_err(|e| HotkeyError::Register(e.to_string()))?;
        self.current = Some(hotkey);
        Ok(())
    }

    pub fn unregister(&mut self) {
        if let Some(prev) = self.current.take() {
            let _ = self.manager.unregister(prev);
        }
    }
}

pub fn is_fn_key(spec: &str) -> bool {
    spec.trim().eq_ignore_ascii_case("fn")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_key() {
        let hk = parse("ShiftRight").unwrap();
        assert_eq!(hk.key, Code::ShiftRight);
        assert_eq!(hk.mods, Modifiers::empty());
    }

    #[test]
    fn parses_combo() {
        let hk = parse("ctrl+alt+KeyG").unwrap();
        assert!(hk.mods.contains(Modifiers::CONTROL));
        assert!(hk.mods.contains(Modifiers::ALT));
        assert_eq!(hk.key, Code::KeyG);
    }

    #[test]
    fn rejects_fn() {
        assert!(matches!(parse("fn"), Err(HotkeyError::UnsupportedKey(_))));
    }

    #[test]
    fn round_trip_code() {
        let c = Code::ArrowUp;
        assert_eq!(code_from_str(&code_to_str(c)).unwrap(), c);
    }
}
