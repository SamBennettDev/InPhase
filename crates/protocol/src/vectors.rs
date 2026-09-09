//! Golden input-packet vectors — the single source of truth for the
//! Rust <-> TypeScript protocol test suite (architecture report §19, Phase 3:
//! *"a golden Rust<->TypeScript protocol test vector suite"*).
//!
//! * `tests/vectors.rs` asserts every vector encodes to exactly `hex` and
//!   decodes back to the same [`InputPacket`].
//! * `examples/dump_vectors.rs` writes these to
//!   `web/src/input/protocol.vectors.json`, which the browser client's own
//!   test (`web/src/input/protocol.test.ts`) checks against.
//!
//! If you change the wire format, regenerate the JSON and bump
//! [`crate::input::INPUT_PROTOCOL_VERSION`].

use crate::input::{
    GamepadState, InputEvent, InputPacket, SnapshotState, FLAG_UNADJUSTED_MOVEMENT, MOD_CTRL,
    MOD_SHIFT,
};

/// One named vector.
pub struct GoldenVector {
    pub name: &'static str,
    pub packet: InputPacket,
    /// Lower-case hex of the exact expected wire bytes.
    pub hex: &'static str,
}

/// The full golden set. Keep names stable; the TS side keys on them.
pub fn golden() -> Vec<GoldenVector> {
    vec![
        GoldenVector {
            name: "mouse_move_unadjusted",
            packet: InputPacket::new(
                42,
                1_234_567,
                FLAG_UNADJUSTED_MOVEMENT,
                InputEvent::MouseMove {
                    dx: -3,
                    dy: 7,
                    wheel_x: 0,
                    wheel_y: -1,
                },
            ),
            hex: "010101002a00000087d6120000000000fdff07000000ffff",
        },
        GoldenVector {
            name: "mouse_buttons_left_right",
            packet: InputPacket::new(7, 0, 0, InputEvent::MouseButtons { buttons: 0b101 }),
            hex: "010200000700000000000000000000000500",
        },
        GoldenVector {
            name: "key_a_down_ctrl_shift",
            packet: InputPacket::new(
                1,
                1,
                0,
                InputEvent::Key {
                    physical_code: 0x001E,
                    down: true,
                    modifiers: MOD_CTRL | MOD_SHIFT,
                },
            ),
            hex: "010300000100000001000000000000001e000103",
        },
        GoldenVector {
            name: "gamepad_extremes",
            packet: InputPacket::new(
                100,
                9_999_999,
                0,
                InputEvent::Gamepad(GamepadState {
                    buttons: 0xDEAD_BEEF,
                    lx: -32768,
                    ly: 32767,
                    rx: 256,
                    ry: -256,
                    lt: 65535,
                    rt: 0,
                }),
            ),
            hex: "01040000640000007f96980000000000efbeadde0080ff7f000100ffffff0000",
        },
        GoldenVector {
            name: "snapshot_empty",
            packet: InputPacket::new(0, 0, 0, InputEvent::Snapshot(SnapshotState::default())),
            // 16-byte header + 6-byte payload (mb u16, mod u8, snap_flags u8, count u16).
            hex: "01050000000000000000000000000000000000000000",
        },
        GoldenVector {
            name: "snapshot_two_keys_with_gamepad",
            packet: InputPacket::new(
                55,
                42,
                0,
                InputEvent::Snapshot(SnapshotState {
                    mouse_buttons: 0b1,
                    modifiers: MOD_SHIFT,
                    held_keys: vec![0x001E, 0x0011],
                    gamepad: Some(GamepadState {
                        buttons: 1,
                        lx: 1,
                        ly: 2,
                        rx: 3,
                        ry: 4,
                        lt: 5,
                        rt: 6,
                    }),
                }),
            ),
            hex: concat!(
                "0105000037000000",                 // version,kind,flags(u16),seq(u32)
                "2a00000000000000",                 // client_monotonic_us(u64)
                "0100",                             // mouse_buttons(u16)
                "01",                               // modifiers(u8)
                "01",                               // snap_flags(u8) = GAMEPAD_PRESENT
                "0200",                             // held_key_count(u16)
                "1e001100",                         // held_keys[2]
                "01000000010002000300040005000600", // gamepad(u32,i16x4,u16x2)
            ),
        },
    ]
}
