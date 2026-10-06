//! macOS 26 Dock-swipe synthesis using private CGEvent fields.
//! Field meanings are documented by Mac Mouse Fix's TouchSimulator research.
//! No opaque event memory or hard-coded object offsets are used.
use super::*;
use input_event::DockSwipe;
use std::ffi::c_void;
use std::ptr;

type Ref = *mut c_void;
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: Ref) -> Ref;
    fn CGEventSetType(event: Ref, kind: u32);
    fn CGEventSetIntegerValueField(event: Ref, field: u32, value: i64);
    fn CGEventSetDoubleValueField(event: Ref, field: u32, value: f64);
    fn CGEventPost(tap: u32, event: Ref);
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: Ref);
}

#[derive(Default)]
pub(super) struct SpacesEmulation {
    active: Rc<Cell<Option<(EmulationHandle, DockSwipe)>>>,
    last: Option<(EmulationHandle, u32, u32)>,
    timeout: Option<JoinHandle<()>>,
    native_sequences: std::collections::HashMap<EmulationHandle, u32>,
}

impl SpacesEmulation {
    pub(super) fn consume_native(
        &mut self,
        gesture: &input_event::MacGesture,
        handle: EmulationHandle,
    ) {
        if std::env::var("LAN_MOUSE_SPACES_SWIPE").as_deref() != Ok("1") {
            return;
        }
        if self
            .native_sequences
            .get(&handle)
            .is_some_and(|last| (gesture.sequence.wrapping_sub(*last) as i32) <= 0)
        {
            return;
        }
        if input_event::macos_gesture::replay(gesture) {
            self.native_sequences.insert(handle, gesture.sequence);
            log::debug!("Native trackpad gesture replayed: {gesture:?}");
        } else {
            log::warn!("Native trackpad gesture could not be reconstructed: {gesture:?}");
        }
    }

    pub(super) fn consume(&mut self, swipe: DockSwipe, handle: EmulationHandle) {
        if std::env::var("LAN_MOUSE_SPACES_SWIPE").as_deref() != Ok("1") || !swipe.is_valid() {
            return;
        }
        // This prototype implements receiving on macOS 26 only. Do not inject
        // the legacy representation into a newer Dock that expects HID data.
        static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*SUPPORTED.get_or_init(|| {
            std::process::Command::new("/usr/bin/sw_vers")
                .arg("-productVersion")
                .output()
                .is_ok_and(|out| out.stdout.starts_with(b"26."))
        }) {
            return;
        }
        if let Some((peer, serial, sequence)) = self.last {
            if peer == handle {
                let older =
                    swipe.serial != serial && (swipe.serial.wrapping_sub(serial) as i32) <= 0;
                if older || (swipe.serial == serial && swipe.sequence <= sequence) {
                    return;
                }
            }
        }
        self.last = Some((handle, swipe.serial, swipe.sequence));
        if swipe.phase == 1 {
            self.cancel(None);
        } else if self
            .active
            .get()
            .is_none_or(|(peer, old)| peer != handle || old.serial != swipe.serial)
        {
            // Recover a lost begin from cumulative progress. An isolated end
            // cannot start a gesture after a disconnect or packet reordering.
            if swipe.phase != 2 {
                return;
            }
            self.cancel(None);
            post(DockSwipe {
                phase: 1,
                progress: 0.0,
                velocity: 0.0,
                ..swipe
            });
        }
        log::debug!("Dock gesture replay: {swipe:?}");
        post(swipe);
        if let Some(task) = self.timeout.take() {
            task.abort();
        }
        if matches!(swipe.phase, 4 | 8) {
            self.active.set(None);
            return;
        }
        self.active.set(Some((handle, swipe)));
        let active = self.active.clone();
        self.timeout = Some(tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            if let Some((_, swipe)) = active.take() {
                log::warn!("Dock gesture timed out; cancelling remote transition");
                post(DockSwipe {
                    phase: 8,
                    velocity: 0.0,
                    ..swipe
                });
            }
        }));
    }

    pub(super) fn cancel(&mut self, handle: Option<EmulationHandle>) {
        if let Some(handle) = handle {
            self.native_sequences.remove(&handle);
        }
        if let Some((peer, swipe)) = self.active.get() {
            if handle.is_some_and(|handle| peer != handle) {
                return;
            }
            post(DockSwipe {
                phase: 8,
                velocity: 0.0,
                ..swipe
            });
            self.active.set(None);
        }
        if let Some(task) = self.timeout.take() {
            task.abort();
        }
    }
}

impl Drop for SpacesEmulation {
    fn drop(&mut self) {
        self.cancel(None);
    }
}

fn post(swipe: DockSwipe) {
    if !Permissions::input_allowed() {
        return;
    }
    // Vertical swipes retain the HID sign; horizontal swipes and pinches use
    // the opposite sign in legacy Dock events. Release must follow the drag.
    let direction = if swipe.motion == 2 { 1.0 } else { -1.0 };
    let progress = direction * swipe.progress;
    let velocity = direction * swipe.velocity;
    // Type 30 carries the Dock swipe; type 29 is the companion gesture event.
    // These private numeric types must never be converted to CGEventType.
    unsafe {
        let event = CGEventCreate(ptr::null_mut());
        let companion = CGEventCreate(ptr::null_mut());
        if event.is_null() || companion.is_null() {
            if !event.is_null() {
                CFRelease(event);
            }
            if !companion.is_null() {
                CFRelease(companion);
            }
            log::warn!("Dock gesture event allocation failed");
            return;
        }
        CGEventSetType(event, 30);
        for (field, value) in [
            (110, 23),
            (132, swipe.phase as i64),
            (134, swipe.phase as i64),
            (135, (progress as f32).to_bits() as i64),
            (136, 0),
            (41, 33231),
        ] {
            CGEventSetIntegerValueField(event, field, value);
        }
        for (field, value) in [
            (124, progress),
            (123, swipe.motion as f64),
            (165, swipe.motion as f64),
            (119, f32::from_bits(swipe.motion as u32) as f64),
            (139, f32::from_bits(swipe.motion as u32) as f64),
            (129, velocity),
            (130, velocity),
        ] {
            CGEventSetDoubleValueField(event, field, value);
        }
        CGEventSetType(companion, 29);
        CGEventSetIntegerValueField(companion, 41, 33231);
        CGEventPost(1, event);
        CGEventPost(1, companion);
        CFRelease(event);
        CFRelease(companion);
    }
}
