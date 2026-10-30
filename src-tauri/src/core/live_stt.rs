//! Live (streaming) speech-to-text through Gemini 3.5 Transcribe Live.
//!
//! The microphone is streamed while the user holds the hotkey. Partials are
//! pushed to the Ask overlay; the final transcript is ready almost immediately
//! on release, removing the post-release batch round trip.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

pub const LIVE_ENDPOINT: &str =
    "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
pub const DEFAULT_LIVE_MODEL: &str = "gemini-3.5-transcribe-live";

enum Control {
    Finish,
    Cancel,
}

pub struct LiveSttSession {
    audio_tx: Sender<Vec<f32>>,
    control_tx: Sender<Control>,
    join: Option<std::thread::JoinHandle<()>>,
    partial: Arc<Mutex<String>>,
    error: Arc<Mutex<Option<String>>>,
    ready: Arc<AtomicBool>,
}

impl LiveSttSession {
    /// Starts connecting immediately and returns without waiting; audio pushed
    /// through `audio_tx` is buffered until the Live setup completes.
    pub fn start(
        api_key: &str,
        selected_model: &str,
        language: Option<&str>,
        app: Option<AppHandle>,
    ) -> Self {
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<f32>>();
        let (control_tx, control_rx) = mpsc::channel::<Control>();
        let partial = Arc::new(Mutex::new(String::new()));
        let error = Arc::new(Mutex::new(None));
        let ready = Arc::new(AtomicBool::new(false));
        let model = live_model(selected_model);
        let api_key = api_key.trim().to_string();
        let language = language.map(bcp47);
        let partial_thread = partial.clone();
        let error_thread = error.clone();
        let ready_thread = ready.clone();
        let join = std::thread::Builder::new()
            .name("gemini-live-stt".into())
            .spawn(move || {
                worker(
                    api_key,
                    model,
                    language,
                    app,
                    audio_rx,
                    control_rx,
                    partial_thread,
                    error_thread,
                    ready_thread,
                );
            })
            .ok();
        Self {
            audio_tx,
            control_tx,
            join,
            partial,
            error,
            ready,
        }
    }

    pub fn audio_sender(&self) -> Sender<Vec<f32>> {
        self.audio_tx.clone()
    }

    pub fn partial(&self) -> String {
        self.partial.lock().clone()
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    /// Ask the live session to finish and wait for the final transcript.
    pub fn finish(mut self) -> Result<String, String> {
        let _ = self.control_tx.send(Control::Finish);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let text = self.partial();
        if let Some(error) = self.error() {
            if text.trim().is_empty() {
                return Err(error);
            }
        }
        if text.trim().is_empty() {
            Err("Live transcription returned no text".into())
        } else {
            Ok(text.trim().to_string())
        }
    }

    pub fn cancel(mut self) {
        let _ = self.control_tx.send(Control::Cancel);
        // Do not join: cancellation can happen while setup is still blocking.
        // The worker owns its socket and exits on its own.
        let _ = self.join.take();
    }
}

impl Drop for LiveSttSession {
    fn drop(&mut self) {
        if self.join.is_some() {
            let _ = self.control_tx.send(Control::Cancel);
            let _ = self.join.take();
        }
    }
}

pub fn live_model(selected: &str) -> String {
    let selected = selected.trim();
    if selected.is_empty() {
        return DEFAULT_LIVE_MODEL.to_string();
    }
    if selected.ends_with("-live") {
        selected.to_string()
    } else if selected.contains("transcribe") {
        format!("{selected}-live")
    } else {
        DEFAULT_LIVE_MODEL.to_string()
    }
}

fn bcp47(code: &str) -> String {
    match code {
        "en" => "en-US".into(),
        "pt" => "pt-BR".into(),
        other => other.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn worker(
    api_key: String,
    model: String,
    language: Option<String>,
    app: Option<AppHandle>,
    audio_rx: Receiver<Vec<f32>>,
    control_rx: Receiver<Control>,
    partial: Arc<Mutex<String>>,
    error: Arc<Mutex<Option<String>>>,
    ready: Arc<AtomicBool>,
) {
    if api_key.is_empty() {
        *error.lock() = Some("Gemini API key required for live transcription".into());
        return;
    }
    let url = format!("{LIVE_ENDPOINT}?key={api_key}");
    let mut socket = match connect_live(&url) {
        Ok(socket) => socket,
        Err(e) => {
            *error.lock() = Some(e);
            return;
        }
    };

    // Wait for setup before any audio is sent.
    set_read_timeout(&mut socket, Some(Duration::from_secs(10)));
    let mut transcription: Value = json!({ "mode": "SMART" });
    if let Some(language) = &language {
        transcription["languageCodes"] = json!([language]);
    }
    let setup = json!({
        "setup": {
            "model": format!("models/{model}"),
            "inputAudioTranscription": transcription,
            "realtimeInputConfig": { "automaticActivityDetection": {} }
        }
    });
    if let Err(e) = socket.send(Message::Text(setup.to_string().into())) {
        *error.lock() = Some(format!("Gemini Live setup failed: {e}"));
        return;
    }
    let setup_started = Instant::now();
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                if let Ok(data) = serde_json::from_str::<Value>(&text) {
                    if data.get("setupComplete").is_some() {
                        break;
                    }
                    if let Some(err) = data.get("error") {
                        *error.lock() = Some(format!(
                            "Gemini Live: {}",
                            err.get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("setup failed")
                        ));
                        return;
                    }
                }
            }
            Ok(Message::Binary(bytes)) => {
                if let Ok(data) = serde_json::from_slice::<Value>(&bytes) {
                    if data.get("setupComplete").is_some() {
                        break;
                    }
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => {
                *error.lock() = Some(format!("Gemini Live setup failed: {e}"));
                return;
            }
        }
        if setup_started.elapsed() > Duration::from_secs(10) {
            *error.lock() = Some("Gemini Live setup timed out".into());
            return;
        }
    }
    ready.store(true, Ordering::SeqCst);
    set_read_timeout(&mut socket, Some(Duration::from_millis(30)));

    let started = Instant::now();
    let mut sent_samples: u64 = 0;
    let mut finishing = false;
    let mut accumulated = String::new();
    let mut interim = String::new();
    let mut final_seen = false;
    let mut generation_complete = false;
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    loop {
        while let Ok(control) = control_rx.try_recv() {
            match control {
                Control::Cancel => {
                    let _ = socket.close(None);
                    return;
                }
                Control::Finish => finishing = true,
            }
        }
        while let Ok(chunk) = audio_rx.try_recv() {
            if chunk.is_empty() {
                continue;
            }
            sent_samples += chunk.len() as u64;
            if !finishing {
                let target = Duration::from_secs_f64(sent_samples as f64 / 16_000.0);
                let elapsed = started.elapsed();
                if target > elapsed + Duration::from_millis(100) {
                    let sleep = (target - elapsed).min(Duration::from_millis(40));
                    std::thread::sleep(sleep);
                }
            }
            let pcm: Vec<i16> = chunk
                .iter()
                .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect();
            let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
            let payload = json!({
                "realtimeInput": {
                    "audio": {
                        "mimeType": "audio/pcm;rate=16000",
                        "data": base64::engine::general_purpose::STANDARD.encode(bytes)
                    }
                }
            });
            if socket
                .send(Message::Text(payload.to_string().into()))
                .is_err()
            {
                *error.lock() = Some("Gemini Live connection was lost".into());
                return;
            }
        }

        // Read one message per loop while speech is streaming so partial
        // transcripts reach the panel; a short timeout keeps sending responsive.
        match socket.read() {
            Ok(Message::Text(text)) => {
                if handle_server_message(
                    &text,
                    &mut accumulated,
                    &mut interim,
                    &partial,
                    &ready,
                    &error,
                    &app,
                    &mut last_emit,
                    &mut final_seen,
                    &mut generation_complete,
                ) {
                    let _ = socket.close(None);
                    return;
                }
            }
            Ok(Message::Binary(bytes)) => {
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    if handle_server_message(
                        text,
                        &mut accumulated,
                        &mut interim,
                        &partial,
                        &ready,
                        &error,
                        &app,
                        &mut last_emit,
                        &mut final_seen,
                        &mut generation_complete,
                    ) {
                        let _ = socket.close(None);
                        return;
                    }
                }
            }
            Ok(Message::Close(_)) => {
                *error.lock() = Some("Gemini Live connection closed".into());
                return;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                *error.lock() = Some("Gemini Live connection was lost".into());
                return;
            }
        }

        if finishing {
            let _ = socket.send(Message::Text(
                json!({"realtimeInput":{"audioStreamEnd":true}})
                    .to_string()
                    .into(),
            ));
            set_read_timeout(&mut socket, Some(Duration::from_millis(250)));
            let deadline = Instant::now() + Duration::from_millis(2_500);
            while Instant::now() < deadline {
                match socket.read() {
                    Ok(Message::Text(text)) => {
                        let _ = handle_server_message(
                            &text,
                            &mut accumulated,
                            &mut interim,
                            &partial,
                            &ready,
                            &error,
                            &app,
                            &mut last_emit,
                            &mut final_seen,
                            &mut generation_complete,
                        );
                    }
                    Ok(Message::Binary(bytes)) => {
                        if let Ok(text) = std::str::from_utf8(&bytes) {
                            let _ = handle_server_message(
                                text,
                                &mut accumulated,
                                &mut interim,
                                &partial,
                                &ready,
                                &error,
                                &app,
                                &mut last_emit,
                                &mut final_seen,
                                &mut generation_complete,
                            );
                        }
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
                if generation_complete {
                    break;
                }
            }
            *partial.lock() = join_text(&accumulated, &interim);
            let _ = socket.close(None);
            return;
        }

        if started.elapsed() > Duration::from_secs(120) {
            *error.lock() = Some("Live transcription exceeded 120 seconds".into());
            let _ = socket.close(None);
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_server_message(
    raw: &str,
    accumulated: &mut String,
    interim: &mut String,
    partial: &Arc<Mutex<String>>,
    ready: &Arc<AtomicBool>,
    error: &Arc<Mutex<Option<String>>>,
    app: &Option<AppHandle>,
    last_emit: &mut Instant,
    final_seen: &mut bool,
    generation_complete: &mut bool,
) -> bool {
    let Ok(data) = serde_json::from_str::<Value>(raw) else {
        return false;
    };
    if data.get("setupComplete").is_some() {
        ready.store(true, Ordering::SeqCst);
        return false;
    }
    if let Some(err) = data.get("error") {
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Gemini Live returned an error");
        *error.lock() = Some(format!("Gemini Live: {message}"));
        return true;
    }
    if let Some(usage) = data.get("usageMetadata") {
        let _ = usage;
    }
    let Some(server) = data.get("serverContent") else {
        return false;
    };
    if server.get("generationComplete").and_then(|v| v.as_bool()) == Some(true) {
        *generation_complete = true;
    }
    if let Some(interim_text) = server
        .get("interimInputTranscription")
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
    {
        *interim = interim_text.trim().to_string();
    }
    if let Some(final_text) = server
        .get("inputTranscription")
        .and_then(|v| v.get("text"))
        .and_then(|v| v.as_str())
    {
        let final_text = final_text.trim();
        if !final_text.is_empty() {
            *accumulated = join_text(accumulated, final_text);
            *interim = String::new();
            *final_seen = true;
        }
    }
    let text = join_text(accumulated, interim);
    if !text.is_empty() {
        *partial.lock() = text.clone();
        if let Some(app) = app {
            if last_emit.elapsed() > Duration::from_millis(120) {
                *last_emit = Instant::now();
                let _ = app.emit("stt-partial", json!({ "text": text }));
            }
        }
    }
    false
}

fn join_text(left: &str, right: &str) -> String {
    match (left.trim(), right.trim()) {
        ("", b) => b.to_string(),
        (a, "") => a.to_string(),
        (a, b) => format!("{a} {b}"),
    }
}

fn connect_live(url: &str) -> Result<WebSocket<MaybeTlsStream<std::net::TcpStream>>, String> {
    use std::net::ToSocketAddrs;
    use tungstenite::client::IntoClientRequest;

    let parsed = url::Url::parse(url).map_err(|e| format!("bad live URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "live URL has no host".to_string())?
        .to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let address = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("live DNS failed: {e}"))?
        .next()
        .ok_or_else(|| "live host did not resolve".to_string())?;
    let tcp = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(8))
        .map_err(|e| format!("Could not connect to Gemini Live: {e}"))?;
    let _ = tcp.set_nodelay(true);
    let tls = native_tls::TlsConnector::new()
        .map_err(|e| format!("TLS setup failed: {e}"))?
        .connect(&host, tcp)
        .map_err(|e| format!("TLS handshake failed: {e}"))?;
    let request = parsed
        .as_str()
        .into_client_request()
        .map_err(|e| format!("bad live request: {e}"))?;
    let (socket, _) = tungstenite::client::client(request, MaybeTlsStream::NativeTls(tls))
        .map_err(|e| format!("Gemini Live handshake failed: {e}"))?;
    Ok(socket)
}

fn set_read_timeout(
    socket: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>,
    timeout: Option<Duration>,
) {
    match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => {
            let _ = stream.set_read_timeout(timeout);
        }
        MaybeTlsStream::NativeTls(stream) => {
            let _ = stream.get_ref().set_read_timeout(timeout);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_model_is_derived_from_the_batch_model() {
        assert_eq!(
            live_model("gemini-3.5-transcribe"),
            "gemini-3.5-transcribe-live"
        );
        assert_eq!(
            live_model("gemini-3.5-transcribe-live"),
            "gemini-3.5-transcribe-live"
        );
        assert_eq!(live_model(""), DEFAULT_LIVE_MODEL);
    }

    #[test]
    fn server_messages_build_a_transcript() {
        let partial = Arc::new(Mutex::new(String::new()));
        let ready = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let mut at = Instant::now() - Duration::from_secs(1);
        let mut final_seen = false;
        let mut generation_complete = false;
        let mut accumulated = String::new();
        let mut interim = String::new();
        let setup = r#"{"setupComplete":{}}"#;
        assert!(!handle_server_message(
            setup,
            &mut accumulated,
            &mut interim,
            &partial,
            &ready,
            &error,
            &None,
            &mut at,
            &mut final_seen,
            &mut generation_complete
        ));
        assert!(ready.load(Ordering::SeqCst));
        let msg = r#"{"serverContent":{"interimInputTranscription":{"text":"hello"}}}"#;
        assert!(!handle_server_message(
            msg,
            &mut accumulated,
            &mut interim,
            &partial,
            &ready,
            &error,
            &None,
            &mut at,
            &mut final_seen,
            &mut generation_complete
        ));
        assert_eq!(partial.lock().as_str(), "hello");
        let msg = r#"{"serverContent":{"inputTranscription":{"text":"hello world"}}}"#;
        assert!(!handle_server_message(
            msg,
            &mut accumulated,
            &mut interim,
            &partial,
            &ready,
            &error,
            &None,
            &mut at,
            &mut final_seen,
            &mut generation_complete
        ));
        assert_eq!(partial.lock().as_str(), "hello world");
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod socket_tests {
    use super::*;

    #[test]
    #[ignore = "uses the configured Gemini key and network"]
    fn native_tls_setup_completes() {
        let path = std::env::var("APPDATA")
            .map(|p| {
                std::path::PathBuf::from(p).join("voicekeyboard/VoiceKeyboard/data/settings.json")
            })
            .ok()
            .filter(|p| p.exists())
            .or_else(|| {
                std::env::var("HOME").ok().map(|p| {
                    std::path::PathBuf::from(p).join(
                        "Library/Application Support/voicekeyboard.VoiceKeyboard/settings.json",
                    )
                })
            });
        let Some(path) = path else { return };
        let raw = std::fs::read_to_string(path).unwrap();
        let value: Value = serde_json::from_str(&raw).unwrap();
        let key = value
            .get("geminiApiKey")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if key.is_empty() {
            return;
        }
        let url = format!("{LIVE_ENDPOINT}?key={key}");
        let mut socket = connect_live(&url).unwrap();
        set_read_timeout(&mut socket, Some(Duration::from_secs(10)));
        let setup = json!({
            "setup": {
                "model": format!("models/{DEFAULT_LIVE_MODEL}"),
                "inputAudioTranscription": { "languageCodes": ["pt-BR"], "mode": "SMART" },
                "realtimeInputConfig": { "automaticActivityDetection": {} }
            }
        });
        socket
            .send(Message::Text(setup.to_string().into()))
            .unwrap();
        let mut ok = false;
        for _ in 0..20 {
            match socket.read() {
                Ok(Message::Text(text)) => {
                    eprintln!("TEXT {}", &text[..text.len().min(300)]);
                    if text.contains("setupComplete") {
                        ok = true;
                        break;
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes);
                    eprintln!("BIN {}", &text[..text.len().min(300)]);
                    if text.contains("setupComplete") {
                        ok = true;
                        break;
                    }
                }
                Ok(other) => eprintln!("OTHER {other:?}"),
                Err(e) => {
                    eprintln!("READ ERROR {e:?}");
                    break;
                }
            }
        }
        assert!(ok, "live setup did not complete");
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod full_live_tests {
    use super::*;

    fn settings_key() -> Option<String> {
        let path = std::env::var("APPDATA")
            .map(|p| {
                std::path::PathBuf::from(p).join("voicekeyboard/VoiceKeyboard/data/settings.json")
            })
            .ok()
            .filter(|p| p.exists())?;
        let raw = std::fs::read_to_string(path).ok()?;
        let value: Value = serde_json::from_str(&raw).ok()?;
        value
            .get("geminiApiKey")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    #[test]
    #[ignore = "uses the configured Gemini key and a local WAV fixture"]
    fn streams_a_wav_and_returns_the_transcript() {
        let Some(key) = settings_key() else { return };
        let wav_path = std::env::temp_dir().join("vk-live-test.wav");
        let mut reader = hound::WavReader::open(&wav_path).unwrap();
        let spec = reader.spec();
        let samples: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
            (hound::SampleFormat::Int, 16) => reader
                .samples::<i16>()
                .map(|s| s.unwrap() as f32 / 32768.0)
                .collect(),
            _ => panic!("unexpected WAV format"),
        };
        let mono: Vec<f32> = if spec.channels == 2 {
            samples.chunks(2).map(|c| (c[0] + c[1]) / 2.0).collect()
        } else {
            samples
        };
        let ratio = spec.sample_rate as f64 / 16_000.0;
        let mut resampled = Vec::new();
        let mut at = 0.0;
        while (at as usize) + 1 < mono.len() {
            let i = at as usize;
            let f = (at - i as f64) as f32;
            resampled.push(mono[i] * (1.0 - f) + mono[i + 1] * f);
            at += ratio;
        }
        let session = LiveSttSession::start(&key, "gemini-3.5-transcribe", Some("pt"), None);
        let sender = session.audio_sender();
        for chunk in resampled.chunks(1600) {
            sender.send(chunk.to_vec()).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(sender);
        let text = session.finish().expect("live transcript");
        eprintln!("TRANSCRIPT: {text}");
        assert!(text.to_lowercase().contains("teste"));
    }
}
