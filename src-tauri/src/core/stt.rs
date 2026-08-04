use parking_lot::Mutex;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

#[derive(Debug)]
pub struct Transcription {
    pub text: String,
    pub language: String,
}

pub struct SttEngine {
    ctx: Mutex<Option<WhisperContext>>,
    model_path: Mutex<Option<std::path::PathBuf>>,
    n_threads: usize,
}

impl Default for SttEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SttEngine {
    pub fn new() -> Self {
        Self {
            ctx: Mutex::new(None),
            model_path: Mutex::new(None),
            n_threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.ctx.lock().is_some()
    }

    pub fn model_path(&self) -> Option<std::path::PathBuf> {
        self.model_path.lock().clone()
    }

    pub fn load_model(&self, path: &str) -> Result<(), String> {
        let params = WhisperContextParameters {
            use_gpu: true,
            flash_attn: true,
            ..Default::default()
        };
        let ctx = WhisperContext::new_with_params(path, params)
            .map_err(|e| format!("failed to load model \"{path}\": {e}"))?;
        *self.ctx.lock() = Some(ctx);
        *self.model_path.lock() = Some(path.into());
        Ok(())
    }

    pub fn transcribe(
        &self,
        samples: &[f32],
        language: Option<&str>,
        initial_prompt: &str,
    ) -> Result<Transcription, String> {
        let mut guard = self.ctx.lock();
        let ctx = guard.as_mut().ok_or("model not loaded")?;
        let mut state = ctx.create_state().map_err(|e| e.to_string())?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 0 });
        params.set_n_threads(self.n_threads as i32);
        params.set_language(language);
        params.set_initial_prompt(initial_prompt);
        params.set_translate(false);
        params.set_no_context(true);
        params.set_no_timestamps(true);
        params.set_suppress_blank(true);
        params.set_print_progress(false);
        params.set_print_special(false);
        params.set_print_realtime(false);

        state.full(params, samples).map_err(|e| e.to_string())?;

        let n = state.full_n_segments().map_err(|e| e.to_string())?;
        let mut text = String::new();
        for i in 0..n {
            if let Ok(seg) = state.full_get_segment_text(i) {
                text.push_str(&seg);
            }
        }

        let language = state
            .full_lang_id_from_state()
            .ok()
            .and_then(|id| ctx.token_to_str(ctx.token_lang(id)).ok())
            .map(|s| s.trim_matches(|c| c == '<' || c == '|' || c == '>').to_string())
            .unwrap_or_else(|| "auto".to_string());

        Ok(Transcription { text, language })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcribe_before_load_fails_gracefully() {
        let engine = SttEngine::new();
        let err = engine.transcribe(&[0.0f32; 1000], None, "").unwrap_err();
        assert!(err.contains("not loaded"));
    }
}
