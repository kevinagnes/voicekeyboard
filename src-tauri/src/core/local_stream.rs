//! Incremental on-device transcription for the local STT engines.
//!
//! Whisper has no true streaming mode, so we cut audio at natural pauses and
//! transcribe each segment with the previous transcript as the prompt. That
//! gives live partials while speaking and leaves only a short tail to transcribe
//! when the hotkey is released. Nemotron uses the same path (its segments are
//! just short windows), so the behaviour is uniform for every local engine.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::json;
use tauri::{AppHandle, Emitter};

use super::capture::{self, SAMPLE_RATE};
use super::stt::SttEngine;

const SILENCE_MS: u32 = 700;
const MIN_SEGMENT_MS: u32 = 800;
const SPEECH_RMS: f32 = 0.01;

enum Control {
    Finish,
    Cancel,
}

pub struct LocalStream {
    audio_tx: Sender<Vec<f32>>,
    control_tx: Sender<Control>,
    join: Option<std::thread::JoinHandle<()>>,
    partial: Arc<Mutex<String>>,
    error: Arc<Mutex<Option<String>>>,
}

impl LocalStream {
    pub fn start(
        stt: Arc<SttEngine>,
        language: Option<String>,
        initial_prompt: String,
        app: Option<AppHandle>,
    ) -> Self {
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<f32>>();
        let (control_tx, control_rx) = mpsc::channel::<Control>();
        let partial = Arc::new(Mutex::new(String::new()));
        let error = Arc::new(Mutex::new(None));
        let partial_thread = partial.clone();
        let error_thread = error.clone();
        let join = std::thread::Builder::new()
            .name("local-stt-stream".into())
            .spawn(move || {
                worker(
                    stt,
                    language,
                    initial_prompt,
                    app,
                    audio_rx,
                    control_rx,
                    partial_thread,
                    error_thread,
                );
            })
            .ok();
        Self {
            audio_tx,
            control_tx,
            join,
            partial,
            error,
        }
    }

    pub fn audio_sender(&self) -> Sender<Vec<f32>> {
        self.audio_tx.clone()
    }

    pub fn partial(&self) -> String {
        self.partial.lock().clone()
    }

    pub fn finish(mut self) -> Result<String, String> {
        let _ = self.control_tx.send(Control::Finish);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let text = self.partial();
        if let Some(error) = self.error.lock().clone() {
            if text.trim().is_empty() {
                return Err(error);
            }
        }
        if text.trim().is_empty() {
            Err("incremental transcription returned no text".into())
        } else {
            Ok(text.trim().to_string())
        }
    }

    pub fn cancel(mut self) {
        let _ = self.control_tx.send(Control::Cancel);
        let _ = self.join.take();
    }
}

impl Drop for LocalStream {
    fn drop(&mut self) {
        if self.join.is_some() {
            let _ = self.control_tx.send(Control::Cancel);
            let _ = self.join.take();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn worker(
    stt: Arc<SttEngine>,
    language: Option<String>,
    initial_prompt: String,
    app: Option<AppHandle>,
    audio_rx: Receiver<Vec<f32>>,
    control_rx: Receiver<Control>,
    partial: Arc<Mutex<String>>,
    error: Arc<Mutex<Option<String>>>,
) {
    let mut pending: Vec<f32> = Vec::new();
    let mut transcript = String::new();
    let mut silence_ms: u32 = 0;
    let mut saw_speech = false;
    let mut finishing = false;
    let mut last_emit = Instant::now() - Duration::from_secs(1);

    loop {
        let mut got_audio = false;
        while let Ok(chunk) = audio_rx.try_recv() {
            got_audio = true;
            if !chunk.is_empty() {
                let rms = (chunk.iter().map(|s| s * s).sum::<f32>() / chunk.len() as f32).sqrt();
                if rms > SPEECH_RMS {
                    silence_ms = 0;
                    saw_speech = true;
                } else if saw_speech {
                    silence_ms += (chunk.len() as u32 * 1000) / SAMPLE_RATE;
                }
                pending.extend_from_slice(&chunk);
            }
        }

        let mut cancel = false;
        while let Ok(control) = control_rx.try_recv() {
            match control {
                Control::Cancel => cancel = true,
                Control::Finish => finishing = true,
            }
        }
        if cancel {
            return;
        }

        let pending_ms = (pending.len() as u32 * 1000) / SAMPLE_RATE;
        let endpoint = pending_ms >= MIN_SEGMENT_MS && silence_ms >= SILENCE_MS && saw_speech;
        if endpoint {
            if let Err(e) = transcribe_segment(
                &stt,
                &mut pending,
                &language,
                &initial_prompt,
                &mut transcript,
                &partial,
                &app,
                &mut last_emit,
            ) {
                *error.lock() = Some(e);
                return;
            }
            silence_ms = 0;
            saw_speech = false;
        }

        if finishing && !got_audio {
            // Drain anything that arrived between the signal and this point.
            while let Ok(chunk) = audio_rx.try_recv() {
                if !chunk.is_empty() {
                    pending.extend_from_slice(&chunk);
                }
            }
            if !pending.is_empty() {
                if let Err(e) = transcribe_segment(
                    &stt,
                    &mut pending,
                    &language,
                    &initial_prompt,
                    &mut transcript,
                    &partial,
                    &app,
                    &mut last_emit,
                ) {
                    *error.lock() = Some(e);
                    return;
                }
            }
            *partial.lock() = transcript;
            return;
        }

        if finishing {
            continue;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[allow(clippy::too_many_arguments)]
fn transcribe_segment(
    stt: &Arc<SttEngine>,
    pending: &mut Vec<f32>,
    language: &Option<String>,
    initial_prompt: &str,
    transcript: &mut String,
    partial: &Arc<Mutex<String>>,
    app: &Option<AppHandle>,
    last_emit: &mut Instant,
) -> Result<(), String> {
    let audio = capture::trim_silence(pending, 25);
    pending.clear();
    if audio.is_empty() {
        return Ok(());
    }
    let prompt = if transcript.trim().is_empty() {
        initial_prompt.to_string()
    } else {
        let tail: String = {
            let chars: Vec<char> = transcript.chars().collect();
            let start = chars.len().saturating_sub(400);
            chars[start..].iter().collect()
        };
        format!("{tail}\n{initial_prompt}")
    };
    let result = stt.transcribe(&audio, language.as_deref(), &prompt)?;
    let text = result.text.trim();
    if !text.is_empty() {
        *transcript = merge_transcript(transcript, text);
        *partial.lock() = transcript.clone();
        if let Some(app) = app {
            if last_emit.elapsed() > Duration::from_millis(120) {
                *last_emit = Instant::now();
                let _ = app.emit("stt-partial", json!({ "text": transcript }));
            }
        }
    }
    Ok(())
}

/// Whisper often repeats the tail of the prompt at the start of a new segment.
/// Remove the largest repeated suffix/prefix before appending.
fn merge_transcript(existing: &str, next: &str) -> String {
    let prev: Vec<&str> = existing.split_whitespace().collect();
    let new: Vec<&str> = next.split_whitespace().collect();
    let max = prev.len().min(new.len()).min(12);
    let mut overlap = 0;
    for k in (1..=max).rev() {
        if words_equal(&prev[prev.len() - k..], &new[..k]) {
            overlap = k;
            break;
        }
    }
    let mut out = existing.trim().to_string();
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(&new[overlap..].join(" "));
    out.trim().to_string()
}

fn words_equal(a: &[&str], b: &[&str]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(left, right)| {
            left.trim_matches(|c: char| c.is_ascii_punctuation())
                .eq_ignore_ascii_case(right.trim_matches(|c: char| c.is_ascii_punctuation()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_threshold_separates_speech_from_silence() {
        let silence = vec![0.0f32; 1600];
        let speech = vec![0.05f32; 1600];
        let rms = |v: &[f32]| (v.iter().map(|s| s * s).sum::<f32>() / v.len() as f32).sqrt();
        assert!(rms(&silence) < SPEECH_RMS);
        assert!(rms(&speech) > SPEECH_RMS);
    }

    #[test]
    fn merge_removes_repeated_prompt_tail() {
        assert_eq!(
            merge_transcript("hello world", "world how are you"),
            "hello world how are you"
        );
        assert_eq!(
            merge_transcript("the quick brown fox", "brown fox jumps"),
            "the quick brown fox jumps"
        );
        assert_eq!(merge_transcript("hello", "goodbye"), "hello goodbye");
        assert_eq!(
            merge_transcript("hello world", "hello world"),
            "hello world"
        );
    }
}
