use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;

pub const CHIME_SAMPLE_RATE: u32 = 44_100;

enum AudioRequest {
    Chime(Chime),
    /// One resampled chunk of spoken audio (mono f32 at `CHIME_SAMPLE_RATE`).
    /// Appended to the sink immediately so playback starts before the full
    /// reply has been received.
    SpeechChunk(Vec<f32>),
    /// End of a speech stream. Blocks the audio thread until the sink has
    /// drained (or `cancel` is set, in which case the queue is cleared), then
    /// signals `done`.
    SpeechDone {
        done: Sender<()>,
        cancel: Arc<AtomicBool>,
    },
}

pub struct Sounds {
    tx: Option<Sender<AudioRequest>>,
    enabled: AtomicBool,
}

impl Default for Sounds {
    fn default() -> Self {
        Self::new()
    }
}

impl Sounds {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<AudioRequest>();
        std::thread::Builder::new()
            .name("audio-output".into())
            .spawn(move || {
                use rodio::{OutputStream, Sink};
                let Ok((_stream, handle)) = OutputStream::try_default() else {
                    let _ = rx.recv();
                    return;
                };
                let mut sink = Sink::try_new(&handle).ok();
                while let Ok(request) = rx.recv() {
                    match request {
                        AudioRequest::Chime(kind) => {
                            if let Some(sink) = sink.as_mut() {
                                sink.append(rodio::buffer::SamplesBuffer::new(
                                    1,
                                    CHIME_SAMPLE_RATE,
                                    kind.samples(),
                                ));
                            }
                        }
                        AudioRequest::SpeechChunk(samples) => {
                            if let Some(sink) = sink.as_mut() {
                                sink.append(rodio::buffer::SamplesBuffer::new(
                                    1,
                                    CHIME_SAMPLE_RATE,
                                    samples,
                                ));
                            }
                        }
                        AudioRequest::SpeechDone { done, cancel } => {
                            if let Some(sink) = sink.as_mut() {
                                while !sink.empty() {
                                    if cancel.load(Ordering::SeqCst) {
                                        sink.clear();
                                        sink.play();
                                        break;
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(50));
                                }
                            }
                            let _ = done.send(());
                        }
                    }
                }
            })
            .expect("failed to spawn audio thread");
        Self {
            tx: Some(tx),
            enabled: AtomicBool::new(true),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn play(&self, kind: Chime) {
        if !self.enabled() {
            return;
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(AudioRequest::Chime(kind));
        }
    }

    /// Open a streaming speech output. Returns `None` when the audio output
    /// device is unavailable. Chunks pushed through the returned handle are
    /// played as they arrive; `finish()` blocks until playback drains (or the
    /// stream is cancelled).
    pub fn open_speech(&self) -> Option<SpeechStreamHandle> {
        self.tx.as_ref().map(|tx| SpeechStreamHandle {
            tx: tx.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
            resampler: Arc::new(std::sync::Mutex::new(SpeechResampler::new(TTS_INPUT_RATE))),
        })
    }
}

/// Raw PCM sample rate produced by the Gemini TTS client.
pub const TTS_INPUT_RATE: u32 = 24_000;

/// A streaming speech output: push raw 16-bit PCM chunks (mono, 24 kHz), each
/// one is resampled and handed to the audio thread immediately. `finish()`
/// waits for playback to complete and can be interrupted via `cancel()`.
#[derive(Clone)]
pub struct SpeechStreamHandle {
    tx: Sender<AudioRequest>,
    cancel: Arc<AtomicBool>,
    resampler: Arc<std::sync::Mutex<SpeechResampler>>,
}

impl SpeechStreamHandle {
    /// Push one raw i16 PCM chunk (mono, 24 kHz). Resampling state carries
    /// across calls so chunk boundaries are click-free.
    pub fn push_i16(&self, pcm: &[i16]) -> Result<(), String> {
        if pcm.is_empty() {
            return Ok(());
        }
        let mut out = Vec::new();
        self.resampler.lock().unwrap().process(pcm, &mut out);
        self.send_chunks(out)
    }

    /// Push the final chunk and flush the resampler's trailing output.
    pub fn push_i16_final(&self, pcm: &[i16]) -> Result<(), String> {
        let mut out = Vec::new();
        {
            let mut resampler = self.resampler.lock().unwrap();
            if !pcm.is_empty() {
                resampler.process(pcm, &mut out);
            }
            resampler.flush(&mut out);
        }
        self.send_chunks(out)
    }

    fn send_chunks(&self, mut out: Vec<f32>) -> Result<(), String> {
        if out.is_empty() {
            return Ok(());
        }
        self.tx
            .send(AudioRequest::SpeechChunk(std::mem::take(&mut out)))
            .map_err(|_| "audio output closed".to_string())
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Signal the end of the stream and wait until the audio has finished
    /// playing (or the stream was cancelled, which drops queued audio).
    pub fn finish(&self) -> Result<(), String> {
        let (done_tx, done_rx) = mpsc::channel();
        self.tx
            .send(AudioRequest::SpeechDone {
                done: done_tx,
                cancel: self.cancel.clone(),
            })
            .map_err(|_| "audio output closed".to_string())?;
        done_rx
            .recv()
            .map_err(|_| "audio output closed".to_string())
    }
}

/// Stateful linear-interpolation resampler (24 kHz → 44.1 kHz). The output
/// grid is anchored to the start of the stream (positions are exact multiples
/// of `step` in input-sample units), so chunked processing produces exactly
/// the same samples as a single-shot pass. The one sample that straddles a
/// chunk boundary is held back (`pending`) until the next chunk's first
/// sample arrives.
struct SpeechResampler {
    step: f64,
    /// Stream position (in input samples) of the next output.
    next: f64,
    /// Stream position of `pcm[0]` in the current call.
    start: f64,
    /// (partial value with the known sample, fraction) of an output whose
    /// second interpolation sample belongs to the next chunk.
    pending: Option<(f32, f32)>,
    /// Last sample of the last processed chunk (for the stream-end flush).
    last: f32,
}

impl SpeechResampler {
    fn new(from_rate: u32) -> Self {
        Self {
            step: from_rate as f64 / CHIME_SAMPLE_RATE as f64,
            next: 0.0,
            start: 0.0,
            pending: None,
            last: 0.0,
        }
    }

    fn process(&mut self, pcm: &[i16], out: &mut Vec<f32>) {
        if pcm.is_empty() {
            return;
        }
        let start = self.start;
        let end = start + pcm.len() as f64;
        if let Some((partial, frac)) = self.pending.take() {
            out.push(partial + frac * (pcm[0] as f32 / 32768.0));
            self.next += self.step;
        }
        let mut next = self.next;
        while next + 1.0 <= end {
            let i0 = (next - start).floor() as usize;
            let frac = (next - (start + i0 as f64)) as f32;
            let a = pcm[i0] as f32 / 32768.0;
            let b = pcm[i0 + 1] as f32 / 32768.0;
            out.push(a * (1.0 - frac) + b * frac);
            next += self.step;
        }
        if next < end {
            let i0 = (next - start).floor() as usize;
            let frac = (next - (start + i0 as f64)) as f32;
            let a = pcm[i0] as f32 / 32768.0;
            self.pending = Some((a * (1.0 - frac), frac));
        }
        self.next = next;
        self.last = pcm[pcm.len() - 1] as f32 / 32768.0;
        self.start = end;
    }

    /// Emit the output still waiting on a sample beyond the stream end,
    /// approximating the missing sample with the last one seen.
    fn flush(&mut self, out: &mut Vec<f32>) {
        if let Some((partial, frac)) = self.pending.take() {
            out.push(partial + frac * self.last);
        }
    }
}

/// Trim the trailing noise/silence that the Gemini TTS API sometimes appends
/// to the end of generated audio (a known upstream artifact). Removes only a
/// suffix, never audio in the middle:
///
/// 1. A trailing quiet tail: if there is a quiet gap (RMS < 10% of peak)
///    within the last second, everything after it is dropped.
/// 2. A noise burst: if the last ≥3 windows (~60 ms) are all "noise-like"
///    (loud, >30% of peak, with a high zero-crossing rate), the burst is
///    dropped. White-noise artifacts have near-random zero crossings; voiced
///    speech (even emphatic endings) does not.
pub fn trim_noise_tail(pcm: &[i16], sample_rate: u32) -> Vec<i16> {
    const WINDOW_MS: usize = 20;
    const GAP_RMS_RATIO: f32 = 0.10;
    const BURST_MIN_WINDOWS: usize = 3;
    const BURST_RMS_RATIO: f32 = 0.30;
    const BURST_ZCR: f32 = 0.40;

    let window = (sample_rate as usize * WINDOW_MS / 1000).max(1);
    if pcm.len() < window * 2 {
        return pcm.to_vec();
    }
    let n_windows = pcm.len() / window;
    let mut rms = Vec::with_capacity(n_windows);
    let mut zcr = Vec::with_capacity(n_windows);
    for chunk in pcm.chunks(window).take(n_windows) {
        let mut sum = 0.0f32;
        let mut crossings = 0usize;
        for j in 0..chunk.len() {
            let v = chunk[j] as f32 / 32768.0;
            sum += v * v;
            if j > 0 && (chunk[j - 1] < 0) != (chunk[j] < 0) {
                crossings += 1;
            }
        }
        rms.push((sum / chunk.len() as f32).sqrt());
        zcr.push(crossings as f32 / chunk.len() as f32);
    }
    let peak = rms.iter().cloned().fold(0.0f32, f32::max);
    if peak <= 0.0 {
        return pcm.to_vec();
    }

    // 1) Trailing quiet run: silence at the very end of the clip. Cut at the
    //    start of the run so the whole quiet tail (and anything quieter than
    //    the speech) is dropped.
    let quiet = GAP_RMS_RATIO * peak;
    let last_win = n_windows - 1;
    if rms[last_win] < quiet {
        let run_start = (0..=last_win)
            .rev()
            .find(|&i| rms[i] >= quiet)
            .map_or(0, |i| i + 1);
        let end = (run_start * window).min(pcm.len());
        return pcm[..end].to_vec();
    }

    // 2) Noise burst directly after speech (no quiet gap before it).
    let mut noise_len = 0usize;
    let mut burst_start = None;
    for i in (0..n_windows).rev() {
        let is_noise = rms[i] > BURST_RMS_RATIO * peak && zcr[i] > BURST_ZCR;
        if is_noise {
            noise_len += 1;
            continue;
        }
        if noise_len >= BURST_MIN_WINDOWS {
            burst_start = Some(i + 1);
        }
        break;
    }
    if let Some(start) = burst_start {
        let end = (start * window).min(pcm.len());
        return pcm[..end].to_vec();
    }

    pcm.to_vec()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chime {
    RecordStart,
    PasteDone,
    Error,
}

impl Chime {
    pub fn samples(self) -> Vec<f32> {
        match self {
            Chime::RecordStart => {
                let mut out = tone(660.0, 0.12, 0.35);
                out.extend(tone(990.0, 0.18, 0.3));
                out
            }
            Chime::PasteDone => {
                let mut out = tone(880.0, 0.15, 0.35);
                out.extend(tone(587.0, 0.2, 0.3));
                out
            }
            Chime::Error => {
                let mut out = tone(330.0, 0.15, 0.3);
                out.extend(tone(220.0, 0.25, 0.25));
                out
            }
        }
    }
}

fn tone(freq: f32, duration: f32, peak: f32) -> Vec<f32> {
    let n = (duration * CHIME_SAMPLE_RATE as f32) as usize;
    let tau = std::f32::consts::TAU;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / CHIME_SAMPLE_RATE as f32;
        let attack = (t / 0.008).min(1.0);
        let decay = (1.0 - t / duration).max(0.0).powi(2);
        let sample = (t * freq * tau).sin() * peak * attack * decay;
        out.push(sample);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chime_has_expected_length() {
        let s = Chime::RecordStart.samples();
        let expected = ((0.12 + 0.18) * CHIME_SAMPLE_RATE as f32) as usize;
        assert_eq!(s.len(), expected);
        assert!(s.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn tone_decays_to_zero() {
        let t = tone(440.0, 0.1, 0.5);
        assert!(t.iter().all(|v| v.abs() <= 0.5 + 1e-6));
    }

    #[test]
    fn resampler_preserves_duration_and_continuity() {
        let input: Vec<i16> = (0..24_000)
            .map(|i| ((i as f32 * 0.001).sin() * 10000.0) as i16)
            .collect();
        let mut r = SpeechResampler::new(TTS_INPUT_RATE);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        r.flush(&mut out);
        let expected = (24_000.0 * CHIME_SAMPLE_RATE as f64 / TTS_INPUT_RATE as f64) as usize;
        assert!((out.len() as f64 - expected as f64).abs() < 8.0);
        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 1.0 + 1e-6));

        // Chunked processing must produce bit-identical output to a single
        // pass (the output grid is anchored to the stream start).
        let mut chunked = SpeechResampler::new(TTS_INPUT_RATE);
        let mut out2 = Vec::new();
        for chunk in input.chunks(500) {
            chunked.process(chunk, &mut out2);
        }
        chunked.flush(&mut out2);
        assert_eq!(out2.len(), out.len());
        let max_diff = (0..out.len())
            .map(|i| (out[i] - out2[i]).abs())
            .fold(0.0f32, f32::max);
        assert!(max_diff < 1e-4, "max diff {max_diff}");
    }

    #[test]
    fn trim_removes_trailing_silence() {
        let mut pcm = vec![0i16; 24_000]; // 1 s
        for (i, s) in pcm.iter_mut().enumerate().take(12_000) {
            *s = ((i as f32 * 0.01).sin() * 8000.0) as i16;
        }
        let trimmed = trim_noise_tail(&pcm, TTS_INPUT_RATE);
        assert_eq!(trimmed.len(), 12_000);
    }

    #[test]
    fn trim_removes_noise_burst_after_gap() {
        let mut pcm = vec![0i16; 24_000];
        for (i, s) in pcm.iter_mut().enumerate().take(9_000) {
            *s = ((i as f32 * 0.01).sin() * 8000.0) as i16; // speech
        }
        // 9..10s: gap, 10..12s: loud noise burst
        let mut state = 12345u32;
        for i in 10_000..24_000 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            pcm[i] = ((state >> 16) as i16).saturating_mul(3) / 4; // near full-scale noise
        }
        let trimmed = trim_noise_tail(&pcm, TTS_INPUT_RATE);
        // Cut at the quiet gap (window boundary) — the noise burst is gone.
        assert!(
            trimmed.len() >= 9_000 && trimmed.len() < 11_000,
            "len {}",
            trimmed.len()
        );
    }

    #[test]
    fn trim_removes_noise_burst_after_speech_no_gap() {
        let mut pcm = vec![0i16; 24_000];
        for (i, s) in pcm.iter_mut().enumerate().take(21_000) {
            *s = ((i as f32 * 0.01).sin() * 5000.0) as i16; // speech
        }
        let mut state = 98765u32;
        for i in 21_000..24_000 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            pcm[i] = ((state >> 16) as i16).wrapping_mul(2); // loud noise, no gap
        }
        let trimmed = trim_noise_tail(&pcm, TTS_INPUT_RATE);
        assert!(
            trimmed.len() > 20_500 && trimmed.len() < 22_500,
            "len {}",
            trimmed.len()
        );
    }

    #[test]
    fn trim_keeps_loud_emphatic_ending() {
        // A final loud, voiced syllable (low ZCR) must NOT be trimmed.
        let mut pcm = vec![0i16; 24_000];
        for (i, s) in pcm.iter_mut().enumerate().take(20_000) {
            *s = ((i as f32 * 0.01).sin() * 5000.0) as i16;
        }
        for (i, s) in pcm.iter_mut().enumerate().take(24_000).skip(20_000) {
            *s = ((i as f32 * 0.005).sin() * 15_000.0) as i16; // loud sustained vowel
        }
        let trimmed = trim_noise_tail(&pcm, TTS_INPUT_RATE);
        assert_eq!(trimmed.len(), pcm.len());
    }

    #[test]
    fn trim_keeps_normal_speech() {
        let mut pcm = vec![0i16; 24_000];
        for (i, s) in pcm.iter_mut().enumerate() {
            let t = i as f32 / 24_000.0;
            *s = ((t * 12.0 * std::f32::consts::TAU).sin() * 9000.0 * (1.0 - t * 0.5)) as i16;
        }
        let trimmed = trim_noise_tail(&pcm, TTS_INPUT_RATE);
        assert_eq!(trimmed.len(), pcm.len());
    }

    #[test]
    fn trim_keeps_pause_then_more_speech() {
        // Speech, pause, more speech, quiet end -> only the final silence goes.
        let mut pcm = vec![0i16; 24_000];
        for (i, s) in pcm.iter_mut().enumerate().take(8_000) {
            *s = ((i as f32 * 0.01).sin() * 8000.0) as i16;
        }
        for (i, s) in pcm.iter_mut().enumerate().take(20_000).skip(14_000) {
            *s = ((i as f32 * 0.01).sin() * 8000.0) as i16;
        }
        let trimmed = trim_noise_tail(&pcm, TTS_INPUT_RATE);
        assert!(
            trimmed.len() >= 20_000 && trimmed.len() <= 20_160,
            "len {}",
            trimmed.len()
        );
    }
}
