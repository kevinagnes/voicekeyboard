use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const UNTITLED: &str = "Untitled session";
const TITLE_MAX_CHARS: usize = 60;

/// One message in a session: a spoken question (`user`) or a Gemini reply
/// (`model`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub role: String,
    pub text: String,
    /// Epoch milliseconds.
    pub at: u64,
}

impl Turn {
    pub fn user(text: &str) -> Self {
        Self {
            role: "user".into(),
            text: text.to_string(),
            at: now_ms(),
        }
    }

    pub fn model(text: &str) -> Self {
        Self {
            role: "model".into(),
            text: text.to_string(),
            at: now_ms(),
        }
    }
}

/// A conversation with the AI assistant. `title` is auto-filled from the first
/// question and can be renamed by the user.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub turns: Vec<Turn>,
}

impl Session {
    pub fn display_title(&self) -> String {
        if self.title.trim().is_empty() {
            UNTITLED.to_string()
        } else {
            self.title.clone()
        }
    }
}

/// Persisted session list. `active_id` is the session the next Ask-LLM
/// question continues; `None` means a clean context — the next question starts
/// a fresh session. Keeping it `None` instead of an empty session means
/// cancelled recordings never leave clutter behind.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionStore {
    /// Most recently updated first.
    pub sessions: Vec<Session>,
    pub active_id: Option<String>,
}

pub fn sessions_path() -> PathBuf {
    crate::app_data_dir().join("sessions.json")
}

pub fn load() -> SessionStore {
    match fs::read_to_string(sessions_path()) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => SessionStore::default(),
    }
}

impl SessionStore {
    pub fn save(&self) -> anyhow::Result<()> {
        let path = sessions_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn active(&self) -> Option<&Session> {
        self.active_id.as_deref().and_then(|id| self.find(id))
    }

    /// Switch to a clean context: the next question starts a new session.
    pub fn start_new(&mut self) {
        self.active_id = None;
    }

    /// Make `id` the active session. Returns false when it no longer exists.
    pub fn set_active(&mut self, id: &str) -> bool {
        if self.find(id).is_some() {
            self.active_id = Some(id.to_string());
            true
        } else {
            false
        }
    }

    /// Append the spoken question to `preferred` (the session captured when
    /// the recording began) or, when it is missing/`None`, create a fresh
    /// session titled from the question. Returns the session id.
    pub fn append_user_turn(&mut self, preferred: Option<&str>, text: &str) -> String {
        let id = match preferred {
            Some(id) if self.find(id).is_some() => id.to_string(),
            _ => {
                let id = new_id();
                let now = now_ms();
                self.sessions.insert(
                    0,
                    Session {
                        id: id.clone(),
                        title: title_from(text),
                        created_at: now,
                        updated_at: now,
                        turns: Vec::new(),
                    },
                );
                id
            }
        };
        self.push_turn(&id, Turn::user(text));
        id
    }

    /// Append a Gemini reply to the session. Returns false when it is gone.
    pub fn append_model_turn(&mut self, id: &str, text: &str) -> bool {
        self.push_turn(id, Turn::model(text))
    }

    pub fn rename(&mut self, id: &str, title: &str) -> bool {
        match self.sessions.iter_mut().find(|s| s.id == id) {
            Some(session) => {
                session.title = title.trim().to_string();
                true
            }
            None => false,
        }
    }

    /// Delete a session. When it was active the context resets to a clean one.
    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.sessions.len();
        self.sessions.retain(|s| s.id != id);
        if self.active_id.as_deref() == Some(id) {
            self.active_id = None;
        }
        self.sessions.len() != before
    }

    /// Keep only the `limit` most recently updated sessions. When the active
    /// session is pruned the context resets to a clean one.
    pub fn prune(&mut self, limit: usize) {
        let limit = limit.max(1);
        self.sessions
            .sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        self.sessions.truncate(limit);
        if let Some(active) = self.active_id.as_deref() {
            if self.find(active).is_none() {
                self.active_id = None;
            }
        }
    }

    fn push_turn(&mut self, id: &str, turn: Turn) -> bool {
        let Some(index) = self.sessions.iter().position(|s| s.id == id) else {
            return false;
        };
        let now = now_ms();
        let session = &mut self.sessions[index];
        if session.title.trim().is_empty() && turn.role == "user" {
            session.title = title_from(&turn.text);
        }
        session.updated_at = now;
        session.turns.push(turn);
        let session = self.sessions.remove(index);
        self.sessions.insert(0, session);
        self.active_id = Some(id.to_string());
        true
    }
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("s{nanos}")
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Session title from the first question: whitespace collapsed, truncated on a
/// character boundary.
pub fn title_from(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title: String = collapsed.chars().take(TITLE_MAX_CHARS).collect();
    if collapsed.chars().count() > TITLE_MAX_CHARS {
        title.push('…');
    }
    title
}

/// One-line preview of a menu title, truncated on a character boundary.
pub fn menu_title(session: &Session, max_chars: usize) -> String {
    let title = session.display_title();
    if title.chars().count() <= max_chars {
        return title;
    }
    let mut short: String = title.chars().take(max_chars.saturating_sub(1)).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(questions: &[&str]) -> SessionStore {
        let mut store = SessionStore::default();
        for (i, q) in questions.iter().enumerate() {
            let id = store.append_user_turn(None, q);
            assert!(store.append_model_turn(&id, &format!("reply {i}")));
        }
        store
    }

    #[test]
    fn first_question_titles_session() {
        let mut store = SessionStore::default();
        let id = store.append_user_turn(None, "  What is   the news?\nToday ");
        let session = store.find(&id).unwrap();
        assert_eq!(session.title, "What is the news? Today");
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].role, "user");
        assert_eq!(store.active_id.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn preferred_session_receives_turn() {
        let mut store = store_with(&["first"]);
        let id = store.sessions[0].id.clone();
        store.start_new();
        let again = store.append_user_turn(Some(&id), "follow up");
        assert_eq!(again, id);
        assert_eq!(store.find(&id).unwrap().turns.len(), 3);
        assert_eq!(store.active_id.as_deref(), Some(id.as_str()));
        assert_eq!(store.sessions.len(), 1);
    }

    #[test]
    fn missing_preferred_session_creates_new() {
        let mut store = store_with(&["first"]);
        let id = store.append_user_turn(Some("gone"), "second question");
        assert_ne!(id, store.sessions[1].id);
        assert_eq!(store.sessions.len(), 2);
        assert_eq!(store.active_id.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn title_truncates_on_char_boundary() {
        let long = "á".repeat(TITLE_MAX_CHARS + 5);
        let title = title_from(&long);
        assert_eq!(title.chars().count(), TITLE_MAX_CHARS + 1);
        assert!(title.ends_with('…'));
        assert!(title.starts_with('á'));
    }

    #[test]
    fn prune_keeps_most_recent_and_clears_active() {
        let mut store = store_with(&["one", "two", "three"]);
        let active = store.active_id.clone().unwrap();
        store.prune(2);
        assert_eq!(store.sessions.len(), 2);
        assert!(store.find(&active).is_some());
        // Pruning below the active session's age clears the context.
        store.prune(1);
        assert_eq!(store.sessions.len(), 1);
        let remaining = store.sessions[0].id.clone();
        assert_eq!(store.find(&active).is_some(), active == remaining);
    }

    #[test]
    fn rename_and_delete() {
        let mut store = store_with(&["first"]);
        let id = store.sessions[0].id.clone();
        assert!(store.rename(&id, "My topic"));
        assert_eq!(store.find(&id).unwrap().display_title(), "My topic");
        assert!(!store.rename("missing", "x"));
        assert!(store.delete(&id));
        assert!(store.sessions.is_empty());
        assert!(store.active_id.is_none());
        assert!(!store.delete(&id));
    }

    #[test]
    fn empty_rename_falls_back_to_untitled() {
        let mut store = store_with(&["first"]);
        let id = store.sessions[0].id.clone();
        store.rename(&id, "   ");
        assert_eq!(store.find(&id).unwrap().display_title(), UNTITLED);
    }

    #[test]
    fn start_new_clears_active_without_deleting() {
        let mut store = store_with(&["first"]);
        store.start_new();
        assert!(store.active_id.is_none());
        assert_eq!(store.sessions.len(), 1);
    }
}
