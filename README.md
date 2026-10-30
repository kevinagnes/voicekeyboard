# VoiceKeyboard

A local, offline-first push-to-talk dictation app (macOS, v1). While any text field is
focused, **hold a hotkey → speak → release** and the transcript is pasted instantly into the
field. Local Whisper/Nemotron dictation keeps audio on-device; the optional **Gemini 3.5
Transcribe** engine and the Ask-Gemini assistant send data to Google.

A second hotkey (**Ask Gemini**, default `ctrl+ShiftRight`) turns dictation into a voice
assistant: **hold → speak a question → release**, and the transcript is sent to the Gemini
LLM; the answer is spoken aloud (Gemini TTS) and shown on an on-screen panel together with
what you said. This flow requires a Gemini API key and sends your text + audio to Google.

Implements the spec in [`prompt.md`](prompt.md).

Technology: https://doc.rust-lang.org/stable/cargo/

## Architecture

| Component | File | Responsibility |
|---|---|---|
| Hotkey | `src-tauri/src/core/hotkey.rs` | Global hold/release hotkeys (Carbon `RegisterEventHotKey`), combo parsing, rebinding (record + ask-LLM) |
| Capture | `src-tauri/src/core/capture.rs` | `cpal` mic → 16 kHz mono ring buffer, streaming resampler, silence trim |
| STT | `src-tauri/src/core/stt.rs`, `gemini_stt.rs` | `whisper-rs` (whisper.cpp + Metal) / ONNX Nemotron local engines, or **Gemini 3.5 Transcribe** batch API; language auto/override, initial prompt |
| Gemini | `src-tauri/src/core/gemini.rs` | Ask-LLM client: chat grounded in **Google Search** (generateContent `google_search` tool, sources from `groundingMetadata`), **streaming** Interactions-API TTS (SSE `step.delta`, 24 kHz PCM), models/voices catalog (`language_code`-filtered) |
| Paste | `src-tauri/src/core/paste.rs` | Clipboard save → set → Cmd/Ctrl+V → restore; secure-field guard; text sanitize |
| Sounds | `src-tauri/src/core/sounds.rs` | Synthesized chimes + **streaming speech playback** (stateful resampler, noise-tail trim, cancellable), mute toggle |
| Settings | `src-tauri/src/core/settings.rs` | Persisted JSON settings (`~/Library/Application Support/.../settings.json`) |
| Agent core | `src-tauri/src/core/agent/` | Durable SQLite step log, single-flight executor, inbox/steering, frozen tool runtime, permission gate, plans/artifacts/verification, compaction, jobs/subagents, run inspector, undo, streaming speech |
| Workspace | `<app data>/workspace` | Sandbox-free file workspace for `read`/`write`/`edit`/`glob`/`grep`, plus `skills/*.md` |
| Registry/Download | `src-tauri/src/model_registry.rs`, `src-tauri/src/downloader.rs` | Model catalog, resumable download with retry/backoff + checksum verify |
| Shell | `src-tauri/src/app.rs` | Menu-bar tray, state machine, Tauri commands/events, worker threads, LLM overlay window |

## Run (dev)  

```sh
cd src-tauri
cargo run
```

A menu-bar icon appears. On launch the app always checks **Microphone** and
**Accessibility** (for pasting) permissions: if either is not granted it prompts
for it, and until both are granted the app is blocked behind a
permissions-gate window that only offers to open System Settings or quit.
Behavior is identical for dev runs and the installed `.app` (note: when running
the raw `cargo run` binary, macOS may attribute the microphone TCC prompt to
the launching terminal).

## Build a release

```sh
cd src-tauri
cargo build --release          # binary only
# or package a .app / .dmg (requires tauri-cli):
#   cargo install tauri-cli
#   cargo tauri build
```

### Ship a signed + notarized DMG (Developer ID)

```sh
# 1. Store notarization credentials once (app-specific password from appleid.apple.com):
xcrun notarytool store-credentials "VoiceKeyboard" \
  --apple-id YOUR_APPLE_ID --team-id YOUR_TEAM_ID --password APP_SPECIFIC_PASSWORD

# 2. Build, sign (Developer ID + hardened runtime), DMG, notarize, staple:
./tools/package.sh
```

`tools/package.sh` produces `dist/VoiceKeyboard.dmg`. It auto-detects the
`VoiceKeyboard` keychain profile, or accepts `VK_APPLE_ID` / `VK_NOTARY_PASSWORD`
env vars instead. `tools/make_app.sh` builds a locally ad-hoc-signed `.app`
without a Developer account.

### One-command release (bump → build → sign → notarize → GitHub release)

```sh
cargo increment -v p     # patch: 1.0.0 → 1.0.1
cargo increment -v m     # minor: 1.0.0 → 1.1.0
cargo increment -v M     # major: 1.0.0 → 2.0.0
cargo increment --dry-run          # preview the bump only
cargo increment --no-push          # build + tag locally, no push/release
```

Bumps the version in `Cargo.toml` / `tauri.conf.json`, builds + signs +
notarizes via `tools/package.sh`, commits, tags `vX.Y.Z`, pushes, and creates a
GitHub release with the DMG. Local-only overrides (e.g. `GH_TOKEN`) can be set
in `.local.env` (gitignored).

## Auto-update

The app checks GitHub releases on launch (toggleable in Settings). When a new
version is available it shows a tray menu entry and a "Download & install"
button in Settings, downloads the DMG, and swaps the app bundle with a
relaunch.

## First run / model download

The default model (`whisper large-v3-turbo`, ~1.6 GB, q8_0) is **not bundled**. It downloads
on first run from `ggml-org/whisper.cpp` with progress in the Settings window, resumable and
retried with exponential backoff. It is cached in the app data dir.

> **Note:** model checksums (`checksumSha256` in `src-tauri/models/registry.json`) are
> currently empty because upstream does not publish sidecar SHA-256 files. The downloader
> verifies the checksum whenever one is present; add real hashes to registry entries to
> enable integrity checks.

## Defaults and deviations from the spec

- **Default hotkey is `ShiftRight`** (hold-to-record). The spec's `Fn` default is not
  technically possible: macOS does not expose the `Fn` key to global-hotkey APIs
  (`RegisterEventHotKey`), so no app can bind it. `ShiftRight` is a rarely-used key (per the
  spec's own conflict-mitigation guidance) and is fully rebindable in Settings — including
  combos such as `ctrl+KeyG`.
- **Ask-LLM hotkey defaults to `ctrl+ShiftRight`** (hold → speak → release → Gemini replies
  by voice). Fully rebindable in Settings. Gemini defaults: `gemini-3.1-flash-lite` for the
  LLM, `gemini-3.8-flash-tts` / voice `Kore` for speech, both editable in Settings.
- Model/language defaults per D3/D5: `large-v3-turbo` q8_0, language auto-detected with
  `en`/`pt` override.

## Behavior

- Ignore recordings shorter than 300 ms (configurable).
- Recordings capped at 120 s (configurable); capture is trimmed of leading/trailing silence
  before inference (reduces hallucination).
- While an utterance is being transcribed (or an Ask-Gemini reply is being spoken), a new
  recording is blocked (brief).
- **Password fields are detected** (macOS AX `AXSecureTextField`) and the transcript is
  silently discarded — no paste, no chime, no notification.
- Clipboard is saved before paste and restored ~1 s after.
- **Ask Gemini** shows a floating panel: live mic level while you speak, then your transcript
  and the LLM's reply. The reply is **streamed** through the speakers as Gemini synthesizes
  it (24 kHz `audio/l16` SSE, resampled on-device) — first audio arrives long before the
  reply is complete. The panel has a **Cancel** button and **Esc** cancels the current
  recording or stops the spoken reply; the pipeline aborts between phases and the panel
  shows "Cancelling…" immediately even while a blocking transcribe/search call is still
  unwinding. The transcript and reply also appear in the Settings Activity log.
- **Gemini TTS end-of-audio noise**: the Gemini TTS API is known to append a static/white
  noise burst at the end of generated audio (upstream artifact, reported for multiple
  `*-tts` models). VoiceKeyboard keeps the last second of audio in a buffer and trims
  trailing silence / noise bursts (windowed RMS + zero-crossing detection) before playing
  it.
- **News & web search**: ask "what's the news about X?" — Gemini's built-in **Grounding
  with Google Search** (`google_search` tool) decides on its own when the question needs
  current information, searches Google and grounds the answer in the results. The sources
  behind the answer are shown as chips on the panel. No API key beyond Gemini and no HTML
  scraping; models that predate grounding fall back to answering from model knowledge.
- **Models & voices**: after entering an API key, "Load models & voices…" fetches the
  catalog (`/v1beta/models` + `/v1beta/voices`); the LLM model, TTS model and TTS voice
  become pickers. **Voices are pre-filtered by the selected language** (e.g. `pt` →
  `pt-BR`, `en` → `en-US`) and re-loaded automatically when you change the language;
  groups still label each voice's language, and the list falls back to all voices if a
  language has none.
- **Dictation engines**: local Whisper and Nemotron are fully on-device. **Gemini 3.5 Transcribe** is an optional cloud engine selected in Settings → Model; after the hotkey is released the recorded utterance (16 kHz WAV) is sent to Google for transcription. It needs the Gemini API key and does not download a model.
- **Sessions**: every Ask-Gemini interaction belongs to a durable session that can be
  recalled and continued. The next question continues the active session, with the whole
  conversation replayed for context. A **Sessions** submenu in the menu bar lists recent
  sessions ("New Session (clean context)" plus the last 12), and a **Sessions tab in
  Settings** lets you read, rename, delete, or continue any session. While the panel is
  listening, its **+ New session** button switches the question you are speaking into a
  fresh context. Sessions live in `agent.db`; the legacy `sessions.json` is imported once
  and left in place.
- **Agent workflow**: the Ask hotkey now runs a durable agent, not a single completion.
  It can plan (`update_plan`), read/write/edit files in the workspace, search and fetch
  the web, use the clipboard and paste, remember facts, save skills, verify artifacts,
  run background jobs and delegate read-only subtasks. Every write or side effect asks
  through the overlay (allow once / always for this session / deny). Context is compacted
  with a durable checkpoint; interrupted runs resume honestly after a restart, and the
  Agent tab exposes a run inspector with file undo. See
  [`docs/agent-core-v2-port.md`](docs/agent-core-v2-port.md).
- **Movable Ask panel**: drag it anywhere by its grip or any non-text area. Its position is
  remembered in `overlay_positions.json` and restored on the next launch; if the monitor
  is gone, it falls back to bottom-center. The panel shows only the current exchange by
  default; click the transcript/reply (or the History pill) to expand the whole session,
  and click again to collapse.
- **Screenshot question**: press the screenshot hotkey (default `Ctrl+Shift+S`), drag a
  region of the screen, then keep holding and speak about it. Release to send the region
  plus your spoken question to the assistant; the answer is spoken and shown with the
  screenshot attached.
- **Live transcription**: audio is transcribed while you speak for every engine.
  Gemini Transcribe uses the Live API (true streaming and a final ready on release).
  Whisper and Nemotron use incremental on-device transcription: audio is cut at
  natural pauses, each segment is transcribed with the previous text as context,
  and only the short tail is processed on release. Partials appear in the Ask panel;
  if incremental transcription fails, VoiceKeyboard falls back to the normal batch path.
- Requires a Gemini API key (Settings → AI Assistant); the API key is stored in the local
  `settings.json`.
- No telemetry, no logs of transcripts. The only network calls are local-model
  downloads and, when used, the Gemini API (Ask-Gemini and Gemini 3.5 Transcribe).

## Tests

```sh
cd src-tauri
cargo test
```

Unit tests cover hotkey parsing, ring buffer, resampler, silence trim, sanitize, chime
synthesis, registry parsing, SHA-256 verification, the session store (titles,
pruning, recall), and the agent core (durable sequencing, idempotent admission,
claims, permissions, schema repair, prompt prefix stability, compaction split,
speech segmentation, path guards and workspace tools).

## Roadmap (per spec)

- [x] M1 — macOS core (hotkey hold/release, record, turbo model, paste, chimes)
- [x] M2 — UX shell (menu bar, tray status, settings, secure-field guard, launch at login)
- [x] M3 — Ship-ready macOS (signing + notarization, DMG) — needs an Apple Developer account
- [x] M4 — Windows port (tray, hotkey + paste parity, installer)
