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
const MEL_FRAMES: usize = 32; // per encoder window
const OUTPUT_FRAMES: usize = 4; // encoder frames emitted per window
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

    /// Pre-emphasis y[n] = x[n] - a*x[n-1], first sample uses the carry from
    /// the previous window. Returns the last raw sample for the next window.
    fn preemphasize(&self, pcm: &mut [f32], carry: f32) -> f32 {
        let mut prev = carry;
        for x in pcm.iter_mut() {
            let cur = *x;
            *x = cur - PREEMPH * prev;
            prev = cur;
        }
        prev
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

        let mut fft = self.planner.plan_fft_forward(N_FFT);
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

fn build_session(path: &Path) -> Result<ort::session::Session, String> {
    let mut builder =
        ort::session::Session::builder().map_err(|e| format!("session builder: {e}"))?;
    builder = builder
        .with_intra_threads(4)
        .map_err(|e| format!("session builder: {e}"))?;
    builder = builder
        .with_optimization_level(ort::session::builder::GraphOptimizationLevel::All)
        .map_err(|e| format!("session builder: {e}"))?;
    builder
        .commit_from_file(path)
        .map_err(|e| format!("failed to load {}: {e}", path.display()))
}

impl NemotronEngine {
    /// Load from a model directory, or from the primary `encoder.onnx` file
    /// path (the app passes the resolved download path).
    pub fn load(dir: &Path) -> Result<Self, String> {
        let dir = if dir.is_dir() {
            dir.to_path_buf()
        } else {
            dir.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| dir.to_path_buf())
        };
        let enc = build_session(&dir.join("encoder.onnx"))?;
        let dec = build_session(&dir.join("decoder.onnx"))?;
        let joint = build_session(&dir.join("joint.onnx"))?;
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

        let win_samples = (MEL_FRAMES - 1) * HOP_LENGTH;
        let mut carry = 0.0f32;
        let mut i = 0usize;
        let mut text = String::new();
        while i + win_samples <= samples.len() {
            let mut chunk = samples[i..i + win_samples].to_vec();
            carry = self.mel.lock().preemphasize(&mut chunk, carry);
            text.push_str(&stream.run_window(&chunk)?);
            i += win_samples;
        }
        // Trailing partial window: pad with silence (matches flush_stream).
        if i < samples.len() {
            let mut chunk = samples[i..].to_vec();
            chunk.resize(win_samples, 0.0);
            carry = self.mel.lock().preemphasize(&mut chunk, carry);
            text.push_str(&stream.run_window(&chunk)?);
        }

        Ok(NemotronTranscription {
            text,
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
        };
        // Prime the predictor with the blank token.
        s.run_decoder_step(BLANK_ID as i64);
        s
    }

    fn run_window(&mut self, chunk: &[f32]) -> Result<String, String> {
        let mut mel = self.engine.mel.lock().compute(chunk);
        // Trim to exactly MEL_FRAMES (centred padding can overshoot).
        let produced = mel.len() / MEL_BINS;
        if produced < MEL_FRAMES {
            return Ok(String::new());
        }
        if produced > MEL_FRAMES {
            let mut trimmed = vec![0.0f32; MEL_BINS * MEL_FRAMES];
            for b in 0..MEL_BINS {
                trimmed[b * MEL_FRAMES..(b + 1) * MEL_FRAMES]
                    .copy_from_slice(&mel[b * produced..b * produced + MEL_FRAMES]);
            }
            mel = trimmed;
        }

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

        let t_mel = ort::value::Tensor::from_array((shape_mel, mel)).map_err(|e| e.to_string())?;
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

        let mut enc = self.engine.enc.lock();
        let outputs = enc
            .run(inputs)
            .map_err(|e| format!("encoder run failed: {e}"))?;

        // Outputs: encoded_output, encoded_length, new_pre_cache,
        // new_cache_last_channel, new_cache_last_time, new_cache_last_channel_len
        let encoded: Vec<f32> = outputs[0].try_extract_tensor::<f32>().map_err(|e| e.to_string())?.1.to_vec();
        self.pre_cache = outputs[2].try_extract_tensor::<f32>().map_err(|e| e.to_string())?.1.to_vec();
        self.cache_last_channel = outputs[3].try_extract_tensor::<f32>().map_err(|e| e.to_string())?.1.to_vec();
        self.cache_last_time = outputs[4].try_extract_tensor::<f32>().map_err(|e| e.to_string())?.1.to_vec();
        self.cache_last_channel_len = {
            let v: &[i32] = outputs[5].try_extract_tensor::<i32>().map_err(|e| e.to_string())?.1;
            v.first().copied().unwrap_or(0)
        };

        // Greedy RNN-T decode over the committed frames.
        let mut emitted = String::new();
        for frame in 0..OUTPUT_FRAMES {
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
        Ok(emitted)
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
        let result = dec.run(inputs).ok().map(|outputs| {
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
            (hidden, h, c)
        });
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
