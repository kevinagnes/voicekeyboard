//! Shared contracts for the agent runtime.
//!
//! These types are deliberately provider-neutral. The Gemini adapter lowers
//! them into `generateContent` bodies; the durable store keeps their JSON so a
//! run can be replayed after a restart.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Short, sortable, collision-resistant identifier. Prefixes make logs and the
/// inspector readable (`ses_`, `run_`, `msg_`, `call_`, `job_`).
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        path: String,
        #[serde(rename = "mediaType")]
        media_type: String,
    },
    ToolCall {
        #[serde(rename = "callId")]
        call_id: String,
        name: String,
        input: Value,
        /// Gemini 3 requires echoing the thought signature that came with a
        /// function-call part; dropping it makes the next request fail.
        #[serde(
            rename = "thoughtSignature",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        thought_signature: Option<String>,
    },
    ToolResponse {
        #[serde(rename = "callId")]
        call_id: String,
        name: String,
        output: String,
        #[serde(rename = "isError")]
        is_error: bool,
    },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub seq: i64,
    /// user | assistant | tool | system
    pub role: String,
    /// user | assistant | tool | synthetic | compaction | idle | notice
    pub kind: String,
    pub parts: Vec<ContentPart>,
    pub created_at: u64,
    pub completed_at: Option<u64>,
    pub error: Option<String>,
    pub model: Option<String>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    pub cost: f64,
}

impl Message {
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_calls(&self) -> Vec<(&str, &str, &Value)> {
        self.parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolCall {
                    call_id,
                    name,
                    input,
                    ..
                } => Some((call_id.as_str(), name.as_str(), input)),
                _ => None,
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolPack {
    Core,
    Workspace,
    Web,
    Desktop,
    Memory,
    Delegation,
}

impl ToolPack {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Workspace => "workspace",
            Self::Web => "web",
            Self::Desktop => "desktop",
            Self::Memory => "memory",
            Self::Delegation => "delegation",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub pack: ToolPack,
    /// Permission action, e.g. `file.write`, `web.fetch`.
    pub action: String,
    /// Read-only tools skip idempotency keys and repeat detection.
    pub read_only: bool,
    /// Side effects are never blindly retried after a crash.
    pub idempotent: bool,
}

impl ToolSpec {
    pub fn new(
        name: &str,
        description: &str,
        input_schema: Value,
        pack: ToolPack,
        action: &str,
    ) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            input_schema,
            pack,
            action: action.to_string(),
            read_only: false,
            idempotent: false,
        }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self.idempotent = true;
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToolResult {
    pub text: String,
    pub data: Option<Value>,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            data: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ToolFailure {
    pub message: String,
    pub retryable: bool,
}

impl ToolFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

impl std::fmt::Display for ToolFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolFailure {}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            _ => Self::Ask,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PermissionRule {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
    pub scope: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Decision {
    #[serde(rename = "allowOnce")]
    AllowOnce,
    #[serde(rename = "alwaysAllow")]
    AlwaysAllow,
    #[serde(rename = "deny")]
    Deny,
    #[serde(rename = "denyWithReason")]
    DenyWithReason(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub id: String,
    pub session_id: String,
    pub action: String,
    pub resource: String,
    pub title: String,
    pub detail: String,
    pub allow_always: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ContentFilter,
    Error,
    Incomplete,
}

#[derive(Clone, Debug)]
pub enum LlmEvent {
    TextDelta(String),
    ToolCall {
        name: String,
        input: Value,
        call_id: Option<String>,
        thought_signature: Option<String>,
    },
    Finish {
        reason: FinishReason,
        usage: Usage,
    },
    Error {
        message: String,
        retryable: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    pub id: String,
    pub title: String,
    /// pending | in_progress | completed | blocked
    pub status: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactInfo {
    pub id: String,
    pub session_id: String,
    pub path: String,
    pub title: String,
    pub mime: String,
    pub bytes: i64,
    pub sha256: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStepInfo {
    pub id: String,
    pub run_id: String,
    pub session_id: String,
    pub step: i64,
    pub attempt: i64,
    pub retry_of: Option<i64>,
    pub provider: String,
    pub model: String,
    pub prompt_hash: String,
    pub prefix_hash: String,
    pub message_count: i64,
    pub tool_names: Vec<String>,
    pub finish_reason: Option<String>,
    pub output_started: bool,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub duration_ms: i64,
    pub first_text_ms: i64,
    pub error: Option<String>,
    pub created_at: u64,
    pub prompt_text: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunInfo {
    pub id: String,
    pub session_id: String,
    pub status: String,
    pub reason: Option<String>,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub step_count: i64,
    pub model: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cost: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRunInfo {
    pub id: String,
    pub session_id: String,
    pub message_id: String,
    pub step: i64,
    pub call_id: String,
    pub name: String,
    pub input: Value,
    pub status: String,
    pub output: Option<String>,
    pub output_path: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobInfo {
    pub id: String,
    pub session_id: Option<String>,
    pub kind: String,
    pub status: String,
    pub progress: f64,
    pub payload: Value,
    pub result: Option<String>,
    pub error: Option<String>,
    pub notify: bool,
    pub created_at: u64,
    pub updated_at: u64,
    pub resume_at: Option<u64>,
}

/// One status frame for the overlay. The final values are always durable; the
/// streamed text is only a hint that the UI reconciles at the end.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    /// idle | transcribing | thinking | tool | speaking | done | cancelled |
    /// cancelling | error
    pub phase: String,
    pub transcript: String,
    pub reply: String,
    pub detail: String,
    pub error: Option<String>,
    pub sources: Vec<String>,
    pub session_id: String,
    pub deep_work: bool,
    pub plan: Vec<PlanItem>,
    pub tools: Vec<String>,
    pub artifacts: Vec<ArtifactInfo>,
    /// Internal: prevents an older run from overwriting a newer overlay.
    #[serde(skip)]
    pub generation: u64,
}

impl AgentStatus {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            phase: "thinking".into(),
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug)]
pub enum AgentEvent {
    Status(AgentStatus),
    Permission(PermissionRequest),
    PermissionClosed(String),
    SessionChanged,
    RunsChanged,
}

/// Budgets are policy, not prompt text.
#[derive(Clone, Copy, Debug)]
pub struct RunPolicy {
    pub max_steps: u32,
    pub deep_work: bool,
    pub output_cap: u32,
}

impl RunPolicy {
    pub fn normal() -> Self {
        Self {
            max_steps: 24,
            deep_work: false,
            output_cap: 2_048,
        }
    }

    pub fn deep_work() -> Self {
        Self {
            max_steps: 40,
            deep_work: true,
            output_cap: 8_192,
        }
    }

    pub fn child() -> Self {
        Self {
            max_steps: 12,
            deep_work: false,
            output_cap: 4_096,
        }
    }

    pub fn hard_cap(self) -> u32 {
        if self.deep_work {
            60
        } else {
            24
        }
    }
}
