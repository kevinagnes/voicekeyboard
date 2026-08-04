# VoiceKeyboard

A local, offline-first push-to-talk dictation app (macOS, v1). While any text field is
focused, **hold a hotkey → speak → release** and the transcript is pasted instantly into the
field. 100% on-device — no audio leaves your machine.

Implements the spec in [`prompt.md`](prompt.md).

## Architecture

| Component | File | Responsibility |
|---|---|---|
| Hotkey | `src-tauri/src/core/hotkey.rs` | Global hold/release hotkey (Carbon `RegisterEventHotKey`), combo parsing, rebinding |
| Capture | `src-tauri/src/core/capture.rs` | `cpal` mic → 16 kHz mono ring buffer, streaming resampler, silence trim |
| STT | `src-tauri/src/core/stt.rs` | `whisper-rs` (whisper.cpp + Metal) engine, language auto/override, initial prompt |
| Paste | `src-tauri/src/core/paste.rs` | Clipboard save → set → Cmd/Ctrl+V → restore; secure-field guard; text sanitize |
| Sounds | `src-tauri/src/core/sounds.rs` | Synthesized chimes (start / done), mute toggle |
| Settings | `src-tauri/src/core/settings.rs` | Persisted JSON settings (`~/Library/Application Support/.../settings.json`) |
| Registry/Download | `src-tauri/src/model_registry.rs`, `src-tauri/src/downloader.rs` | Model catalog, resumable download with retry/backoff + checksum verify |
| Shell | `src-tauri/src/app.rs` | Menu-bar tray, state machine, Tauri commands/events, worker threads |

## Run (dev)

```sh
cd src-tauri
cargo run
```

A menu-bar icon appears. First run will ask for Microphone access and (after the model is
downloaded) Accessibility access for pasting.

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
  --apple-id YOUR_APPLE_ID --team-id 2JNC7VSQ8N --password APP_SPECIFIC_PASSWORD

# 2. Build, sign (Developer ID + hardened runtime), DMG, notarize, staple:
./tools/package.sh
```

`tools/package.sh` produces `dist/VoiceKeyboard.dmg`. It auto-detects the
`VoiceKeyboard` keychain profile, or accepts `VK_APPLE_ID` / `VK_NOTARY_PASSWORD`
env vars instead. `tools/make_app.sh` builds a locally ad-hoc-signed `.app`
without a Developer account.

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
- Model/language defaults per D3/D5: `large-v3-turbo` q8_0, language auto-detected with
  `en`/`pt` override.

## Behavior

- Ignore recordings shorter than 300 ms (configurable).
- Recordings capped at 120 s (configurable); capture is trimmed of leading/trailing silence
  before inference (reduces hallucination).
- While an utterance is being transcribed, a new recording is blocked (brief).
- **Password fields are detected** (macOS AX `AXSecureTextField`) and the transcript is
  silently discarded — no paste, no chime, no notification.
- Clipboard is saved before paste and restored ~1 s after.
- No telemetry, no network calls other than the model download, no logs of transcripts.

## Tests

```sh
cd src-tauri
cargo test
```

Unit tests cover hotkey parsing, ring buffer, resampler, silence trim, sanitize, chime
synthesis, registry parsing, and SHA-256 verification.

## Roadmap (per spec)

- [x] M1 — macOS core (hotkey hold/release, record, turbo model, paste, chimes)
- [x] M2 — UX shell (menu bar, tray status, settings, secure-field guard, launch at login)
- [x] M3 — Ship-ready macOS (signing + notarization, DMG) — needs an Apple Developer account
- [ ] M4 — Windows port (tray, hotkey + paste parity, installer)
