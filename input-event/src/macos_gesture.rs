//! Quartz's documented network event representation preserves application
//! gesture fields and touch/pressure payloads without depending on their layout.
use crate::MacGesture;
use std::ffi::{c_char, c_void};
use std::ptr;

type Ref = *mut c_void;
#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}
#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> Ref;
    fn sel_registerName(name: *const c_char) -> Ref;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> Ref;
    fn objc_autoreleasePoolPop(pool: Ref);
}
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: Ref) -> Ref;
    fn CGEventGetType(event: Ref) -> u32;
    fn CGEventGetIntegerValueField(event: Ref, field: u32) -> i64;
    fn CGEventSetIntegerValueField(event: Ref, field: u32, value: i64);
    fn CGEventGetLocation(event: Ref) -> Point;
    fn CGEventSetLocation(event: Ref, point: Point);
    fn CGEventGetTimestamp(event: Ref) -> u64;
    fn CGEventSetTimestamp(event: Ref, timestamp: u64);
    fn CGEventCreateData(allocator: Ref, event: Ref) -> Ref;
    fn CGEventCreateFromData(allocator: Ref, data: Ref) -> Ref;
    fn CGEventPost(tap: u32, event: Ref);
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDataCreate(allocator: Ref, bytes: *const u8, length: isize) -> Ref;
    fn CFDataGetLength(data: Ref) -> isize;
    fn CFDataGetBytePtr(data: Ref) -> *const u8;
    fn CFRelease(object: Ref);
}

/// Classify only trackpad events. Dock swipes need explicit OS conversion.
///
/// # Safety
/// `event` must be a live CGEventRef for the duration of this call.
unsafe fn kind(event: Ref) -> Option<u8> {
    match CGEventGetType(event) {
        22 if CGEventGetIntegerValueField(event, 88) != 0 => return Some(22),
        29..=34 => {}
        _ => return None,
    }
    if CGEventGetIntegerValueField(event, 110) == 23 {
        return None;
    }
    let pool = objc_autoreleasePoolPush();
    let bridge: unsafe extern "C" fn(Ref, Ref, Ref) -> Ref =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    let get_type: unsafe extern "C" fn(Ref, Ref) -> usize =
        std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
    let ns = bridge(
        objc_getClass(c"NSEvent".as_ptr()),
        sel_registerName(c"eventWithCGEvent:".as_ptr()),
        event,
    );
    let value = if ns.is_null() {
        0
    } else {
        get_type(ns, sel_registerName(c"type".as_ptr()))
    };
    objc_autoreleasePoolPop(pool);
    matches!(value, 18..=20 | 29..=34).then_some(value as u8)
}

/// Capture an application gesture with the native Quartz transport API.
///
/// # Safety
/// `event` must be a live CGEventRef for the duration of this call.
pub unsafe fn capture(event: Ref, sequence: u32) -> Option<MacGesture> {
    let kind = kind(event)?;
    let data = CGEventCreateData(ptr::null_mut(), event);
    if data.is_null() {
        return None;
    }
    let length = CFDataGetLength(data);
    let gesture = if length > 0 && length as usize <= crate::MAX_MAC_GESTURE_SIZE {
        Some(MacGesture {
            sequence,
            kind,
            data: std::slice::from_raw_parts(CFDataGetBytePtr(data), length as usize).to_vec(),
        })
    } else {
        None
    };
    CFRelease(data);
    gesture
}

/// Reconstruct and validate a native application gesture without posting it.
/// Used by transport diagnostics and compatibility tests.
pub fn reconstructed_kind(gesture: &MacGesture) -> Option<u8> {
    with_event(gesture, |event| unsafe { kind(event) }).flatten()
}

fn with_event<T>(gesture: &MacGesture, action: impl FnOnce(Ref) -> T) -> Option<T> {
    if !gesture.is_valid() {
        return None;
    }
    unsafe {
        let data = CFDataCreate(
            ptr::null_mut(),
            gesture.data.as_ptr(),
            gesture.data.len() as isize,
        );
        if data.is_null() {
            return None;
        }
        let event = CGEventCreateFromData(ptr::null_mut(), data);
        CFRelease(data);
        if event.is_null() {
            return None;
        }
        // Check reconstructed contents, not just the kind asserted by the peer.
        if kind(event) != Some(gesture.kind) {
            CFRelease(event);
            return None;
        }
        let result = action(event);
        CFRelease(event);
        Some(result)
    }
}

/// Replay at the receiver's cursor with receiver-local routing and time.
pub fn replay(gesture: &MacGesture) -> bool {
    with_event(gesture, |event| unsafe {
        let seed = CGEventCreate(ptr::null_mut());
        if seed.is_null() {
            return false;
        }
        CGEventSetLocation(event, CGEventGetLocation(seed));
        CGEventSetTimestamp(event, CGEventGetTimestamp(seed));
        // Never reuse a sender PID, user identity, window route, or event source.
        for field in [39, 40, 91, 92] {
            CGEventSetIntegerValueField(event, field, 0);
        }
        for field in [41, 42, 43, 44, 45] {
            CGEventSetIntegerValueField(event, field, CGEventGetIntegerValueField(seed, field));
        }
        CFRelease(seed);
        CGEventPost(0, event);
        true
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSetType(event: Ref, kind: u32);
        fn CGEventSetDoubleValueField(event: Ref, field: u32, value: f64);
    }

    #[test]
    fn native_zoom_preserves_payload_and_rejects_forged_kind() {
        unsafe {
            let event = CGEventCreate(ptr::null_mut());
            assert!(!event.is_null());
            CGEventSetType(event, 29);
            CGEventSetIntegerValueField(event, 110, 8);
            CGEventSetIntegerValueField(event, 132, 2);
            CGEventSetDoubleValueField(event, 113, 0.375);
            let mut gesture = capture(event, 7).expect("zoom capture");
            CFRelease(event);
            assert_eq!(gesture.kind, 30);
            assert_eq!(reconstructed_kind(&gesture), Some(30));
            let amount = with_event(&gesture, |event| {
                #[link(name = "CoreGraphics", kind = "framework")]
                extern "C" {
                    fn CGEventGetDoubleValueField(event: Ref, field: u32) -> f64;
                }
                CGEventGetDoubleValueField(event, 113)
            });
            assert_eq!(amount, Some(0.375));
            gesture.kind = 18;
            assert_eq!(reconstructed_kind(&gesture), None);
        }
    }

    #[test]
    fn invalid_native_data_is_not_reconstructed() {
        for data in [vec![], vec![0], vec![0; 256]] {
            let gesture = MacGesture {
                sequence: 1,
                kind: 30,
                data,
            };
            assert_eq!(reconstructed_kind(&gesture), None);
        }
    }
}
