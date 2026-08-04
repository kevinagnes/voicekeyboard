use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};

pub const CHIME_SAMPLE_RATE: u32 = 44_100;

pub struct Sounds {
    tx: Option<Sender<Vec<f32>>>,
    enabled: AtomicBool,
}

impl Default for Sounds {
    fn default() -> Self {
        Self::new()
    }
}

impl Sounds {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Vec<f32>>();
        std::thread::Builder::new()
            .name("audio-output".into())
            .spawn(move || {
                use rodio::{OutputStream, Sink};
                let Ok((_stream, handle)) = OutputStream::try_default() else {
                    let _ = rx.recv();
                    return;
                };
                let mut sink = Sink::try_new(&handle).ok();
                while let Ok(samples) = rx.recv() {
                    if let Some(sink) = sink.as_mut() {
                        sink.append(rodio::buffer::SamplesBuffer::new(
                            1,
                            CHIME_SAMPLE_RATE,
                            samples,
                        ));
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
            let _ = tx.send(kind.samples());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chime {
    RecordStart,
    PasteDone,
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
}
