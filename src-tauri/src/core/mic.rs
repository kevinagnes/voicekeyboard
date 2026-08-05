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

/// Ask macOS to show the microphone permission prompt.
///
/// Fire-and-forget: if the status is not "undetermined", the system will not
/// prompt (the answer is final and can only be changed in System Settings),
/// so this returns immediately. When undetermined, the prompt is triggered and
/// this returns right away — call [`mic_permission`] later to read the answer.
#[cfg(target_os = "macos")]
pub fn request_mic_permission() -> MicPermission {
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};

    let status = mic_permission();
    if status != MicPermission::NotDetermined {
        return status;
    }
    // AVFoundation retains/copies the block, so dropping the RcBlock is fine.
    let block = block2::RcBlock::new(|_: Bool| {});
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(
            AVMediaTypeAudio.unwrap(),
            &block,
        );
    }
    MicPermission::NotDetermined
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
pub fn open_mic_settings() {}
