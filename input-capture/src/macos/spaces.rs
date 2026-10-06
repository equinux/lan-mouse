//! Private Dock gesture capture. Raw integer event types avoid constructing
//! invalid discriminants in core-graphics' CGEventType enum (types 29/30).
use super::*;
use input_event::DockSwipe;
use std::ptr;

type Ref = *mut c_void;
type CopyHid = unsafe extern "C" fn(Ref) -> Ref;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        mask: u64,
        callback: unsafe extern "C" fn(Ref, u32, Ref, Ref) -> Ref,
        context: Ref,
    ) -> Ref;
    fn CGEventTapEnable(tap: Ref, enable: bool);
    fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
    fn CGEventGetDoubleValueField(event: Ref, field: u32) -> f64;
}
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDEventGetEventWithOptions(event: Ref, kind: u32, options: u32) -> Ref;
    fn IOHIDEventGetIntegerValue(event: Ref, field: u32) -> isize;
    fn IOHIDEventGetFloatValue(event: Ref, field: u32) -> f64;
    fn IOHIDEventGetPhase(event: Ref) -> u16;
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFMachPortCreateRunLoopSource(allocator: Ref, port: Ref, order: isize) -> Ref;
    fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: Ref);
    fn CFRunLoopRemoveSource(run_loop: Ref, source: Ref, mode: Ref);
    fn CFMachPortInvalidate(port: Ref);
}

struct Context {
    state: Arc<Mutex<InputCaptureState>>,
    tx: Sender<(Position, CaptureEvent)>,
    copy_hid: Option<CopyHid>,
    port: Ref,
    serial: u32,
    sequence: u32,
    active: bool,
    native_sequence: u32,
    permissions: Arc<InputPermissionGate>,
}

pub(super) struct Tap {
    context: Box<Context>,
    source: Ref,
}

impl Tap {
    pub(super) fn new(
        state: Arc<Mutex<InputCaptureState>>,
        tx: Sender<(Position, CaptureEvent)>,
        permissions: Arc<InputPermissionGate>,
    ) -> Option<Self> {
        if !permissions.enabled() {
            return None;
        }
        if std::env::var("LAN_MOUSE_SPACES_SWIPE").as_deref() != Ok("1") {
            return None;
        }
        if !std::process::Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()
            .is_ok_and(|out| out.stdout.starts_with(b"27."))
        {
            return None;
        }
        // Optional symbol: older macOS releases can use the legacy fields.
        let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"CGEventCopyIOHIDEvent".as_ptr()) };
        let copy_hid = if symbol.is_null() {
            None
        } else {
            Some(unsafe { std::mem::transmute::<Ref, CopyHid>(symbol) })
        };
        let mut context = Box::new(Context {
            state,
            tx,
            copy_hid,
            port: ptr::null_mut(),
            serial: 0,
            sequence: 0,
            active: false,
            native_sequence: 0,
            permissions,
        });
        let port = unsafe {
            CGEventTapCreate(
                0,
                0,
                0,
                (1 << 22) | (1 << 29) | (1 << 30) | (1 << 31) | (1 << 32) | (1 << 33) | (1 << 34),
                callback,
                (&mut *context as *mut Context).cast(),
            )
        };
        if port.is_null() {
            log::warn!("Dock gesture: HID event tap unavailable");
            return None;
        }
        context.port = port;
        if !context.permissions.enabled() {
            unsafe {
                CGEventTapEnable(port, false);
                CFMachPortInvalidate(port);
                CFRelease(port.cast());
            }
            return None;
        }
        let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null_mut(), port, 0) };
        if source.is_null() {
            unsafe {
                CFMachPortInvalidate(port);
                CFRelease(port.cast());
            }
            return None;
        }
        unsafe {
            CFRunLoopAddSource(
                CFRunLoop::get_current().as_concrete_TypeRef().cast(),
                source,
                kCFRunLoopCommonModes.cast_mut().cast(),
            );
            CGEventTapEnable(port, true);
        }
        log::info!(
            "Dock gesture: HID tap enabled (HID payload reader: {})",
            copy_hid.is_some()
        );
        Some(Self { context, source })
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // Invalidate before freeing the callback context, on its owning thread.
        unsafe {
            CGEventTapEnable(self.context.port, false);
            CFMachPortInvalidate(self.context.port);
            CFRunLoopRemoveSource(
                CFRunLoop::get_current().as_concrete_TypeRef().cast(),
                self.source,
                kCFRunLoopCommonModes.cast_mut().cast(),
            );
            CFRelease(self.source.cast());
            CFRelease(self.context.port.cast());
        }
    }
}

unsafe fn decode(event: Ref, copy: Option<CopyHid>) -> Option<DockSwipe> {
    if let Some(copy) = copy {
        let hid = copy(event);
        if !hid.is_null() {
            let dock = IOHIDEventGetEventWithOptions(hid, 23, 0);
            let motion = if dock.is_null() {
                0
            } else {
                IOHIDEventGetIntegerValue(dock, (23 << 16) + 1)
            };
            let result = if matches!(motion, 1..=3) {
                let velocity = IOHIDEventGetEventWithOptions(dock, 9, 0);
                Some(DockSwipe {
                    motion: motion as u8,
                    serial: 0,
                    sequence: 0,
                    phase: IOHIDEventGetPhase(dock) as u8,
                    progress: IOHIDEventGetFloatValue(dock, (23 << 16) + 2),
                    velocity: if velocity.is_null() {
                        0.0
                    } else {
                        IOHIDEventGetFloatValue(velocity, (9 << 16) + u32::from(motion == 2))
                    },
                })
            } else {
                None
            };
            CFRelease(hid.cast());
            if result.is_some() {
                return result;
            }
        }
    }
    let motion = CGEventGetIntegerValueField(event, 123);
    if CGEventGetIntegerValueField(event, 110) != 23 || !matches!(motion, 1..=3) {
        return None;
    }
    Some(DockSwipe {
        motion: motion as u8,
        serial: 0,
        sequence: 0,
        phase: CGEventGetIntegerValueField(event, 132) as u8,
        progress: CGEventGetDoubleValueField(event, 124),
        velocity: CGEventGetDoubleValueField(event, 129),
    })
}

unsafe extern "C" fn callback(_proxy: Ref, kind: u32, event: Ref, info: Ref) -> Ref {
    let context = &mut *info.cast::<Context>();
    if !context.permissions.enabled() || kind == u32::MAX {
        context.permissions.disable();
        context.active = false;
        CGEventTapEnable(context.port, false);
        if let Ok(mut state) = context.state.try_lock() {
            state.current_pos = None;
        }
        let _ = CGDisplay::show_cursor(&CGDisplay::main());
        CFRunLoop::get_current().stop();
        return event;
    }
    if kind == u32::MAX - 1 {
        CGEventTapEnable(context.port, true);
        return event;
    }
    let Ok(state) = context.state.try_lock() else {
        return event;
    };
    let Some(position) = state.current_pos else {
        context.active = false;
        return event;
    };
    if kind == 29 && context.active {
        // Generic companion events belong to the intercepted Dock sequence.
        return ptr::null_mut();
    }
    let Some(mut swipe) = (if kind == 30 {
        decode(event, context.copy_hid)
    } else {
        None
    }) else {
        context.native_sequence = context.native_sequence.wrapping_add(1);
        if let Some(gesture) = input_event::macos_gesture::capture(event, context.native_sequence) {
            log::debug!("Native trackpad gesture captured: {gesture:?}");
            if let Err(e) = context
                .tx
                .try_send((position, CaptureEvent::Input(Event::MacGesture(gesture))))
            {
                log::warn!("Native trackpad gesture capture queue: {e}");
                return event;
            }
            return ptr::null_mut();
        }
        return event;
    };
    log::debug!("Dock gesture captured: {swipe:?}");
    // Prevent Dock from preparing a local transition before the recognized
    // gesture begins. This phase does not start a remote transition.
    if swipe.phase == 128 {
        return ptr::null_mut();
    }
    if !swipe.is_valid() {
        return event;
    }
    if swipe.phase == 1 {
        context.serial = context.serial.wrapping_add(1);
        context.sequence = 0;
        context.active = true;
    }
    if !context.active {
        return event;
    }
    swipe.serial = context.serial;
    context.sequence = context.sequence.wrapping_add(1);
    swipe.sequence = context.sequence;
    if let Err(e) = context
        .tx
        .try_send((position, CaptureEvent::Input(Event::DockSwipe(swipe))))
    {
        log::warn!("Dock gesture capture queue: {e}");
        context.active = false;
        return event;
    }
    if matches!(swipe.phase, 4 | 8) {
        context.active = false;
    }
    ptr::null_mut()
}
