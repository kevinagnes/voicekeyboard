use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MicPermission {
    NotDetermined,
    Denied,
    Authorized,
}

/// Current microphone authorization status.
///
/// On macOS the TCC prompt is only ever shown by AVFoundation's
/// `requestAccessForMediaType:completionHandler:` — CoreAudio (cpal) will
/// silently fail with "no input device" while the status is undetermined, so
/// the prompt must be requested explicitly before creating the capture stream.
#[cfg(target_os = "macos")]
pub fn mic_permission() -> MicPermission {
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
    unsafe {
        match AVCaptureDevice::authorizationStatusForMediaType(AVMediaTypeAudio.unwrap()) {
            AVAuthorizationStatus::Authorized => MicPermission::Authorized,
            AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted => {
                MicPermission::Denied
            }
            _ => MicPermission::NotDetermined,
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn mic_permission() -> MicPermission {
    MicPermission::Authorized
}

/// Ask macOS to show the microphone permission prompt and wait for the user's
/// answer (up to `timeout`). MUST be called on the main thread — macOS
/// suppresses TCC prompts from background threads. Returns the status after
/// the user responds (or the current status on timeout).
#[cfg(target_os = "macos")]
pub fn request_mic_permission_blocking(timeout: std::time::Duration) -> MicPermission {
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};
    use std::sync::mpsc;

    let status = mic_permission();
    if status != MicPermission::NotDetermined {
        return status;
    }
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    // The block must stay alive until the callback fires; AVFoundation copies
    // it, but leak a copy anyway so a copy is never deallocated mid-flight.
    let block: block2::RcBlock<dyn Fn(Bool) + 'static> = block2::RcBlock::new(move |_: Bool| {
        let _ = tx.send(());
    });
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(
            AVMediaTypeAudio.unwrap(),
            &block,
        );
    }
    let _ = Box::leak(Box::new(block));
    let _ = rx.recv_timeout(timeout);
    mic_permission()
}

/// Fire-and-forget variant used when we cannot block (e.g. from commands).
#[cfg(target_os = "macos")]
pub fn request_mic_permission() -> MicPermission {
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};

    let status = mic_permission();
    if status != MicPermission::NotDetermined {
        return status;
    }
    let block: block2::RcBlock<dyn Fn(Bool) + 'static> =
        block2::RcBlock::new(|_: Bool| {});
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(
            AVMediaTypeAudio.unwrap(),
            &block,
        );
    }
    let _ = Box::leak(Box::new(block));
    MicPermission::NotDetermined
}

/// Clear all TCC microphone decisions for this app so the next request shows
/// a fresh prompt. Works without admin rights for the current user.
#[cfg(target_os = "macos")]
pub fn reset_mic_permission() -> Result<(), String> {
    let out = std::process::Command::new("tccutil")
        .args(["reset", "Microphone", crate::APP_ID])
        .output()
        .map_err(|e| format!("tccutil failed: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

/// Deep-link to the Microphone pane of System Settings (macOS 13+). This is
/// the only way to reset a previously-denied TCC decision.
pub fn open_mic_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")
            .spawn();
    }
}

/// True when the process is running inside a proper .app bundle (TCC treats
/// unbundled binaries differently and attributes the mic request to the
/// launching terminal instead).
pub fn running_from_bundle() -> bool {
    std::env::current_exe()
        .map(|p| {
            let s = p.to_string_lossy().to_lowercase();
            s.contains(".app/contents/macos/")
        })
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn request_mic_permission() -> MicPermission {
    MicPermission::Authorized
}

#[cfg(not(target_os = "macos"))]
pub fn reset_mic_permission() -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn open_mic_settings() {}
