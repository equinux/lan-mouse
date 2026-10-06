//! Tiny macOS Privacy-pane helpers used by the GUI.
//!
//! On macOS 13+, the Accessibility grant transitively confers the
//! listen-only event-tap privilege that Input Monitoring gates and the
//! synthesize-event privilege that Post Event gates, and the bundle
//! typically isn't even listed in those separate panes. So the single
//! user-facing action for any missing-capture or missing-emulation
//! scenario is "re-toggle Accessibility" — we don't route elsewhere.

use std::ffi::{c_uchar, c_void};
use std::process::Command;
use std::sync::Once;

use gtk::glib;

// Apple declares `AXIsProcessTrusted` as returning `Boolean` (`unsigned char`),
// NOT C's `bool`. Rust's `bool` has a strict bit pattern (0 or 1) so binding
// a `Boolean`-returning function as `-> bool` is technically UB if Apple ever
// returns a non-canonical true value. Keep these as `c_uchar` and normalize.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> c_uchar;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> c_uchar;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFAllocatorDefault: *const c_void;
    static kCFBooleanTrue: *const c_void;
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

// kAXTrustedCheckOptionPrompt is a CFStringRef exported from ApplicationServices.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    static kAXTrustedCheckOptionPrompt: *const c_void;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGRequestListenEventAccess() -> c_uchar;
    fn CGRequestPostEventAccess() -> c_uchar;

    fn CGPreflightListenEventAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;

}

pub fn accessibility_granted() -> bool {
    let raw = unsafe { AXIsProcessTrusted() };
    log::debug!("AXIsProcessTrusted() = {raw}");
    raw != 0
}

pub enum AccessibilityChange {
    /// AX was missing at startup and the user has now granted it.
    /// Capture/emulation still need a relaunch to take effect, since
    /// the daemon subprocess already bailed.
    Granted,
    /// AX was granted and the user has now revoked it. Quit immediately
    /// — leaving the process alive with an active CGEventTap at
    /// HeadInsertEventTap can wedge system input (clicks/keys silently
    /// consumed) until the process dies. See
    /// macos-cgeventtap-drop-fallthrough-tcc-revoke skill for the
    /// underlying event-tap-disable footgun.
    Revoked,
}

/// Poll for Accessibility grant/revoke transitions. Starts a 1-second
/// GLib timer that fires `on_change` every time `AXIsProcessTrusted()`
/// flips, and keeps running for the lifetime of the process.
///
/// We rely on polling rather than AXObserver because the AX notification
/// API requires a trusted process to subscribe — the precondition we
/// can't assume. This runs on the GTK main thread (via
/// `timeout_add_seconds_local`).
pub fn watch_accessibility_state<F>(mut on_change: F)
where
    F: FnMut(AccessibilityChange) + 'static,
{
    let mut last = accessibility_granted();
    log::info!("watching Accessibility state (initial = {last})");
    glib::timeout_add_seconds_local(1, move || {
        let current = accessibility_granted();
        if current != last {
            log::info!("Accessibility state flip: {last} -> {current}");
            on_change(if current {
                AccessibilityChange::Granted
            } else {
                AccessibilityChange::Revoked
            });
            last = current;
        }
        glib::ControlFlow::Continue
    });
}

pub fn open_accessibility_settings() {
    open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility");
}

/// Spawn a fresh instance of the current `.app` bundle via Launch Services
/// after a 1-second delay, so the new instance starts *after* the current
/// process has exited — otherwise Launch Services reactivates the existing
/// process instead of launching a fresh one, and the stale IPC socket
/// would block the new daemon subprocess. The caller is responsible for
/// quitting the current process (e.g. `Application::quit()`) after this.
pub fn relaunch_bundle() {
    // Resolve the .app bundle path from the current executable: it lives
    // at <bundle>/Contents/MacOS/lan-mouse, so three parents up is the
    // bundle root we hand to `open`.
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(bundle) = exe
        .parent()
        .and_then(std::path::Path::parent)
        .and_then(std::path::Path::parent)
    else {
        return;
    };

    // Trailing `&` backgrounds the sleep+open so our shell call returns
    // immediately; the spawned shell is adopted by launchd once we exit.
    let cmd = format!("(sleep 1 && open {bundle:?}) &");
    let _ = Command::new("sh").arg("-c").arg(cmd).spawn();
}

fn open_url(url: &str) {
    if let Err(e) = Command::new("open").arg(url).spawn() {
        log::warn!("failed to open {url}: {e}");
    }
}

/// One-shot, at GUI startup: if a permission is missing, fire the system
/// prompt. This is where the familiar first-launch "Lan Mouse.app would
/// like to control this computer" alert comes from. Subsequent clicks on
/// the Reenable button use URL-scheme navigation instead, so we never
/// double up alerts on retries.
///
/// Guarded with a `Once` because GApplication::activate can fire more
/// than once in a process (reactivation, window presentation) and we
/// must not re-pop the TCC alert on each activation — that looks like a
/// bug to the user.
pub fn fire_initial_prompts() {
    static FIRED: Once = Once::new();
    FIRED.call_once(fire_initial_prompts_inner);
}

fn fire_initial_prompts_inner() {
    if !accessibility_granted() {
        // When Accessibility isn't granted yet, ONLY fire the Accessibility
        // prompt. Do NOT also try to register Input Monitoring or Post Event
        // — those paths have been observed to surface a second Accessibility
        // dialog on top of the one we fire explicitly (Post Event is part of
        // the Accessibility category on modern macOS, and CGEventTap attempts
        // can bail on Accessibility before they reach the Input Monitoring
        // check). Once the user grants Accessibility and relaunches, this
        // branch is skipped and we register the other grants cleanly below.
        log::info!("firing first-launch Accessibility prompt");
        unsafe {
            let key = kAXTrustedCheckOptionPrompt;
            let value = kCFBooleanTrue;
            let options = CFDictionaryCreate(
                kCFAllocatorDefault,
                &key as *const _,
                &value as *const _,
                1,
                // These constants live for the process lifetime; no retain or
                // release callbacks are needed for this temporary dictionary.
                std::ptr::null(),
                std::ptr::null(),
            );
            if !options.is_null() {
                AXIsProcessTrustedWithOptions(options);
                CFRelease(options);
            }
        }
        return;
    }
    // Permission requests must never create a temporary tap. The daemon
    // stays blocked until all preflight checks succeed on a fresh launch.
    unsafe {
        if !CGPreflightListenEventAccess() {
            CGRequestListenEventAccess();
        }
        if !CGPreflightPostEventAccess() {
            CGRequestPostEventAccess();
        }
    }
}
