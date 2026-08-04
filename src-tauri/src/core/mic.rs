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

/// Ask macOS to show the microphone permission prompt. Returns the status
/// after the system has processed the request (the prompt itself is
/// asynchronous).
#[cfg(target_os = "macos")]
pub fn request_mic_permission() -> MicPermission {
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};
    use std::sync::mpsc;

    let (tx, rx) = mpsc::sync_channel::<()>(1);
    let block = block2::RcBlock::new(move |_: Bool| {
        let _ = tx.send(());
    });
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(
            AVMediaTypeAudio.unwrap(),
            &block,
        );
    }
    // The completion handler fires once the user answered (or immediately if
    // already determined). Poll the channel briefly, then re-read status.
    let _ = rx.recv_timeout(std::time::Duration::from_secs(60));
    mic_permission()
}

#[cfg(not(target_os = "macos"))]
pub fn request_mic_permission() -> MicPermission {
    MicPermission::Authorized
}
