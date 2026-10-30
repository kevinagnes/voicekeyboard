# Agent core v2 → VoiceKeyboard

This document records the analysis of Personal Companion's `feat/agent-core-v2`
work and how its principles were ported into VoiceKeyboard's Ask flow. The
sandbox (Alpine/proot) was deliberately excluded, per the product decision; the
runtime does not depend on it.

## 1. What v2 changed

v2 was not a feature release. It replaced an in-memory `while (true)` chat loop
with a durable, step-oriented runtime. The load-bearing ideas, taken from
`docs/agent-core-v2-plan.md` and the seven ADRs, are:

1. **A step is one model call and one durable record.** Continuation is derived
   from stored tool runs + finish reason; there is no local loop state that a
   crash can lose.
2. **Durability precedes concurrency.** Admit the input, write the claim, write
   the tool row, then act. Recovery is at-least-once and effects are idempotent.
3. **Admission is not execution.** A durable inbox with `steer` (next step
   boundary) and `queue` (next idle), plus a single-flight executor and a
   doorbell instead of a second loop.
4. **The tool set is frozen before the request.** Typed schema validation with
   conservative repair, model-readable errors, bounded/spilled output, repeat
   detection, and a permission gate on the seam.
5. **The model never authors its own consent.** Deterministic prompt text from
   the actual arguments; allow-once / always / deny / deny-with-reason; a plain
   deny is not model-visible.
6. **Context is measured, estimated, then compacted.** Stable prompt prefix,
   checkpoint summary, verbatim newest exchange; never prune by message count.
7. **Plans, artifacts, and verification are durable state.** A plan drives
   continuation and budgets; artifacts are a registry; verification reads the
   files back.
8. **Retry, continue, compact, and fail are different decisions.** Retry before
   visible output; continue after it; context overflow compacts once.
9. **Persist facts, stream hints.** Deltas are UI-only; a restart rebuilds from
   SQLite. Runs, steps, tools, prompts, usage and events are inspectable.
10. **Voice is a first-class path.** Streaming text is segmented at sentence
    boundaries and spoken while the model is still generating; barge-in cancels
    TTS and the model call; time-to-first-audio is the metric.

Phases 20–28 in Personal Companion are applications of these laws. The sandbox
only supplied an adapter; removing it does not change the architecture.

## 2. Mapping to VoiceKeyboard

| v2 concept | VoiceKeyboard implementation |
|---|---|
| contracts | `src/core/agent/types.rs` |
| SQLite store/repositories | `src/core/agent/store.rs` (`agent.db`, WAL) |
| canonical model boundary | `src/core/agent/gemini.rs` |
| runner/executor | `src/core/agent/service.rs` |
| prompt parts | `src/core/agent/prompt.rs` |
| budget/compaction | `src/core/agent/budget.rs` + `Service::compact` |
| tool runtime | `src/core/agent/tools.rs`, `validation.rs` |
| permission gate | `src/core/agent/permissions.rs` + overlay sheet |
| streaming speech | `src/core/agent/speech.rs` |
| desktop port | `src/core/agent/desktop.rs` |
| commands/read models | `src/agent_commands.rs` |
| workspace UI | `ui/index.html`, `ui/app.js` (Agent tab) |
| voice overlay | `ui/llm.html`, `ui/llm.js` |

The existing product surface (hotkeys, capture, Whisper/Nemotron/gemini-3.5-transcribe
STT, Gemini TTS with noise-tail trimming, tray, sessions, updater) is kept. The old
one-shot `run_ask_llm` is now only transcription + admission; the agent worker
does the rest.

## 3. Durable model

One database at `<app data>/agent.db`:

- `sessions`, `messages` (per-session `seq`), `tool_runs`, `runs`, `run_steps`
- `inbox` (durable admission), `events` (audit), `checkpoints`
- `plans`, `artifacts`, `permissions`, `file_snapshots`
- `jobs`, `usage`, `memory`

`sessions.json` is imported once, idempotently, behind a `kv` marker. The
original file is left untouched.

The transcript is a projection: the overlay may render deltas, but the final
values come from rows, and the Sessions/Agent tabs rebuild from SQLite.

## 4. Runtime

```
voice → STT worker → admit (durable inbox) → wake
                                      ↓
                         agent worker (single-flight per session)
                         claim → settle stale → promote steer
                         → compact if due → frozen tool snapshot
                         → one streamed model call
                         → settle assistant + tool rows
                         → continuation from the record
                         → release
```

- **Tools:** `core` (time, plan, artifacts, verify), `workspace` (`read`,
  `write`, `edit`, `glob`, `grep`, `list_files`, `file_info`,
  `create_directory`, `delete_file`), `web` (`web_search`, `web_fetch`),
  `desktop` (clipboard, paste, open URL), `memory`/`skills` (`remember`,
  `recall`, `forget`, `list_skills`, `read_skill`, `write_skill`,
  `delete_skill`) and one-level `task` subagents in deep work.
- **Permissions:** built-in defaults (reads allow, writes/side effects ask),
  session-scoped saved grants, deterministic prompt text, deny semantics.
  Background jobs and child agents convert `ask` to `deny`.
- **Budgets:** 24 steps normal, 40 deep work (60 hard cap), 120k-token default
  context ceiling. Plan progress keeps a turn alive; two no-tool stalls stop it.
- **Recovery:** a surviving claim means the process died. Read-only tool rows
  are aborted; side-effect rows become `unknown`; the assistant row is marked
  interrupted; up to three synthetic resumes are admitted.
- **Undo:** `write`/`edit`/`delete` record before/after snapshots in run-scoped
  change sets. Undo applies newest-first and reports files above the 1 MiB cap
  instead of silently deleting them.

## 5. Voice and UI

- Text deltas stream into the overlay. A `Segmenter` speaks at `. ! ? ;` (or
  280 chars) and hands segments to Gemini TTS on a separate thread.
- When Gemini Transcribe is selected, `live_stt.rs` streams 16 kHz PCM chunks to
  `gemini-3.5-transcribe-live` while the hotkey is held. For Whisper/Nemotron,
  `local_stream.rs` cuts the audio at natural pauses (700 ms of silence) and
  transcribes each segment with the growing transcript as context, so only the
  tail remains at release. Both paths fall back to batch transcription on error.
- The Ask overlay is draggable by its whole surface (text blocks opt out so they can be
  clicked to expand the full session history) and remembers its physical position in
  `overlay_positions.json`. It shows only the current exchange until expanded.
- The screenshot hotkey opens a full-screen selection overlay; the selected region is
  captured with `screenshots`, stored under `<app data>/screenshots`, attached as an
  `Image` content part, and sent with the spoken question to the agent.
- Barge-in (Ask hotkey while active/speaking) cancels the model call and TTS,
  then admits the new utterance as a steer.
- The overlay shows the current plan item, tool activity, artifacts, sources and
  a permission sheet with allow-once / always / deny / deny-with-reason.
- The Settings **Agent** tab exposes the runtime budget/workspace, run
  inspector (steps, tools, events, undo), plan/artifacts, saved permissions,
  background jobs and metrics. The Sessions tab shows each session's plan,
  artifacts and tool timeline.

## 6. Testing

`cargo test --lib` covers the new contracts: store sequencing/admission/claims/
idempotency, permission matching, schema repair/validation, prompt prefix
stability, budget/split logic, speech segmentation, path guarding and the
workspace tool round-trip. A scripted-engine test drives the full runner through
plan → tool → answer and overflow → compact → retry; it runs on non-Windows hosts
because the Windows unit-test harness needs an extra comctl32 manifest that the
Tauri app binary receives automatically. `cargo check`, `cargo build` and
`node --check` pass for the app and UI.

## 7. Deliberate exclusions and follow-ups

- No sandbox; shell execution is not exposed (the user did not select it).
- Provider boundary is Gemini-only by decision.
- Tool calls in a batch are currently settled sequentially. The contract and
  result ordering already support parallel execution; this is a latency
  optimisation, not a correctness gap.
- Skills are listed/fetched by tools but not yet auto-selected by intent.
- Cost accounting is recorded as zero for Gemini (no price table); tokens are
  recorded per step.
- A folder-scoped "always allow" is offered as a parent-glob grant per session;
  global grants remain an explicit future decision.
