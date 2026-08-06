//! Nemotron 3.5 multilingual streaming ASR engine (ONNX Runtime).
//!
//! Ported from soniqo/speech-core's `OnnxNemotronStreamingStt` for the
//! `soniqo/Nemotron-3.5-ASR-Streaming-Multilingual-0.6B-ONNX-*` exports:
//! a cache-aware streaming FastConformer encoder + RNN-T decoder/joint,
//! with a 128-bin log-mel front end (preemphasis, hann window, slaney
//! mel scaling) and SentencePiece-style BPE output.
//!
//! Cross-platform: the mel front end and greedy decoder are pure Rust; ONNX
//! Runtime comes from the `ort` crate (prebuilt binaries per platform).

use std::collections::HashMap;
use std::path::Path;

use parking_lot::Mutex;
use rustfft::{num_complex::Complex, FftPlanner};

pub const SAMPLE_RATE: u32 = 16_000;
const MEL_BINS: usize = 128;
const N_FFT: usize = 512;
const HOP_LENGTH: usize = 160;
const WIN_LENGTH: usize = 400;
const PREEMPH: f32 = 0.97;
const LOG_FLOOR: f32 = 5.9604645e-8; // 2^-24

const ENCODER_LAYERS: usize = 24;
const ENCODER_HIDDEN: usize = 1024;
const DECODER_LAYERS: usize = 2;
const DECODER_HIDDEN: usize = 640;
const PRE_CACHE_SIZE: usize = 9;
const ATTN_LEFT_CONTEXT: usize = 56;
const CONV_CACHE_SIZE: usize = 8;
const MEL_FRAMES: usize = 32; // per encoder window (320 ms)
const WIN_SAMPLES: usize = MEL_FRAMES * HOP_LENGTH; // 5120 samples per window
const MAX_SYMBOLS: usize = 10; // RNN-T expansions per encoder frame

const BLANK_ID: usize = 13_087;
const VOCAB_SIZE: usize = 13_087;
const NUM_PROMPTS: usize = 128;
const AUTO_SLOT: usize = 101;

// ---------------------------------------------------------------------------
// Mel front end
// ---------------------------------------------------------------------------

fn hz_to_mel(hz: f64) -> f64 {
    if hz < 1000.0 {
        3.0 * hz / 200.0
    } else {
        15.0 + (hz / 1000.0).ln() * (27.0 / 6.4f64.ln())
    }
}

fn mel_to_hz(mel: f64) -> f64 {
    if mel < 15.0 {
        200.0 * mel / 3.0
    } else {
        1000.0 * ((mel - 15.0) / (27.0 / 6.4f64.ln())).exp()
    }
}

/// Slaney-normalised mel filterbank, [MEL_BINS, N_FFT/2+1], in f32.
fn mel_filterbank() -> Vec<f32> {
    let num_bins = N_FFT / 2 + 1;
    let mel_low = hz_to_mel(0.0);
    let mel_high = hz_to_mel(SAMPLE_RATE as f64 / 2.0);
    let bin_hz = SAMPLE_RATE as f64 / N_FFT as f64;

    let mut hz_points = vec![0.0f64; MEL_BINS + 2];
    for i in 0..MEL_BINS + 2 {
        let mel = mel_low + (mel_high - mel_low) * i as f64 / (MEL_BINS + 1) as f64;
        hz_points[i] = mel_to_hz(mel);
    }

    let mut fb = vec![0.0f32; MEL_BINS * num_bins];
    for m in 0..MEL_BINS {
        let left = hz_points[m];
        let center = hz_points[m + 1];
        let right = hz_points[m + 2];
        let enorm = if right > left { 2.0 / (right - left) } else { 0.0 };
        for f in 0..num_bins {
            let f_hz = f as f64 * bin_hz;
            let w = if f_hz >= left && f_hz <= center && center > left {
                (f_hz - left) / (center - left)
            } else if f_hz > center && f_hz <= right && right > center {
                (right - f_hz) / (right - center)
            } else {
                0.0
            };
            fb[m * num_bins + f] = (w * enorm) as f32;
        }
    }
    fb
}

/// Symmetric hann window (torch.hann_window periodic=False), length WIN_LENGTH.
fn hann_window() -> Vec<f32> {
    let denom = (WIN_LENGTH - 1) as f32;
    (0..WIN_LENGTH)
        .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / denom).cos()))
        .collect()
}

struct MelFrontEnd {
    filterbank: Vec<f32>,
    window: Vec<f32>,
    planner: FftPlanner<f32>,
}

impl MelFrontEnd {
    fn new() -> Self {
        Self {
            filterbank: mel_filterbank(),
            window: hann_window(),
            planner: FftPlanner::new(),
        }
    }

    /// Log-mel spectrogram: [MEL_BINS, frames] column-major (bin-major).
    fn compute(&mut self, pcm: &[f32]) -> Vec<f32> {
        let pad = N_FFT / 2;
        let sig_len = pcm.len() + 2 * pad;
        let mut sig = vec![0.0f32; sig_len];
        sig[pad..pad + pcm.len()].copy_from_slice(pcm);
        // Reflect padding (matches torch.stft center=True)
        for i in 0..pad {
            let l = (i + 1).min(pcm.len() - 1);
            sig[pad - 1 - i] = pcm[l];
            let r = (pcm.len() as isize - 2 - i as isize).max(0) as usize;
            sig[pad + pcm.len() + i] = pcm[r];
        }

        let num_frames = (sig_len as isize - WIN_LENGTH as isize) / HOP_LENGTH as isize + 1;
        if num_frames <= 0 {
            return Vec::new();
        }
        let num_frames = num_frames as usize;

        let fft = self.planner.plan_fft_forward(N_FFT);
        let mut mel = vec![0.0f32; MEL_BINS * num_frames];
        let mut frame = vec![Complex::new(0.0f32, 0.0f32); N_FFT];

        for t in 0..num_frames {
            frame.iter_mut().for_each(|c| *c = Complex::new(0.0, 0.0));
            let start = t * HOP_LENGTH;
            for i in 0..WIN_LENGTH {
                frame[i] = Complex::new(sig[start + i] * self.window[i], 0.0);
            }
            fft.process(&mut frame);

            for m in 0..MEL_BINS {
                let mut sum = 0.0f32;
                for f in 0..N_FFT / 2 + 1 {
                    let power = frame[f].re * frame[f].re + frame[f].im * frame[f].im;
                    sum += power * self.filterbank[m * (N_FFT / 2 + 1) + f];
                }
                mel[m * num_frames + t] = (sum + LOG_FLOOR).ln();
            }
        }
        mel
    }
}

// ---------------------------------------------------------------------------
// Vocabulary / language prompt
// ---------------------------------------------------------------------------

fn load_json(path: &Path) -> Result<serde_json::Value, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&raw).map_err(|e| e.to_string())
}

/// Map of token id -> text piece (SentencePiece BPE, "▁" = space).
fn load_vocab(path: &Path) -> Result<Vec<String>, String> {
    let v = load_json(path)?;
    let map = v.as_object().ok_or("vocab.json: not an object")?;
    let mut vocab = vec![String::new(); VOCAB_SIZE];
    for (k, val) in map {
        let id: usize = k.parse().map_err(|_| "vocab.json: bad key")?;
        if let Some(s) = val.as_str() {
            if id < vocab.len() {
                vocab[id] = s.to_string();
            }
        }
    }
    Ok(vocab)
}

/// Locale/iso code -> prompt slot (one-hot language mask).
fn load_prompt_dictionary(path: &Path) -> Result<(HashMap<String, usize>, usize), String> {
    let v = load_json(path)?;
    let dict = v
        .get("promptDictionary")
        .and_then(|d| d.as_object())
        .ok_or("languages.json: missing promptDictionary")?;
    let mut map = HashMap::new();
    for (k, val) in dict {
        if let Some(slot) = val.as_u64() {
            map.insert(k.clone(), slot as usize);
        }
    }
    let auto = v
        .get("autoSlot")
        .and_then(|s| s.as_u64())
        .unwrap_or(AUTO_SLOT as u64) as usize;
    Ok((map, auto))
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

pub struct NemotronEngine {
    enc: Mutex<ort::session::Session>,
    dec: Mutex<ort::session::Session>,
    joint: Mutex<ort::session::Session>,
    vocab: Vec<String>,
    prompt_slots: HashMap<String, usize>,
    auto_slot: usize,
    mel: Mutex<MelFrontEnd>,
}

#[derive(Debug, Clone)]
pub struct NemotronTranscription {
    pub text: String,
    pub language: String,
    pub inference_ms: u64,
}

#[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
fn build_session(path: &Path, use_gpu: bool) -> Result<ort::session::Session, String> {
    let mut builder =
        ort::session::Session::builder().map_err(|e| format!("session builder: {e}"))?;
    let n_threads = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(4, 16))
        .unwrap_or(4);
    builder = builder
        .with_intra_threads(n_threads)
        .map_err(|e| format!("session builder: {e}"))?;
    builder = builder
        .with_optimization_level(ort::session::builder::GraphOptimizationLevel::All)
        .map_err(|e| format!("session builder: {e}"))?;

    #[cfg(target_os = "windows")]
    {
        use ort::execution_providers::{CUDA, DirectML};
        // GPU backends are opt-in (VK_NEMOTRON_CUDA / VK_NEMOTRON_DML); CPU is
        // the default — measured identical speed for the 320 ms streaming
        // windows (encoder is launch/IO-bound, not compute-bound).
        let use_cuda = use_gpu
            && std::env::var_os("VK_NEMOTRON_CPU").is_none()
            && std::env::var_os("VK_NEMOTRON_CUDA").is_some()
            && std::env::var_os("VK_NEMOTRON_DML").is_none();
        let use_dml = use_gpu
            && std::env::var_os("VK_NEMOTRON_CPU").is_none()
            && std::env::var_os("VK_NEMOTRON_DML").is_some()
            && std::env::var_os("VK_NEMOTRON_CUDA").is_none();
        if use_dml {
            // DirectML: graph fusion silently corrupts the encoder output,
            // so optimization is disabled (opt-in for experimentation).
            let opt = match std::env::var("VK_NEMOTRON_OPT").ok().as_deref() {
                Some("1") => ort::session::builder::GraphOptimizationLevel::Level1,
                Some("2") => ort::session::builder::GraphOptimizationLevel::Level2,
                Some("3") => ort::session::builder::GraphOptimizationLevel::Level3,
                _ => ort::session::builder::GraphOptimizationLevel::Disable,
            };
            let dml_builder = builder
                .clone()
                .with_optimization_level(opt)
                .map_err(|e| format!("session builder: {e}"))?;
            match dml_builder.with_execution_providers([DirectML::default().build()]) {
                Ok(mut dml) => match dml.commit_from_file(path) {
                    Ok(session) => {
                        crate::app::debug_log("nemotron: session ready (DirectML GPU)");
                        return Ok(session);
                    }
                    Err(e) => {
                        crate::app::debug_log(&format!(
                            "nemotron: DirectML session failed ({e}), using CPU"
                        ));
                    }
                },
                Err(e) => {
                    crate::app::debug_log(&format!(
                        "nemotron: DirectML unavailable ({e}), using CPU"
                    ));
                }
            }
        } else if use_cuda {
            match builder
                .clone()
                .with_execution_providers([CUDA::default()
                    .with_cuda_graph(true)
                    .with_prefer_nhwc(true)
                    .build()])
            {
                Ok(mut cuda) => match cuda.commit_from_file(path) {
                    Ok(session) => {
                        crate::app::debug_log("nemotron: session ready (CUDA GPU)");
                        return Ok(session);
                    }
                    Err(e) => {
                        crate::app::debug_log(&format!(
                            "nemotron: CUDA session failed ({e}), using CPU"
                        ));
                    }
                },
                Err(e) => {
                    crate::app::debug_log(&format!(
                        "nemotron: CUDA unavailable ({e}), using CPU"
                    ));
                }
            }
        }
    }

    builder
        .commit_from_file(path)
        .map_err(|e| format!("failed to load {}: {e}", path.display()))
}

impl NemotronEngine {
    pub fn load(dir: &Path) -> Result<Self, String> {
        if std::env::var_os("VK_NEMOTRON_DEBUG").is_some() {
            if let Ok(env) = ort::environment::Environment::current() {
                env.set_log_level(ort::logging::LogLevel::Warning);
            }
        }
        let enc = build_session(&dir.join("encoder.onnx"), true)?;
        let dec = build_session(&dir.join("decoder.onnx"), false)?;
        let joint = build_session(&dir.join("joint.onnx"), false)?;
        let vocab = load_vocab(&dir.join("vocab.json"))?;
        let (prompt_slots, auto_slot) = load_prompt_dictionary(&dir.join("languages.json"))?;
        Ok(Self {
            enc: Mutex::new(enc),
            dec: Mutex::new(dec),
            joint: Mutex::new(joint),
            vocab,
            prompt_slots,
            auto_slot,
            mel: Mutex::new(MelFrontEnd::new()),
        })
    }

    pub fn transcribe(
        &self,
        samples: &[f32],
        language: Option<&str>,
    ) -> Result<NemotronTranscription, String> {
        let start = std::time::Instant::now();

        let mut stream = Stream::new(self);
        stream.prompt_slot = language
            .and_then(|l| self.prompt_slots.get(l).copied())
            .unwrap_or(self.auto_slot);

        let mut pcm = samples.to_vec();
        if !pcm.is_empty() {
            let mut prev = 0.0f32;
            for x in pcm.iter_mut() {
                let cur = *x;
                *x = cur - PREEMPH * prev;
                prev = cur;
            }
        }

        let total = if pcm.is_empty() {
            0
        } else {
            (pcm.len() + WIN_SAMPLES - 1) / WIN_SAMPLES
        };
        if total > 0 && pcm.len() % WIN_SAMPLES != 0 {
            pcm.resize(total * WIN_SAMPLES, 0.0);
        }

        if total > 0 {
            let mel = self.mel.lock().compute(&pcm);
            let produced = mel.len() / MEL_BINS;
            for w in 0..total {
                let f0 = w * MEL_FRAMES;
                if f0 + MEL_FRAMES > produced {
                    break;
                }
                let mut window = vec![0.0f32; MEL_BINS * MEL_FRAMES];
                for b in 0..MEL_BINS {
                    let src = b * produced + f0;
                    window[b * MEL_FRAMES..(b + 1) * MEL_FRAMES]
                        .copy_from_slice(&mel[src..src + MEL_FRAMES]);
                }
                stream.run_window(&window)?;
            }
        }

        Ok(NemotronTranscription {
            text: stream.accumulated,
            language: language.unwrap_or("auto").to_string(),
            inference_ms: start.elapsed().as_millis() as u64,
        })
    }
}

struct Stream<'a> {
    engine: &'a NemotronEngine,
    pre_cache: Vec<f32>,
    cache_last_channel: Vec<f32>,
    cache_last_time: Vec<f32>,
    cache_last_channel_len: i32,
    dec_h: Vec<f32>,
    dec_c: Vec<f32>,
    dec_hidden: Vec<f32>,
    prompt_slot: usize,
    accumulated: String,
    last_enc_ms: u64,
}

impl<'a> Stream<'a> {
    fn new(engine: &'a NemotronEngine) -> Self {
        let mut s = Self {
            engine,
            pre_cache: vec![0.0; MEL_BINS * PRE_CACHE_SIZE],
            cache_last_channel: vec![0.0; ENCODER_LAYERS * ATTN_LEFT_CONTEXT * ENCODER_HIDDEN],
            cache_last_time: vec![0.0; ENCODER_LAYERS * ENCODER_HIDDEN * CONV_CACHE_SIZE],
            cache_last_channel_len: 0,
            dec_h: vec![0.0; DECODER_LAYERS * DECODER_HIDDEN],
            dec_c: vec![0.0; DECODER_LAYERS * DECODER_HIDDEN],
            dec_hidden: vec![0.0; DECODER_HIDDEN],
            prompt_slot: engine.auto_slot,
            accumulated: String::new(),
            last_enc_ms: 0,
        };
        // Prime the predictor with the blank token.
        s.run_decoder_step(BLANK_ID as i64);
        s
    }

    fn run_window(&mut self, window: &[f32]) -> Result<(), String> {
        // Build language mask (one-hot prompt slot).
        let mut lang_mask = vec![0.0f32; NUM_PROMPTS];
        lang_mask[self.prompt_slot.min(NUM_PROMPTS - 1)] = 1.0;

        let shape_mel: [i64; 3] = [1, MEL_BINS as i64, MEL_FRAMES as i64];
        let shape_lang: [i64; 2] = [1, NUM_PROMPTS as i64];
        let shape_pre: [i64; 3] = [1, MEL_BINS as i64, PRE_CACHE_SIZE as i64];
        let shape_clc: [i64; 4] = [
            ENCODER_LAYERS as i64,
            1,
            ATTN_LEFT_CONTEXT as i64,
            ENCODER_HIDDEN as i64,
        ];
        let shape_clt: [i64; 4] = [
            ENCODER_LAYERS as i64,
            1,
            ENCODER_HIDDEN as i64,
            CONV_CACHE_SIZE as i64,
        ];

        let t_mel =
            ort::value::Tensor::from_array((shape_mel, window.to_vec())).map_err(|e| e.to_string())?;
        let t_len = ort::value::Tensor::from_array((vec![1i64], vec![MEL_FRAMES as i32]))
            .map_err(|e| e.to_string())?;
        let t_lang =
            ort::value::Tensor::from_array((shape_lang, lang_mask)).map_err(|e| e.to_string())?;
        let t_pre = ort::value::Tensor::from_array((shape_pre, self.pre_cache.clone()))
            .map_err(|e| e.to_string())?;
        let t_clc = ort::value::Tensor::from_array((shape_clc, self.cache_last_channel.clone()))
            .map_err(|e| e.to_string())?;
        let t_clt = ort::value::Tensor::from_array((shape_clt, self.cache_last_time.clone()))
            .map_err(|e| e.to_string())?;
        let t_chl = ort::value::Tensor::from_array((vec![1i64], vec![self.cache_last_channel_len]))
            .map_err(|e| e.to_string())?;

        let inputs = ort::inputs![
            "audio_signal" => t_mel,
            "audio_length" => t_len,
            "language_mask" => t_lang,
            "pre_cache" => t_pre,
            "cache_last_channel" => t_clc,
            "cache_last_time" => t_clt,
            "cache_last_channel_len" => t_chl,
        ];

        let t_enc = std::time::Instant::now();
        let mut enc = self.engine.enc.lock();
        let outputs = enc
            .run(inputs)
            .map_err(|e| format!("encoder run failed: {e}"))?;
        self.last_enc_ms = t_enc.elapsed().as_millis() as u64;

        // Outputs: encoded_output, encoded_length, new_pre_cache,
        // new_cache_last_channel, new_cache_last_time, new_cache_last_channel_len
        let encoded: Vec<f32> = outputs[0].try_extract_tensor::<f32>().map_err(|e| e.to_string())?.1.to_vec();
        let enc_len: usize = {
            let v: &[i32] = outputs[1].try_extract_tensor::<i32>().map_err(|e| e.to_string())?.1;
            v.first().copied().unwrap_or(0).max(0) as usize
        };
        self.pre_cache = outputs[2].try_extract_tensor::<f32>().map_err(|e| e.to_string())?.1.to_vec();
        self.cache_last_channel = outputs[3].try_extract_tensor::<f32>().map_err(|e| e.to_string())?.1.to_vec();
        self.cache_last_time = outputs[4].try_extract_tensor::<f32>().map_err(|e| e.to_string())?.1.to_vec();
        self.cache_last_channel_len = {
            let v: &[i32] = outputs[5].try_extract_tensor::<i32>().map_err(|e| e.to_string())?.1;
            v.first().copied().unwrap_or(0)
        };

        // Greedy RNN-T decode over the encoder-emitted frames.
        let frames = enc_len.min(encoded.len() / ENCODER_HIDDEN);
        let mut emitted = String::new();
        let t0 = std::time::Instant::now();
        for frame in 0..frames {
            let frame_off = frame * ENCODER_HIDDEN;
            for _ in 0..MAX_SYMBOLS {
                let logits = self.joint_logits(&encoded[frame_off..frame_off + ENCODER_HIDDEN])?;
                let best = argmax(&logits);
                if best == BLANK_ID {
                    break;
                }
                emitted.push_str(&self.token_to_text(best));
                self.run_decoder_step(best as i64);
            }
        }
        self.accumulated.push_str(&emitted);
        if std::env::var_os("VK_NEMOTRON_DEBUG").is_some() {
            let mean: f32 = encoded.iter().map(|v| v * v).sum::<f32>() / encoded.len().max(1) as f32;
            eprintln!(
                "[nem] window: enc_len={frames} chl_len={} enc_rms={:.3} decode_ms={} enc_ms={}",
                self.cache_last_channel_len,
                mean.sqrt(),
                t0.elapsed().as_millis(),
                self.last_enc_ms
            );
        }
        Ok(())
    }

    fn joint_logits(&self, enc_frame: &[f32]) -> Result<Vec<f32>, String> {
        let shape_enc: [i64; 3] = [1, 1, ENCODER_HIDDEN as i64];
        let shape_dec: [i64; 3] = [1, 1, DECODER_HIDDEN as i64];
        let t_enc = ort::value::Tensor::from_array((shape_enc, enc_frame.to_vec()))
            .map_err(|e| e.to_string())?;
        let t_dec = ort::value::Tensor::from_array((shape_dec, self.dec_hidden.clone()))
            .map_err(|e| e.to_string())?;
        let inputs = ort::inputs!["encoder_output" => t_enc, "decoder_output" => t_dec];
        let mut joint = self.engine.joint.lock();
        let outputs = joint
            .run(inputs)
            .map_err(|e| format!("joint run failed: {e}"))?;
        Ok(outputs[0].try_extract_tensor::<f32>().map_err(|e| e.to_string())?.1.to_vec())
    }

    fn run_decoder_step(&mut self, token: i64) {
        let shape_tok: [i64; 2] = [1, 1];
        let shape_state: [i64; 3] = [DECODER_LAYERS as i64, 1, DECODER_HIDDEN as i64];
        let build = || -> Result<_, String> {
            let t_tok =
                ort::value::Tensor::from_array((shape_tok, vec![token])).map_err(|e| e.to_string())?;
            let t_h = ort::value::Tensor::from_array((shape_state, self.dec_h.clone()))
                .map_err(|e| e.to_string())?;
            let t_c = ort::value::Tensor::from_array((shape_state, self.dec_c.clone()))
                .map_err(|e| e.to_string())?;
            Ok(ort::inputs!["token" => t_tok, "h" => t_h, "c" => t_c])
        };
        let Ok(inputs) = build() else {
            return;
        };
        let mut dec = self.engine.dec.lock();
        let result = match dec.run(inputs) {
            Ok(outputs) => {
                let hidden = outputs[0]
                    .try_extract_tensor::<f32>()
                    .map(|t| t.1.to_vec())
                    .ok();
                let h = outputs[1]
                    .try_extract_tensor::<f32>()
                    .map(|t| t.1.to_vec())
                    .ok();
                let c = outputs[2]
                    .try_extract_tensor::<f32>()
                    .map(|t| t.1.to_vec())
                    .ok();
                Some((hidden, h, c))
            }
            Err(e) => {
                if std::env::var_os("VK_NEMOTRON_DEBUG").is_some() {
                    eprintln!("[nem] decoder step failed: {e}");
                }
                None
            }
        };
        drop(dec);
        if let Some((hidden, h, c)) = result {
            if let Some(v) = hidden {
                self.dec_hidden = v;
            }
            if let Some(v) = h {
                self.dec_h = v;
            }
            if let Some(v) = c {
                self.dec_c = v;
            }
        }
    }

    fn token_to_text(&self, id: usize) -> String {
        let piece = self.engine.vocab.get(id).cloned().unwrap_or_default();
        // SentencePiece U+2581 (▁) = leading space.
        if let Some(rest) = piece.strip_prefix('\u{2581}') {
            format!(" {rest}")
        } else {
            piece
        }
    }
}

fn argmax(v: &[f32]) -> usize {
    v.iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "needs VK_TEST_PCM (raw f32 mono 16 kHz) and VK_TEST_MODEL_DIR"]
    fn transcribe_real_speech() {
        let pcm_path = std::env::var("VK_TEST_PCM").expect("VK_TEST_PCM not set");
        let model_dir = std::env::var("VK_TEST_MODEL_DIR").expect("VK_TEST_MODEL_DIR not set");
        let raw = std::fs::read(&pcm_path).unwrap();
        let samples: Vec<f32> = raw
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let engine = NemotronEngine::load(Path::new(&model_dir)).unwrap();
        let out = engine.transcribe(&samples, Some("en")).unwrap();
        println!("nemotron-test text: {out:?}");
        assert!(!out.text.trim().is_empty(), "transcript was empty");
    }
}
