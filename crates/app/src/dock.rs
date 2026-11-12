//! macOS dock icon: sets the app mark as the application icon at runtime.
//!
//! `cargo run` starts an unbundled binary, so the dock would otherwise show a
//! generic icon. A bundled .app already gets its icon from `AppIcon.icns`;
//! setting it here as well is harmless.

use objc2::{AnyThread as _, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::NSData;

const ICON: &[u8] = include_bytes!("../../../assets/app/app-icon-1024.png");

/// Sets the dock icon. Call once on the main thread after launch. Logs and
/// skips on failure, never panics.
pub fn init() {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("chaos-control: dock icon not set: not on the main thread");
        return;
    };
    let data = NSData::with_bytes(ICON);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        eprintln!("chaos-control: dock icon not set: could not decode app-icon-1024.png");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    // SAFETY: on the main thread with a valid, retained NSImage.
    unsafe { app.setApplicationIconImage(Some(&image)) };
}
