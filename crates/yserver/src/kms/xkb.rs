use xkbcommon::xkb::Keymap;

/// XKB UseExtension reply (minor=0). Fixed 32 bytes.
/// Reports success and server protocol version 1.0.
pub(super) fn reply_use_extension() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // success
    // [2..4] sequence: rewritten by caller
    // [4..8] extra length in 4-byte units = 0
    r[8] = 1; // server-major
    r[9] = 0; // server-minor
    r
}

/// XKB GetControls reply (minor=6). Fixed 92 bytes.
/// Reports repeat delay/interval and enable flags.
pub(super) fn reply_get_controls(_keymap: &Keymap) -> Vec<u8> {
    let mut r = vec![0u8; 92];
    r[0] = 1; // reply type
    // [4..8] extra length = (92-32)/4 = 15
    r[4..8].copy_from_slice(&15u32.to_le_bytes());
    // Repeat delay = 500ms, interval = 33ms (≈30 Hz)
    let delay: u16 = 500;
    let interval: u16 = 33;
    r[12..14].copy_from_slice(&delay.to_le_bytes());
    r[14..16].copy_from_slice(&interval.to_le_bytes());
    // EnabledControls: RepeatKeys (bit 28) | PerKeyRepeat (bit 0)
    let flags: u32 = 0x1000_0001;
    r[32..36].copy_from_slice(&flags.to_le_bytes());
    r
}

/// XKB GetMap reply (minor=8). Per xkbproto, fixed 40 bytes —
/// `sz_xkbGetMapReply`. Reports min/max keycode and present=0 so
/// no type/sym/mod tables follow. xcb-rs (used by wezterm) is
/// strict about the 40-byte size; libxcb is more lenient.
pub(super) fn reply_get_map(keymap: &Keymap) -> Vec<u8> {
    #[allow(clippy::cast_possible_truncation)]
    let min_kc = keymap.min_keycode().raw() as u8;
    #[allow(clippy::cast_possible_truncation)]
    let max_kc = keymap.max_keycode().raw() as u8;
    let mut r = vec![0u8; 40];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = (40-32)/4 = 2
    r[4..8].copy_from_slice(&2u32.to_le_bytes());
    // [8..10] pad0[2]
    r[10] = min_kc;
    r[11] = max_kc;
    // [12..14] present = 0 (no tables follow)
    // [14..38] all the firstX/nX/totalX fields = 0
    // [38..40] virtualMods = 0
    r
}

/// XKB GetNames reply (minor=17). Empty name lists.
pub(super) fn reply_get_names(_keymap: &Keymap) -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1;
    // which=0 → no name arrays follow; extra length = 0
    r
}

/// XKB GetCompatMap reply (minor=10). Per xkbproto, fixed 32 bytes
/// — `sz_xkbGetCompatMapReply`. Empty compat map (no SI entries
/// follow).
pub(super) fn reply_get_compat_map() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply type
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = 0
    // [8] groupsRtrn = 0
    // [9] pad1
    // [10..12] firstSIRtrn = 0
    // [12..14] nSIRtrn = 0
    // [14..16] nTotalSI = 0
    // [16..32] pad2[16] = 0
    r
}

/// XKB GetDeviceInfo reply (minor=24). Per xkbproto, fixed 32 bytes
/// — `sz_xkbGetDeviceInfoReply`. Empty: no LED feedbacks, no
/// buttons, no name. Verified via `sizeof` against the real header.
pub(super) fn reply_get_device_info() -> Vec<u8> {
    let mut r = vec![0u8; 32];
    r[0] = 1; // reply
    r[1] = 1; // deviceID = 1
    // [4..8] extra length = 0
    // [8..10] present, [10..12] supported, [12..14] unsupported = 0
    // [14..16] nDeviceLedFBs = 0
    // [16] firstBtnWanted, [17] nBtnsWanted
    // [18] firstBtnRtrn, [19] nBtnsRtrn
    // [20..22] totalBtns = 0
    // [22] hasOwnState
    // [23] (padding/alignment)
    // [24..26] dfltKbdFB, [26..28] dfltLedFB
    // [28..32] devType atom = 0
    r
}

/// Minimal all-zero 32-byte reply for XKB minors that clients tolerate silently.
/// Only use for minors with no required reply content (e.g. SetControls has none).
pub(super) fn reply_minimal(minor: u8) -> Vec<u8> {
    log::debug!("xkb: unimplemented minor {minor}, returning minimal reply");
    let mut r = vec![0u8; 32];
    r[0] = 1;
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_keymap() -> xkbcommon::xkb::Keymap {
        let ctx = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        xkbcommon::xkb::Keymap::new_from_names(
            &ctx,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &ctx,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .expect("test xkb keymap")
    }

    #[test]
    fn use_extension_reply_length() {
        assert_eq!(reply_use_extension().len(), 32);
    }

    #[test]
    fn use_extension_success_flag() {
        let r = reply_use_extension();
        assert_eq!(r[1], 1, "success must be 1");
    }

    #[test]
    fn get_controls_reply_length() {
        let km = test_keymap();
        assert_eq!(reply_get_controls(&km).len(), 92);
    }

    #[test]
    fn get_map_reply_size_40() {
        let km = test_keymap();
        let r = reply_get_map(&km);
        assert_eq!(r.len(), 40, "sz_xkbGetMapReply = 40 per xkbproto");
        assert!(r[10] <= r[11], "min_keycode <= max_keycode");
    }

    #[test]
    fn get_compat_map_reply_size_32() {
        assert_eq!(reply_get_compat_map().len(), 32);
    }

    #[test]
    fn get_device_info_reply_size_32() {
        assert_eq!(reply_get_device_info().len(), 32);
    }
}
