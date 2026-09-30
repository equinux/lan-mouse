use std::fmt::{self, Display};

pub mod error;
pub mod scancode;

#[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
mod libei;

// FIXME
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_BACK: u32 = 0x113;
pub const BTN_FORWARD: u32 = 0x114;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum PointerEvent {
    /// relative motion event
    Motion { time: u32, dx: f64, dy: f64 },
    /// mouse button event
    Button { time: u32, button: u32, state: u32 },
    /// axis event, scroll event for touchpads
    Axis { time: u32, axis: u8, value: f64 },
    /// discrete axis event, scroll event for mice - 120 = one scroll tick
    AxisDiscrete120 { axis: u8, value: i32 },
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum KeyboardEvent {
    /// a key press / release event
    Key { time: u32, key: u32, state: u8 },
    /// modifiers changed state
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
}

#[derive(PartialEq, Debug, Clone)]
pub enum Event {
    /// pointer event (motion / button / axis)
    Pointer(PointerEvent),
    /// keyboard events (key / modifiers)
    Keyboard(KeyboardEvent),
    /// Interactive macOS Dock gesture (Spaces, Mission Control, desktop, apps).
    DockSwipe(DockSwipe),
    /// Native macOS application gesture, including phased trackpad scrolling.
    MacGesture(MacGesture),
}

/// Limit native Quartz event data to keep allocations and datagrams bounded.
pub const MAX_MAC_GESTURE_SIZE: usize = 4096;

#[derive(PartialEq, Clone)]
pub struct MacGesture {
    pub sequence: u32,
    /// AppKit event type; the receiver verifies this against reconstructed data.
    pub kind: u8,
    pub data: Vec<u8>,
}

impl MacGesture {
    pub fn is_valid(&self) -> bool {
        matches!(self.kind, 18..=20 | 22 | 29..=34)
            && !self.data.is_empty()
            && self.data.len() <= MAX_MAC_GESTURE_SIZE
    }
}

impl fmt::Debug for MacGesture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MacGesture")
            .field("sequence", &self.sequence)
            .field("kind", &self.kind)
            .field("bytes", &self.data.len())
            .finish()
    }
}

#[cfg(target_os = "macos")]
pub mod macos_gesture;

#[derive(PartialEq, Debug, Clone, Copy)]
pub struct DockSwipe {
    /// HID motion: horizontal=1, vertical=2, pinch/spread=3.
    pub motion: u8,
    pub serial: u32,
    pub sequence: u32,
    /// HID phases: began=1, changed=2, ended=4, cancelled=8.
    pub phase: u8,
    pub progress: f64,
    pub velocity: f64,
}

impl DockSwipe {
    pub fn is_valid(self) -> bool {
        matches!(self.motion, 1..=3)
            && matches!(self.phase, 1 | 2 | 4 | 8)
            && self.progress.is_finite()
            && self.velocity.is_finite()
    }
}

impl Display for PointerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PointerEvent::Motion { time: _, dx, dy } => write!(f, "motion({dx},{dy})"),
            PointerEvent::Button {
                time: _,
                button,
                state,
            } => {
                let str = match *button {
                    BTN_LEFT => Some("left"),
                    BTN_RIGHT => Some("right"),
                    BTN_MIDDLE => Some("middle"),
                    BTN_FORWARD => Some("forward"),
                    BTN_BACK => Some("back"),
                    _ => None,
                };
                if let Some(button) = str {
                    write!(f, "button({button}, {state})")
                } else {
                    write!(f, "button({button}, {state}")
                }
            }
            PointerEvent::Axis {
                time: _,
                axis,
                value,
            } => write!(f, "scroll({axis}, {value})"),
            PointerEvent::AxisDiscrete120 { axis, value } => {
                write!(f, "scroll-120 ({axis}, {value})")
            }
        }
    }
}

impl Display for KeyboardEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyboardEvent::Key {
                time: _,
                key,
                state,
            } => {
                let scan = scancode::Linux::try_from(*key);
                if let Ok(scan) = scan {
                    write!(f, "key({scan:?}, {state})")
                } else {
                    write!(f, "key({key}, {state})")
                }
            }
            KeyboardEvent::Modifiers {
                depressed: mods_depressed,
                latched: mods_latched,
                locked: mods_locked,
                group,
            } => write!(
                f,
                "modifiers({mods_depressed},{mods_latched},{mods_locked},{group})"
            ),
        }
    }
}

impl Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Pointer(p) => write!(f, "{p}"),
            Event::Keyboard(k) => write!(f, "{k}"),
            Event::DockSwipe(s) => write!(f, "dock-swipe({s:?})"),
            Event::MacGesture(g) => write!(f, "mac-gesture({g:?})"),
        }
    }
}
