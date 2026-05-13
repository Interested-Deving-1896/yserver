//! yserver-local input event enum.
//!
//! Deliberately minimal: keycodes, pointer deltas, button + state.
//! No keysym translation — that's xkbcommon's job and lives in C.

#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    KeyPress {
        keycode: u32,
    },
    KeyRelease {
        keycode: u32,
    },
    /// Relative pointer motion (mouse).
    PointerMotion {
        dx: f64,
        dy: f64,
    },
    /// Absolute pointer motion (tablet).  Coordinates are in 0..1 over the
    /// device's logical surface; the backend scales to scanout dimensions.
    PointerMotionAbsolute {
        x_norm: f64,
        y_norm: f64,
    },
    Button {
        code: u32,
        pressed: bool,
    },
    /// Pointer scroll wheel / two-finger / continuous scroll, in v120
    /// high-resolution units. 120 v120 ≈ one "click" of a discrete wheel.
    /// `dx_v120 > 0` is scroll-right, `dy_v120 > 0` is scroll-down (matches
    /// libinput's convention).
    PointerScroll {
        dx_v120: i32,
        dy_v120: i32,
    },
}
