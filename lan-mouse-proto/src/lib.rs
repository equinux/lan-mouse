use input_event::{DockSwipe, Event as InputEvent, KeyboardEvent, PointerEvent};
use input_event::{MAX_MAC_GESTURE_SIZE, MacGesture};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    borrow::Borrow,
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

/// Maximum native gesture frame: type, sequence, AppKit kind, length, and data.
/// Existing pointer/keyboard events retain their original wire lengths.
pub const MAX_EVENT_SIZE: usize = 1 + 4 + 1 + 4 + MAX_MAC_GESTURE_SIZE;

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("truncated event")]
    TruncatedEvent,
    #[error("invalid Dock gesture")]
    InvalidGesture,
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main lan-mouse protocol event type
#[derive(Clone, Debug)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(Position),
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
    /// Build identification for the sending peer. Sent by the
    /// connect side once after the connection authenticates, and
    /// echoed back by the listen side in reply, so each end can
    /// display the peer's build hash and warn (soft) on mismatch.
    /// `commit` is the 8-byte ASCII short commit hash from
    /// `shadow_rs`'s `SHORT_COMMIT`. Old peers that don't
    /// recognize the event type silently skip it per the
    /// forward-compat handling in the receive loop.
    Hello { commit: [u8; 8] },
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
            ProtoEvent::Hello { commit } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                write!(f, "Hello({s})")
            }
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    Hello,
    DockSwipe,
    MacGesture,
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
                InputEvent::DockSwipe(_) => EventType::DockSwipe,
                InputEvent::MacGesture(_) => EventType::MacGesture,
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Hello { .. } => EventType::Hello,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        Self::try_from(&buf[..])
    }
}

impl TryFrom<&[u8]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(mut buf: &[u8]) -> Result<Self, Self::Error> {
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::MacGesture => {
                let sequence = decode_u32(&mut buf)?;
                let kind = decode_u8(&mut buf)?;
                let len = decode_u32(&mut buf)? as usize;
                if len == 0 || len > MAX_MAC_GESTURE_SIZE {
                    return Err(ProtocolError::InvalidGesture);
                }
                if len > buf.len() {
                    return Err(ProtocolError::TruncatedEvent);
                }
                let gesture = MacGesture {
                    sequence,
                    kind,
                    data: buf[..len].to_vec(),
                };
                if !gesture.is_valid() {
                    return Err(ProtocolError::InvalidGesture);
                }
                Ok(Self::Input(InputEvent::MacGesture(gesture)))
            }
            EventType::DockSwipe => {
                let swipe = DockSwipe {
                    motion: decode_u8(&mut buf)?,
                    serial: decode_u32(&mut buf)?,
                    sequence: decode_u32(&mut buf)?,
                    phase: decode_u8(&mut buf)?,
                    progress: decode_f64(&mut buf)?,
                    velocity: decode_f64(&mut buf)?,
                };
                if !swipe.is_valid() {
                    return Err(ProtocolError::InvalidGesture);
                }
                Ok(Self::Input(InputEvent::DockSwipe(swipe)))
            }
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(decode_u8(&mut buf)?.try_into()?)),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::Hello => {
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::Hello { commit })
            }
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        (&event).into()
    }
}

impl From<&ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: &ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::MacGesture(g) => {
                        encode_u32(buf, len, g.sequence);
                        encode_u8(buf, len, g.kind);
                        // Invalid local data is represented by a rejected empty
                        // frame, rather than allowing an oversized slice copy.
                        let bytes = if g.is_valid() { &g.data[..] } else { &[] };
                        encode_u32(buf, len, bytes.len() as u32);
                        buf[..bytes.len()].copy_from_slice(bytes);
                        *len += bytes.len();
                    }
                    InputEvent::DockSwipe(s) => {
                        encode_u8(buf, len, s.motion);
                        encode_u32(buf, len, s.serial);
                        encode_u32(buf, len, s.sequence);
                        encode_u8(buf, len, s.phase);
                        encode_f64(buf, len, s.progress);
                        encode_f64(buf, len, s.velocity);
                    }
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, *alive as u8),
                ProtoEvent::Enter(pos) => encode_u8(buf, len, *pos as u8),
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Hello { commit } => {
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at_checked(size_of::<$t>()).ok_or(ProtocolError::TruncatedEvent)?;
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: impl Borrow<$t>) {
                let src = n.borrow().to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dock_swipe_preserves_all_motions_phases_and_signed_progress() {
        for (motion, phase) in (1..=3).flat_map(|motion| [1, 2, 4, 8].map(|phase| (motion, phase)))
        {
            let swipe = DockSwipe {
                motion,
                serial: 42,
                sequence: 7,
                phase,
                progress: -0.37,
                velocity: -12.5,
            };
            let (buf, len) = ProtoEvent::Input(InputEvent::DockSwipe(swipe)).into();
            assert_eq!(len, 27);
            let ProtoEvent::Input(InputEvent::DockSwipe(decoded)) =
                ProtoEvent::try_from(buf).unwrap()
            else {
                panic!("wrong event");
            };
            assert_eq!(decoded, swipe);
        }
    }

    #[test]
    fn rejects_invalid_gesture_phases_and_nonfinite_values() {
        for (phase, progress, velocity) in [
            (0, 0.0, 0.0),
            (3, 0.0, 0.0),
            (2, f64::NAN, 0.0),
            (4, 0.0, f64::INFINITY),
        ] {
            let swipe = DockSwipe {
                motion: 1,
                serial: 1,
                sequence: 1,
                phase,
                progress,
                velocity,
            };
            let (buf, _) = ProtoEvent::Input(InputEvent::DockSwipe(swipe)).into();
            assert!(matches!(
                ProtoEvent::try_from(buf),
                Err(ProtocolError::InvalidGesture)
            ));
        }
    }

    #[test]
    fn rejects_unknown_dock_motions() {
        for motion in [0, 4, 255] {
            let swipe = DockSwipe {
                motion,
                serial: 1,
                sequence: 1,
                phase: 1,
                progress: 0.0,
                velocity: 0.0,
            };
            let (buf, _) = ProtoEvent::Input(InputEvent::DockSwipe(swipe)).into();
            assert!(matches!(
                ProtoEvent::try_from(buf),
                Err(ProtocolError::InvalidGesture)
            ));
        }
    }

    #[test]
    fn native_gestures_roundtrip_and_reject_truncated_frames() {
        for kind in [18, 19, 20, 22, 29, 30, 31, 32, 33, 34] {
            for size in [1, 128, MAX_MAC_GESTURE_SIZE] {
                let gesture = MacGesture {
                    sequence: 17,
                    kind,
                    data: vec![0x5a; size],
                };
                let event = ProtoEvent::Input(InputEvent::MacGesture(gesture.clone()));
                let (buf, len) = (&event).into();
                let ProtoEvent::Input(InputEvent::MacGesture(decoded)) =
                    ProtoEvent::try_from(&buf[..len]).unwrap()
                else {
                    panic!("wrong event");
                };
                assert_eq!(decoded, gesture);
                assert!(matches!(
                    ProtoEvent::try_from(&buf[..len - 1]),
                    Err(ProtocolError::TruncatedEvent)
                ));
            }
        }
    }

    #[test]
    fn native_gestures_reject_non_gesture_kinds_and_invalid_lengths() {
        for (kind, size) in [(10, 32), (30, 0), (30, MAX_MAC_GESTURE_SIZE + 1)] {
            let (buf, len) = ProtoEvent::Input(InputEvent::MacGesture(MacGesture {
                sequence: 1,
                kind,
                data: vec![1; size],
            }))
            .into();
            assert!(matches!(
                ProtoEvent::try_from(&buf[..len]),
                Err(ProtocolError::InvalidGesture)
            ));
        }
        for size in 0..21 {
            assert!(ProtoEvent::try_from(&vec![0; size][..]).is_err());
        }
    }

    #[test]
    fn legacy_motion_encoding_is_unchanged() {
        let (buf, len) = ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
            time: 123,
            dx: 1.5,
            dy: -2.5,
        }))
        .into();
        assert_eq!(len, 21);
        assert_eq!(buf[0], 0);
        let ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion { time, dx, dy })) =
            ProtoEvent::try_from(buf).unwrap()
        else {
            panic!("wrong event");
        };
        assert_eq!((time, dx, dy), (123, 1.5, -2.5));
    }
}
