//! Per-opcode length specifications for X11 core protocol requests
//! (opcodes 1–127).
//!
//! Each request has a wire length expressed in 4-byte units, declared in
//! the request header. The X11 protocol defines, per opcode, either a
//! fixed length or a minimum length plus a variable tail. xts5 probes
//! both under-length and (for fixed-length opcodes) over-length headers
//! and expects the server to reply with `BadLength`.

/// Length contract for a single core opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LenSpec {
    /// The request is exactly this many 4-byte units.
    Fixed(u32),
    /// The request is at least this many 4-byte units; the tail is
    /// variable (list, string, value-list, image data, ...).
    AtLeast(u32),
}

/// Return the length contract for a core X11 opcode (1–127), or `None`
/// for opcodes outside that range or that we don't enforce.
#[must_use]
pub fn core_request_length(opcode: u8) -> Option<LenSpec> {
    use LenSpec::{AtLeast, Fixed};
    Some(match opcode {
        1 => AtLeast(8),   // CreateWindow         8 + n
        2 => AtLeast(3),   // ChangeWindowAttrs    3 + n
        3 => Fixed(2),     // GetWindowAttributes
        4 => Fixed(2),     // DestroyWindow
        5 => Fixed(2),     // DestroySubwindows
        6 => Fixed(2),     // ChangeSaveSet
        7 => Fixed(4),     // ReparentWindow
        8 => Fixed(2),     // MapWindow
        9 => Fixed(2),     // MapSubwindows
        10 => Fixed(2),    // UnmapWindow
        11 => Fixed(2),    // UnmapSubwindows
        12 => AtLeast(3),  // ConfigureWindow      3 + n
        13 => Fixed(2),    // CirculateWindow
        14 => Fixed(2),    // GetGeometry
        15 => Fixed(2),    // QueryTree
        16 => AtLeast(2),  // InternAtom           2 + (n+p)/4
        17 => Fixed(2),    // GetAtomName
        18 => AtLeast(6),  // ChangeProperty       6 + (n+p)/4
        19 => Fixed(3),    // DeleteProperty
        20 => Fixed(6),    // GetProperty
        21 => Fixed(2),    // ListProperties
        22 => Fixed(4),    // SetSelectionOwner
        23 => Fixed(2),    // GetSelectionOwner
        24 => Fixed(6),    // ConvertSelection
        25 => Fixed(11),   // SendEvent
        26 => Fixed(6),    // GrabPointer
        27 => Fixed(2),    // UngrabPointer
        28 => Fixed(6),    // GrabButton
        29 => Fixed(3),    // UngrabButton
        30 => Fixed(4),    // ChangeActivePointerGrab
        31 => Fixed(4),    // GrabKeyboard
        32 => Fixed(2),    // UngrabKeyboard
        33 => Fixed(4),    // GrabKey
        34 => Fixed(3),    // UngrabKey
        35 => Fixed(2),    // AllowEvents
        36 => Fixed(1),    // GrabServer
        37 => Fixed(1),    // UngrabServer
        38 => Fixed(2),    // QueryPointer
        39 => Fixed(4),    // GetMotionEvents
        40 => Fixed(4),    // TranslateCoordinates
        41 => Fixed(6),    // WarpPointer
        42 => Fixed(3),    // SetInputFocus
        43 => Fixed(1),    // GetInputFocus
        44 => Fixed(1),    // QueryKeymap
        45 => AtLeast(3),  // OpenFont             3 + (n+p)/4
        46 => Fixed(2),    // CloseFont
        47 => Fixed(2),    // QueryFont
        48 => AtLeast(2),  // QueryTextExtents     2 + (2n+p)/4
        49 => AtLeast(2),  // ListFonts            2 + (n+p)/4
        50 => AtLeast(2),  // ListFontsWithInfo    2 + (n+p)/4
        51 => AtLeast(2),  // SetFontPath          2 + (n+p)/4
        52 => Fixed(1),    // GetFontPath
        53 => Fixed(4),    // CreatePixmap
        54 => Fixed(2),    // FreePixmap
        55 => AtLeast(4),  // CreateGC             4 + n
        56 => AtLeast(3),  // ChangeGC             3 + n
        57 => Fixed(4),    // CopyGC
        58 => AtLeast(3),  // SetDashes            3 + (n+p)/4
        59 => AtLeast(3),  // SetClipRectangles    3 + 2n
        60 => Fixed(2),    // FreeGC
        61 => Fixed(4),    // ClearArea
        62 => Fixed(7),    // CopyArea
        63 => Fixed(8),    // CopyPlane
        64 => AtLeast(3),  // PolyPoint            3 + n
        65 => AtLeast(3),  // PolyLine             3 + n
        66 => AtLeast(3),  // PolySegment          3 + 2n
        67 => AtLeast(3),  // PolyRectangle        3 + 2n
        68 => AtLeast(3),  // PolyArc              3 + 3n
        69 => AtLeast(4),  // FillPoly             4 + n
        70 => AtLeast(3),  // PolyFillRectangle    3 + 2n
        71 => AtLeast(3),  // PolyFillArc          3 + 3n
        72 => AtLeast(6),  // PutImage             6 + (n+p)/4
        73 => Fixed(5),    // GetImage
        74 => AtLeast(4),  // PolyText8            4 + (n+p)/4
        75 => AtLeast(4),  // PolyText16           4 + (n+p)/4
        76 => AtLeast(4),  // ImageText8           4 + (n+p)/4
        77 => AtLeast(4),  // ImageText16          4 + (n+p)/4
        78 => Fixed(4),    // CreateColormap
        79 => Fixed(2),    // FreeColormap
        80 => Fixed(3),    // CopyColormapAndFree
        81 => Fixed(2),    // InstallColormap
        82 => Fixed(2),    // UninstallColormap
        83 => Fixed(2),    // ListInstalledColormaps
        84 => Fixed(4),    // AllocColor
        85 => AtLeast(3),  // AllocNamedColor      3 + (n+p)/4
        86 => Fixed(3),    // AllocColorCells
        87 => Fixed(4),    // AllocColorPlanes
        88 => AtLeast(3),  // FreeColors           3 + n
        89 => AtLeast(1),  // StoreColors          1 + 3n
        90 => AtLeast(4),  // StoreNamedColor      4 + (n+p)/4
        91 => AtLeast(2),  // QueryColors          2 + n
        92 => AtLeast(3),  // LookupColor          3 + (n+p)/4
        93 => Fixed(8),    // CreateCursor
        94 => Fixed(8),    // CreateGlyphCursor
        95 => Fixed(2),    // FreeCursor
        96 => Fixed(5),    // RecolorCursor
        97 => Fixed(3),    // QueryBestSize
        98 => AtLeast(2),  // QueryExtension       2 + (n+p)/4
        99 => Fixed(1),    // ListExtensions
        100 => AtLeast(2), // ChangeKeyboardMapping 2 + nm
        101 => Fixed(2),   // GetKeyboardMapping
        102 => AtLeast(2), // ChangeKeyboardControl 2 + n
        103 => Fixed(1),   // GetKeyboardControl
        104 => Fixed(1),   // Bell
        105 => Fixed(3),   // ChangePointerControl
        106 => Fixed(1),   // GetPointerControl
        107 => Fixed(3),   // SetScreenSaver
        108 => Fixed(1),   // GetScreenSaver
        109 => AtLeast(2), // ChangeHosts          2 + (n+p)/4
        110 => Fixed(1),   // ListHosts
        111 => Fixed(1),   // SetAccessControl
        112 => Fixed(1),   // SetCloseDownMode
        113 => Fixed(2),   // KillClient
        114 => AtLeast(3), // RotateProperties     3 + n
        115 => Fixed(1),   // ForceScreenSaver
        116 => AtLeast(1), // SetPointerMapping    1 + (n+p)/4
        117 => Fixed(1),   // GetPointerMapping
        118 => AtLeast(1), // SetModifierMapping   1 + 2n
        119 => Fixed(1),   // GetModifierMapping
        127 => AtLeast(1), // NoOperation          1 + n
        _ => return None,
    })
}

/// Returns `true` if `length_units` (the value carried in the request
/// header, possibly extended via BIG-REQUESTS) satisfies the spec for
/// `opcode`. Opcodes outside the core range or unknown to us return
/// `true` (the dispatcher decides).
#[must_use]
pub fn validate_core_request_length(opcode: u8, length_units: u32) -> bool {
    match core_request_length(opcode) {
        Some(LenSpec::Fixed(n)) => length_units == n,
        Some(LenSpec::AtLeast(n)) => length_units >= n,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{LenSpec, core_request_length, validate_core_request_length};

    #[test]
    fn fixed_opcode_rejects_under_and_over() {
        // GetGeometry is fixed at 2 units.
        assert!(!validate_core_request_length(14, 1));
        assert!(validate_core_request_length(14, 2));
        assert!(!validate_core_request_length(14, 3));
    }

    #[test]
    fn variable_opcode_rejects_under_only() {
        // CreateWindow is at least 8 units.
        assert!(!validate_core_request_length(1, 7));
        assert!(validate_core_request_length(1, 8));
        assert!(validate_core_request_length(1, 100));
    }

    #[test]
    fn extension_opcodes_pass_through() {
        // 128+ are extensions; we don't enforce here.
        assert!(core_request_length(128).is_none());
        assert!(core_request_length(146).is_none());
        assert!(validate_core_request_length(128, 1));
    }

    #[test]
    fn alloc_color_is_fixed_4() {
        assert_eq!(core_request_length(84), Some(LenSpec::Fixed(4)));
    }

    #[test]
    fn send_event_is_fixed_11() {
        assert_eq!(core_request_length(25), Some(LenSpec::Fixed(11)));
    }
}
