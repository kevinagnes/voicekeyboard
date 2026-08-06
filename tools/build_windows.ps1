# Builds a standalone VoiceKeyboard.exe for Windows (no signing).
# The Tauri UI is embedded at compile time, so the exe is self-contained.
#
# Requirements:
#   - Rust (stable) + Visual Studio 2022 17.14+ (MSVC 14.44+). The prebuilt
#     ONNX Runtime static lib used by `ort` is compiled with MSVC 14.44 and
#     will not link with older toolsets (LNK2001 __std_* symbols).
#   - No CMAKE_GENERATOR override: CMake must use the Visual Studio generator
#     (whisper-rs-sys' cl.exe build needs the MSVC environment, which the VS
#     generator provides automatically).
$ErrorActionPreference = "Stop"

$ROOT = Split-Path -Parent $PSScriptRoot
Set-Location "$ROOT\src-tauri"

Write-Host "==> Building release binary..."
cargo build --release
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$DIST = "$ROOT\dist"
New-Item -ItemType Directory -Force -Path $DIST | Out-Null

Write-Host "==> Copying executable..."
Copy-Item -Force "target\release\voicekeyboard.exe" "$DIST\VoiceKeyboard.exe"

Write-Host
Write-Host "Done: $DIST\VoiceKeyboard.exe"
Write-Host "Launch with:"
Write-Host "  & `"$DIST\VoiceKeyboard.exe`""
