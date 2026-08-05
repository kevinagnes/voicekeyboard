use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 16_000;

pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|devices| {
            devices
                .filter_map(|d| d.name().ok().map(|n| n.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn find_input_device(name: &str) -> Option<cpal::Device> {
    if name.is_empty() {
        return cpal::default_host().default_input_device();
    }
    let host = cpal::default_host();
    host.input_devices()
        .ok()
        .and_then(|devices| {
            devices
                .filter_map(|d| d.name().ok().map(|n| (n, d)))
                .find(|(n, _)| n == name)
                .map(|(_, d)| d)
        })
}

pub struct RingBuffer {
    data: Vec<f32>,
    capacity: usize,
    start: usize,
    len: usize,
}

impl RingBuffer {
    pub fn new(capacity_seconds: u64) -> Self {
        let capacity = (capacity_seconds as usize).saturating_mul(SAMPLE_RATE as usize).max(1);
        Self {
            data: vec![0.0; capacity],
            capacity,
            start: 0,
            len: 0,
        }
    }

    pub fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            if self.len < self.capacity {
                let idx = (self.start + self.len) % self.capacity;
                self.data[idx] = s;
                self.len += 1;
            } else {
                self.data[self.start] = s;
                self.start = (self.start + 1) % self.capacity;
            }
        }
    }

    pub fn clear(&mut self) {
        self.start = 0;
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn to_vec(&self) -> Vec<f32> {
        if self.len == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.len);
        for i in 0..self.len {
            out.push(self.data[(self.start + i) % self.capacity]);
        }
        out
    }
}

pub struct Resampler {
    step: f64,
    pos: f64,
}

impl Resampler {
    pub fn new(source_rate: u32) -> Self {
        Self {
            step: source_rate as f64 / SAMPLE_RATE as f64,
            pos: 0.0,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len() as f64;
        let mut i = self.pos;
        while i + 1.0 < n {
            let i0 = i.floor() as usize;
            let frac = (i - i0 as f64) as f32;
            out.push(input[i0] * (1.0 - frac) + input[i0 + 1] * frac);
            i += self.step;
        }
        self.pos = i - n;
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
    }
}

pub struct AudioRecorder {
    ring: Arc<Mutex<RingBuffer>>,
    capturing: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
}

impl AudioRecorder {
    pub fn new(max_recording_secs: u64, device_name: &str) -> Result<Self, anyhow::Error> {
        let ring = Arc::new(Mutex::new(RingBuffer::new(max_recording_secs)));
        let capturing = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let running = Arc::new(AtomicBool::new(true));

        let ring_cb = ring.clone();
        let capturing_cb = capturing.clone();
        let level_cb = level.clone();
        let running_cb = running.clone();
        let device_name = device_name.to_string();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

        std::thread::Builder::new()
            .name("audio-capture".into())
            .spawn(move || {
                // Keep the stream alive for the lifetime of this thread: a dropped
                // cpal::Stream stops the microphone. The thread polls `running` and
                // exits when the recorder is dropped.
                match build_stream(ring_cb, capturing_cb, level_cb, &device_name) {
                    Ok(stream) => {
                        if let Err(e) = stream.play() {
                            let _ = tx.send(Err(format!("failed to start microphone: {e}")));
                            return;
                        }
                        let _ = tx.send(Ok(()));
                        while running_cb.load(Ordering::SeqCst) {
                            std::thread::park_timeout(std::time::Duration::from_millis(100));
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn audio thread: {e}"))?;

        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self {
                ring,
                capturing,
                level,
                running,
            }),
            Ok(Err(e)) => Err(anyhow::anyhow!(e)),
            Err(_) => Err(anyhow::anyhow!("audio thread did not start within 10s")),
        }
    }

    pub fn start(&self) {
        self.ring.lock().clear();
        self.level.store(0, Ordering::SeqCst);
        self.capturing.store(true, Ordering::SeqCst);
    }

    pub fn stop(&self) -> Vec<f32> {
        self.capturing.store(false, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(20));
        self.level.store(0, Ordering::SeqCst);
        self.ring.lock().to_vec()
    }

    pub fn is_capturing(&self) -> bool {
        self.capturing.load(Ordering::SeqCst)
    }

    pub fn level(&self) -> u32 {
        self.level.load(Ordering::SeqCst)
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

fn build_stream(
    ring: Arc<Mutex<RingBuffer>>,
    capturing: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    device_name: &str,
) -> Result<cpal::Stream, String> {
    let device = find_input_device(device_name)
        .ok_or_else(|| "no input device found".to_string())?;
    let config = device.default_input_config().map_err(|e| e.to_string())?;
    let channels = config.channels();
    let sample_rate = config.sample_rate().0;
    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();

    let stream = match sample_format {
        cpal::SampleFormat::F32 => build::<f32>(&device, &stream_config, channels, sample_rate, ring, capturing, level),
        cpal::SampleFormat::I16 => build::<i16>(&device, &stream_config, channels, sample_rate, ring, capturing, level),
        cpal::SampleFormat::U16 => build::<u16>(&device, &stream_config, channels, sample_rate, ring, capturing, level),
        cpal::SampleFormat::I32 => build::<i32>(&device, &stream_config, channels, sample_rate, ring, capturing, level),
        cpal::SampleFormat::U32 => build::<u32>(&device, &stream_config, channels, sample_rate, ring, capturing, level),
        other => return Err(format!("unsupported input sample format: {other}")),
    }?;

    Ok(stream)
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: u16,
    sample_rate: u32,
    ring: Arc<Mutex<RingBuffer>>,
    capturing: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    <T as cpal::Sample>::Float: Into<f32>,
{
    let mut resampler = Resampler::new(sample_rate);
    let stream = device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                if !capturing.load(Ordering::SeqCst) {
                    return;
                }
                let Some(mut ring) = ring.try_lock() else {
                    return;
                };
                let mono: Vec<f32> = if channels == 1 {
                    data.iter().map(|&s| to_f32(s)).collect()
                } else {
                    data.chunks_exact(channels as usize)
                        .map(|frame| {
                            frame.iter().map(|&s| to_f32(s)).sum::<f32>() / channels as f32
                        })
                        .collect()
                };
                let rms = mono.iter().fold(0.0f32, |acc, &s| acc + s * s);
                let rms = (rms / mono.len().max(1) as f32).sqrt();
                let scaled = (1000.0 * rms / (rms + 0.02)).min(1000.0) as u32;
                level.store(scaled, Ordering::SeqCst);
                let mut out = Vec::new();
                resampler.process(&mono, &mut out);
                ring.push(&out);
            },
            |_err| {},
            None,
        )
        .map_err(|e| e.to_string())?;
    Ok(stream)
}

fn to_f32<T>(sample: T) -> f32
where
    T: cpal::Sample,
    <T as cpal::Sample>::Float: Into<f32>,
{
    sample.to_float_sample().into()
}

pub fn trim_silence(samples: &[f32], frame_ms: usize) -> Vec<f32> {
    if samples.is_empty() {
        return Vec::new();
    }
    let frame = (SAMPLE_RATE as usize * frame_ms / 1000).max(1);
    let mut rms = Vec::with_capacity(samples.len() / frame + 1);
    let mut peak = 0.0f32;
    for chunk in samples.chunks(frame) {
        let mut sum = 0.0f32;
        for &s in chunk {
            sum += s * s;
        }
        let v = (sum / chunk.len() as f32).sqrt();
        rms.push(v);
        peak = peak.max(v);
    }
    let threshold = (peak * 0.1).clamp(0.001, 0.05);
    let first = rms.iter().position(|&v| v > threshold);
    let Some(first) = first else {
        return Vec::new();
    };
    let last = rms.iter().rposition(|&v| v > threshold).unwrap_or(first);
    let pad = 4;
    let start = first.saturating_sub(pad) * frame;
    let end = ((last + 1 + pad).min(rms.len())) * frame;
    samples[start.min(samples.len())..end.min(samples.len())].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_overwrites_oldest() {
        let mut r = RingBuffer::new(1);
        let chunk: Vec<f32> = (0..SAMPLE_RATE as usize).map(|i| i as f32).collect();
        r.push(&chunk);
        r.push(&chunk);
        assert_eq!(r.len(), SAMPLE_RATE as usize);
        let v = r.to_vec();
        assert_eq!(v[0], 0.0);
        assert_eq!(v[v.len() - 1], (SAMPLE_RATE - 1) as f32);
    }

    #[test]
    fn resampler_downsample_preserves_duration() {
        let input: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.001).sin()).collect();
        let mut r = Resampler::new(48_000);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert!((out.len() as f64 - 16_000.0).abs() < 8.0);
    }

    #[test]
    fn trim_removes_silence() {
        let mut audio = vec![0.0f32; 16_000];
        for (i, s) in audio.iter_mut().enumerate().take(8_000).skip(2_000) {
            *s = (i as f32 * 0.01).sin() * 0.3;
        }
        let trimmed = trim_silence(&audio, 25);
        assert!(!trimmed.is_empty());
        assert!(trimmed.len() < audio.len());
    }

    #[test]
    fn trim_empty_on_pure_silence() {
        let audio = vec![0.0f32; 16_000];
        assert!(trim_silence(&audio, 25).is_empty());
    }
}
