//! Gemini adapter for the agent runtime.
//!
//! The provider only streams events; the runner owns the step. Function calls
//! are lowered from canonical content parts into `functionCall` parts and back.
//! Google Search grounding is exposed as an explicit `web_search` tool so it
//! composes with multi-step tasks (built-in grounding cannot always be combined
//! with function declarations).

use std::io::{BufRead, BufReader};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use super::types::{FinishReason, LlmEvent, Usage};

const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/models";

pub const WEB_SEARCH_PROMPT: &str = "Search Google for the query below. Return a compact, \
factual digest of the most relevant results (5-10 bullets), including the source title \
and URL for each. If the query is time-sensitive, prefer recent sources.\n\nQuery: ";

#[derive(Clone)]
pub struct GeminiAgentClient {
    http: reqwest::blocking::Client,
    api_key: String,
    model: String,
    system_prompt: String,
}

#[derive(Debug)]
pub struct GeminiFailure {
    pub message: String,
    pub retryable: bool,
}

impl GeminiFailure {
    fn new(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            message: message.into(),
            retryable,
        }
    }
}

impl std::fmt::Display for GeminiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GeminiFailure {}

pub fn is_context_overflow(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("context") && (m.contains("long") || m.contains("window") || m.contains("exceed"))
        || m.contains("too many tokens")
        || m.contains("input token")
        || m.contains("maximum token")
}

pub fn is_retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

pub trait ChatEngine: Send + Sync {
    fn configured(&self) -> bool;
    fn model(&self) -> &str;
    fn stream_step(
        &self,
        contents: &[Value],
        tools: &[Value],
        terminal: bool,
        max_output_tokens: u32,
        cancel: &std::sync::atomic::AtomicBool,
        on_event: &mut dyn FnMut(LlmEvent) -> bool,
    ) -> Result<(), GeminiFailure>;
    fn summarize(&self, text: &str) -> Result<String, GeminiFailure>;
}

impl ChatEngine for GeminiAgentClient {
    fn configured(&self) -> bool {
        GeminiAgentClient::configured(self)
    }

    fn model(&self) -> &str {
        GeminiAgentClient::model(self)
    }

    fn stream_step(
        &self,
        contents: &[Value],
        tools: &[Value],
        terminal: bool,
        max_output_tokens: u32,
        cancel: &std::sync::atomic::AtomicBool,
        on_event: &mut dyn FnMut(LlmEvent) -> bool,
    ) -> Result<(), GeminiFailure> {
        GeminiAgentClient::stream_step(
            self,
            contents,
            tools,
            terminal,
            max_output_tokens,
            cancel,
            on_event,
        )
    }

    fn summarize(&self, text: &str) -> Result<String, GeminiFailure> {
        GeminiAgentClient::summarize(self, text)
    }
}

impl GeminiAgentClient {
    pub fn new(api_key: &str, model: &str, system_prompt: &str) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self {
            http,
            api_key: api_key.trim().to_string(),
            model: model.trim().to_string(),
            system_prompt: system_prompt.to_string(),
        }
    }

    pub fn configured(&self) -> bool {
        !self.api_key.is_empty()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Stream one model call. Emits exactly one terminal `Finish` or `Error`.
    /// Tool calls are complete parts; partial JSON deltas are not surfaced.
    pub fn stream_step(
        &self,
        contents: &[Value],
        tools: &[Value],
        terminal: bool,
        max_output_tokens: u32,
        cancel: &std::sync::atomic::AtomicBool,
        mut on_event: impl FnMut(LlmEvent) -> bool,
    ) -> Result<(), GeminiFailure> {
        if !self.configured() {
            return Err(GeminiFailure::new("Gemini is not configured", false));
        }
        let url = format!("{ENDPOINT}/{}:streamGenerateContent", self.model);
        let mut body = json!({
            "system_instruction": { "parts": [{ "text": self.system_prompt }] },
            "contents": contents,
            "generationConfig": {
                "maxOutputTokens": max_output_tokens,
                "temperature": 0.4
            }
        });
        if !tools.is_empty() {
            body["tools"] = json!([{ "functionDeclarations": tools }]);
            body["toolConfig"] = json!({
                "functionCallingConfig": {
                    "mode": if terminal { "NONE" } else { "AUTO" }
                }
            });
        }

        let response = self
            .http
            .post(&url)
            .query(&[("key", self.api_key.as_str()), ("alt", "sse")])
            .json(&body)
            .send()
            .map_err(|e| {
                let retryable = e.is_timeout() || e.is_connect() || e.is_request();
                GeminiFailure::new(format!("could not reach Gemini: {e}"), retryable)
            })?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().unwrap_or_default();
            let message = api_error_message(&text, status.as_u16());
            crate::app::debug_log(&format!(
                "agent gemini: stream {url} -> {} {message}",
                status.as_u16()
            ));
            return Err(GeminiFailure::new(
                message,
                is_retryable_status(status.as_u16()),
            ));
        }

        let mut usage = Usage::default();
        let mut finish: Option<FinishReason> = None;
        let mut saw_terminal = false;
        let reader = BufReader::new(response);
        for line in reader.lines() {
            if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(GeminiFailure::new("cancelled", false));
            }
            let Ok(line) = line else { break };
            let Some(raw) = line.strip_prefix("data:") else {
                continue;
            };
            let raw = raw.trim();
            if raw.is_empty() || raw == "[DONE]" {
                continue;
            }
            let Ok(data) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            if let Some(err) = data.get("error") {
                let message = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Gemini returned an error");
                return Err(GeminiFailure::new(message, !is_context_overflow(message)));
            }
            if let Some(meta) = data.get("usageMetadata") {
                usage.input_tokens = meta
                    .get("promptTokenCount")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(usage.input_tokens);
                usage.output_tokens = meta
                    .get("candidatesTokenCount")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(usage.output_tokens);
                usage.cache_read = meta
                    .get("cachedContentTokenCount")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(usage.cache_read);
            }
            let Some(candidate) = data.get("candidates").and_then(|c| c.get(0)).cloned() else {
                continue;
            };
            if let Some(parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        if !text.is_empty() && !on_event(LlmEvent::TextDelta(text.to_string())) {
                            return Err(GeminiFailure::new("cancelled", false));
                        }
                    }
                    if let Some(call) = part.get("functionCall") {
                        let name = call
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let input = call.get("args").cloned().unwrap_or_else(|| json!({}));
                        let call_id = call.get("id").and_then(|v| v.as_str()).map(str::to_string);
                        let thought_signature = part
                            .get("thoughtSignature")
                            .or_else(|| part.get("thought_signature"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        if !name.is_empty()
                            && !on_event(LlmEvent::ToolCall {
                                name,
                                input,
                                call_id,
                                thought_signature,
                            })
                        {
                            return Err(GeminiFailure::new("cancelled", false));
                        }
                    }
                }
            }
            if let Some(reason) = candidate.get("finishReason").and_then(|v| v.as_str()) {
                finish = Some(match reason {
                    "MAX_TOKENS" => FinishReason::Length,
                    "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => {
                        FinishReason::ContentFilter
                    }
                    _ => FinishReason::Stop,
                });
                saw_terminal = true;
            }
        }

        if !saw_terminal {
            return Err(GeminiFailure::new(
                "incomplete stream (no finish reason)",
                true,
            ));
        }
        let _ = on_event(LlmEvent::Finish {
            reason: finish.unwrap_or(FinishReason::Stop),
            usage,
        });
        Ok(())
    }

    /// One non-streaming call, used by compaction and the web-search tool.
    pub fn generate(
        &self,
        contents: &[Value],
        grounding: bool,
        max_output_tokens: u32,
        temperature: f32,
    ) -> Result<(String, Vec<String>), GeminiFailure> {
        if !self.configured() {
            return Err(GeminiFailure::new("Gemini is not configured", false));
        }
        let url = format!("{ENDPOINT}/{}:generateContent", self.model);
        let mut body = json!({
            "system_instruction": { "parts": [{ "text": self.system_prompt }] },
            "contents": contents,
            "generationConfig": {
                "maxOutputTokens": max_output_tokens,
                "temperature": temperature
            }
        });
        if grounding {
            body["tools"] = json!([{ "google_search": {} }]);
        }
        let response = self
            .http
            .post(&url)
            .query(&[("key", self.api_key.as_str())])
            .json(&body)
            .send()
            .map_err(|e| GeminiFailure::new(format!("could not reach Gemini: {e}"), true))?;
        let status = response.status();
        let data: Value = response
            .json()
            .map_err(|e| GeminiFailure::new(format!("bad Gemini response: {e}"), false))?;
        if !status.is_success() {
            let message = data
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("Gemini returned an error");
            crate::app::debug_log(&format!(
                "agent gemini: generate {url} -> {} {message}",
                status.as_u16()
            ));
            // Some accounts/models reject grounding. Retry without it once.
            if grounding && message.to_lowercase().contains("tool") {
                return self.generate(contents, false, max_output_tokens, temperature);
            }
            return Err(GeminiFailure::new(
                message,
                is_retryable_status(status.as_u16()),
            ));
        }
        let (text, sources) = parse_candidates(&data);
        Ok((text, sources))
    }

    pub fn web_search(&self, query: &str) -> Result<(String, Vec<String>), GeminiFailure> {
        let prompt = format!("{WEB_SEARCH_PROMPT}{query}");
        let contents = vec![json!({
            "role": "user",
            "parts": [{ "text": prompt }]
        })];
        self.generate(&contents, true, 2048, 0.2)
    }

    pub fn summarize(&self, text: &str) -> Result<String, GeminiFailure> {
        let contents = vec![json!({
            "role": "user",
            "parts": [{ "text": text }]
        })];
        let (summary, _) = self.generate(&contents, false, 2048, 0.1)?;
        Ok(summary)
    }
}

fn parse_candidates(data: &Value) -> (String, Vec<String>) {
    let mut text = String::new();
    let mut sources = Vec::new();
    let Some(candidates) = data.get("candidates").and_then(|c| c.as_array()) else {
        return (text, sources);
    };
    for candidate in candidates {
        if let Some(parts) = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
        }
        if let Some(chunks) = candidate
            .get("groundingMetadata")
            .and_then(|g| g.get("groundingChunks"))
            .and_then(|c| c.as_array())
        {
            for chunk in chunks {
                let web = chunk.get("web");
                let title = web
                    .and_then(|w| w.get("title"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let uri = web
                    .and_then(|w| w.get("uri"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !title.is_empty() || !uri.is_empty() {
                    let label = if title.is_empty() {
                        uri.to_string()
                    } else {
                        title.to_string()
                    };
                    if !sources.contains(&label) {
                        sources.push(label);
                    }
                }
            }
        }
    }
    (text.trim().to_string(), sources)
}

fn api_error_message(body: &str, status: u16) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(message) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|v| v.as_str())
        {
            return format!("Gemini error ({status}): {message}");
        }
    }
    format!("Gemini error ({status}): {}", body.trim())
}

/// Convert durable messages into provider `contents`.
pub fn contents_from_messages(messages: &[super::types::Message]) -> Vec<Value> {
    use super::types::ContentPart;
    let mut contents = Vec::new();
    for message in messages {
        let role = if message.role == "assistant" {
            "model"
        } else {
            "user"
        };
        let mut parts = Vec::new();
        for part in &message.parts {
            match part {
                ContentPart::Text { text } => {
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
                ContentPart::Image { path, media_type } => {
                    if let Ok(bytes) = std::fs::read(path) {
                        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                        parts.push(json!({
                            "inlineData": {
                                "mimeType": media_type,
                                "data": data
                            }
                        }));
                    }
                }
                ContentPart::ToolCall {
                    call_id,
                    name,
                    input,
                    thought_signature,
                } => {
                    let mut part = json!({
                        "functionCall": {
                            "name": name,
                            "args": input,
                            "id": call_id
                        }
                    });
                    if let Some(signature) = thought_signature {
                        part["thoughtSignature"] = json!(signature);
                    }
                    parts.push(part);
                }
                ContentPart::ToolResponse {
                    call_id,
                    name,
                    output,
                    is_error,
                } => {
                    parts.push(json!({
                        "functionResponse": {
                            "id": call_id,
                            "name": name,
                            "response": if *is_error {
                                json!({ "error": output })
                            } else {
                                json!({ "result": output })
                            }
                        }
                    }));
                }
            }
        }
        if parts.is_empty() {
            continue;
        }
        contents.push(json!({ "role": role, "parts": parts }));
    }
    contents
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_targets_the_models_resource() {
        assert!(ENDPOINT.ends_with("/v1beta/models"));
        let url = format!("{ENDPOINT}/gemini-3.1-flash-lite:streamGenerateContent");
        assert_eq!(
            url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.1-flash-lite:streamGenerateContent"
        );
    }

    #[test]
    fn api_errors_keep_the_status_and_message() {
        let message = api_error_message(r#"{"error":{"message":"model not found"}}"#, 404);
        assert!(message.contains("404"));
        assert!(message.contains("model not found"));
    }
}

#[cfg(test)]
mod signature_tests {
    use super::*;
    use crate::core::agent::types::{ContentPart, Message};

    #[test]
    fn function_calls_round_trip_id_and_thought_signature() {
        let message = Message {
            id: "m".into(),
            session_id: "s".into(),
            seq: 1,
            role: "assistant".into(),
            kind: "assistant".into(),
            parts: vec![ContentPart::ToolCall {
                call_id: "call_123".into(),
                name: "web_search".into(),
                input: serde_json::json!({"query": "news"}),
                thought_signature: Some("sig-abc".into()),
            }],
            ..Default::default()
        };
        let contents = contents_from_messages(&[message]);
        let part = &contents[0]["parts"][0];
        assert_eq!(part["functionCall"]["id"], "call_123");
        assert_eq!(part["functionCall"]["name"], "web_search");
        assert_eq!(part["thoughtSignature"], "sig-abc");

        let reply = Message {
            id: "t".into(),
            session_id: "s".into(),
            seq: 2,
            role: "tool".into(),
            kind: "tool".into(),
            parts: vec![ContentPart::ToolResponse {
                call_id: "call_123".into(),
                name: "web_search".into(),
                output: "ok".into(),
                is_error: false,
            }],
            ..Default::default()
        };
        let contents = contents_from_messages(&[reply]);
        assert_eq!(
            contents[0]["parts"][0]["functionResponse"]["id"],
            "call_123"
        );
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;
    use crate::core::agent::types::{ContentPart, Message};

    #[test]
    fn image_parts_become_inline_data() {
        let path = std::env::temp_dir().join("vk-agent-image-test.png");
        std::fs::write(&path, [137u8, 80, 78, 71]).unwrap();
        let message = Message {
            id: "m".into(),
            session_id: "s".into(),
            seq: 1,
            role: "user".into(),
            kind: "user".into(),
            parts: vec![
                ContentPart::text("what is this?"),
                ContentPart::Image {
                    path: path.to_string_lossy().to_string(),
                    media_type: "image/png".into(),
                },
            ],
            ..Default::default()
        };
        let contents = contents_from_messages(&[message]);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert!(parts.iter().any(|part| part.get("inlineData").is_some()));
        assert_eq!(
            parts
                .iter()
                .find_map(|part| part.get("inlineData"))
                .unwrap()["mimeType"],
            "image/png"
        );
        let _ = std::fs::remove_file(path);
    }
}
