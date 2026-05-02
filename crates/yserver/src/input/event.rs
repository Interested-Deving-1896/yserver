//! yserver-local input event enum.
//!
//! Deliberately minimal: keycodes, pointer deltas, button + state.
//! No keysym translation — that's xkbcommon's job and lives in C.

#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    KeyPress { keycode: u32 },
    KeyRelease { keycode: u32 },
    PointerMotion { dx: f64, dy: f64 },
    Button { code: u32, pressed: bool },
}
