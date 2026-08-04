# Voice Keyboard

## Push-to-Talk Dictation App — Specification

*Status:* v1.0 — all decisions incorporated
*Date:* 2026-08-03
*Author:* Kevin (via OpenClaw-Cookie)
*Target hardware (dev):* Apple M1 Pro, 16 GB RAM
*Target OS:* macOS (v1) → Windows (v2); Linux optional (stretch)

---

## 1. Overview

A local, offline-first dictation utility: while any text input field is focused, the
user *presses and holds a hotkey* to record their voice. On *release*, the audio
is transcribed on-device by a speech-to-text model, and the resulting text is
*pasted into the currently focused input field* — instantly, ready to send.

No cloud calls. No audio leaves the machine.

### Core interaction loop

1.⁠ ⁠User focuses any text field (browser, IDE, chat app, terminal, etc.).
2.⁠ ⁠User presses and holds the hotkey (*Fn* by default; fully rebindable, combo-capable).
3.⁠ ⁠*Chime plays* → recording starts; menu bar / tray icon shows recording state.
4.⁠ ⁠User releases the key.
5.⁠ ⁠Audio is transcribed locally (target: ~0.65 s for 10 s of audio).
6.⁠ ⁠Text is *instantly pasted* into the focused field.
7.⁠ ⁠*Chime plays* → app returns to idle. Hotkey works globally, in any app.

---

## 2. Decisions (confirmed with Kevin, 2026-08-03)

| # | Decision |
|---|----------|
| D1 | Languages: *English + PT-BR*. Whisper auto-detects language; manual override in settings. |
| D2 | Platform order: *macOS first, then Windows*. Linux = stretch goal. |
| D3 | Priority: *fast and efficient*. Model/language defaults chosen by the implementer (see §5). |
| D4 | Hotkey: *fully rebindable; supports key combos. Default: **Fn* (hold-to-record). |
| D5 | Output: *clean, punctuated, capitalized* text — ready to send (Whisper native behavior). |
| D6 | Injection: *instant paste* (clipboard + Cmd/Ctrl+V) with clipboard save/restore. |
| D7 | Distribution: *macOS may ship* (Apple Developer account) → build signing/notarization-ready. Windows: nothing now, but architecture must not block it. |
| D8 | Sounds: *chime on recording start, **chime on paste* (mutable). |
| D9 | UI: macOS *menu bar* app (no dock icon); Windows *system tray* icon. |
| D10 | Code rules: best-practice security, *no file > 1000 lines, **medium complexity* (not high). |
| D11 | App name: *VoiceKeyboard* (menu bar title / app name). |
| D12 | Model: *first-run download* (robust: retries, resumable, integrity check); settings allow *model swapping* as more models are added. |

---

## 3. Functional Requirements

| ID | Requirement |
|----|-------------|
| F1 | Global hotkey (hold = record, release = transcribe); rebindable; no conflicts with focused app. |
| F2 | Microphone capture from key-down to key-up, 16 kHz mono (STT standard input). |
| F3 | Chime on record start; chime on successful paste. Optional mute. |
| F4 | Menu bar (macOS) / tray (Windows) status indicator while recording. |
| F5 | On release: run STT inference on captured audio. |
| F6 | Paste result into focused field (clipboard swap + paste hotkey; restore clipboard). |
| F7 | Ignore recordings < 300 ms (accidental taps). |
| F8 | Settings UI: hotkey (combo-capable), model selector (extensible registry), language (Auto/EN/PT-BR), sounds on/off, launch at login. |
| F9 | Model loaded once at startup (warm); language auto-detected per utterance. |
| F10 | macOS: signed & notarization-ready build (hardened runtime); no dock icon (LSUIElement). |
| F11 | Model downloader: first-run fetch with progress, retry/backoff, resume, checksum verification, cached. |

---

## 4. Performance Targets (M1 Pro, 16 GB)

Measured from hotkey *release* to text *pasted*:

| Metric | Target |
|--------|--------|
| P50 latency (10 s audio, default model) | ≤ 0.7 s |
| P95 latency (10 s audio, default model) | ≤ 1.0 s |
| Model warm-up at launch | ≤ 2 s (background) |
| Peak RAM (default model) | ≤ 2 GB |
| Recording overhead | 0 (captured in real time) |

### Estimated inference time — 10 s audio, M1 Pro (from published RTF benchmarks)

| Model | Params | Quant | Est. time (10 s) | RAM | Verdict |
|-------|--------|-------|------------------|-----|---------|
| Whisper large-v3-turbo | 809M | q8_0 | *~0.65 s* | ~1.7 GB | ✅ *DEFAULT* — EN + PT-BR, punctuated output, whisper.cpp = portable to Win |
| Whisper small | 244M | q5_0 | ~0.5–0.7 s | ~0.5 GB | Fallback for weak hardware |
| Moonshine 245M | 245M | — | ~0.1–0.2 s | ~0.5 GB | English-only → does not meet D1 |
| Parakeet TDT 0.6B | 600M | — | ~0.35 s | ~1.2 GB | Fast, but PT-BR support weaker + NeMo stack on Win = liability |
| Whisper large-v3 | 1.55B | q5_1 | ~2.5 s | ~3 GB | Too slow for this UX |

	⁠*Model rationale:* Whisper large-v3-turbo (q8) is the only pick that satisfies
	⁠all three hard constraints: EN + PT-BR accuracy, punctuated/clean output, and
	⁠≤ 0.7 s inference on M1 Pro — while running on a single portable C/C++ runtime
	⁠(whisper.cpp) that carries over to Windows unchanged. Language is auto-detected
	⁠per utterance (Whisper does this natively); settings allow forcing EN or PT-BR.

---

## 5. Model & Language

•⁠  ⁠*Default model:* ⁠ large-v3-turbo ⁠ (809M), quantized q8_0, via *whisper.cpp*.
•⁠  ⁠*Secondary (settings):* ⁠ small ⁠ (q5) for low-RAM/low-end machines.
•⁠  ⁠*Language:* ⁠ auto ⁠ (default) → detected per utterance; override: ⁠ en ⁠ / ⁠ pt ⁠.
•⁠  ⁠*Initial prompt:* optional hint like ⁠ "The transcript is a message to be sent as-is." ⁠
  to nudge clean, ready-to-send formatting (punctuation/capitalization).
•⁠  ⁠*Portability:* whisper.cpp ships a single binary with Metal (macOS), CUDA/Vulkan/CPU
  (Windows) — one codebase for D2.
•⁠  ⁠*Model registry:* a registry file lists available models (id, name, size, download URL,
  checksum). Settings reads the registry and lets the user swap the active model; adding
  future models = adding a registry entry + download handler, no core code changes (D12).

---

## 6. Text Injection (Instant Paste)

Flow on release:
1.⁠ ⁠Run STT inference → text.
2.⁠ ⁠Save current clipboard contents.
3.⁠ ⁠Set clipboard to transcript (plain text).
4.⁠ ⁠Send global paste keystroke (⁠ Cmd+V ⁠ / ⁠ Ctrl+V ⁠) to focused field.
5.⁠ ⁠After ~1 s, restore previous clipboard contents.

Rules:
•⁠  ⁠*Never paste into secure/password fields* — detect via accessibility APIs
  (macOS ⁠ AXUIElement ⁠ secure-field trait; Windows UIA ⁠ IsPassword ⁠) and **silently
  discard** the transcript: no paste, no chime, no notification.
•⁠  ⁠If accessibility permission is missing, show a guided setup screen.
•⁠  ⁠Sanitize transcript: strip leading/trailing whitespace; collapse duplicate blank lines.

---

## 7. Architecture


┌────────────────────────────────────────────────────────┐
│                      App process                        │
│                                                         │
│  Global hotkey listener (hold/release)                  │
│        │ key-down                          │ key-up     │
│        ▼                                   ▼            │
│  Audio capture ──────────────────────────► Audio buffer │
│  (16 kHz mono ring buffer)                 (16 kHz WAV) │
│        ▲                                               │
│   chime (start)                                         │
│                                               │         │
│                                               ▼         │
│  STT engine (whisper.cpp, warm at launch) ──► transcript│
│                                               │         │
│                                               ▼         │
│  Paste pipeline (clipboard save → set → Cmd/Ctrl+V →    │
│  restore) ─────────────────────────────────► focused    │
│                                               field     │
│                                               │         │
│                                          chime (done)   │
│                                                         │
│  Menu bar / tray UI • Settings • Status indicator      │
└────────────────────────────────────────────────────────┘


### Components (each ≤ 1000 lines, medium complexity)
•⁠  ⁠*hotkey.rs* — global hotkey registration, hold/release events, rebinding, conflict detection.
•⁠  ⁠*capture.rs* — mic input, ring buffer, WAV export, min-length gate (300 ms).
•⁠  ⁠*stt.rs* — whisper.cpp wrapper: model load at startup, queue, language auto-detect/override.
•⁠  ⁠*paste.rs* — clipboard save/restore, paste keystroke, secure-field guard, text sanitize.
•⁠  ⁠*ui (Tauri/tray)* — menu bar/tray icon, recording indicator, settings window, sounds.
•⁠  ⁠*sounds.rs* — chime playback (start / done), mute toggle.

---

## 8. Edge Cases & Risks

| Risk | Mitigation |
|------|-----------|
| Hotkey conflict with focused app | Default to a rarely-used key; conflict warning; full rebinding (D4). |
| Secure/password field focused | Detect → *silently skip* (no paste, no chime, no notification); transcript discarded. |
| Clipboard clobbering | Save → paste → restore after ~1 s. |
| Accidental tap (< 300 ms) | Discard recording. |
| Very long hold (> 60 s) | Keep recording; note inference scales with length; cap at 120 s with chime warning. |
| No mic permission | First-run permission prompt; clear error state in menu. |
| Missing accessibility permission (macOS) | Guided setup; paste silently disabled until granted. |
| Whisper hallucination on silence | Trim leading/trailing silence before inference. |
| Model warm-up on first press | Preload at app launch (F9). |
| Model download fails / interrupted | Retry with exponential backoff; resumable download; checksum verify; clear retry UI in settings (F11). |
| Recording while inference running | Single worker queue; block new record during inference (brief). |
| Keyboard repeat (OS auto-repeat) | Ignore repeat key-down events; only first press starts recording. |

---

## 9. Privacy & Security (best practice)

•⁠  ⁠100% local; zero network calls; no telemetry.
•⁠  ⁠Audio buffers deleted immediately after inference.
•⁠  ⁠Clipboard contents held only for the ~1 s restore window, in memory only.
•⁠  ⁠macOS: sandboxed where feasible, hardened runtime, notarization-ready (D7/D10).
•⁠  ⁠Minimal permissions: mic + accessibility + global hotkey only; no file access beyond app container.
•⁠  ⁠No secrets or hardcoded credentials; no third-party analytics.
•⁠  ⁠Code review checklist: input validation (audio length, settings), safe clipboard handling,
  no eval/dynamic code, pinned dependency versions.

---

## 10. Code Standards (D10)

•⁠  ⁠*File length:* hard cap 1000 lines per file; split modules instead.
•⁠  ⁠*Complexity:* medium — clear modules, one responsibility each, no clever metaprogramming.
•⁠  ⁠*Security:* OWASP-lite review; least-privilege permissions; sanitize all injected text.
•⁠  ⁠*Style:* rustfmt/clippy clean (or language-equivalent), documented public APIs.
•⁠  ⁠*Testing:* unit tests for paste pipeline + audio trimming; manual smoke matrix per OS.

---

## 11. Tech Stack

### Option A — Rust + Tauri (recommended)
•⁠  ⁠Hotkeys: ⁠ rwh ⁠ (global, cross-platform) • Audio: ⁠ cpal ⁠ • STT: ⁠ whisper-rs ⁠
  (whisper.cpp bindings) • Paste: ⁠ enigo ⁠ + ⁠ arboard ⁠ (clipboard) • UI: Tauri + tray.
•⁠  ⁠One codebase → macOS now, Windows later (D2). Small binary (~10–20 MB + model).
•⁠  ⁠Complexity: medium. ✅

### Option B — Swift native (macOS-only)
•⁠  ⁠Cleanest menu-bar UX, best macOS integration, easy notarization; but a separate
  Windows codebase later. Only if macOS-only ever becomes the plan.

*Chosen:* Option A. Model (~1.6 GB q8) is not bundled — downloaded on first run
with progress UI, *robust retry* (exponential backoff, resume, checksum verification,
cached), and a *model registry* in settings for swapping models as new ones are
added (D12, F11).

---

## 12. Milestones

| Milestone | Scope | Exit criteria |
|-----------|-------|---------------|
| M1 — macOS core | Hotkey hold/release, record, turbo model, paste, chimes | P50 ≤ 0.7 s for 10 s audio; works in 5+ apps |
| M2 — UX shell | Menu bar app, tray status, settings (hotkey/model/language/sounds), launch at login, secure-field guard | Settings persist; rebind works |
| M3 — Ship-ready macOS | Signing + notarization, model download flow, icon, error states | Notarized DMG installs clean on clean Mac |
| M4 — Windows port | Tray icon, hotkey + paste parity, installer | Smoke test matrix on Win 10/11 |

---

## 13. Decisions Complete

All open questions resolved. Secure-field behavior: *Option A — silently skip*
(detect → discard transcript, no feedback to user).

---

## 14. References

•⁠  ⁠whisper.cpp benchmarks (Apple Silicon, Metal): github.com/ggml-org/whisper.cpp
•⁠  ⁠Moonshine benchmarks (M1 CPU): github.com/moonshine-ai/moonshine
•⁠  ⁠Parakeet-MLX (Apple Silicon RTF): github.com/moona3k/macparakeet
•⁠  ⁠Open ASR Leaderboard (WER): huggingface.co/spaces/hf-audio/open_asr_leaderboard