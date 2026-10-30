//! Manual smoke test for incremental local transcription.
//!
//! Usage (Windows/macOS with the Whisper model already downloaded):
//!   cargo run --example local_stream_demo -- <model-path> <first.wav> <second.wav>

use std::sync::Arc;

use voicekeyboard::core::local_stream::LocalStream;
use voicekeyboard::core::stt::SttEngine;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: local_stream_demo <model> <first.wav> <second.wav>");
        std::process::exit(2);
    }
    let engine = Arc::new(SttEngine::new());
    engine.load_model(&args[1], "whisper").expect("model loads");

    let stream = LocalStream::start(engine, Some("en".into()), "Transcribe as-is.".into(), None);
    let audio = stream.audio_sender();

    let mut all = Vec::new();
    all.extend(read_wav(&args[2]));
    all.extend(vec![0.0f32; 16_000]); // one second of silence
    all.extend(read_wav(&args[3]));
    all.extend(vec![0.0f32; 16_000]);

    for chunk in all.chunks(1600) {
        audio.send(chunk.to_vec()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    drop(audio);
    match stream.finish() {
        Ok(text) => println!("TRANSCRIPT: {text}"),
        Err(error) => {
            eprintln!("FAILED: {error}");
            std::process::exit(1);
        }
    }
}

fn read_wav(path: &str) -> Vec<f32> {
    let mut reader = hound::WavReader::open(path).expect("wav opens");
    let spec = reader.spec();
    let samples: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32768.0)
            .collect(),
        _ => panic!("expected 16-bit PCM WAV"),
    };
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
