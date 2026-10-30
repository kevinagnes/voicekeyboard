//! Manual smoke test for Gemini Live transcription.
//!
//! Usage:
//!   cargo run --example live_stream_demo -- <wav>
//! Reads the Gemini API key from the app settings file.

use std::time::Duration;

use voicekeyboard::core::live_stt::LiveSttSession;

fn main() {
    let wav = std::env::args().nth(1).expect("wav path");
    let settings = std::env::var("APPDATA")
        .map(|p| std::path::PathBuf::from(p).join("voicekeyboard/VoiceKeyboard/data/settings.json"))
        .ok()
        .filter(|p| p.exists())
        .expect("settings.json");
    let raw = std::fs::read_to_string(settings).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let key = value
        .get("geminiApiKey")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(!key.is_empty(), "Gemini API key missing");

    let audio = read_wav(&wav);
    let session = LiveSttSession::start(&key, "gemini-3.5-transcribe", Some("pt"), None);
    let sender = session.audio_sender();
    let total = audio.len();
    for (index, chunk) in audio.chunks(1600).enumerate() {
        sender.send(chunk.to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        if index == total / 3200 {
            println!("PARTIAL: {}", session.partial());
        }
    }
    drop(sender);
    match session.finish() {
        Ok(text) => println!("FINAL: {text}"),
        Err(error) => {
            eprintln!("FAILED: {error}");
            std::process::exit(1);
        }
    }
}

fn read_wav(path: &str) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("wav opens");
    let spec = reader.spec();
    let samples: Vec<f32> = reader
        .samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect();
    let mono: Vec<f32> = if spec.channels == 2 {
        samples.chunks(2).map(|c| (c[0] + c[1]) / 2.0).collect()
    } else {
        samples
    };
    let ratio = spec.sample_rate as f64 / 16_000.0;
    let mut out = Vec::new();
    let mut at = 0.0;
    while (at as usize) + 1 < mono.len() {
        let i = at as usize;
        let f = (at - i as f64) as f32;
        out.push(mono[i] * (1.0 - f) + mono[i + 1] * f);
        at += ratio;
    }
    out
}
