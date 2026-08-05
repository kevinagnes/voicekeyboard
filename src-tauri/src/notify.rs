//! Native macOS notifications via UNUserNotificationCenter.

/// Post a notification banner. Callers should dispatch to the main thread
/// (e.g. via `AppHandle::run_on_main_thread`); the center tolerates calls
/// from other threads but the main thread is safest.
#[cfg(target_os = "macos")]
pub fn show(title: &str, body: &str) {
    use objc2::runtime::Bool;
    use objc2_foundation::{NSString, NSError};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
        UNUserNotificationCenter,
    };

    let center = UNUserNotificationCenter::currentNotificationCenter();

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));

    let identifier = NSString::from_str("voicekeyboard-notification");
    let req = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &identifier,
        &content,
        None,
    );

    let request_auth: block2::RcBlock<dyn Fn(Bool, *mut NSError) + 'static> =
        block2::RcBlock::new(|_: Bool, _: *mut NSError| {});
    let opts = UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound;
    center.requestAuthorizationWithOptions_completionHandler(opts, &request_auth);
    center.addNotificationRequest_withCompletionHandler(&req, None);
}

#[cfg(not(target_os = "macos"))]
pub fn show(_title: &str, _body: &str) {}
