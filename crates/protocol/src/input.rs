//! Binary input packet **v1** (architecture report §12.2).
//!
//! Wire format, **little-endian**, 16-byte header followed by a kind-specific
//! payload:
//!
//! ```text
//! Header (16 bytes)
//!   version:              u8   // == INPUT_PROTOCOL_VERSION (1)
//!   kind:                 u8   // InputKind
//!   flags:                u16  // FLAG_* bitset
//!   sequence:             u32  // per-connection monotonic, wraps
//!   client_monotonic_us:  u64  // client performance clock, microseconds
//!
//! Payloads
//!   MouseMove     dx:i16, dy:i16, wheel_x:i16, wheel_y:i16
//!   MouseButtons  buttons:u16
//!   Key           physical_code:u16, down:u8, modifiers:u8
//!   Gamepad       buttons:u32, lx:i16, ly:i16, rx:i16, ry:i16, lt:u16, rt:u16
//!   Snapshot      mouse_buttons:u16, modifiers:u8, snap_flags:u8,
//!                 held_key_count:u16, held_keys:[u16; held_key_count],
//!                 [if GAMEPAD_PRESENT] Gamepad payload
//!   Touch         pointer_id:u32, phase:u8, _pad:u8, x:u16, y:u16   (experimental, Phase 6)
//! ```
//!
//! Design intent from the report: *"A 30 ms-old mouse packet is worse than a
//! dropped packet."* The host therefore discards any packet whose `sequence` is
//! older than the newest one it has processed, and treats [`InputEvent::Snapshot`]
//! as the authoritative state so a single lost key-up cannot wedge a key.

use thiserror::Error;

/// Wire version for the binary input protocol. Bumped on any layout change.
pub const INPUT_PROTOCOL_VERSION: u8 = 1;

/// Size of the fixed packet header in bytes.
pub const INPUT_HEADER_LEN: usize = 16;

// ---- header flag bits -------------------------------------------------------

/// The mouse delta was produced with `movementX/Y` from an unadjusted-movement
/// Pointer Lock (raw device movement, no OS pointer acceleration) — §12.1.
pub const FLAG_UNADJUSTED_MOVEMENT: u16 = 0x0001;
/// The key event is an auto-repeat, not a fresh physical transition.
pub const FLAG_KEY_REPEAT: u16 = 0x0002;

// ---- modifier bits (Key + Snapshot) --------------------------------------

pub const MOD_SHIFT: u8 = 0x01;
pub const MOD_CTRL: u8 = 0x02;
pub const MOD_ALT: u8 = 0x04;
pub const MOD_META: u8 = 0x08;

// ---- snapshot sub-flags ----------------------------------------------------

/// A [`GamepadState`] payload follows the held-keys array in a snapshot.
pub const SNAP_GAMEPAD_PRESENT: u8 = 0x01;

/// Discriminant byte for the packet payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum InputKind {
    MouseMove = 1,
    MouseButtons = 2,
    Key = 3,
    Gamepad = 4,
    Snapshot = 5,
    /// Experimental; wired end-to-end in Phase 6 (§19).
    Touch = 6,
}

impl InputKind {
    pub fn from_u8(v: u8) -> Result<Self, InputCodecError> {
        Ok(match v {
            1 => Self::MouseMove,
            2 => Self::MouseButtons,
            3 => Self::Key,
            4 => Self::Gamepad,
            5 => Self::Snapshot,
            6 => Self::Touch,
            other => return Err(InputCodecError::UnknownKind(other)),
        })
    }
}

/// Fixed 16-byte packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputHeader {
    pub version: u8,
    pub kind: InputKind,
    pub flags: u16,
    pub sequence: u32,
    pub client_monotonic_us: u64,
}

/// Canonical gamepad snapshot (§12.2). Axes are normalised to `i16`
/// full-scale; triggers are `u16` full-scale. Button bits follow the
/// browser Gamepad API "standard" mapping order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GamepadState {
    pub buttons: u32,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u16,
    pub rt: u16,
}

/// Authoritative full input state, sent periodically (30–60 Hz) so a lost
/// transition cannot leave a stuck key/button (§12.2).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotState {
    pub mouse_buttons: u16,
    pub modifiers: u8,
    /// Physical key codes currently held (browser `KeyboardEvent.code` mapped to
    /// a stable numeric table — see `web/src/input/keycodes.ts`).
    pub held_keys: Vec<u16>,
    pub gamepad: Option<GamepadState>,
}

/// A decoded input event payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    MouseMove {
        dx: i16,
        dy: i16,
        wheel_x: i16,
        wheel_y: i16,
    },
    MouseButtons {
        buttons: u16,
    },
    Key {
        physical_code: u16,
        down: bool,
        modifiers: u8,
    },
    Gamepad(GamepadState),
    Snapshot(SnapshotState),
    Touch {
        pointer_id: u32,
        phase: u8,
        x: u16,
        y: u16,
    },
}

impl InputEvent {
    pub fn kind(&self) -> InputKind {
        match self {
            InputEvent::MouseMove { .. } => InputKind::MouseMove,
            InputEvent::MouseButtons { .. } => InputKind::MouseButtons,
            InputEvent::Key { .. } => InputKind::Key,
            InputEvent::Gamepad(_) => InputKind::Gamepad,
            InputEvent::Snapshot(_) => InputKind::Snapshot,
            InputEvent::Touch { .. } => InputKind::Touch,
        }
    }
}

/// A fully decoded input packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputPacket {
    pub header: InputHeader,
    pub event: InputEvent,
}

impl InputPacket {
    /// Build a packet with the current protocol version.
    pub fn new(sequence: u32, client_monotonic_us: u64, flags: u16, event: InputEvent) -> Self {
        Self {
            header: InputHeader {
                version: INPUT_PROTOCOL_VERSION,
                kind: event.kind(),
                flags,
                sequence,
                client_monotonic_us,
            },
            event,
        }
    }

    /// Serialise to the wire format.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(INPUT_HEADER_LEN + 24);
        w.u8(self.header.version);
        w.u8(self.event.kind() as u8);
        w.u16(self.header.flags);
        w.u32(self.header.sequence);
        w.u64(self.header.client_monotonic_us);
        match &self.event {
            InputEvent::MouseMove {
                dx,
                dy,
                wheel_x,
                wheel_y,
            } => {
                w.i16(*dx);
                w.i16(*dy);
                w.i16(*wheel_x);
                w.i16(*wheel_y);
            }
            InputEvent::MouseButtons { buttons } => w.u16(*buttons),
            InputEvent::Key {
                physical_code,
                down,
                modifiers,
            } => {
                w.u16(*physical_code);
                w.u8(u8::from(*down));
                w.u8(*modifiers);
            }
            InputEvent::Gamepad(g) => write_gamepad(&mut w, g),
            InputEvent::Snapshot(s) => {
                w.u16(s.mouse_buttons);
                w.u8(s.modifiers);
                w.u8(if s.gamepad.is_some() {
                    SNAP_GAMEPAD_PRESENT
                } else {
                    0
                });
                w.u16(u16::try_from(s.held_keys.len()).unwrap_or(u16::MAX));
                for k in s.held_keys.iter().take(u16::MAX as usize) {
                    w.u16(*k);
                }
                if let Some(g) = &s.gamepad {
                    write_gamepad(&mut w, g);
                }
            }
            InputEvent::Touch {
                pointer_id,
                phase,
                x,
                y,
            } => {
                w.u32(*pointer_id);
                w.u8(*phase);
                w.u8(0);
                w.u16(*x);
                w.u16(*y);
            }
        }
        w.into_inner()
    }

    /// Parse the wire format. Rejects unknown versions and truncated buffers so
    /// a stale client asset fails loudly (§16).
    pub fn decode(bytes: &[u8]) -> Result<Self, InputCodecError> {
        if bytes.len() < INPUT_HEADER_LEN {
            return Err(InputCodecError::Truncated {
                need: INPUT_HEADER_LEN,
                got: bytes.len(),
            });
        }
        let mut r = Reader::new(bytes);
        let version = r.u8()?;
        if version != INPUT_PROTOCOL_VERSION {
            return Err(InputCodecError::UnsupportedVersion(version));
        }
        let kind = InputKind::from_u8(r.u8()?)?;
        let flags = r.u16()?;
        let sequence = r.u32()?;
        let client_monotonic_us = r.u64()?;

        let event = match kind {
            InputKind::MouseMove => InputEvent::MouseMove {
                dx: r.i16()?,
                dy: r.i16()?,
                wheel_x: r.i16()?,
                wheel_y: r.i16()?,
            },
            InputKind::MouseButtons => InputEvent::MouseButtons { buttons: r.u16()? },
            InputKind::Key => InputEvent::Key {
                physical_code: r.u16()?,
                down: r.u8()? != 0,
                modifiers: r.u8()?,
            },
            InputKind::Gamepad => InputEvent::Gamepad(read_gamepad(&mut r)?),
            InputKind::Snapshot => {
                let mouse_buttons = r.u16()?;
                let modifiers = r.u8()?;
                let snap_flags = r.u8()?;
                let held_count = r.u16()? as usize;
                let mut held_keys = Vec::with_capacity(held_count.min(1024));
                for _ in 0..held_count {
                    held_keys.push(r.u16()?);
                }
                let gamepad = if snap_flags & SNAP_GAMEPAD_PRESENT != 0 {
                    Some(read_gamepad(&mut r)?)
                } else {
                    None
                };
                InputEvent::Snapshot(SnapshotState {
                    mouse_buttons,
                    modifiers,
                    held_keys,
                    gamepad,
                })
            }
            InputKind::Touch => {
                let pointer_id = r.u32()?;
                let phase = r.u8()?;
                let _pad = r.u8()?;
                InputEvent::Touch {
                    pointer_id,
                    phase,
                    x: r.u16()?,
                    y: r.u16()?,
                }
            }
        };

        Ok(Self {
            header: InputHeader {
                version,
                kind,
                flags,
                sequence,
                client_monotonic_us,
            },
            event,
        })
    }
}

fn write_gamepad(w: &mut Writer, g: &GamepadState) {
    w.u32(g.buttons);
    w.i16(g.lx);
    w.i16(g.ly);
    w.i16(g.rx);
    w.i16(g.ry);
    w.u16(g.lt);
    w.u16(g.rt);
}

fn read_gamepad(r: &mut Reader) -> Result<GamepadState, InputCodecError> {
    Ok(GamepadState {
        buttons: r.u32()?,
        lx: r.i16()?,
        ly: r.i16()?,
        rx: r.i16()?,
        ry: r.i16()?,
        lt: r.u16()?,
        rt: r.u16()?,
    })
}

/// Errors from [`InputPacket::decode`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InputCodecError {
    #[error("packet truncated: need at least {need} bytes, got {got}")]
    Truncated { need: usize, got: usize },
    #[error("unsupported input protocol version {0} (this host speaks {INPUT_PROTOCOL_VERSION})")]
    UnsupportedVersion(u8),
    #[error("unknown input kind discriminant {0}")]
    UnknownKind(u8),
}

// ---- tiny endian-explicit reader/writer -----------------------------------

struct Writer(Vec<u8>);
impl Writer {
    fn with_capacity(n: usize) -> Self {
        Self(Vec::with_capacity(n))
    }
    fn into_inner(self) -> Vec<u8> {
        self.0
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i16(&mut self, v: i16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn take<const N: usize>(&mut self) -> Result<[u8; N], InputCodecError> {
        let end = self.pos + N;
        if end > self.buf.len() {
            return Err(InputCodecError::Truncated {
                need: end,
                got: self.buf.len(),
            });
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.buf[self.pos..end]);
        self.pos = end;
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, InputCodecError> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, InputCodecError> {
        Ok(u16::from_le_bytes(self.take::<2>()?))
    }
    fn i16(&mut self) -> Result<i16, InputCodecError> {
        Ok(i16::from_le_bytes(self.take::<2>()?))
    }
    fn u32(&mut self) -> Result<u32, InputCodecError> {
        Ok(u32::from_le_bytes(self.take::<4>()?))
    }
    fn u64(&mut self) -> Result<u64, InputCodecError> {
        Ok(u64::from_le_bytes(self.take::<8>()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(ev: InputEvent) {
        let pkt = InputPacket::new(42, 1_234_567, 0, ev.clone());
        let bytes = pkt.encode();
        let back = InputPacket::decode(&bytes).expect("decode");
        assert_eq!(pkt, back);
        assert_eq!(back.event, ev);
    }

    #[test]
    fn roundtrip_all_kinds() {
        roundtrip(InputEvent::MouseMove {
            dx: -3,
            dy: 7,
            wheel_x: 0,
            wheel_y: -1,
        });
        roundtrip(InputEvent::MouseButtons { buttons: 0b101 });
        roundtrip(InputEvent::Key {
            physical_code: 0x001E,
            down: true,
            modifiers: MOD_CTRL | MOD_SHIFT,
        });
        roundtrip(InputEvent::Gamepad(GamepadState {
            buttons: 0xDEAD_BEEF,
            lx: -32768,
            ly: 32767,
            rx: 100,
            ry: -100,
            lt: 65535,
            rt: 0,
        }));
        roundtrip(InputEvent::Snapshot(SnapshotState {
            mouse_buttons: 0b1,
            modifiers: MOD_ALT,
            held_keys: vec![0x001E, 0x001F, 0x0020],
            gamepad: Some(GamepadState::default()),
        }));
        roundtrip(InputEvent::Snapshot(SnapshotState::default()));
        roundtrip(InputEvent::Touch {
            pointer_id: 9,
            phase: 1,
            x: 40000,
            y: 12345,
        });
    }

    #[test]
    fn rejects_bad_version() {
        let mut bytes = InputPacket::new(1, 1, 0, InputEvent::MouseButtons { buttons: 0 }).encode();
        bytes[0] = 2;
        assert_eq!(
            InputPacket::decode(&bytes),
            Err(InputCodecError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn rejects_truncated_header() {
        assert!(matches!(
            InputPacket::decode(&[1, 2, 3]),
            Err(InputCodecError::Truncated { .. })
        ));
    }

    #[test]
    fn header_is_16_bytes() {
        let bytes = InputPacket::new(0, 0, 0, InputEvent::MouseButtons { buttons: 0 }).encode();
        assert_eq!(bytes.len(), INPUT_HEADER_LEN + 2);
    }
}
