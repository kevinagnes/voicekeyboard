//! Streaming speech output.
//!
//! Text deltas are segmented at sentence boundaries and handed to Gemini TTS on
//! a dedicated thread. Playback starts before the model reply is complete.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::core::gemini::{GeminiClient, GeminiError};
use crate::core::sounds::{self, Sounds, SpeechStreamHandle};

enum SpeechMsg {
    Segment(String),
    Finish,
    Cancel,
}

pub struct SpeechPipeline {
    tx: Sender<SpeechMsg>,
    join: Option<std::thread::JoinHandle<()>>,
    pub started: Arc<AtomicBool>,
    pub failed: Arc<Mutex<Option<String>>>,
    cancel: Arc<AtomicBool>,
}

impl SpeechPipeline {
    pub fn start(
        client: GeminiClient,
        sounds: Arc<Sounds>,
        cancel: Arc<AtomicBool>,
    ) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<SpeechMsg>();
        let started = Arc::new(AtomicBool::new(false));
        let failed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let started_thread = started.clone();
        let failed_thread = failed.clone();
        let cancel_thread = cancel.clone();
        let join = std::thread::Builder::new()
            .name("agent-tts".into())
            .spawn(move || {
                let mut handle: Option<SpeechStreamHandle> = None;
                let mut tail: Vec<i16> = Vec::new();
                const TAIL_KEEP_SAMPLES: usize = 24_000;
                while let Ok(message) = rx.recv() {
                    match message {
                        SpeechMsg::Segment(text) => {
                            if cancel_thread.load(Ordering::SeqCst) {
                                break;
                            }
                            if handle.is_none() {
                                handle = sounds.open_speech();
                                if handle.is_none() {
                                    *failed_thread.lock() =
                                        Some("No audio output available".into());
                                    break;
                                }
                                started_thread.store(true, Ordering::SeqCst);
                            }
                            let Some(speech) = handle.as_ref() else { break };
                            let result = client.tts_stream(&text, |chunk| {
                                if cancel_thread.load(Ordering::SeqCst) {
                                    return Err(GeminiError::Cancelled);
                                }
                                let samples: Vec<i16> = chunk
                                    .chunks_exact(2)
                                    .map(|c| i16::from_le_bytes([c[0], c[1]]))
                                    .collect();
                                tail.extend_from_slice(&samples);
                                if tail.len() > TAIL_KEEP_SAMPLES {
                                    let flush = tail.len() - TAIL_KEEP_SAMPLES;
                                    speech.push_i16(&tail[..flush]).map_err(GeminiError::Tts)?;
                                    tail.drain(..flush);
                                }
                                Ok(())
                            });
                            if let Err(e) = result {
                                if matches!(e, GeminiError::Cancelled) {
                                    break;
                                }
                                *failed_thread.lock() = Some(e.to_string());
                                break;
                            }
                        }
                        SpeechMsg::Finish => {
                            if let Some(speech) = handle.as_ref() {
                                if !cancel_thread.load(Ordering::SeqCst) {
                                    let trimmed = sounds::trim_noise_tail(
                                        &tail,
                                        crate::core::gemini::TTS_SAMPLE_RATE,
                                    );
                                    let _ = speech.push_i16_final(&trimmed);
                                    let _ = speech.finish();
                                } else {
                                    speech.cancel();
                                }
                            }
                            tail.clear();
                            break;
                        }
                        SpeechMsg::Cancel => {
                            if let Some(speech) = handle.as_ref() {
                                speech.cancel();
                            }
                            break;
                        }
                    }
                }
                if let Some(speech) = handle.as_ref() {
                    if cancel_thread.load(Ordering::SeqCst) {
                        speech.cancel();
                    }
                }
            })
            .ok();
        Some(Self {
            tx,
            join,
            started,
            failed,
            cancel,
        })
    }

    pub fn push(&self, segment: String) {
        if !segment.trim().is_empty() {
            let _ = self.tx.send(SpeechMsg::Segment(segment));
        }
    }

    pub fn finish(&mut self) {
        let _ = self.tx.send(SpeechMsg::Finish);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.tx.send(SpeechMsg::Cancel);
    }
}

impl Drop for SpeechPipeline {
    fn drop(&mut self) {
        if self.join.is_some() {
            self.cancel();
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }
}

/// Buffers deltas into speakable sentences, with a minimum and maximum length.
pub struct Segmenter {
    buffer: String,
}

impl Default for Segmenter {
    fn default() -> Self {
        Self::new()
    }
}

impl Segmenter {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
        }
    }

    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.buffer.push_str(delta);
        let mut out = Vec::new();
        loop {
            let chars: Vec<char> = self.buffer.chars().collect();
            if chars.is_empty() {
                break;
            }
            let mut cut = None;
            for (index, ch) in chars.iter().enumerate() {
                let boundary = matches!(ch, '.' | '!' | '?' | ';' | '。' | '！' | '？' | '；');
                let next_is_space = chars
                    .get(index + 1)
                    .map(|c| c.is_whitespace())
                    .unwrap_or(true);
                if boundary && next_is_space && index + 1 >= 30 {
                    cut = Some(index + 1);
                    break;
                }
                if index + 1 >= 280 {
                    cut = Some(index + 1);
                    break;
                }
            }
            let Some(cut) = cut else { break };
            let text: String = chars[..cut].iter().collect();
            let rest: String = chars[cut..].iter().collect();
            self.buffer = rest;
            let text = text.trim().to_string();
            if !text.is_empty() {
                out.push(text);
            }
        }
        out
    }

    pub fn flush(&mut self) -> Option<String> {
        let text = self.buffer.trim().to_string();
        self.buffer.clear();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_on_sentence_boundaries() {
        let mut segmenter = Segmenter::new();
        let out = segmenter
            .push("This is a complete sentence that is long enough to be spoken now. And this");
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("now."));
        let rest = segmenter.flush().unwrap();
        assert!(rest.starts_with("And this"));
    }

    #[test]
    fn flushes_short_tail() {
        let mut segmenter = Segmenter::new();
        assert!(segmenter.push("Short.").is_empty());
        assert_eq!(segmenter.flush().unwrap(), "Short.");
    }

    #[test]
    fn splits_very_long_text_without_punctuation() {
        let mut segmenter = Segmenter::new();
        let out = segmenter.push(&"a".repeat(600));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), 280);
        assert_eq!(out[1].len(), 280);
    }
}
