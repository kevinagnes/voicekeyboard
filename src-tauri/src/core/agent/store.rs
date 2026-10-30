//! Durable agent store (SQLite).
//!
//! One database file holds the transcript, tool runs, plans, artifacts,
//! permissions, inbox, checkpoints, jobs, snapshots and the audit event log.
//! The UI is a projection of these rows; the live stream is only a hint.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use super::types::{
    new_id, now_ms, ArtifactInfo, ContentPart, Effect, JobInfo, Message, PermissionRule, PlanItem,
    RunInfo, RunStepInfo, ToolRunInfo, Usage,
};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS kv (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  id              TEXT PRIMARY KEY,
  parent_id       TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  title           TEXT NOT NULL DEFAULT '',
  model           TEXT NOT NULL DEFAULT '',
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  last_seq        INTEGER NOT NULL DEFAULT 0,
  claim_at        INTEGER,
  claim_owner     TEXT,
  resume_attempts INTEGER NOT NULL DEFAULT 0,
  state           TEXT NOT NULL DEFAULT 'idle'
);
CREATE TABLE IF NOT EXISTS messages (
  id            TEXT PRIMARY KEY,
  session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq           INTEGER NOT NULL,
  role          TEXT NOT NULL,
  type          TEXT NOT NULL,
  data          TEXT NOT NULL,
  created_at    INTEGER NOT NULL,
  completed_at  INTEGER,
  error         TEXT,
  model         TEXT,
  tokens_in     INTEGER NOT NULL DEFAULT 0,
  tokens_out    INTEGER NOT NULL DEFAULT 0,
  tokens_cache_read  INTEGER NOT NULL DEFAULT 0,
  tokens_cache_write INTEGER NOT NULL DEFAULT 0,
  cost          REAL NOT NULL DEFAULT 0,
  UNIQUE(session_id, seq)
);
CREATE INDEX IF NOT EXISTS messages_session_seq ON messages(session_id, seq);
CREATE TABLE IF NOT EXISTS tool_runs (
  id              TEXT PRIMARY KEY,
  session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  run_id          TEXT,
  message_id      TEXT NOT NULL,
  step            INTEGER NOT NULL,
  call_id         TEXT NOT NULL,
  name            TEXT NOT NULL,
  input           TEXT NOT NULL,
  status          TEXT NOT NULL,
  output          TEXT,
  output_path     TEXT,
  error           TEXT,
  idempotency_key TEXT,
  read_only       INTEGER NOT NULL DEFAULT 0,
  started_at      INTEGER,
  completed_at    INTEGER,
  UNIQUE(session_id, message_id, call_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS tool_runs_idem ON tool_runs(idempotency_key)
  WHERE idempotency_key IS NOT NULL;
CREATE TABLE IF NOT EXISTS runs (
  id            TEXT PRIMARY KEY,
  session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  status        TEXT NOT NULL,
  reason        TEXT,
  started_at    INTEGER NOT NULL,
  completed_at  INTEGER,
  step_count    INTEGER NOT NULL DEFAULT 0,
  model         TEXT NOT NULL DEFAULT '',
  tokens_in     INTEGER NOT NULL DEFAULT 0,
  tokens_out    INTEGER NOT NULL DEFAULT 0,
  cost          REAL NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS run_steps (
  id              TEXT PRIMARY KEY,
  run_id          TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  session_id      TEXT NOT NULL,
  step_no         INTEGER NOT NULL,
  attempt         INTEGER NOT NULL,
  retry_of        INTEGER,
  provider        TEXT NOT NULL DEFAULT '',
  model           TEXT NOT NULL DEFAULT '',
  prompt_text     TEXT,
  prompt_hash     TEXT NOT NULL DEFAULT '',
  prefix_hash     TEXT NOT NULL DEFAULT '',
  message_count   INTEGER NOT NULL DEFAULT 0,
  tool_names      TEXT NOT NULL DEFAULT '[]',
  finish_reason   TEXT,
  output_started  INTEGER NOT NULL DEFAULT 0,
  input_tokens    INTEGER NOT NULL DEFAULT 0,
  output_tokens   INTEGER NOT NULL DEFAULT 0,
  duration_ms     INTEGER NOT NULL DEFAULT 0,
  first_text_ms   INTEGER NOT NULL DEFAULT 0,
  error           TEXT,
  created_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS run_steps_run ON run_steps(run_id);
CREATE TABLE IF NOT EXISTS inbox (
  id           TEXT PRIMARY KEY,
  session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq          INTEGER NOT NULL,
  kind         TEXT NOT NULL,
  delivery     TEXT NOT NULL,
  client_id    TEXT,
  payload      TEXT NOT NULL,
  enqueued_at  INTEGER NOT NULL,
  delivered_at INTEGER,
  UNIQUE(session_id, client_id)
);
CREATE TABLE IF NOT EXISTS events (
  id          TEXT PRIMARY KEY,
  session_id  TEXT,
  run_id      TEXT,
  seq         INTEGER NOT NULL,
  type        TEXT NOT NULL,
  data        TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  durable     INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS events_session_seq ON events(session_id, seq);
CREATE TABLE IF NOT EXISTS checkpoints (
  id          TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  through_seq INTEGER NOT NULL,
  summary     TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  tokens      INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS permissions (
  id          TEXT PRIMARY KEY,
  action      TEXT NOT NULL,
  resource    TEXT NOT NULL,
  effect      TEXT NOT NULL,
  scope       TEXT NOT NULL DEFAULT 'global',
  created_at  INTEGER NOT NULL,
  UNIQUE(action, resource, scope)
);
CREATE TABLE IF NOT EXISTS plans (
  session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  data       TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS artifacts (
  id          TEXT PRIMARY KEY,
  session_id  TEXT,
  job_id      TEXT,
  path        TEXT NOT NULL,
  title       TEXT NOT NULL DEFAULT '',
  mime        TEXT NOT NULL DEFAULT '',
  bytes       INTEGER NOT NULL DEFAULT 0,
  sha256      TEXT NOT NULL DEFAULT '',
  created_at  INTEGER NOT NULL,
  UNIQUE(session_id, path)
);
CREATE TABLE IF NOT EXISTS file_snapshots (
  id          TEXT PRIMARY KEY,
  session_id  TEXT,
  run_id      TEXT,
  tool_run_id TEXT,
  path        TEXT NOT NULL,
  before_hash TEXT,
  after_hash  TEXT,
  before_blob TEXT,
  after_blob  TEXT,
  created_at  INTEGER NOT NULL,
  status      TEXT NOT NULL DEFAULT 'staged'
);
CREATE INDEX IF NOT EXISTS snapshots_run ON file_snapshots(run_id);
CREATE TABLE IF NOT EXISTS usage (
  id          TEXT PRIMARY KEY,
  session_id  TEXT,
  run_id      TEXT,
  step_no     INTEGER,
  phase       TEXT NOT NULL DEFAULT 'chat',
  provider    TEXT NOT NULL DEFAULT 'gemini',
  model       TEXT NOT NULL DEFAULT '',
  input_tokens  INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  cache_read    INTEGER NOT NULL DEFAULT 0,
  cache_write   INTEGER NOT NULL DEFAULT 0,
  cost          REAL,
  duration_ms   INTEGER,
  created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS jobs (
  id          TEXT PRIMARY KEY,
  session_id  TEXT,
  kind        TEXT NOT NULL,
  status      TEXT NOT NULL,
  progress    REAL NOT NULL DEFAULT 0,
  payload     TEXT NOT NULL,
  result      TEXT,
  error       TEXT,
  notify      INTEGER NOT NULL DEFAULT 1,
  attempts    INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  resume_at   INTEGER
);
CREATE INDEX IF NOT EXISTS jobs_status ON jobs(status, created_at);
CREATE TABLE IF NOT EXISTS memory (
  id          TEXT PRIMARY KEY,
  text        TEXT NOT NULL,
  source      TEXT NOT NULL DEFAULT '',
  created_at  INTEGER NOT NULL
);
"#;

#[derive(Clone, Debug)]
pub struct SessionRow {
    pub id: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub model: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub last_seq: i64,
    pub resume_attempts: i64,
    pub state: String,
}

#[derive(Clone, Debug)]
pub struct PermissionRow {
    pub id: String,
    pub action: String,
    pub resource: String,
    pub effect: String,
    pub scope: String,
    pub created_at: u64,
}

#[derive(Clone, Debug)]
pub struct SnapshotRow {
    pub id: String,
    pub path: String,
    pub before_blob: Option<String>,
    pub after_blob: Option<String>,
    pub before_hash: Option<String>,
    pub status: String,
}

#[derive(Clone, Debug)]
pub struct CheckpointRow {
    pub through_seq: i64,
    pub summary: String,
    pub tokens: i64,
}

pub struct Db {
    conn: Mutex<Connection>,
    pub path: PathBuf,
}

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Self>> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create agent db dir {}", dir.display()))?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("open agent db {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        // Additive migrations for databases created by earlier builds.
        let _ = conn.execute("ALTER TABLE run_steps ADD COLUMN prompt_text TEXT", []);
        conn.pragma_update(None, "user_version", 1)?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
            path,
        }))
    }

    fn conn(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    // ---------------------------------------------------------------- sessions

    pub fn create_session(&self, title: &str, parent_id: Option<&str>, model: &str) -> String {
        let id = new_id("ses");
        let at = now_ms() as i64;
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO sessions (id, parent_id, title, model, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, parent_id, title, model, at, at],
        );
        id
    }

    pub fn session(&self, id: &str) -> Option<SessionRow> {
        let conn = self.conn();
        conn.query_row(
            "SELECT id, parent_id, title, model, created_at, updated_at, last_seq,
                    resume_attempts, state
             FROM sessions WHERE id = ?1",
            params![id],
            |r| {
                Ok(SessionRow {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    title: r.get(2)?,
                    model: r.get(3)?,
                    created_at: r.get::<_, i64>(4)? as u64,
                    updated_at: r.get::<_, i64>(5)? as u64,
                    last_seq: r.get(6)?,
                    resume_attempts: r.get(7)?,
                    state: r.get(8)?,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn sessions(&self, limit: usize) -> Vec<SessionRow> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, parent_id, title, model, created_at, updated_at, last_seq,
                    resume_attempts, state
             FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                title: r.get(2)?,
                model: r.get(3)?,
                created_at: r.get::<_, i64>(4)? as u64,
                updated_at: r.get::<_, i64>(5)? as u64,
                last_seq: r.get(6)?,
                resume_attempts: r.get(7)?,
                state: r.get(8)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn active_session(&self) -> Option<String> {
        let conn = self.conn();
        conn.query_row(
            "SELECT value FROM kv WHERE key = 'active_session'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn set_active_session(&self, id: Option<&str>) {
        let conn = self.conn();
        match id {
            Some(id) => {
                let _ = conn.execute(
                    "INSERT INTO kv (key, value) VALUES ('active_session', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![id],
                );
            }
            None => {
                let _ = conn.execute("DELETE FROM kv WHERE key = 'active_session'", []);
            }
        }
    }

    pub fn rename_session(&self, id: &str, title: &str) -> bool {
        let conn = self.conn();
        conn.execute(
            "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title.trim(), now_ms() as i64],
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    pub fn delete_session(&self, id: &str) -> bool {
        let ok = {
            let conn = self.conn();
            conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])
                .map(|n| n > 0)
                .unwrap_or(false)
        };
        if self.active_session().as_deref() == Some(id) {
            self.set_active_session(None);
        }
        ok
    }

    pub fn prune_sessions(&self, limit: usize) {
        let limit = limit.max(1) as i64;
        let active = self.active_session();
        let conn = self.conn();
        let _ = conn.execute(
            "DELETE FROM sessions WHERE id NOT IN (
                SELECT id FROM sessions ORDER BY updated_at DESC LIMIT ?1
             ) AND (?2 IS NULL OR id != ?2)",
            params![limit, active],
        );
    }

    pub fn touch_session(&self, id: &str, title_if_empty: Option<&str>) {
        let conn = self.conn();
        if let Some(title) = title_if_empty {
            let _ = conn.execute(
                "UPDATE sessions SET updated_at = ?2,
                     title = CASE WHEN trim(title) = '' THEN ?3 ELSE title END
                 WHERE id = ?1",
                params![id, now_ms() as i64, title],
            );
        } else {
            let _ = conn.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![id, now_ms() as i64],
            );
        }
    }

    pub fn messages(&self, session_id: &str) -> Vec<Message> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, seq, role, type, data, created_at, completed_at,
                    error, model, tokens_in, tokens_out, tokens_cache_read,
                    tokens_cache_write, cost
             FROM messages WHERE session_id = ?1 ORDER BY seq ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![session_id], |r| {
            let raw: String = r.get(5)?;
            Ok(Message {
                id: r.get(0)?,
                session_id: r.get(1)?,
                seq: r.get(2)?,
                role: r.get(3)?,
                kind: r.get(4)?,
                parts: serde_json::from_str(&raw).unwrap_or_default(),
                created_at: r.get::<_, i64>(6)? as u64,
                completed_at: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                error: r.get(8)?,
                model: r.get(9)?,
                tokens_in: r.get(10)?,
                tokens_out: r.get(11)?,
                tokens_cache_read: r.get(12)?,
                tokens_cache_write: r.get(13)?,
                cost: r.get(14)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn append_message(
        &self,
        session_id: &str,
        role: &str,
        kind: &str,
        parts: &[ContentPart],
        model: Option<&str>,
    ) -> Message {
        let mut conn = self.conn();
        let Ok(tx) = conn.transaction() else {
            return fallback_message(session_id, role, kind, parts);
        };
        let message = append_message_tx(&tx, session_id, role, kind, parts, model);
        let _ = tx.commit();
        message
    }

    pub fn complete_message(
        &self,
        id: &str,
        parts: &[ContentPart],
        usage: &Usage,
        error: Option<&str>,
    ) {
        let data = serde_json::to_string(parts).unwrap_or_else(|_| "[]".into());
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE messages SET data = ?2, completed_at = ?3, error = ?4,
                 tokens_in = ?5, tokens_out = ?6, tokens_cache_read = ?7,
                 tokens_cache_write = ?8
             WHERE id = ?1",
            params![
                id,
                data,
                now_ms() as i64,
                error,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read,
                usage.cache_write
            ],
        );
    }

    pub fn settle_message_error(&self, id: &str, error: &str) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE messages SET completed_at = ?2, error = ?3 WHERE id = ?1",
            params![id, now_ms() as i64, error],
        );
    }

    pub fn last_message(&self, session_id: &str) -> Option<Message> {
        self.messages(session_id).pop()
    }

    // --------------------------------------------------------------- tool runs

    #[allow(clippy::too_many_arguments)]
    pub fn insert_tool_run(
        &self,
        session_id: &str,
        run_id: &str,
        message_id: &str,
        step: u32,
        call_id: &str,
        name: &str,
        input: &Value,
        idempotency_key: Option<&str>,
        read_only: bool,
    ) -> String {
        let id = new_id("tool");
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT OR IGNORE INTO tool_runs
                (id, session_id, run_id, message_id, step, call_id, name, input,
                 status, idempotency_key, read_only)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'called', ?9, ?10)",
            params![
                id,
                session_id,
                run_id,
                message_id,
                step as i64,
                call_id,
                name,
                serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                idempotency_key,
                if read_only { 1 } else { 0 }
            ],
        );
        id
    }

    pub fn mark_tool_running(&self, id: &str) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE tool_runs SET status = 'running', started_at = ?2 WHERE id = ?1",
            params![id, now_ms() as i64],
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn settle_tool_run(
        &self,
        id: &str,
        status: &str,
        output: Option<&str>,
        output_path: Option<&str>,
        error: Option<&str>,
    ) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE tool_runs SET status = ?2, output = ?3, output_path = ?4,
                 error = ?5, completed_at = ?6
             WHERE id = ?1",
            params![id, status, output, output_path, error, now_ms() as i64],
        );
    }

    pub fn tool_run_by_key(&self, key: &str) -> Option<ToolRunInfo> {
        let conn = self.conn();
        conn.query_row(
            "SELECT id, session_id, message_id, step, call_id, name, input, status,
                    output, output_path, error, started_at, completed_at
             FROM tool_runs WHERE idempotency_key = ?1",
            params![key],
            tool_run_row,
        )
        .optional()
        .ok()
        .flatten()
    }

    pub fn tool_runs(&self, session_id: &str) -> Vec<ToolRunInfo> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, message_id, step, call_id, name, input, status,
                    output, output_path, error, started_at, completed_at
             FROM tool_runs WHERE session_id = ?1 ORDER BY rowid ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![session_id], tool_run_row);
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn tool_runs_for_run(&self, run_id: &str) -> Vec<ToolRunInfo> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, message_id, step, call_id, name, input, status,
                    output, output_path, error, started_at, completed_at
             FROM tool_runs WHERE run_id = ?1 ORDER BY rowid ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![run_id], tool_run_row);
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn tool_runs_for_message(&self, message_id: &str) -> Vec<ToolRunInfo> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, message_id, step, call_id, name, input, status,
                    output, output_path, error, started_at, completed_at
             FROM tool_runs WHERE message_id = ?1 ORDER BY rowid ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![message_id], tool_run_row);
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// A `running` or `called` row survives only if the process died between
    /// writing it and settling it. Read-only work is aborted; side effects are
    /// reported as unknown and never re-run.
    pub fn settle_stale_tools(&self, session_id: &str) -> Vec<(String, String)> {
        let rows = self.tool_runs(session_id);
        let mut out = Vec::new();
        for row in rows {
            if row.status == "running" || row.status == "called" {
                let unknown = !row.input.is_null() && !row.name.starts_with("read");
                let status = if unknown { "unknown" } else { "aborted" };
                self.settle_tool_run(
                    &row.id,
                    status,
                    None,
                    None,
                    Some("interrupted by app restart"),
                );
                out.push((row.name, status.to_string()));
            }
        }
        out
    }

    // ------------------------------------------------------------------- runs

    pub fn start_run(&self, session_id: &str, model: &str) -> String {
        let id = new_id("run");
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO runs (id, session_id, status, started_at, model)
             VALUES (?1, ?2, 'running', ?3, ?4)",
            params![id, session_id, now_ms() as i64, model],
        );
        id
    }

    pub fn finish_run(
        &self,
        id: &str,
        status: &str,
        reason: Option<&str>,
        steps: u32,
        usage: &Usage,
        cost: f64,
    ) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE runs SET status = ?2, reason = ?3, completed_at = ?4,
                 step_count = ?5, tokens_in = ?6, tokens_out = ?7, cost = ?8
             WHERE id = ?1",
            params![
                id,
                status,
                reason,
                now_ms() as i64,
                steps as i64,
                usage.input_tokens,
                usage.output_tokens,
                cost
            ],
        );
    }

    pub fn runs(&self, limit: usize) -> Vec<RunInfo> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, status, reason, started_at, completed_at,
                    step_count, model, tokens_in, tokens_out, cost
             FROM runs ORDER BY started_at DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(RunInfo {
                id: r.get(0)?,
                session_id: r.get(1)?,
                status: r.get(2)?,
                reason: r.get(3)?,
                started_at: r.get::<_, i64>(4)? as u64,
                completed_at: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                step_count: r.get(6)?,
                model: r.get(7)?,
                tokens_in: r.get(8)?,
                tokens_out: r.get(9)?,
                cost: r.get(10)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_run_step(
        &self,
        run_id: &str,
        session_id: &str,
        step: u32,
        attempt: u32,
        model: &str,
        prompt_text: Option<&str>,
        prompt_hash: &str,
        prefix_hash: &str,
        message_count: usize,
        tool_names: &[String],
    ) -> String {
        let id = new_id("step");
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO run_steps
                (id, run_id, session_id, step_no, attempt, provider, model,
                 prompt_text, prompt_hash, prefix_hash, message_count, tool_names, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'gemini', ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                id,
                run_id,
                session_id,
                step as i64,
                attempt as i64,
                model,
                prompt_text,
                prompt_hash,
                prefix_hash,
                message_count as i64,
                serde_json::to_string(tool_names).unwrap_or_else(|_| "[]".into()),
                now_ms() as i64
            ],
        );
        id
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_run_step(
        &self,
        id: &str,
        finish_reason: Option<&str>,
        output_started: bool,
        usage: &Usage,
        duration_ms: u64,
        first_text_ms: u64,
        error: Option<&str>,
    ) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE run_steps SET finish_reason = ?2, output_started = ?3,
                 input_tokens = ?4, output_tokens = ?5, duration_ms = ?6,
                 first_text_ms = ?7, error = ?8
             WHERE id = ?1",
            params![
                id,
                finish_reason,
                if output_started { 1 } else { 0 },
                usage.input_tokens,
                usage.output_tokens,
                duration_ms as i64,
                first_text_ms as i64,
                error
            ],
        );
    }

    pub fn run_steps(&self, run_id: &str) -> Vec<RunStepInfo> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, run_id, session_id, step_no, attempt, retry_of, provider,
                    model, prompt_hash, prefix_hash, message_count, tool_names,
                    finish_reason, output_started, input_tokens, output_tokens,
                    duration_ms, first_text_ms, error, created_at, prompt_text
             FROM run_steps WHERE run_id = ?1 ORDER BY rowid ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![run_id], |r| {
            let tools: String = r.get(11)?;
            Ok(RunStepInfo {
                id: r.get(0)?,
                run_id: r.get(1)?,
                session_id: r.get(2)?,
                step: r.get(3)?,
                attempt: r.get(4)?,
                retry_of: r.get(5)?,
                provider: r.get(6)?,
                model: r.get(7)?,
                prompt_hash: r.get(8)?,
                prefix_hash: r.get(9)?,
                message_count: r.get(10)?,
                tool_names: serde_json::from_str(&tools).unwrap_or_default(),
                finish_reason: r.get(12)?,
                output_started: r.get::<_, i64>(13)? != 0,
                input_tokens: r.get(14)?,
                output_tokens: r.get(15)?,
                duration_ms: r.get(16)?,
                first_text_ms: r.get(17)?,
                error: r.get(18)?,
                created_at: r.get::<_, i64>(19)? as u64,
                prompt_text: r.get(20)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    // ------------------------------------------------------------------ inbox

    pub fn admit(
        &self,
        session_id: &str,
        kind: &str,
        delivery: &str,
        client_id: Option<&str>,
        payload: &Value,
    ) -> String {
        let id = new_id("in");
        let mut conn = self.conn();
        let Ok(tx) = conn.transaction() else {
            return id;
        };
        let _ = tx.execute(
            "UPDATE sessions SET last_seq = last_seq + 1, updated_at = ?2 WHERE id = ?1",
            params![session_id, now_ms() as i64],
        );
        let seq: i64 = tx
            .query_row(
                "SELECT last_seq FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let _ = tx.execute(
            "INSERT OR IGNORE INTO inbox
                (id, session_id, seq, kind, delivery, client_id, payload, enqueued_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                session_id,
                seq,
                kind,
                delivery,
                client_id,
                serde_json::to_string(payload).unwrap_or_else(|_| "{}".into()),
                now_ms() as i64
            ],
        );
        let _ = tx.commit();
        id
    }

    pub fn pending(&self, session_id: &str, delivery: Option<&str>) -> Vec<(String, Value)> {
        let conn = self.conn();
        let sql = match delivery {
            Some(_) => {
                "SELECT id, payload FROM inbox
                        WHERE session_id = ?1 AND delivery = ?2
                        ORDER BY seq ASC"
            }
            None => {
                "SELECT id, payload FROM inbox WHERE session_id = ?1
                     ORDER BY seq ASC"
            }
        };
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let map = |r: &rusqlite::Row<'_>| {
            let id: String = r.get(0)?;
            let raw: String = r.get(1)?;
            Ok((id, serde_json::from_str(&raw).unwrap_or(Value::Null)))
        };
        let rows = match delivery {
            Some(d) => stmt.query_map(params![session_id, d], map),
            None => stmt.query_map(params![session_id], map),
        };
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn has_pending(&self, session_id: &str, delivery: &str) -> bool {
        let conn = self.conn();
        conn.query_row(
            "SELECT 1 FROM inbox WHERE session_id = ?1 AND delivery = ?2 LIMIT 1",
            params![session_id, delivery],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
    }

    /// Deliver exactly one item: delete the inbox row, then append the user
    /// message. Both happen in one transaction so a crash cannot separate them.
    pub fn deliver(&self, inbox_id: &str) -> Option<Message> {
        let mut conn = self.conn();
        let tx = conn.transaction().ok()?;
        let (session_id, kind, payload): (String, String, String) = tx
            .query_row(
                "SELECT session_id, kind, payload FROM inbox WHERE id = ?1",
                params![inbox_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()
            .flatten()?;
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let text = payload
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mut parts = vec![ContentPart::text(text)];
        if let Some(path) = payload.get("image").and_then(|v| v.as_str()) {
            parts.push(ContentPart::Image {
                path: path.to_string(),
                media_type: "image/png".into(),
            });
        }
        let _ = tx.execute("DELETE FROM inbox WHERE id = ?1", params![inbox_id]);
        let message = append_message_tx(&tx, &session_id, "user", &kind, &parts, None);
        let _ = tx.commit();
        Some(message)
    }

    pub fn sessions_with_pending(&self) -> Vec<String> {
        let conn = self.conn();
        let mut stmt =
            match conn.prepare("SELECT DISTINCT session_id FROM inbox ORDER BY enqueued_at ASC") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
        let rows = stmt.query_map([], |r| r.get::<_, String>(0));
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    // ---------------------------------------------------------------- claims

    pub fn claim(&self, session_id: &str, owner: &str) -> bool {
        let conn = self.conn();
        conn.execute(
            "UPDATE sessions SET claim_at = ?2, claim_owner = ?3, state = 'running'
             WHERE id = ?1 AND claim_at IS NULL",
            params![session_id, now_ms() as i64, owner],
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    pub fn release(&self, session_id: &str) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE sessions SET claim_at = NULL, claim_owner = NULL,
                 state = 'idle' WHERE id = ?1",
            params![session_id],
        );
    }

    pub fn stale_sessions(&self) -> Vec<String> {
        let conn = self.conn();
        let mut stmt = match conn.prepare("SELECT id FROM sessions WHERE claim_at IS NOT NULL") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |r| r.get::<_, String>(0));
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn bump_resume(&self, session_id: &str) -> i64 {
        {
            let conn = self.conn();
            let _ = conn.execute(
                "UPDATE sessions SET resume_attempts = resume_attempts + 1 WHERE id = ?1",
                params![session_id],
            );
        }
        self.session(session_id)
            .map(|s| s.resume_attempts)
            .unwrap_or(0)
    }

    // ----------------------------------------------------------------- events

    pub fn event(&self, session_id: Option<&str>, run_id: Option<&str>, kind: &str, data: &Value) {
        let conn = self.conn();
        let seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM events
                 WHERE (?1 IS NULL AND session_id IS NULL) OR session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .unwrap_or(1);
        let _ = conn.execute(
            "INSERT INTO events (id, session_id, run_id, seq, type, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                new_id("ev"),
                session_id,
                run_id,
                seq,
                kind,
                serde_json::to_string(data).unwrap_or_else(|_| "{}".into()),
                now_ms() as i64
            ],
        );
    }

    pub fn events_for_run(&self, run_id: &str) -> Vec<Value> {
        let conn = self.conn();
        let mut stmt = match conn
            .prepare("SELECT type, data, created_at FROM events WHERE run_id = ?1 ORDER BY seq ASC")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![run_id], |r| {
            let kind: String = r.get(0)?;
            let raw: String = r.get(1)?;
            let at: i64 = r.get(2)?;
            let mut value: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            if let Some(obj) = value.as_object_mut() {
                obj.insert("type".into(), Value::String(kind));
                obj.insert("at".into(), Value::from(at));
            }
            Ok(value)
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    // ------------------------------------------------------------- checkpoints

    pub fn save_checkpoint(&self, session_id: &str, through_seq: i64, summary: &str, tokens: i64) {
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO checkpoints (id, session_id, through_seq, summary, created_at, tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                new_id("ck"),
                session_id,
                through_seq,
                summary,
                now_ms() as i64,
                tokens
            ],
        );
    }

    pub fn latest_checkpoint(&self, session_id: &str) -> Option<CheckpointRow> {
        let conn = self.conn();
        conn.query_row(
            "SELECT through_seq, summary, tokens FROM checkpoints
             WHERE session_id = ?1 ORDER BY through_seq DESC LIMIT 1",
            params![session_id],
            |r| {
                Ok(CheckpointRow {
                    through_seq: r.get(0)?,
                    summary: r.get(1)?,
                    tokens: r.get(2)?,
                })
            },
        )
        .optional()
        .ok()
        .flatten()
    }

    // ------------------------------------------------------------- plans/artifacts

    pub fn plan(&self, session_id: &str) -> Vec<PlanItem> {
        let conn = self.conn();
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM plans WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        raw.and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn set_plan(&self, session_id: &str, plan: &[PlanItem]) {
        let data = serde_json::to_string(plan).unwrap_or_else(|_| "[]".into());
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO plans (session_id, data, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET data = excluded.data,
                 updated_at = excluded.updated_at",
            params![session_id, data, now_ms() as i64],
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register_artifact(
        &self,
        session_id: Option<&str>,
        job_id: Option<&str>,
        path: &str,
        title: &str,
        mime: &str,
        bytes: i64,
        sha256: &str,
    ) {
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO artifacts
                (id, session_id, job_id, path, title, mime, bytes, sha256, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(session_id, path) DO UPDATE SET
                title = excluded.title, mime = excluded.mime,
                bytes = excluded.bytes, sha256 = excluded.sha256,
                created_at = excluded.created_at",
            params![
                new_id("art"),
                session_id,
                job_id,
                path,
                title,
                mime,
                bytes,
                sha256,
                now_ms() as i64
            ],
        );
    }

    pub fn artifacts(&self, session_id: &str) -> Vec<ArtifactInfo> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, path, title, mime, bytes, sha256, created_at
             FROM artifacts WHERE session_id = ?1 ORDER BY created_at DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(ArtifactInfo {
                id: r.get(0)?,
                session_id: r.get(1)?,
                path: r.get(2)?,
                title: r.get(3)?,
                mime: r.get(4)?,
                bytes: r.get(5)?,
                sha256: r.get(6)?,
                created_at: r.get::<_, i64>(7)? as u64,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    // -------------------------------------------------------------- permissions

    pub fn permission_rules(&self, session_id: &str) -> Vec<PermissionRule> {
        let rows = self.permissions();
        rows.into_iter()
            .filter(|r| r.scope == "global" || r.scope == session_id)
            .map(|r| PermissionRule {
                action: r.action,
                resource: r.resource,
                effect: Effect::parse(&r.effect),
                scope: r.scope,
            })
            .collect()
    }

    pub fn permissions(&self) -> Vec<PermissionRow> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, action, resource, effect, scope, created_at
             FROM permissions ORDER BY created_at ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |r| {
            Ok(PermissionRow {
                id: r.get(0)?,
                action: r.get(1)?,
                resource: r.get(2)?,
                effect: r.get(3)?,
                scope: r.get(4)?,
                created_at: r.get::<_, i64>(5)? as u64,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn add_permission(&self, action: &str, resource: &str, effect: &str, scope: &str) {
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO permissions (id, action, resource, effect, scope, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(action, resource, scope) DO UPDATE SET effect = excluded.effect",
            params![
                new_id("perm"),
                action,
                resource,
                effect,
                scope,
                now_ms() as i64
            ],
        );
    }

    pub fn revoke_permission(&self, id: &str) -> bool {
        let conn = self.conn();
        conn.execute("DELETE FROM permissions WHERE id = ?1", params![id])
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    // ------------------------------------------------------------ snapshots/undo

    #[allow(clippy::too_many_arguments)]
    pub fn add_snapshot(
        &self,
        session_id: &str,
        run_id: &str,
        tool_run_id: &str,
        path: &str,
        before: Option<(&str, &str)>,
        after: Option<(&str, &str)>,
    ) {
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO file_snapshots
                (id, session_id, run_id, tool_run_id, path, before_hash, after_hash,
                 before_blob, after_blob, created_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'staged')",
            params![
                new_id("snap"),
                session_id,
                run_id,
                tool_run_id,
                path,
                before.map(|b| b.0),
                after.map(|a| a.0),
                before.map(|b| b.1),
                after.map(|a| a.1),
                now_ms() as i64
            ],
        );
    }

    pub fn snapshots_for_run(&self, run_id: &str) -> Vec<SnapshotRow> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, path, before_blob, after_blob, before_hash, status
             FROM file_snapshots WHERE run_id = ?1 ORDER BY created_at DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![run_id], |r| {
            Ok(SnapshotRow {
                id: r.get(0)?,
                path: r.get(1)?,
                before_blob: r.get(2)?,
                after_blob: r.get(3)?,
                before_hash: r.get(4)?,
                status: r.get(5)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn commit_snapshots(&self, run_id: &str) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE file_snapshots SET status = 'committed' WHERE run_id = ?1",
            params![run_id],
        );
    }

    pub fn mark_snapshot_undone(&self, id: &str) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE file_snapshots SET status = 'undone' WHERE id = ?1",
            params![id],
        );
    }

    // -------------------------------------------------------------------- usage

    #[allow(clippy::too_many_arguments)]
    pub fn usage(
        &self,
        session_id: &str,
        run_id: &str,
        step: u32,
        phase: &str,
        model: &str,
        usage: &Usage,
        duration_ms: u64,
    ) {
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO usage (id, session_id, run_id, step_no, phase, provider,
                 model, input_tokens, output_tokens, cache_read, cache_write,
                 duration_ms, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'gemini', ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                new_id("use"),
                session_id,
                run_id,
                step as i64,
                phase,
                model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read,
                usage.cache_write,
                duration_ms as i64,
                now_ms() as i64
            ],
        );
    }

    pub fn metrics(&self) -> Value {
        let conn = self.conn();
        let (runs, steps, tin, tout): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM runs),
                        (SELECT COUNT(*) FROM run_steps),
                        (SELECT COALESCE(SUM(input_tokens), 0) FROM usage),
                        (SELECT COALESCE(SUM(output_tokens), 0) FROM usage)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap_or((0, 0, 0, 0));
        let tool_calls: i64 = conn
            .query_row("SELECT COUNT(*) FROM tool_runs", [], |r| r.get(0))
            .unwrap_or(0);
        let tool_failures: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tool_runs WHERE status IN ('failed', 'unknown')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let compactions: i64 = conn
            .query_row("SELECT COUNT(*) FROM checkpoints", [], |r| r.get(0))
            .unwrap_or(0);
        let retries: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM run_steps WHERE attempt > 0",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let first_text: f64 = conn
            .query_row(
                "SELECT COALESCE(AVG(first_text_ms), 0) FROM run_steps
                 WHERE first_text_ms > 0",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0.0);
        serde_json::json!({
            "runs": runs,
            "steps": steps,
            "tools": tool_calls,
            "toolFailures": tool_failures,
            "compactions": compactions,
            "retries": retries,
            "inputTokens": tin,
            "outputTokens": tout,
            "avgFirstTextMs": first_text,
        })
    }

    // --------------------------------------------------------------------- jobs

    pub fn enqueue_job(
        &self,
        session_id: Option<&str>,
        kind: &str,
        payload: &Value,
        notify: bool,
    ) -> String {
        let id = new_id("job");
        let at = now_ms() as i64;
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO jobs (id, session_id, kind, status, payload, notify,
                 created_at, updated_at)
             VALUES (?1, ?2, ?3, 'queued', ?4, ?5, ?6, ?6)",
            params![
                id,
                session_id,
                kind,
                serde_json::to_string(payload).unwrap_or_else(|_| "{}".into()),
                if notify { 1 } else { 0 },
                at
            ],
        );
        id
    }

    pub fn claim_job(&self) -> Option<JobInfo> {
        let mut conn = self.conn();
        let Ok(tx) = conn.transaction() else {
            return None;
        };
        let row = tx
            .query_row(
                "SELECT id, session_id, kind, status, progress, payload, result,
                        error, notify, created_at, updated_at, resume_at
                 FROM jobs
                 WHERE status IN ('queued', 'waiting')
                   AND (resume_at IS NULL OR resume_at <= ?1)
                 ORDER BY created_at ASC LIMIT 1",
                params![now_ms() as i64],
                job_row,
            )
            .optional()
            .ok()
            .flatten();
        if let Some(job) = &row {
            let _ = tx.execute(
                "UPDATE jobs SET status = 'running', attempts = attempts + 1,
                     updated_at = ?2 WHERE id = ?1",
                params![job.id, now_ms() as i64],
            );
        }
        let _ = tx.commit();
        row
    }

    pub fn job_progress(&self, id: &str, progress: f64) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE jobs SET progress = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, progress, now_ms() as i64],
        );
    }

    pub fn finish_job(&self, id: &str, status: &str, result: Option<&str>, error: Option<&str>) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE jobs SET status = ?2, result = ?3, error = ?4, updated_at = ?5,
                 progress = CASE WHEN ?2 = 'succeeded' THEN 1 ELSE progress END
             WHERE id = ?1",
            params![id, status, result, error, now_ms() as i64],
        );
    }

    pub fn requeue_job(&self, id: &str, resume_at: Option<u64>) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE jobs SET status = 'waiting', resume_at = ?2, updated_at = ?3
             WHERE id = ?1",
            params![id, resume_at.map(|v| v as i64), now_ms() as i64],
        );
    }

    pub fn cancel_job(&self, id: &str) -> bool {
        let conn = self.conn();
        conn.execute(
            "UPDATE jobs SET status = 'cancelled', updated_at = ?2
             WHERE id = ?1 AND status IN ('queued', 'waiting', 'paused')",
            params![id, now_ms() as i64],
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    pub fn jobs(&self, limit: usize) -> Vec<JobInfo> {
        let conn = self.conn();
        let mut stmt = match conn.prepare(
            "SELECT id, session_id, kind, status, progress, payload, result,
                    error, notify, created_at, updated_at, resume_at
             FROM jobs ORDER BY created_at DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![limit as i64], job_row);
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn recover_jobs(&self) {
        let conn = self.conn();
        let _ = conn.execute(
            "UPDATE jobs SET status = CASE WHEN attempts >= 3 THEN 'failed' ELSE 'queued' END,
                 error = CASE WHEN attempts >= 3
                     THEN 'Interrupted three times; giving up.' ELSE error END,
                 updated_at = ?1
             WHERE status = 'running'",
            params![now_ms() as i64],
        );
        let _ = conn.execute(
            "UPDATE runs SET status = 'interrupted', reason = 'app restart',
                 completed_at = ?1
             WHERE status = 'running'",
            params![now_ms() as i64],
        );
    }

    // ------------------------------------------------------------------ memory

    pub fn remember(&self, text: &str, source: &str) -> String {
        let id = new_id("mem");
        let conn = self.conn();
        let _ = conn.execute(
            "INSERT INTO memory (id, text, source, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, text.trim(), source, now_ms() as i64],
        );
        id
    }

    pub fn recall(&self, query: &str, limit: usize) -> Vec<String> {
        self.recall_items(query, limit)
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    pub fn recall_items(&self, query: &str, limit: usize) -> Vec<(String, String)> {
        let conn = self.conn();
        let like = format!("%{}%", query.trim());
        let mut stmt = match conn.prepare(
            "SELECT id, text FROM memory
             WHERE ?1 = '%%' OR text LIKE ?1 OR source LIKE ?1
             ORDER BY created_at DESC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(params![like, limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn forget(&self, id: &str) -> bool {
        let conn = self.conn();
        conn.execute("DELETE FROM memory WHERE id = ?1", params![id])
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    pub fn all_memory(&self, limit: usize) -> Vec<(String, String)> {
        let conn = self.conn();
        let mut stmt =
            match conn.prepare("SELECT id, text FROM memory ORDER BY created_at DESC LIMIT ?1") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    // --------------------------------------------------------------- migration

    /// Import legacy `sessions.json` once. Original file is left untouched.
    pub fn import_legacy(&self, store: &crate::core::sessions::SessionStore) {
        if store.sessions.is_empty() {
            return;
        }
        let done = {
            let conn = self.conn();
            conn.query_row(
                "SELECT value FROM kv WHERE key = 'legacy_imported'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
        };
        if done {
            return;
        }
        // Mark first: a crash mid-import must not duplicate sessions on the
        // next launch. Partial imports are preferable to duplicate history.
        {
            let conn = self.conn();
            let _ = conn.execute(
                "INSERT INTO kv (key, value) VALUES ('legacy_imported', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![now_ms().to_string()],
            );
        }
        for session in &store.sessions {
            let id = self.create_session(&session.title, None, "");
            let at = session.created_at as i64;
            let conn = self.conn();
            let _ = conn.execute(
                "UPDATE sessions SET created_at = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, at, session.updated_at as i64],
            );
            drop(conn);
            for turn in &session.turns {
                let role = if turn.role == "user" {
                    "user"
                } else {
                    "assistant"
                };
                let kind = role;
                self.append_message(
                    &id,
                    role,
                    kind,
                    &[ContentPart::text(turn.text.clone())],
                    None,
                );
            }
            if store.active_id.as_deref() == Some(session.id.as_str()) {
                self.set_active_session(Some(&id));
            }
        }
    }
}

fn append_message_tx(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    role: &str,
    kind: &str,
    parts: &[ContentPart],
    model: Option<&str>,
) -> Message {
    let id = new_id("msg");
    let at = now_ms();
    let data = serde_json::to_string(parts).unwrap_or_else(|_| "[]".into());
    let _ = tx.execute(
        "UPDATE sessions SET last_seq = last_seq + 1, updated_at = ?2 WHERE id = ?1",
        params![session_id, at as i64],
    );
    let seq: i64 = tx
        .query_row(
            "SELECT last_seq FROM sessions WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let _ = tx.execute(
        "INSERT INTO messages
            (id, session_id, seq, role, type, data, created_at, completed_at, model)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            session_id,
            seq,
            role,
            kind,
            data,
            at as i64,
            if role == "assistant" && kind == "assistant" {
                None
            } else {
                Some(at as i64)
            },
            model
        ],
    );
    Message {
        id,
        session_id: session_id.to_string(),
        seq,
        role: role.into(),
        kind: kind.into(),
        parts: parts.to_vec(),
        created_at: at,
        ..Default::default()
    }
}

fn fallback_message(session_id: &str, role: &str, kind: &str, parts: &[ContentPart]) -> Message {
    Message {
        id: new_id("msg"),
        session_id: session_id.to_string(),
        seq: 0,
        role: role.into(),
        kind: kind.into(),
        parts: parts.to_vec(),
        created_at: now_ms(),
        ..Default::default()
    }
}

fn tool_run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ToolRunInfo> {
    let raw: String = r.get(6)?;
    Ok(ToolRunInfo {
        id: r.get(0)?,
        session_id: r.get(1)?,
        message_id: r.get(2)?,
        step: r.get(3)?,
        call_id: r.get(4)?,
        name: r.get(5)?,
        input: serde_json::from_str(&raw).unwrap_or(Value::Null),
        status: r.get(7)?,
        output: r.get(8)?,
        output_path: r.get(9)?,
        error: r.get(10)?,
        started_at: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        completed_at: r.get::<_, Option<i64>>(12)?.map(|v| v as u64),
    })
}

fn job_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobInfo> {
    let raw: String = r.get(5)?;
    Ok(JobInfo {
        id: r.get(0)?,
        session_id: r.get(1)?,
        kind: r.get(2)?,
        status: r.get(3)?,
        progress: r.get(4)?,
        payload: serde_json::from_str(&raw).unwrap_or(Value::Null),
        result: r.get(6)?,
        error: r.get(7)?,
        notify: r.get::<_, i64>(8)? != 0,
        created_at: r.get::<_, i64>(9)? as u64,
        updated_at: r.get::<_, i64>(10)? as u64,
        resume_at: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> Arc<Db> {
        let dir = std::env::temp_dir().join(format!("vk-agent-test-{}", new_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        Db::open(dir.join("agent.db")).unwrap()
    }

    #[test]
    fn session_messages_have_monotonic_sequences() {
        let db = temp_db();
        let id = db.create_session("Test", None, "gemini");
        let a = db.append_message(&id, "user", "user", &[ContentPart::text("one")], None);
        let b = db.append_message(
            &id,
            "assistant",
            "assistant",
            &[ContentPart::text("two")],
            None,
        );
        let c = db.append_message(&id, "user", "user", &[ContentPart::text("three")], None);
        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
        assert_eq!(db.messages(&id).len(), 3);
    }

    #[test]
    fn inbox_admission_is_idempotent_and_delivery_is_atomic() {
        let db = temp_db();
        let id = db.create_session("Test", None, "gemini");
        let payload = serde_json::json!({"text": "hello"});
        db.admit(&id, "user", "steer", Some("client-1"), &payload);
        db.admit(&id, "user", "steer", Some("client-1"), &payload);
        let pending = db.pending(&id, Some("steer"));
        assert_eq!(pending.len(), 1);
        let delivered = db.deliver(&pending[0].0).unwrap();
        assert_eq!(delivered.text(), "hello");
        assert!(db.pending(&id, Some("steer")).is_empty());
        assert_eq!(db.messages(&id).len(), 1);
    }

    #[test]
    fn claim_is_single_flight() {
        let db = temp_db();
        let id = db.create_session("Test", None, "gemini");
        assert!(db.claim(&id, "a"));
        assert!(!db.claim(&id, "b"));
        db.release(&id);
        assert!(db.claim(&id, "b"));
    }

    #[test]
    fn plan_and_permissions_round_trip() {
        let db = temp_db();
        let id = db.create_session("Test", None, "gemini");
        let plan = vec![PlanItem {
            id: "p1".into(),
            title: "Do it".into(),
            status: "in_progress".into(),
            note: String::new(),
        }];
        db.set_plan(&id, &plan);
        assert_eq!(db.plan(&id).len(), 1);
        db.add_permission("file.write", "notes/*", "allow", &id);
        let rules = db.permission_rules(&id);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].effect, Effect::Allow);
    }

    #[test]
    fn tool_runs_settle_and_idempotency_lookup_works() {
        let db = temp_db();
        let id = db.create_session("Test", None, "gemini");
        let run = db.start_run(&id, "gemini");
        let message = db.append_message(&id, "assistant", "assistant", &[], None);
        let tool = db.insert_tool_run(
            &id,
            &run,
            &message.id,
            0,
            "call_0",
            "write",
            &serde_json::json!({"path": "a.txt"}),
            Some("key-1"),
            false,
        );
        db.mark_tool_running(&tool);
        db.settle_tool_run(&tool, "succeeded", Some("ok"), None, None);
        let found = db.tool_run_by_key("key-1").unwrap();
        assert_eq!(found.status, "succeeded");
        assert_eq!(db.tool_runs_for_run(&run).len(), 1);
    }

    #[test]
    fn metrics_are_derived_from_rows() {
        let db = temp_db();
        let id = db.create_session("Test", None, "gemini");
        let run = db.start_run(&id, "gemini");
        db.usage(
            &id,
            &run,
            0,
            "chat",
            "gemini",
            &Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            },
            100,
        );
        let metrics = db.metrics();
        assert_eq!(metrics["runs"], 1);
        assert_eq!(metrics["inputTokens"], 10);
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use crate::core::sessions::SessionStore;

    #[test]
    fn imports_legacy_json_once() {
        let dir = std::env::temp_dir().join(format!("vk-agent-migrate-{}", new_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("agent.db")).unwrap();
        let mut legacy = SessionStore::default();
        let id = legacy.append_user_turn(None, "Hello from v1");
        legacy.append_model_turn(&id, "Hi there");
        // Pin the legacy active id so the import maps it.
        db.import_legacy(&legacy);
        let sessions = db.sessions(10);
        assert_eq!(sessions.len(), 1);
        assert_eq!(db.messages(&sessions[0].id).len(), 2);
        assert_eq!(
            db.active_session().as_deref(),
            Some(sessions[0].id.as_str())
        );
        // A second import is a no-op.
        db.import_legacy(&legacy);
        assert_eq!(db.sessions(10).len(), 1);
    }
}
