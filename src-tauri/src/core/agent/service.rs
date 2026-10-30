//! Agent service: durable admission, single-flight execution, tools, plans,
//! compaction, permissions, streaming speech, recovery and undo.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use super::budget;
use super::gemini::{self as gemini_mod, GeminiAgentClient, GeminiFailure};
use super::permissions;
use super::prompt;
use super::speech::{Segmenter, SpeechPipeline};
use super::store::{Db, SessionRow};
use super::tools::{self, ToolContext, ToolDefinition};
use super::types::*;
use super::validation;

const AGENT_EVENT: &str = "llm-status";
const PERMISSION_EVENT: &str = "agent-permission";
const SESSION_EVENT: &str = "llm-session";
const RUNS_EVENT: &str = "agent-runs";
const PERMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

pub struct AgentService {
    pub db: Arc<Db>,
    pub desktop: Arc<super::desktop::SystemDesktop>,
    sounds: Arc<crate::core::sounds::Sounds>,
    app: OnceLock<AppHandle>,
    tx: Sender<()>,
    permissions: Mutex<HashMap<String, Sender<Decision>>>,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
    stop_requested: AtomicBool,
    pub active: AtomicBool,
    pub speaking: AtomicBool,
    voice_active: AtomicBool,
    weak: OnceLock<Weak<AgentService>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    pending_permission: Mutex<Option<PermissionRequest>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub updated_at: u64,
    pub turns: usize,
    pub active: bool,
    pub parent_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnView {
    pub role: String,
    pub text: String,
    pub at: u64,
}

impl AgentService {
    pub fn start(
        db_path: PathBuf,
        sounds: Arc<crate::core::sounds::Sounds>,
        desktop: Arc<super::desktop::SystemDesktop>,
    ) -> Arc<Self> {
        let db = Db::open(&db_path).expect("agent database opens");
        db.import_legacy(&crate::core::sessions::load());
        let (tx, rx) = mpsc::channel();
        let service = Arc::new(Self {
            db,
            desktop,
            sounds,
            app: OnceLock::new(),
            tx,
            permissions: Mutex::new(HashMap::new()),
            cancel: Mutex::new(None),
            stop_requested: AtomicBool::new(false),
            active: AtomicBool::new(false),
            speaking: AtomicBool::new(false),
            voice_active: AtomicBool::new(false),
            weak: OnceLock::new(),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pending_permission: Mutex::new(None),
        });
        let _ = service.weak.set(Arc::downgrade(&service));
        let worker = Arc::downgrade(&service);
        std::thread::Builder::new()
            .name("agent-worker".into())
            .spawn(move || {
                while rx.recv().is_ok() {
                    let Some(service) = worker.upgrade() else {
                        break;
                    };
                    service.run_pending();
                }
            })
            .expect("agent worker starts");
        service
    }

    pub fn attach(&self, app: AppHandle) {
        self.desktop.attach(app.clone());
        let _ = self.app.set(app);
    }

    fn emit(&self, event: AgentEvent) {
        let Some(app) = self.app.get() else { return };
        match event {
            AgentEvent::Status(status) => {
                if status.generation == 0
                    || status.generation == self.generation.load(Ordering::SeqCst)
                {
                    let _ = app.emit(AGENT_EVENT, status);
                }
            }
            AgentEvent::Permission(request) => {
                let _ = app.emit(PERMISSION_EVENT, request);
            }
            AgentEvent::PermissionClosed(id) => {
                let _ = app.emit("agent-permission-closed", id);
            }
            AgentEvent::SessionChanged => {
                let _ = app.emit(SESSION_EVENT, ());
            }
            AgentEvent::RunsChanged => {
                let _ = app.emit(RUNS_EVENT, ());
            }
        }
    }

    fn hide_overlay_later(&self, millis: u64) {
        let Some(app) = self.app.get().cloned() else {
            return;
        };
        let generation = self.generation.clone();
        let current = generation.load(Ordering::SeqCst);
        std::thread::Builder::new()
            .name("agent-hide-overlay".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(millis));
                if generation.load(Ordering::SeqCst) == current {
                    if let Some(window) = app.get_webview_window("llm-overlay") {
                        let _ = window.hide();
                    }
                }
            })
            .ok();
    }

    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn wake(&self) {
        let _ = self.tx.send(());
    }

    // -------------------------------------------------------------- admission

    #[allow(clippy::too_many_arguments)]
    pub fn admit_user_text(
        &self,
        session: Option<&str>,
        text: &str,
        client_id: Option<&str>,
        delivery: &str,
        deep_work: bool,
    ) -> String {
        let text = text.trim();
        let session_id = match session.filter(|id| self.db.session(id).is_some()) {
            Some(id) => id.to_string(),
            None => {
                let model = crate::core::settings::load().llm_model;
                let id =
                    self.db
                        .create_session(&crate::core::sessions::title_from(text), None, &model);
                self.db.set_active_session(Some(&id));
                id
            }
        };
        self.db
            .touch_session(&session_id, Some(&crate::core::sessions::title_from(text)));
        let payload = json!({"text": text, "deepWork": deep_work});
        self.db
            .admit(&session_id, "user", delivery, client_id, &payload);
        self.bump_generation();
        self.emit(AgentEvent::SessionChanged);
        self.wake();
        session_id
    }

    /// Admit a spoken question together with a screenshot.
    pub fn admit_user_image(
        &self,
        session: Option<&str>,
        text: &str,
        image_path: Option<&str>,
        delivery: &str,
        deep_work: bool,
    ) -> String {
        let text = text.trim();
        let session_id = match session.filter(|id| self.db.session(id).is_some()) {
            Some(id) => id.to_string(),
            None => {
                let model = crate::core::settings::load().llm_model;
                let id =
                    self.db
                        .create_session(&crate::core::sessions::title_from(text), None, &model);
                self.db.set_active_session(Some(&id));
                id
            }
        };
        self.db
            .touch_session(&session_id, Some(&crate::core::sessions::title_from(text)));
        let payload = match image_path {
            Some(path) => json!({"text": text, "deepWork": deep_work, "image": path}),
            None => json!({"text": text, "deepWork": deep_work}),
        };
        self.db.admit(&session_id, "user", delivery, None, &payload);
        self.bump_generation();
        self.emit(AgentEvent::SessionChanged);
        self.wake();
        session_id
    }

    pub fn notify_barge_in(&self) {
        self.bump_generation();
        self.stop_requested.store(false, Ordering::SeqCst);
        self.deny_pending_permissions();
        if let Some(cancel) = self.cancel.lock().as_ref() {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    pub fn stop(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        self.deny_pending_permissions();
        if let Some(cancel) = self.cancel.lock().as_ref() {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    fn deny_pending_permissions(&self) {
        let pending: Vec<(String, Sender<Decision>)> = {
            let mut guard = self.permissions.lock();
            guard.drain().collect()
        };
        for (id, tx) in pending {
            let _ = tx.send(Decision::Deny);
            self.emit(AgentEvent::PermissionClosed(id));
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking.load(Ordering::SeqCst)
    }

    pub fn is_voice_active(&self) -> bool {
        self.voice_active.load(Ordering::SeqCst)
    }

    // -------------------------------------------------------------- scheduling

    fn run_pending(&self) {
        loop {
            let mut progressed = false;
            while let Some(job) = self.db.claim_job() {
                progressed = true;
                self.run_job(job);
            }
            let sessions = self.db.sessions_with_pending();
            if sessions.is_empty() {
                if !progressed {
                    break;
                }
                continue;
            }
            for session in sessions {
                if self.db.session(&session).is_none() {
                    continue;
                }
                if !self
                    .db
                    .claim(&session, &format!("agent-{}", std::process::id()))
                {
                    continue;
                }
                let policy = self.policy_for(&session, false);
                let silent = self
                    .db
                    .pending(&session, Some("steer"))
                    .first()
                    .and_then(|(_, payload)| payload.get("silent").and_then(|v| v.as_bool()))
                    .unwrap_or(false);
                self.drain_session(&session, policy, !silent, 0, None);
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
    }

    fn run_job(&self, job: JobInfo) {
        if job.kind != "agent" {
            self.db
                .finish_job(&job.id, "failed", None, Some("Unknown job kind"));
            return;
        }
        let Some(session_id) = job.session_id.clone() else {
            self.db
                .finish_job(&job.id, "failed", None, Some("Job has no session"));
            return;
        };
        if !self.db.claim(&session_id, &format!("job-{}", job.id)) {
            self.db
                .requeue_job(&job.id, Some(now_ms().saturating_add(5_000)));
            return;
        }
        let policy = self.policy_for(&session_id, true);
        let result = self.drain_session(&session_id, policy, false, 0, Some(&job.id));
        match result {
            DrainResult::Succeeded(text) => {
                self.db.finish_job(&job.id, "succeeded", Some(&text), None);
            }
            DrainResult::Interrupted => {
                self.db.finish_job(&job.id, "cancelled", None, None);
            }
            DrainResult::Failed(error) => {
                self.db.finish_job(&job.id, "failed", None, Some(&error));
            }
        }
        self.emit(AgentEvent::RunsChanged);
    }

    pub fn enqueue_job(&self, prompt: &str, deep: bool) -> String {
        let model = crate::core::settings::load().llm_model;
        let session_id =
            self.db
                .create_session(&crate::core::sessions::title_from(prompt), None, &model);
        let payload = json!({"text": prompt, "deepWork": deep});
        self.db.admit(&session_id, "user", "steer", None, &payload);
        let job = self
            .db
            .enqueue_job(Some(&session_id), "agent", &json!({"deepWork": deep}), true);
        self.emit(AgentEvent::SessionChanged);
        self.wake();
        job
    }

    fn policy_for(&self, session_id: &str, job_deep: bool) -> RunPolicy {
        let settings = crate::core::settings::load();
        let pending_text =
            self.db
                .pending(session_id, Some("steer"))
                .first()
                .and_then(|(_, payload)| {
                    payload
                        .get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                });
        let history_text = self
            .db
            .messages(session_id)
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.text());
        let heuristic = pending_text
            .or(history_text)
            .map(|text| {
                let text = text.to_lowercase();
                [
                    "deep work",
                    "take your time",
                    "research",
                    "write a guide",
                    "draft a",
                ]
                .iter()
                .any(|needle| text.contains(needle))
            })
            .unwrap_or(false);
        if job_deep || settings.agent_deep_work || heuristic {
            RunPolicy::deep_work()
        } else {
            let mut policy = RunPolicy::normal();
            if settings.agent_max_steps > 0 {
                policy.max_steps = settings.agent_max_steps;
            }
            policy
        }
    }

    // ------------------------------------------------------------------ drain

    fn drain_session(
        &self,
        session_id: &str,
        policy: RunPolicy,
        speak: bool,
        depth: u8,
        job_id: Option<&str>,
    ) -> DrainResult {
        let settings = crate::core::settings::load();
        let engine = GeminiAgentClient::new(
            &settings.gemini_api_key,
            &settings.llm_model,
            &settings.llm_system_prompt,
        );
        if !engine.configured() {
            let generation = self.generation.load(Ordering::SeqCst);
            self.emit(AgentEvent::Status(status_error(
                session_id,
                "No Gemini API key — add it in Settings",
            )));
            if generation == self.generation.load(Ordering::SeqCst) {
                self.hide_overlay_later(2_400);
            }
            self.db.release(session_id);
            self.active.store(false, Ordering::SeqCst);
            return DrainResult::Failed("Gemini is not configured".into());
        }
        self.drain_with_engine(session_id, policy, speak, depth, job_id, &engine)
    }

    fn drain_with_engine(
        &self,
        session_id: &str,
        policy: RunPolicy,
        speak: bool,
        depth: u8,
        job_id: Option<&str>,
        engine: &dyn super::gemini::ChatEngine,
    ) -> DrainResult {
        self.active.store(true, Ordering::SeqCst);
        if speak {
            self.voice_active.store(true, Ordering::SeqCst);
        }
        self.stop_requested.store(false, Ordering::SeqCst);
        let run_generation = self.generation.load(Ordering::SeqCst);
        let settings = crate::core::settings::load();
        let workspace = workspace_dir(&settings);
        let model = engine.model().to_string();
        let tool_client = GeminiAgentClient::new(
            &settings.gemini_api_key,
            &model,
            &settings.llm_system_prompt,
        );

        let run_id = self.db.start_run(session_id, &model);
        self.db.event(
            Some(session_id),
            Some(&run_id),
            "run_started",
            &json!({"model": model, "deepWork": policy.deep_work}),
        );
        let interactive = job_id.is_none() && depth == 0;
        let mut run_usage = Usage::default();
        let mut status = AgentStatus::new(session_id);
        status.deep_work = policy.deep_work;
        status.generation = if depth == 0 { run_generation } else { u64::MAX };
        let cancel = Arc::new(AtomicBool::new(false));
        if depth == 0 {
            *self.cancel.lock() = Some(cancel.clone());
        }
        let mut speech = if speak {
            let tts = crate::core::gemini::GeminiClient::new(
                &settings.gemini_api_key,
                &model,
                &settings.llm_system_prompt,
                &settings.tts_model,
                &settings.tts_voice,
            );
            SpeechPipeline::start(tts, self.sounds.clone(), cancel.clone())
        } else {
            None
        };
        let mut segmenter = Segmenter::new();
        let mut reply = String::new();
        let mut sources: Vec<String> = Vec::new();
        let mut step: u32 = 0;
        let mut needs_continue = true;
        let mut last_emit = Instant::now();
        let mut consecutive_empty = 0u32;
        let mut last_call_key: Option<String> = None;
        let mut attempt_no = 0u32;
        let mut last_step = u32::MAX;
        let mut continues = 0u32;
        let mut denied = false;
        let mut max_steps = policy.max_steps;
        let mut extended = false;
        status.phase = "thinking".into();
        self.emit(AgentEvent::Status(status.clone()));

        loop {
            // Promote steering at every step boundary.
            for (id, _) in self.db.pending(session_id, Some("steer")) {
                self.db.deliver(&id);
            }
            if !self.db.has_pending(session_id, "steer") && !needs_continue {
                if let Some((id, _)) = self.db.pending(session_id, Some("queue")).first() {
                    let id = id.clone();
                    self.db.deliver(&id);
                    needs_continue = true;
                } else {
                    break;
                }
            }
            if step >= max_steps {
                let plan = self.db.plan(session_id);
                let plan_pending = plan
                    .iter()
                    .any(|p| p.status == "pending" || p.status == "in_progress");
                let plan_progress = plan.iter().any(|p| p.status == "completed");
                if policy.deep_work
                    && !extended
                    && plan_pending
                    && plan_progress
                    && max_steps < policy.hard_cap()
                {
                    max_steps = (max_steps + 20).min(policy.hard_cap());
                    extended = true;
                    status.detail = "Extending the step budget…".into();
                    self.emit(AgentEvent::Status(status.clone()));
                    continue;
                }
                break;
            }
            if last_step != step {
                attempt_no = 0;
                last_step = step;
            }

            if budget::needs_compaction(&self.db, session_id, settings.agent_context_budget) {
                status.phase = "thinking".into();
                status.detail = "Compacting conversation…".into();
                self.emit(AgentEvent::Status(status.clone()));
                self.compact(session_id, engine);
            }

            let messages = self.provider_messages(session_id);
            if messages.is_empty() {
                break;
            }
            if let Some(last_user) = messages.iter().rev().find(|m| m.role == "user") {
                status.transcript = last_user.text();
            }
            let checkpoint = self.db.latest_checkpoint(session_id);
            let summary = checkpoint.as_ref().map(|c| c.summary.as_str());
            let memory = prompt::memory_block(&self.db, 40);
            let plan = self.db.plan(session_id);
            let skills = tools::skill_catalog(&workspace);
            let mut catalog = tools::catalog(
                &workspace,
                self.db.clone(),
                tool_client.clone(),
                self.desktop.clone(),
                session_id,
                &run_id,
                depth,
                policy.deep_work,
                cancel.clone(),
            );
            if let Some(weak) = self.weak.get().cloned() {
                replace_task_tool(&mut catalog, weak, session_id.to_string(), depth);
            }
            if depth > 0 {
                // Child agents are read-only by default. Workspace writes stay
                // with the parent, which owns the user-facing permission gate.
                catalog.retain(|tool| {
                    !matches!(
                        tool.spec.action.as_str(),
                        "file.write"
                            | "file.delete"
                            | "desktop.paste"
                            | "desktop.open"
                            | "memory.forget"
                            | "skill.write"
                            | "agent.task"
                    )
                });
            }
            let specs: Vec<ToolSpec> = catalog.iter().map(|t| t.spec.clone()).collect();
            let built = prompt::build(prompt::PromptInput {
                base_prompt: &settings.llm_system_prompt,
                tools: &specs,
                messages: &messages,
                memory: &memory,
                plan: &plan,
                skills: &skills,
                deep_work: policy.deep_work,
                summary,
            });
            let terminal = step + 1 >= max_steps;
            let step_row = self.db.insert_run_step(
                &run_id,
                session_id,
                step,
                attempt_no,
                &model,
                if settings.agent_log_everything {
                    Some(built.system.as_str())
                } else {
                    None
                },
                &built.prompt_hash,
                &built.prefix_hash,
                built.contents.len(),
                &specs.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            );
            let assistant =
                self.db
                    .append_message(session_id, "assistant", "assistant", &[], Some(&model));
            status.phase = "thinking".into();
            status.detail = if terminal {
                "Wrapping up…".into()
            } else {
                String::new()
            };
            status.tools.clear();
            status.plan = plan.clone();
            self.emit(AgentEvent::Status(status.clone()));

            let mut step_text = String::new();
            let mut calls: Vec<(String, String, Value, Option<String>)> = Vec::new();
            let mut usage = Usage::default();
            let mut finish_reason = FinishReason::Stop;
            let mut stream_error: Option<GeminiFailure> = None;
            let mut output_started = false;
            let started = Instant::now();
            let mut first_text_ms = 0u64;
            let attempt_result = engine.stream_step(
                &built.contents,
                &built.tools,
                terminal,
                policy.output_cap,
                &cancel,
                &mut |event| {
                    match event {
                        LlmEvent::TextDelta(delta) => {
                            if !output_started {
                                output_started = true;
                                first_text_ms = started.elapsed().as_millis() as u64;
                            }
                            reply.push_str(&delta);
                            step_text.push_str(&delta);
                            for segment in segmenter.push(&delta) {
                                if let Some(pipeline) = speech.as_ref() {
                                    pipeline.push(segment);
                                    if pipeline.started.load(Ordering::SeqCst) {
                                        self.speaking.store(true, Ordering::SeqCst);
                                        status.phase = "speaking".into();
                                    }
                                }
                            }
                            if last_emit.elapsed().as_millis() > 90 {
                                last_emit = Instant::now();
                                status.phase = if self.speaking.load(Ordering::SeqCst) {
                                    "speaking".into()
                                } else {
                                    "thinking".into()
                                };
                                status.reply = reply.clone();
                                self.emit(AgentEvent::Status(status.clone()));
                            }
                        }
                        LlmEvent::ToolCall {
                            name,
                            input,
                            call_id,
                            thought_signature,
                        } => {
                            let call_id = call_id
                                .filter(|id| !id.is_empty())
                                .unwrap_or_else(|| format!("call_{}", calls.len()));
                            calls.push((call_id, name, input, thought_signature));
                        }
                        LlmEvent::Finish { reason, usage: u } => {
                            usage = u;
                            finish_reason = reason;
                        }
                        LlmEvent::Error { message, .. } => {
                            stream_error = Some(GeminiFailure {
                                message,
                                retryable: true,
                            });
                        }
                    }
                    !cancel.load(Ordering::SeqCst)
                },
            );
            let duration = started.elapsed().as_millis() as u64;
            run_usage.input_tokens += usage.input_tokens;
            run_usage.output_tokens += usage.output_tokens;
            run_usage.cache_read += usage.cache_read;
            run_usage.cache_write += usage.cache_write;
            self.db
                .usage(session_id, &run_id, step, "chat", &model, &usage, duration);
            let failure = stream_error.or_else(|| attempt_result.err());

            if let Some(error) = failure {
                let cancelled = cancel.load(Ordering::SeqCst);
                let message = if cancelled {
                    "interrupted".to_string()
                } else {
                    error.message.clone()
                };
                self.db.complete_message(
                    &assistant.id,
                    &[ContentPart::text(step_text.clone())],
                    &usage,
                    Some(&message),
                );
                self.db.finish_run_step(
                    &step_row,
                    Some("error"),
                    output_started,
                    &usage,
                    duration,
                    first_text_ms,
                    Some(&message),
                );
                if cancelled {
                    status.phase = "cancelled".into();
                    status.reply = reply.clone();
                    if run_generation == self.generation.load(Ordering::SeqCst) {
                        self.emit(AgentEvent::Status(status.clone()));
                    }
                    self.finish_run(&run_id, "interrupted", Some("cancelled"), step, &run_usage);
                    break;
                }
                if gemini_mod::is_context_overflow(&error.message) {
                    if attempt_no < 1 {
                        self.compact(session_id, engine);
                        self.db
                            .settle_message_error(&assistant.id, "context overflow");
                        attempt_no += 1;
                        continue;
                    }
                    self.db
                        .settle_message_error(&assistant.id, "context overflow");
                } else if error.retryable && !output_started && attempt_no < 2 {
                    // Transparent retry: same step, a new attempt row and a new
                    // assistant message. Nothing was shown to the user yet.
                    self.db.settle_message_error(&assistant.id, &message);
                    attempt_no += 1;
                    continue;
                } else if error.retryable && output_started && continues < 2 {
                    // The user already saw/heard output; never repeat it.
                    self.db.settle_message_error(&assistant.id, &message);
                    self.db.append_message(
                        session_id,
                        "user",
                        "synthetic",
                        &[ContentPart::text(
                            "The previous response was interrupted. Continue from where you left off without repeating completed content.",
                        )],
                        None,
                    );
                    continues += 1;
                    needs_continue = true;
                    step += 1;
                    continue;
                }
                status.phase = "error".into();
                status.error = Some(error.message.clone());
                status.reply = reply.clone();
                self.emit(AgentEvent::Status(status.clone()));
                self.finish_run(&run_id, "failed", Some(&error.message), step, &run_usage);
                if let Some(mut pipeline) = speech.take() {
                    pipeline.finish();
                }
                *self.cancel.lock() = None;
                self.speaking.store(false, Ordering::SeqCst);
                self.db.release(session_id);
                self.active.store(false, Ordering::SeqCst);
                if speak {
                    self.voice_active.store(false, Ordering::SeqCst);
                }
                return DrainResult::Failed(error.message);
            }

            let mut parts = Vec::new();
            if !step_text.is_empty() {
                parts.push(ContentPart::text(step_text.clone()));
            }
            for (call_id, name, input, thought_signature) in calls.iter() {
                parts.push(ContentPart::ToolCall {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    thought_signature: thought_signature.clone(),
                });
            }
            self.db
                .complete_message(&assistant.id, &parts, &usage, None);
            let finish_label = format!("{finish_reason:?}");
            self.db.finish_run_step(
                &step_row,
                Some(&finish_label),
                output_started,
                &usage,
                duration,
                first_text_ms,
                None,
            );
            self.db.event(
                Some(session_id),
                Some(&run_id),
                "step",
                &json!({"step": step, "tools": calls.len(), "usage": usage}),
            );

            if calls.is_empty() {
                consecutive_empty += 1;
                let plan_pending = self
                    .db
                    .plan(session_id)
                    .iter()
                    .any(|p| p.status == "pending" || p.status == "in_progress");
                if terminal {
                    break;
                }
                if plan_pending && consecutive_empty < 3 {
                    self.db.append_message(
                        session_id,
                        "user",
                        "synthetic",
                        &[ContentPart::text(
                            "Continue with the next pending plan item. Do not recap.",
                        )],
                        None,
                    );
                    needs_continue = true;
                    step += 1;
                    continue;
                }
                if consecutive_empty == 2 && finish_reason == FinishReason::Length {
                    needs_continue = true;
                    step += 1;
                    continue;
                }
                needs_continue = false;
                step += 1;
                continue;
            }

            consecutive_empty = 0;
            if terminal {
                self.db.append_message(
                    session_id,
                    "user",
                    "synthetic",
                    &[ContentPart::text(
                        "Tools are disabled until the next user input. Summarize what you completed, what remains, and any artifact paths.",
                    )],
                    None,
                );
            }
            // Execute the batch. Permission prompts happen first, in order.
            let mut responses = Vec::new();
            let mut deny_all = false;
            for (call_id, name, input, _thought_signature) in calls.iter() {
                if deny_all {
                    break;
                }
                let call_id = call_id.clone();
                let spec = catalog.iter().find(|t| t.spec.name == *name).cloned();
                let Some(definition) = spec else {
                    responses.push((
                        call_id,
                        name.clone(),
                        format!("Unknown tool `{name}`. It was not advertised this step."),
                        true,
                    ));
                    continue;
                };
                if terminal {
                    responses.push((
                        call_id,
                        name.clone(),
                        "Tools are disabled on this final step. Summarize instead.".into(),
                        true,
                    ));
                    continue;
                }
                let repaired = validation::repair(&definition.spec.input_schema, input.clone());
                let issues = validation::validate(&definition.spec.input_schema, &repaired);
                if !issues.is_empty() {
                    responses.push((
                        call_id,
                        name.clone(),
                        validation::error_text(name, &issues, &repaired),
                        true,
                    ));
                    continue;
                }
                let resource = permissions::resource_for(&definition.spec.action, name, &repaired);
                let decision = self.ask_permission(
                    session_id,
                    &definition.spec.action,
                    &resource,
                    name,
                    &repaired,
                    interactive,
                );
                match decision {
                    Decision::Deny => {
                        deny_all = true;
                        denied = true;
                        status.phase = "cancelled".into();
                        status.detail = "Declined by user".into();
                        self.emit(AgentEvent::Status(status.clone()));
                        break;
                    }
                    Decision::DenyWithReason(reason) => {
                        responses.push((
                            call_id,
                            name.clone(),
                            format!("The user declined: {reason}"),
                            true,
                        ));
                        continue;
                    }
                    _ => {}
                }
                let key = if definition.spec.read_only {
                    None
                } else {
                    Some(prompt::hash(&format!(
                        "{name}{}",
                        serde_json::to_string(&repaired).unwrap_or_default()
                    )))
                };
                if let Some(key) = key.as_deref() {
                    if let Some(previous) = self.db.tool_run_by_key(key) {
                        if previous.status == "succeeded" {
                            responses.push((
                                call_id,
                                name.clone(),
                                previous
                                    .output
                                    .unwrap_or_else(|| "Already completed.".into()),
                                false,
                            ));
                            continue;
                        }
                    }
                }
                let tool_run_id = self.db.insert_tool_run(
                    session_id,
                    &run_id,
                    &assistant.id,
                    step,
                    &call_id,
                    name,
                    &repaired,
                    key.as_deref(),
                    definition.spec.read_only,
                );
                self.db.mark_tool_running(&tool_run_id);
                status.phase = "tool".into();
                status.detail = format!("{name}…");
                status.tools.push(name.clone());
                self.emit(AgentEvent::Status(status.clone()));
                let mut ctx = ToolContext {
                    session_id: session_id.to_string(),
                    run_id: run_id.clone(),
                    tool_run_id: tool_run_id.clone(),
                    workspace: workspace.clone(),
                    db: self.db.clone(),
                    gemini: tool_client.clone(),
                    desktop: self.desktop.clone(),
                    cancel: cancel.clone(),
                    depth,
                };
                ctx.tool_run_id = tool_run_id.clone();
                let handler = definition.handler.clone();
                let execute = std::thread::scope(|scope| {
                    let handle = scope.spawn(|| handler(&ctx, repaired.clone()));
                    handle.join()
                });
                let (outcome, is_error) = match execute {
                    Ok(Ok(result)) => (result, false),
                    Ok(Err(error)) => (ToolResult::text(error.message), true),
                    Err(_) => (ToolResult::text("tool panicked"), true),
                };
                let (bounded, spill) = bound_output(&run_id, &call_id, &outcome.text);
                self.db.settle_tool_run(
                    &tool_run_id,
                    if is_error { "failed" } else { "succeeded" },
                    Some(&bounded),
                    spill.as_deref(),
                    if is_error { Some(&bounded) } else { None },
                );
                self.db.event(
                    Some(session_id),
                    Some(&run_id),
                    "tool",
                    &json!({"name": name, "status": if is_error {"failed"} else {"succeeded"}}),
                );
                if let Some(data) = &outcome.data {
                    if let Some(list) = data.get("sources").and_then(|v| v.as_array()) {
                        for source in list.iter().filter_map(|v| v.as_str()) {
                            if !sources.contains(&source.to_string()) {
                                sources.push(source.to_string());
                            }
                        }
                    }
                }
                let mut text = bounded;
                let repeat_key = prompt::hash(&format!(
                    "{name}{}",
                    serde_json::to_string(&repaired).unwrap_or_default()
                ));
                if last_call_key.as_deref() == Some(&repeat_key) {
                    text = format!(
                        "You already called {name} with these arguments. Do not repeat it; change the approach or explain why it is needed.\n\n{text}"
                    );
                }
                last_call_key = Some(repeat_key);
                responses.push((call_id, name.clone(), text, is_error));
            }

            if deny_all {
                // A plain decline is not model-visible: drop the tool-call parts
                // so the next request does not contain an unanswered call.
                self.db.complete_message(
                    &assistant.id,
                    &[ContentPart::text(step_text.clone())],
                    &usage,
                    Some("declined by user"),
                );
                self.db.append_message(
                    session_id,
                    "assistant",
                    "notice",
                    &[ContentPart::text("Stopped at your request.")],
                    None,
                );
                status.phase = "cancelled".into();
                self.emit(AgentEvent::Status(status.clone()));
                break;
            }

            if !responses.is_empty() {
                let parts: Vec<ContentPart> = responses
                    .iter()
                    .map(|(call_id, name, output, is_error)| {
                        tools::response_part(call_id, name, output, *is_error)
                    })
                    .collect();
                self.db
                    .append_message(session_id, "tool", "tool", &parts, None);
            }

            needs_continue = true;
            step += 1;
            if let Some(id) = job_id {
                self.db
                    .job_progress(id, (step as f64 / max_steps as f64).min(0.95));
            }
            status.phase = "thinking".into();
            status.detail.clear();
            self.emit(AgentEvent::Status(status.clone()));
        }

        if let Some(rest) = segmenter.flush() {
            if let Some(pipeline) = speech.as_ref() {
                pipeline.push(rest);
            }
        }
        if let Some(mut pipeline) = speech.take() {
            pipeline.finish();
            if speak && !cancel.load(Ordering::SeqCst) {
                self.sounds.play(crate::core::sounds::Chime::PasteDone);
            }
            if let Some(error) = pipeline.failed.lock().clone() {
                status.error = Some(format!("Speech playback failed: {error}"));
            }
        }
        self.speaking.store(false, Ordering::SeqCst);
        let was_cancelled = cancel.load(Ordering::SeqCst);
        if depth == 0 {
            *self.cancel.lock() = None;
        }
        if let Some(id) = job_id {
            self.db.job_progress(id, 1.0);
        }

        let interrupted = self.stop_requested.load(Ordering::SeqCst) || was_cancelled || denied;
        let state = if interrupted {
            "interrupted"
        } else if status.error.is_some() {
            "failed"
        } else {
            "succeeded"
        };
        self.finish_run(&run_id, state, None, step, &run_usage);
        self.db.event(
            Some(session_id),
            Some(&run_id),
            "run_finished",
            &json!({"status": state, "steps": step}),
        );
        self.db.commit_snapshots(&run_id);
        self.db.release(session_id);
        self.active.store(false, Ordering::SeqCst);
        if speak {
            self.voice_active.store(false, Ordering::SeqCst);
        }
        if interrupted {
            if depth == 0 {
                status.phase = "cancelled".into();
                status.reply = reply.clone();
                status.plan = self.db.plan(session_id);
                if run_generation == self.generation.load(Ordering::SeqCst) {
                    self.emit(AgentEvent::Status(status));
                    self.hide_overlay_later(700);
                }
            }
        } else {
            let delay = if status.error.is_some() { 3_500 } else { 900 };
            if speak && status.error.is_some() {
                self.sounds.play(crate::core::sounds::Chime::Error);
            }
            status.phase = "done".into();
            status.reply = reply.clone();
            status.sources = sources;
            status.plan = self.db.plan(session_id);
            status.artifacts = self.db.artifacts(session_id);
            if depth == 0 && run_generation == self.generation.load(Ordering::SeqCst) {
                self.emit(AgentEvent::Status(status));
                self.hide_overlay_later(delay);
            }
        }
        self.emit(AgentEvent::SessionChanged);
        self.emit(AgentEvent::RunsChanged);
        if interrupted {
            DrainResult::Interrupted
        } else {
            DrainResult::Succeeded(reply)
        }
    }

    fn finish_run(
        &self,
        run_id: &str,
        status: &str,
        reason: Option<&str>,
        steps: u32,
        usage: &Usage,
    ) {
        self.db
            .finish_run(run_id, status, reason, steps, usage, 0.0);
    }

    fn provider_messages(&self, session_id: &str) -> Vec<Message> {
        let checkpoint = self.db.latest_checkpoint(session_id);
        let through = checkpoint.map(|c| c.through_seq).unwrap_or(0);
        self.db
            .messages(session_id)
            .into_iter()
            .filter(|m| m.seq > through && m.kind != "compaction" && m.completed_at.is_some())
            .collect()
    }

    fn compact(&self, session_id: &str, engine: &dyn super::gemini::ChatEngine) {
        let messages = self.db.messages(session_id);
        let Some(split) = budget::split_point(&messages) else {
            return;
        };
        if split == 0 {
            return;
        }
        let previous = self.db.latest_checkpoint(session_id);
        let from_seq = previous.as_ref().map(|c| c.through_seq).unwrap_or(0);
        let to_summarize: Vec<Message> = messages
            .iter()
            .filter(|m| m.seq > from_seq && m.seq < messages[split].seq && m.kind != "compaction")
            .cloned()
            .collect();
        if to_summarize.is_empty() {
            return;
        }
        let prompt_text = budget::summary_prompt(
            previous.as_ref().map(|c| c.summary.as_str()),
            &to_summarize,
            600,
        );
        let summary = match engine.summarize(&prompt_text) {
            Ok(summary) if !summary.trim().is_empty() => summary,
            _ => {
                // Deterministic fallback so a failed summary cannot wedge the run.
                format!(
                    "Earlier work: {} messages. The newest exchange is kept verbatim.",
                    to_summarize.len()
                )
            }
        };
        let through = messages[split - 1].seq;
        self.db.save_checkpoint(session_id, through, &summary, 0);
        self.db.append_message(
            session_id,
            "system",
            "compaction",
            &[ContentPart::text(format!(
                "<conversation-summary>\n{summary}\n</conversation-summary>"
            ))],
            None,
        );
        self.db.event(
            Some(session_id),
            None,
            "compaction",
            &json!({"throughSeq": through}),
        );
    }

    // ------------------------------------------------------------- permission

    fn ask_permission(
        &self,
        session_id: &str,
        action: &str,
        resource: &str,
        tool: &str,
        input: &Value,
        interactive: bool,
    ) -> Decision {
        let rules = self.db.permission_rules(session_id);
        match permissions::evaluate(action, resource, &rules) {
            Effect::Allow => return Decision::AllowOnce,
            Effect::Deny => return Decision::Deny,
            Effect::Ask => {}
        }
        if !interactive {
            // Background work and child agents have no human to ask.
            return Decision::Deny;
        }
        let id = new_id("ask");
        let (title, detail) = permissions::prompt(tool, action, resource, input);
        let request = PermissionRequest {
            id: id.clone(),
            session_id: session_id.to_string(),
            action: action.to_string(),
            resource: resource.to_string(),
            title,
            detail,
            allow_always: permissions::always_resource(action, resource).is_some(),
        };
        let (tx, rx) = mpsc::channel();
        self.permissions.lock().insert(id.clone(), tx);
        *self.pending_permission.lock() = Some(request.clone());
        self.show_overlay();
        self.db.event(
            Some(session_id),
            None,
            "permission_asked",
            &json!({"id": id, "action": action, "resource": resource}),
        );
        self.emit(AgentEvent::Permission(request));
        let decision = rx
            .recv_timeout(PERMISSION_TIMEOUT)
            .unwrap_or(Decision::Deny);
        self.permissions.lock().remove(&id);
        let clear_pending = {
            let guard = self.pending_permission.lock();
            guard.as_ref().is_some_and(|request| request.id == id)
        };
        if clear_pending {
            *self.pending_permission.lock() = None;
        }
        self.db.event(
            Some(session_id),
            None,
            "permission_replied",
            &json!({"id": id, "decision": format!("{decision:?}")}),
        );
        self.emit(AgentEvent::PermissionClosed(id));
        if let Decision::AlwaysAllow = decision {
            if let Some(pattern) = permissions::always_resource(action, resource) {
                self.db
                    .add_permission(action, &pattern, "allow", session_id);
            }
        }
        decision
    }

    pub fn respond_permission(&self, id: &str, decision: Decision) -> bool {
        if let Some(tx) = self.permissions.lock().remove(id) {
            let _ = tx.send(decision);
            true
        } else {
            false
        }
    }

    pub fn pending_permission(&self) -> Option<PermissionRequest> {
        self.pending_permission.lock().clone()
    }

    fn show_overlay(&self) {
        let Some(app) = self.app.get() else { return };
        if let Some(state) = app.try_state::<Arc<crate::app::AppState>>() {
            crate::app::show_llm_overlay(app, state.inner());
        }
    }

    // -------------------------------------------------------------- subagents

    fn run_child(&self, parent: &str, prompt: &str, pack: &str) -> Result<String, String> {
        let settings = crate::core::settings::load();
        let title = format!("Subtask: {}", crate::core::sessions::title_from(prompt));
        let child = self
            .db
            .create_session(&title, Some(parent), &settings.llm_model);
        self.db.admit(
            &child,
            "user",
            "steer",
            None,
            &json!({"text": prompt, "deepWork": false}),
        );
        if self
            .db
            .claim(&child, &format!("sub-{}", std::process::id()))
        {
            let mut policy = RunPolicy::child();
            if pack == "workspace" {
                policy.output_cap = 4_096;
            }
            let result = self.drain_session(&child, policy, false, 1, None);
            self.active.store(true, Ordering::SeqCst);
            let artifacts = self.db.artifacts(&child);
            let mut text = match result {
                DrainResult::Succeeded(text) => text,
                DrainResult::Interrupted => "Subtask interrupted.".into(),
                DrainResult::Failed(error) => return Err(error),
            };
            if !artifacts.is_empty() {
                text.push_str("\n\nArtifact paths:\n");
                for artifact in artifacts {
                    text.push_str(&format!("- {}\n", artifact.path));
                }
            }
            Ok(text)
        } else {
            Err("Could not start the child agent.".into())
        }
    }

    // ---------------------------------------------------------------- recovery

    pub fn recover(&self) {
        self.db.recover_jobs();
        for session in self.db.stale_sessions() {
            let stale = self.db.settle_stale_tools(&session);
            for message in self.db.messages(&session) {
                if message.role == "assistant" && message.completed_at.is_none() {
                    self.db
                        .settle_message_error(&message.id, "interrupted by app restart");
                }
            }
            let attempts = self.db.bump_resume(&session);
            if attempts <= 3 {
                let note = if stale.is_empty() {
                    "The app restarted while you were working. Continue from where you left off without repeating completed work.".to_string()
                } else {
                    format!(
                        "The app restarted while you were working. These tool calls were interrupted: {}. Continue from where you left off without repeating completed work.",
                        stale
                            .iter()
                            .map(|(name, status)| format!("{name} ({status})"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                self.db.admit(
                    &session,
                    "synthetic",
                    "steer",
                    None,
                    &json!({"text": note, "silent": true}),
                );
            } else {
                self.db.append_message(
                    &session,
                    "assistant",
                    "idle",
                    &[ContentPart::text(
                        "Work could not be resumed after several restarts. Please ask again.",
                    )],
                    None,
                );
            }
            self.db.release(&session);
        }
        self.wake();
    }

    // ------------------------------------------------------------------- undo

    pub fn undo_run(&self, run_id: &str) -> Result<usize, String> {
        let settings = crate::core::settings::load();
        let workspace = workspace_dir(&settings);
        let snapshots = self.db.snapshots_for_run(run_id);
        let mut restored = 0;
        let mut skipped = 0;
        for snapshot in snapshots {
            if snapshot.status == "undone" {
                continue;
            }
            if snapshot.before_blob.as_deref() == Some(tools::SNAPSHOT_TOO_LARGE) {
                skipped += 1;
                continue;
            }
            let path = tools::resolve_path(&workspace, &snapshot.path).map_err(|e| e.message)?;
            if let Some(before) = &snapshot.before_blob {
                std::fs::write(&path, before).map_err(|e| e.to_string())?;
            } else if snapshot.before_hash.is_none() {
                let _ = std::fs::remove_file(&path);
            }
            self.db.mark_snapshot_undone(&snapshot.id);
            restored += 1;
        }
        self.emit(AgentEvent::RunsChanged);
        if skipped > 0 {
            return Err(format!(
                "Restored {restored} file(s); skipped {skipped} larger than the 1 MiB snapshot cap."
            ));
        }
        Ok(restored)
    }

    // -------------------------------------------------------------- read models

    pub fn sessions(&self, limit: usize) -> Vec<SessionSummary> {
        let active = self.db.active_session();
        self.db
            .sessions(limit)
            .into_iter()
            .map(|session| {
                let turns = self
                    .db
                    .messages(&session.id)
                    .iter()
                    .filter(|m| {
                        (m.role == "user" && m.kind == "user")
                            || (m.role == "assistant"
                                && (m.kind == "assistant" || m.kind == "notice"))
                    })
                    .count();
                SessionSummary {
                    turns,
                    active: active.as_deref() == Some(session.id.as_str()),
                    id: session.id.clone(),
                    title: display_title(&session),
                    updated_at: session.updated_at,
                    parent_id: session.parent_id,
                }
            })
            .collect()
    }

    pub fn session_detail(&self, id: &str) -> Option<Value> {
        let session = self.db.session(id)?;
        let active = self.db.active_session().as_deref() == Some(id);
        let messages = self.db.messages(id);
        let turns: Vec<TurnView> = messages
            .iter()
            .filter(|m| {
                (m.role == "user" && m.kind == "user")
                    || (m.role == "assistant" && (m.kind == "assistant" || m.kind == "notice"))
            })
            .map(|m| TurnView {
                role: m.role.clone(),
                text: {
                    let mut text = m.text();
                    if m.parts
                        .iter()
                        .any(|part| matches!(part, ContentPart::Image { .. }))
                    {
                        text.push_str("\n📷 Screenshot");
                    }
                    text
                },
                at: m.created_at,
            })
            .collect();
        Some(json!({
            "id": session.id,
            "title": display_title(&session),
            "createdAt": session.created_at,
            "updatedAt": session.updated_at,
            "active": active,
            "parentId": session.parent_id,
            "turns": turns,
            "plan": self.db.plan(id),
            "artifacts": self.db.artifacts(id),
            "toolRuns": self.db.tool_runs(id),
        }))
    }

    pub fn new_session(&self) {
        self.db.set_active_session(None);
        self.emit(AgentEvent::SessionChanged);
    }

    pub fn active_session_id(&self) -> Option<String> {
        self.db.active_session()
    }

    pub fn set_active(&self, id: &str) -> bool {
        if self.db.session(id).is_some() {
            self.db.set_active_session(Some(id));
            self.emit(AgentEvent::SessionChanged);
            true
        } else {
            false
        }
    }

    pub fn rename_session(&self, id: &str, title: &str) -> bool {
        let ok = self.db.rename_session(id, title);
        if ok {
            self.emit(AgentEvent::SessionChanged);
        }
        ok
    }

    pub fn delete_session(&self, id: &str) {
        self.db.delete_session(id);
        self.emit(AgentEvent::SessionChanged);
    }

    pub fn plan(&self, id: &str) -> Vec<PlanItem> {
        self.db.plan(id)
    }

    pub fn artifacts(&self, id: &str) -> Vec<ArtifactInfo> {
        self.db.artifacts(id)
    }

    pub fn runs(&self, limit: usize) -> Vec<RunInfo> {
        self.db.runs(limit)
    }

    pub fn run_detail(&self, run_id: &str) -> Value {
        let run = self.db.runs(500).into_iter().find(|r| r.id == run_id);
        let steps = self.db.run_steps(run_id);
        let events = self.db.events_for_run(run_id);
        let tools = self.db.tool_runs_for_run(run_id);
        json!({"run": run, "steps": steps, "events": events, "tools": tools})
    }

    pub fn permissions(&self) -> Vec<super::store::PermissionRow> {
        self.db.permissions()
    }

    pub fn revoke_permission(&self, id: &str) -> bool {
        self.db.revoke_permission(id)
    }

    pub fn jobs(&self, limit: usize) -> Vec<JobInfo> {
        self.db.jobs(limit)
    }

    pub fn cancel_job(&self, id: &str) -> bool {
        if self.db.cancel_job(id) {
            return true;
        }
        let running = self
            .db
            .jobs(200)
            .into_iter()
            .find(|job| job.id == id)
            .is_some_and(|job| job.status == "running");
        if running {
            self.stop();
        }
        running
    }

    pub fn metrics(&self) -> Value {
        self.db.metrics()
    }
}

pub enum DrainResult {
    Succeeded(String),
    Interrupted,
    Failed(String),
}

fn display_title(session: &SessionRow) -> String {
    if session.title.trim().is_empty() {
        crate::core::sessions::UNTITLED.to_string()
    } else {
        session.title.clone()
    }
}

fn workspace_dir(settings: &crate::core::settings::Settings) -> PathBuf {
    let path = if settings.workspace_root.trim().is_empty() {
        crate::app_data_dir().join("workspace")
    } else {
        PathBuf::from(settings.workspace_root.trim())
    };
    let _ = std::fs::create_dir_all(&path);
    path
}

fn status_error(session_id: &str, error: &str) -> AgentStatus {
    let mut status = AgentStatus::new(session_id);
    status.phase = "error".into();
    status.error = Some(error.to_string());
    status
}

fn replace_task_tool(
    catalog: &mut [ToolDefinition],
    service: Weak<AgentService>,
    parent: String,
    depth: u8,
) {
    for definition in catalog.iter_mut() {
        if tools::is_task_tool(&definition.spec.name) {
            let service = service.clone();
            let parent = parent.clone();
            definition.handler = Arc::new(move |_ctx, input| {
                let prompt = input
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolFailure::new("prompt is required"))?;
                let pack = input
                    .get("pack")
                    .and_then(|v| v.as_str())
                    .unwrap_or("read_only");
                let service = service
                    .upgrade()
                    .ok_or_else(|| ToolFailure::new("Agent service is no longer available"))?;
                let text = service
                    .run_child(&parent, prompt, pack)
                    .map_err(ToolFailure::new)?;
                let _ = depth;
                Ok(ToolResult::text(text))
            });
        }
    }
}

fn bound_output(run_id: &str, call_id: &str, text: &str) -> (String, Option<String>) {
    const MAX_CHARS: usize = 50_000;
    const MAX_LINES: usize = 2_000;
    let lines = text.lines().count();
    let chars = text.chars().count();
    if chars <= MAX_CHARS && lines <= MAX_LINES {
        return (text.to_string(), None);
    }
    let dir = crate::app_data_dir().join("tool_output").join(run_id);
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{call_id}.txt"));
    let _ = std::fs::write(&path, text);
    let kept: String = text.lines().take(MAX_LINES).collect::<Vec<_>>().join("\n");
    let kept = kept.chars().take(MAX_CHARS).collect::<String>();
    let note = format!(
        "\n\n[showing the first {} of {} lines; full output saved to {}]",
        kept.lines().count(),
        lines,
        path.display()
    );
    (
        format!("{kept}{note}"),
        Some(path.to_string_lossy().to_string()),
    )
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;
    use crate::core::agent::gemini::{ChatEngine, GeminiFailure};
    use crate::core::sounds::Sounds;
    use std::sync::Mutex as StdMutex;

    struct Scripted {
        steps: StdMutex<Vec<Vec<LlmEvent>>>,
    }

    impl ChatEngine for Scripted {
        fn configured(&self) -> bool {
            true
        }
        fn model(&self) -> &str {
            "scripted"
        }
        fn stream_step(
            &self,
            _contents: &[Value],
            _tools: &[Value],
            _terminal: bool,
            _max: u32,
            _cancel: &AtomicBool,
            on_event: &mut dyn FnMut(LlmEvent) -> bool,
        ) -> Result<(), GeminiFailure> {
            let events = self.steps.lock().unwrap().remove(0);
            for event in events {
                if !on_event(event) {
                    return Err(GeminiFailure {
                        message: "cancelled".into(),
                        retryable: false,
                    });
                }
            }
            Ok(())
        }
        fn summarize(&self, _text: &str) -> Result<String, GeminiFailure> {
            Ok("summary".into())
        }
    }

    fn service() -> (Arc<AgentService>, Arc<Db>) {
        let dir = std::env::temp_dir().join(format!("vk-agent-service-{}", new_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(dir.join("agent.db")).unwrap();
        let service = Arc::new(AgentService {
            db: db.clone(),
            desktop: Arc::new(super::super::desktop::SystemDesktop::new()),
            sounds: Arc::new(Sounds::new()),
            app: OnceLock::new(),
            tx: mpsc::channel().0,
            permissions: Mutex::new(HashMap::new()),
            cancel: Mutex::new(None),
            stop_requested: AtomicBool::new(false),
            active: AtomicBool::new(false),
            speaking: AtomicBool::new(false),
            voice_active: AtomicBool::new(false),
            weak: OnceLock::new(),
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pending_permission: Mutex::new(None),
        });
        let _ = service.weak.set(Arc::downgrade(&service));
        (service, db)
    }

    #[test]
    fn scripted_plan_then_answer_completes_the_run() {
        let (service, db) = service();
        let id = db.create_session("scripted", None, "scripted");
        db.admit(
            &id,
            "user",
            "steer",
            None,
            &serde_json::json!({"text": "make a plan and finish"}),
        );
        assert!(db.claim(&id, "test"));
        let engine = Scripted {
            steps: StdMutex::new(vec![
                vec![
                    LlmEvent::TextDelta("Planning.".into()),
                    LlmEvent::ToolCall {
                        name: "update_plan".into(),
                        input: serde_json::json!({"items":[{"title":"Do it","status":"completed"}]}),
                        call_id: None,
                        thought_signature: None,
                    },
                    LlmEvent::Finish {
                        reason: FinishReason::Stop,
                        usage: Usage::default(),
                    },
                ],
                vec![
                    LlmEvent::TextDelta("Done.".into()),
                    LlmEvent::Finish {
                        reason: FinishReason::Stop,
                        usage: Usage::default(),
                    },
                ],
            ]),
        };
        let result = service.drain_with_engine(&id, RunPolicy::normal(), false, 0, None, &engine);
        match result {
            DrainResult::Succeeded(text) => assert!(text.contains("Done.")),
            _ => panic!("unexpected drain result"),
        }
        assert_eq!(db.plan(&id).len(), 1);
        assert_eq!(db.plan(&id)[0].status, "completed");
        let runs = db.runs(10);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "succeeded");
        assert!(db.tool_runs(&id).iter().any(|t| t.name == "update_plan"));
        assert!(db.messages(&id).iter().any(|m| m.text().contains("Done.")));
    }

    #[test]
    fn context_overflow_compacts_once_and_retries() {
        let (service, db) = service();
        let id = db.create_session("scripted", None, "scripted");
        db.append_message(
            &id,
            "user",
            "user",
            &[ContentPart::text("earlier question")],
            None,
        );
        db.append_message(
            &id,
            "assistant",
            "assistant",
            &[ContentPart::text("earlier answer")],
            None,
        );
        db.admit(
            &id,
            "user",
            "steer",
            None,
            &serde_json::json!({"text": "hello"}),
        );
        assert!(db.claim(&id, "test"));
        // The first call fails with an overflow-shaped message; the retry succeeds.
        struct OverflowOnce {
            first: StdMutex<bool>,
        }
        impl ChatEngine for OverflowOnce {
            fn configured(&self) -> bool {
                true
            }
            fn model(&self) -> &str {
                "scripted"
            }
            fn stream_step(
                &self,
                _c: &[Value],
                _t: &[Value],
                _term: bool,
                _m: u32,
                _cancel: &AtomicBool,
                on_event: &mut dyn FnMut(LlmEvent) -> bool,
            ) -> Result<(), GeminiFailure> {
                let mut first = self.first.lock().unwrap();
                if *first {
                    *first = false;
                    return Err(GeminiFailure {
                        message: "context window exceeded".into(),
                        retryable: false,
                    });
                }
                let _ = on_event(LlmEvent::TextDelta("ok".into()));
                let _ = on_event(LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: Usage::default(),
                });
                Ok(())
            }
            fn summarize(&self, _t: &str) -> Result<String, GeminiFailure> {
                Ok("summary".into())
            }
        }
        let engine = OverflowOnce {
            first: StdMutex::new(true),
        };
        let _ = service.drain_with_engine(&id, RunPolicy::normal(), false, 0, None, &engine);
        assert!(db.latest_checkpoint(&id).is_some());
    }
}
