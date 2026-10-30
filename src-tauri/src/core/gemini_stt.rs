//! Batch speech-to-text through Gemini 3.5 Transcribe.
//!
//! VoiceKeyboard is push-to-talk, so the whole utterance is available when the
//! hotkey is released. The recording is wrapped in a 16 kHz mono WAV and sent
//! to the dedicated batch transcription model (`generateContent`).

use base64::Engine as _;
use serde_json::{json, Value};

use super::stt::Transcription;

const ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta/models";
pub const DEFAULT_GEMINI_STT_MODEL: &str = "gemini-3.5-transcribe";

pub struct GeminiStt {
    model: String,
}

impl GeminiStt {
    pub fn new(model: &str) -> Self {
        Self {
            model: if model.trim().is_empty() {
                DEFAULT_GEMINI_STT_MODEL.to_string()
            } else {
                model.trim().to_string()
            },
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn transcribe(
        &self,
        samples: &[f32],
        language: Option<&str>,
        initial_prompt: &str,
        api_key: &str,
    ) -> Result<Transcription, String> {
        if api_key.trim().is_empty() {
            return Err(
                "Gemini API key required for Gemini Transcribe — add it in Settings".into(),
            );
        }
        if samples.is_empty() {
            return Err("no audio captured".into());
        }

        let wav = wav_16k_mono(samples);
        let encoded = base64::engine::general_purpose::STANDARD.encode(wav);
        let instruction = instruction(language, initial_prompt);
        let body = json!({
            "contents": [{
                "role": "user",
                "parts": [
                    { "text": instruction },
                    {
                        "inlineData": {
                            "mimeType": "audio/wav",
                            "data": encoded
                        }
                    }
                ]
            }],
            "generationConfig": {
                "temperature": 0,
                "maxOutputTokens": 8192
            }
        });

        let url = format!("{ENDPOINT}/{}:generateContent", self.model);
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| format!("Gemini client error: {e}"))?;
        let start = std::time::Instant::now();
        let response = client
            .post(&url)
            .query(&[("key", api_key.trim())])
            .json(&body)
            .send()
            .map_err(|e| format!("Could not reach Gemini Transcribe: {e}"))?;
        let status = response.status();
        let data: Value = response
            .json()
            .map_err(|e| format!("Bad Gemini Transcribe response: {e}"))?;
        if !status.is_success() {
            let message = data
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("Gemini Transcribe returned an error");
            return Err(format!(
                "Gemini Transcribe ({}): {message}",
                status.as_u16()
            ));
        }

        let text = parse_transcript(&data).trim().to_string();
        if text.is_empty() {
            return Err("Gemini Transcribe returned no text".into());
        }
        let elapsed = start.elapsed().as_millis() as u64;
        Ok(Transcription {
            n_segments: text.split_whitespace().count() as i32,
            language: language.map(bcp47).unwrap_or_else(|| "auto".into()),
            text,
            inference_ms: elapsed,
        })
    }
}

fn instruction(language: Option<&str>, initial_prompt: &str) -> String {
    let language = language
        .filter(|code| !code.is_empty() && *code != "auto")
        .map(|code| format!("The speaker is using {}.", bcp47(code)))
        .unwrap_or_else(|| "Detect the spoken language.".into());
    let formatting = initial_prompt.trim();
    let mut text = format!(
        "Transcribe the attached audio verbatim. {language} Return only the transcript, \
         preserving punctuation and capitalization. Do not translate, summarize or add \
         commentary."
    );
    if !formatting.is_empty() {
        text.push(' ');
        text.push_str(formatting);
    }
    text
}

fn bcp47(code: &str) -> String {
    match code {
        "en" => "en-US".into(),
        "pt" => "pt-BR".into(),
        other if other.contains('-') => other.to_string(),
        other => other.to_string(),
    }
}

/// Parse the dedicated model's transcript. The batch API has returned the text
/// in `audioTranscription.text`; ordinary `parts[].text` is accepted too so the
/// option keeps working if the response shape is unified with other models.
pub fn parse_transcript(data: &Value) -> String {
    if let Some(text) = data
        .get("audioTranscription")
        .and_then(|a| a.get("text"))
        .and_then(|v| v.as_str())
    {
        return text.to_string();
    }
    if let Some(candidates) = data.get("candidates").and_then(|c| c.as_array()) {
        for candidate in candidates {
            if let Some(text) = candidate
                .get("audioTranscription")
                .and_then(|a| a.get("text"))
                .and_then(|v| v.as_str())
            {
                return text.to_string();
            }
            if let Some(text) = candidate
                .get("content")
                .and_then(|c| c.get("audioTranscription"))
                .and_then(|a| a.get("text"))
                .and_then(|v| v.as_str())
            {
                return text.to_string();
            }
            if let Some(parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            {
                for part in parts {
                    if let Some(text) = part
                        .get("audioTranscription")
                        .and_then(|a| a.get("text"))
                        .and_then(|v| v.as_str())
                    {
                        return text.to_string();
                    }
                    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                        if !text.trim().is_empty() {
                            return text.to_string();
                        }
                    }
                }
            }
        }
    }
    String::new()
}

/// 16 kHz mono 16-bit PCM WAV, the format the batch endpoint accepts.
pub fn wav_16k_mono(samples: &[f32]) -> Vec<u8> {
    const RATE: u32 = 16_000;
    let pcm: Vec<i16> = samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
        .collect();
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in pcm {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_valid_mono_16k() {
        let bytes = wav_16k_mono(&[0.0, 0.5, -0.5, 1.0]);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            16_000
        );
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 1);
        assert_eq!(bytes.len(), 44 + 4 * 2);
    }

    #[test]
    fn parses_dedicated_and_generic_shapes() {
        let dedicated = serde_json::json!({
            "candidates": [{"content": {"parts": [{"audioTranscription": {"text": "hello world"}}]}}]
        });
        assert_eq!(parse_transcript(&dedicated), "hello world");
        let generic = serde_json::json!({
            "candidates": [{"content": {"parts": [{"text": "hi there"}]}}]
        });
        assert_eq!(parse_transcript(&generic), "hi there");
        let top_level = serde_json::json!({"audioTranscription": {"text": "top"}});
        assert_eq!(parse_transcript(&top_level), "top");
    }
}
