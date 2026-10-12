use base64::Engine as _;
use serde::Serialize;
use serde_json::{json, Value};
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use crate::app::debug_log;

pub const GEMINI_API_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/models";
pub const GEMINI_INTERACTIONS_ENDPOINT: &str =
    "https://generativelanguage.googleapis.com/v1beta/interactions";
pub const GEMINI_VOICES_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/voices";
pub const TTS_SAMPLE_RATE: u32 = 24_000;

#[derive(Debug, Clone)]
pub enum GeminiError {
    NoApiKey,
    Chat(String),
    Tts(String),
    Catalog(String),
    Cancelled,
}

impl fmt::Display for GeminiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoApiKey => write!(f, "No Gemini API key — add it in Settings"),
            Self::Chat(msg) | Self::Tts(msg) | Self::Catalog(msg) => write!(f, "{msg}"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for GeminiError {}

/// Thin Gemini client for the "Ask LLM" flow: chat grounded in Google Search
/// (generateContent `google_search` tool), streaming speech synthesis
/// (Interactions API, SSE) and the models/voices catalog. Request/response
/// shapes mirror the reference implementation in the personalcompanion
/// project (`language_code`-filtered voice lists).
#[derive(Clone)]
pub struct GeminiClient {
    http: reqwest::blocking::Client,
    /// No overall timeout — used for long-running TTS streams.
    http_stream: reqwest::blocking::Client,
    api_key: String,
    model: String,
    system_prompt: String,
    tts_model: String,
    tts_voice: String,
}

/// One turn of a session sent to Gemini (`role` is `user` or `model`).
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub text: String,
}

impl GeminiClient {
    pub fn new(
        api_key: &str,
        model: &str,
        system_prompt: &str,
        tts_model: &str,
        tts_voice: &str,
    ) -> Self {
        Self {
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(90))
                .build()
                .expect("reqwest client builds"),
            http_stream: reqwest::blocking::Client::builder()
                .timeout(None)
                .build()
                .expect("reqwest streaming client builds"),
            api_key: api_key.trim().to_string(),
            model: model.trim().to_string(),
            system_prompt: system_prompt.to_string(),
            tts_model: tts_model.trim().to_string(),
            tts_voice: tts_voice.trim().to_string(),
        }
    }

    pub fn configured(&self) -> bool {
        !self.api_key.is_empty()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// One-shot chat grounded in Google Search. `messages` is the whole session
    /// so far, ending with the new question, so Gemini keeps the conversation
    /// context. The `google_search` tool is enabled natively: Gemini decides on
    /// its own when a question needs current information, runs the searches and
    /// grounds the answer in the results. The sources behind the answer come
    /// back in `ChatOutcome::sources`.
    ///
    /// Models that predate grounding (or accounts where it is unavailable)
    /// reject the tool with a 400; the request is then retried without it so
    /// the question still gets an answer from model knowledge.
    pub fn chat(&self, messages: &[ChatMessage]) -> Result<ChatOutcome, GeminiError> {
        if !self.configured() {
            return Err(GeminiError::NoApiKey);
        }
        let last = messages.last().map(|m| m.text.trim()).unwrap_or_default();
        if last.is_empty() {
            return Err(GeminiError::Chat("empty question".into()));
        }
        let contents: Vec<Value> = messages
            .iter()
            .map(|m| json!({"role": m.role, "parts": [{"text": m.text}]}))
            .collect();
        let data = match self.generate(&contents, true) {
            Ok(data) => data,
            Err(GeminiError::Chat(msg)) if rejects_grounding(&msg) => {
                debug_log(&format!(
                    "gemini: grounding rejected ({msg}); answering without web search"
                ));
                self.generate(&contents, false)?
            }
            Err(e) => return Err(e),
        };
        let (text, sources) = parse_grounded_candidates(&data);
        let text = text.trim().to_string();
        if text.is_empty() {
            return Err(GeminiError::Chat("Gemini returned an empty reply".into()));
        }
        Ok(ChatOutcome { text, sources })
    }

    /// POST to `generateContent`, optionally enabling the `google_search`
    /// grounding tool.
    fn generate(&self, contents: &[Value], grounding: bool) -> Result<Value, GeminiError> {
        let url = format!("{GEMINI_API_ENDPOINT}/{}:generateContent", self.model);
        let mut body = json!({
            "system_instruction": {
                "parts": [{ "text": self.system_prompt }]
            },
            "contents": contents,
            "generationConfig": {
                "maxOutputTokens": 8000,
                "temperature": 0.4
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
            .map_err(|e| GeminiError::Chat(format!("could not reach Gemini: {e}")))?;
        let status = response.status();
        let data: Value = response
            .json()
            .map_err(|e| GeminiError::Chat(format!("bad Gemini response: {e}")))?;
        if !status.is_success() {
            return Err(GeminiError::Chat(api_error_message(&data, status)));
        }
        Ok(data)
    }

    /// Stream speech for `text` via the Gemini Interactions API
    /// (`stream: true`, `audio/l16`, 24 kHz — raw little-endian 16-bit PCM).
    /// Each SSE `step.delta` audio payload is delivered to `on_audio` as it
    /// arrives, so playback can start long before the reply is complete.
    pub fn tts_stream(
        &self,
        text: &str,
        mut on_audio: impl FnMut(&[u8]) -> Result<(), GeminiError>,
    ) -> Result<(), GeminiError> {
        if !self.configured() {
            return Err(GeminiError::NoApiKey);
        }
        let body = json!({
            "model": self.tts_model,
            "input": [
                {
                    "type": "user_input",
                    "content": [{ "type": "text", "text": text }]
                }
            ],
            "response_format": {
                "type": "audio",
                "mime_type": "audio/l16",
                "sample_rate": TTS_SAMPLE_RATE
            },
            "generation_config": {
                "speech_config": [{ "voice": self.tts_voice }]
            },
            "stream": true
        });
        let response = self
            .http_stream
            .post(GEMINI_INTERACTIONS_ENDPOINT)
            .query(&[("key", self.api_key.as_str())])
            .json(&body)
            .send()
            .map_err(|e| GeminiError::Tts(format!("could not reach Gemini TTS: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().unwrap_or_default();
            let data: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
            return Err(GeminiError::Tts(api_error_message(&data, status)));
        }

        let mut reader = BufReader::new(response);
        let emitted = parse_sse(&mut reader, |bytes| {
            on_audio(bytes)?;
            Ok(())
        })
        .map_err(GeminiError::Tts)?;
        if !emitted {
            return Err(GeminiError::Tts("Gemini TTS returned no audio".into()));
        }
        Ok(())
    }

    /// Fetch the Gemini model catalog (chat models + TTS models) and the TTS
    /// voices for `language` (a BCP-47 code, e.g. `pt-BR`). When the filtered
    /// list is empty the unfiltered list is returned instead, mirroring the
    /// reference client.
    pub fn fetch_catalog(
        api_key: &str,
        language: Option<&str>,
    ) -> Result<GeminiCatalog, GeminiError> {
        let key = api_key.trim();
        if key.is_empty() {
            return Err(GeminiError::NoApiKey);
        }
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| GeminiError::Catalog(e.to_string()))?;

        let response = http
            .get(GEMINI_API_ENDPOINT)
            .query(&[("key", key)])
            .send()
            .map_err(|e| GeminiError::Catalog(format!("could not reach Gemini: {e}")))?;
        let status = response.status();
        let data: Value = response
            .json()
            .map_err(|e| GeminiError::Catalog(format!("bad Gemini response: {e}")))?;
        if !status.is_success() {
            return Err(GeminiError::Catalog(api_error_message(&data, status)));
        }
        let mut chat_models = Vec::new();
        let mut tts_models = Vec::new();
        for model in data["models"].as_array().unwrap_or(&Vec::new()) {
            let Some(name) = model["name"].as_str() else {
                continue;
            };
            let Some(name) = name.strip_prefix("models/") else {
                continue;
            };
            if !name.starts_with("gemini-") {
                continue;
            }
            if name.contains("tts") {
                tts_models.push(name.to_string());
            } else if !name.contains("image") && !name.contains("embedding") {
                chat_models.push(name.to_string());
            }
        }
        chat_models.sort();
        tts_models.sort();

        let voices = fetch_voices(&http, key, language)?;
        Ok(GeminiCatalog {
            chat_models,
            tts_models,
            voices,
        })
    }
}

/// A web source behind a grounded reply.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
}

/// The final spoken answer, plus the Google Search sources it was grounded in.
#[derive(Debug, Clone)]
pub struct ChatOutcome {
    pub text: String,
    pub sources: Vec<SearchResult>,
}

/// Extract the reply text and the grounded sources from a `generateContent`
/// response. Grounded candidates carry a `groundingMetadata` object whose
/// `groundingChunks` list one `web` entry (uri + title) per source, and whose
/// `webSearchQueries` list the searches Gemini ran.
fn parse_grounded_candidates(data: &Value) -> (String, Vec<SearchResult>) {
    let mut text = String::new();
    let mut sources: Vec<SearchResult> = Vec::new();
    let mut queries: Vec<String> = Vec::new();
    for candidate in data["candidates"].as_array().unwrap_or(&Vec::new()) {
        for part in candidate["content"]["parts"]
            .as_array()
            .unwrap_or(&Vec::new())
        {
            if let Some(t) = part["text"].as_str() {
                text.push_str(t);
            }
        }
        let metadata = &candidate["groundingMetadata"];
        if let Some(list) = metadata["webSearchQueries"].as_array() {
            queries.extend(list.iter().filter_map(|q| q.as_str().map(str::to_string)));
        }
        for chunk in metadata["groundingChunks"]
            .as_array()
            .unwrap_or(&Vec::new())
        {
            let web = &chunk["web"];
            let Some(uri) = web["uri"].as_str().filter(|u| !u.is_empty()) else {
                continue;
            };
            let title = web["title"].as_str().unwrap_or("").trim();
            let source = SearchResult {
                title: if title.is_empty() {
                    host_of(uri)
                } else {
                    title.to_string()
                },
                url: uri.to_string(),
            };
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
    }
    if !queries.is_empty() {
        debug_log(&format!(
            "gemini: Google Search queries: {}",
            queries.join(", ")
        ));
    }
    (text, sources)
}

/// The host of a URL, used as a source label when Google Search returns no
/// title for a grounding chunk.
fn host_of(uri: &str) -> String {
    let rest = uri.split_once("://").map(|(_, rest)| rest).unwrap_or(uri);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .to_string()
}

/// Whether an API error means this model/account cannot use the Google Search
/// grounding tool (rather than a transient failure worth surfacing). Grounding
/// rejections come back as 400 `INVALID_ARGUMENT` naming the tool.
fn rejects_grounding(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("google_search")
        || message.contains("google search")
        || message.contains("tool")
        || message.contains("unknown name")
}

/// Map an app STT language code to a Gemini BCP-47 locale for voice lists.
pub fn gemini_locale(code: &str) -> Option<String> {
    match code {
        "" | "auto" => None,
        "en" => Some("en-US".into()),
        "pt" => Some("pt-BR".into()),
        "es" => Some("es-ES".into()),
        "fr" => Some("fr-FR".into()),
        "de" => Some("de-DE".into()),
        "it" => Some("it-IT".into()),
        "nl" => Some("nl-NL".into()),
        "pl" => Some("pl-PL".into()),
        "tr" => Some("tr-TR".into()),
        "ru" => Some("ru-RU".into()),
        "uk" => Some("uk-UA".into()),
        "ja" => Some("ja-JP".into()),
        "ko" => Some("ko-KR".into()),
        "zh" => Some("zh-CN".into()),
        "hi" => Some("hi-IN".into()),
        other => Some(other.to_string()),
    }
}

/// A voice from the Gemini voices catalog, with the languages it can speak.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiVoice {
    pub id: String,
    pub languages: Vec<String>,
    pub gender: String,
    pub persona: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiCatalog {
    pub chat_models: Vec<String>,
    pub tts_models: Vec<String>,
    pub voices: Vec<GeminiVoice>,
}

fn fetch_voices(
    http: &reqwest::blocking::Client,
    api_key: &str,
    language: Option<&str>,
) -> Result<Vec<GeminiVoice>, GeminiError> {
    if let Some(lang) = language {
        let voices = fetch_voices_lang(http, api_key, lang)?;
        if !voices.is_empty() {
            return Ok(voices);
        }
    }
    fetch_voices_lang(http, api_key, "")
}

fn fetch_voices_lang(
    http: &reqwest::blocking::Client,
    api_key: &str,
    language: &str,
) -> Result<Vec<GeminiVoice>, GeminiError> {
    let mut query: Vec<(&str, &str)> = vec![("key", api_key), ("page_size", "1000")];
    if !language.is_empty() {
        query.push(("language_code", language));
    }
    let response = http
        .get(GEMINI_VOICES_ENDPOINT)
        .query(&query)
        .send()
        .map_err(|e| GeminiError::Catalog(format!("could not reach Gemini voices: {e}")))?;
    let status = response.status();
    let data: Value = response
        .json()
        .map_err(|e| GeminiError::Catalog(format!("bad Gemini voices response: {e}")))?;
    if !status.is_success() {
        return Err(GeminiError::Catalog(api_error_message(&data, status)));
    }
    let mut voices = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for voice in data["voices"].as_array().unwrap_or(&Vec::new()) {
        let Some(id) = voice["id"].as_str() else {
            continue;
        };
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        let languages = voice["language_codes"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|l| l.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| {
                voice["language_code"]
                    .as_str()
                    .map(|l| vec![l.to_string()])
                    .unwrap_or_default()
            });
        voices.push(GeminiVoice {
            id: id.to_string(),
            languages,
            gender: voice["gender"].as_str().unwrap_or("").to_string(),
            persona: voice["persona"].as_str().unwrap_or("").to_string(),
        });
    }
    voices.sort_by(|a, b| a.id.to_lowercase().cmp(&b.id.to_lowercase()));
    Ok(voices)
}

/// Extract the audio payload from a streaming Interactions SSE event. Mirrors
/// the reference client: only `step.delta` events whose delta is audio.
fn audio_delta(event: &Value) -> Option<Vec<u8>> {
    if event["event_type"].as_str() != Some("step.delta") {
        return None;
    }
    let delta = event["delta"].as_object()?;
    let is_audio = delta.get("type").and_then(Value::as_str) == Some("audio")
        || delta
            .get("mime_type")
            .and_then(Value::as_str)
            .is_some_and(|m| m.starts_with("audio"));
    if !is_audio {
        return None;
    }
    let data = delta.get("data")?.as_str()?;
    if data.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

/// Consume an SSE stream, calling `on_audio` for every audio `step.delta`.
/// Returns whether at least one audio chunk was delivered.
fn parse_sse<R, F>(reader: R, mut on_audio: F) -> Result<bool, String>
where
    R: Read,
    F: for<'a> FnMut(&'a [u8]) -> Result<(), GeminiError>,
{
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut pending: Option<String> = None;
    let mut emitted = false;
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();
        if let Some(payload) = trimmed.strip_prefix("data:") {
            let payload = payload.trim();
            pending = Some(match pending {
                Some(prev) => format!("{prev}\n{payload}"),
                None => payload.to_string(),
            });
        } else if trimmed.is_empty() {
            if let Some(payload) = pending.take() {
                if payload == "[DONE]" {
                    break;
                }
                let Ok(event) = serde_json::from_str::<Value>(&payload) else {
                    continue;
                };
                if let Some(delta) = audio_delta(&event) {
                    emitted = true;
                    on_audio(&delta).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    Ok(emitted)
}

/// A user-friendly error message extracted from a Gemini error body.
fn api_error_message(data: &Value, status: reqwest::StatusCode) -> String {
    let detail = data["error"]["message"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
    format!("Gemini API error: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_api_key_reported() {
        let client = GeminiClient::new("", "gemini-x", "", "gemini-tts", "Kore");
        assert!(!client.configured());
        let messages = vec![ChatMessage {
            role: "user".into(),
            text: "hi".into(),
        }];
        assert!(matches!(client.chat(&messages), Err(GeminiError::NoApiKey)));
    }

    #[test]
    fn empty_question_rejected() {
        let client = GeminiClient::new("key", "gemini-x", "", "gemini-tts", "Kore");
        assert!(matches!(client.chat(&[]), Err(GeminiError::Chat(_))));
        let messages = vec![ChatMessage {
            role: "user".into(),
            text: "   ".into(),
        }];
        assert!(matches!(client.chat(&messages), Err(GeminiError::Chat(_))));
    }

    #[test]
    fn audio_delta_parses_l16() {
        let event = serde_json::from_str(
            r#"{"event_type":"step.delta","delta":{"type":"audio","mime_type":"audio/l16","data":"cGF0Y2g="}}"#,
        )
        .unwrap();
        let data = audio_delta(&event).unwrap();
        assert_eq!(data, b"patch");
    }

    #[test]
    fn audio_delta_ignores_non_audio() {
        let event = serde_json::from_str(
            r#"{"event_type":"step.delta","delta":{"type":"text","text":"hi"}}"#,
        )
        .unwrap();
        assert!(audio_delta(&event).is_none());

        let other = serde_json::from_str(r#"{"event_type":"step.complete"}"#).unwrap();
        assert!(audio_delta(&other).is_none());
    }

    #[test]
    fn sse_pipeline_yields_audio_chunks() {
        let sse = concat!(
            "event: step.delta\n",
            "data: {\"event_type\":\"step.delta\",\"delta\":{\"type\":\"audio\",\"data\":\"QUFB\"}}\n\n",
            "event: step.delta\n",
            "data: {\"event_type\":\"step.delta\",\"delta\":{\"type\":\"audio\",\"data\":\"QkJC\"}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut chunks = Vec::new();
        let emitted = parse_sse(sse.as_bytes(), &mut |bytes: &[u8]| {
            chunks.push(bytes.to_vec());
            Ok(())
        })
        .unwrap();
        assert!(emitted);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], b"AAA");
        assert_eq!(chunks[1], b"BBB");
    }

    #[test]
    fn sse_pipeline_no_audio_reports_empty() {
        let sse = "event: step.complete\ndata: {\"event_type\":\"step.complete\"}\n\n";
        let emitted = parse_sse(sse.as_bytes(), &mut |_: &[u8]| Ok(())).unwrap();
        assert!(!emitted);
    }

    #[test]
    fn locale_mapping() {
        assert_eq!(gemini_locale("pt"), Some("pt-BR".into()));
        assert_eq!(gemini_locale("en"), Some("en-US".into()));
        assert_eq!(gemini_locale("auto"), None);
    }

    #[test]
    fn grounded_reply_parsed_with_sources() {
        let data: Value = serde_json::from_str(
            r#"{"candidates":[{"content":{"parts":[{"text":"Spain won Euro 2024."}]},"groundingMetadata":{
                "webSearchQueries":["UEFA Euro 2024 winner"],
                "groundingChunks":[
                    {"web":{"uri":"https://example.com/euro","title":"example.com"}},
                    {"web":{"uri":"https://other.example/euro","title":"other.example"}},
                    {"web":{"uri":"https://example.com/euro","title":"example.com"}}
                ]}}]}"#,
        )
        .unwrap();
        let (text, sources) = parse_grounded_candidates(&data);
        assert_eq!(text, "Spain won Euro 2024.");
        assert_eq!(
            sources,
            vec![
                SearchResult {
                    title: "example.com".into(),
                    url: "https://example.com/euro".into(),
                },
                SearchResult {
                    title: "other.example".into(),
                    url: "https://other.example/euro".into(),
                },
            ]
        );
    }

    #[test]
    fn ungrounded_reply_has_no_sources() {
        let data: Value =
            serde_json::from_str(r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#)
                .unwrap();
        let (text, sources) = parse_grounded_candidates(&data);
        assert_eq!(text, "hi");
        assert!(sources.is_empty());
    }

    #[test]
    fn grounded_source_without_title_uses_host() {
        let data: Value = serde_json::from_str(
            r#"{"candidates":[{"content":{"parts":[{"text":"x"}]},"groundingMetadata":{"groundingChunks":[
                {"web":{"uri":"https://news.example.com/story?id=1"}}
            ]}}]}"#,
        )
        .unwrap();
        let (_, sources) = parse_grounded_candidates(&data);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].title, "news.example.com");
    }

    #[test]
    fn grounding_rejection_detected() {
        assert!(rejects_grounding(
            "Gemini API error: google_search is not supported for this model"
        ));
        assert!(rejects_grounding(
            "Gemini API error: Unknown name \"google_search\": Cannot find field."
        ));
        assert!(!rejects_grounding(
            "Gemini API error: API key not valid. Please pass a valid API key."
        ));
    }
}
