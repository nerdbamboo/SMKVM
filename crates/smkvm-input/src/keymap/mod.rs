//! Translating keys between the wire and what a platform wants.
//!
//! Keys cross the network as USB HID usage IDs, which describe a physical
//! position rather than a character. Each platform converts at its own edge, so
//! two machines never have to agree on a keyboard layout: pressing the key
//! where `Q` sits produces the key where `Q` sits, whatever either machine has
//! that key configured to type.

mod table;

use smkvm_proto::Key;

pub use table::HID_TO_EVDEV;

/// The Linux keycode for a HID usage.
pub fn hid_to_evdev(key: Key) -> Option<u16> {
    HID_TO_EVDEV
        .binary_search_by_key(&key.0, |(usage, _)| *usage)
        .ok()
        .map(|i| HID_TO_EVDEV[i].1)
}

/// The HID usage for a Linux keycode.
///
/// A handful of usages share a keycode, so this is the reverse of the first
/// usage that maps to it.
pub fn evdev_to_hid(code: u16) -> Option<Key> {
    HID_TO_EVDEV
        .iter()
        .find(|(_, c)| *c == code)
        .map(|(usage, _)| Key(*usage))
}

/// The X11 keycode for a HID usage.
///
/// Xorg driven by evdev offsets Linux keycodes by 8, and X11 keycodes are a
/// single byte. Going through the physical code rather than through keysyms is
/// what keeps the result independent of whatever layout the X server has
/// loaded.
pub fn hid_to_x11_keycode(key: Key) -> Option<u8> {
    let code = hid_to_evdev(key)?;
    u8::try_from(code + 8).ok()
}

/// The HID usage for an X11 keycode.
pub fn x11_keycode_to_hid(keycode: u8) -> Option<Key> {
    evdev_to_hid(u16::from(keycode).checked_sub(8)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_so_lookups_can_be_binary() {
        assert!(HID_TO_EVDEV.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn known_keys_land_where_the_kernel_puts_them() {
        assert_eq!(hid_to_evdev(Key(0x04)), Some(30)); // A
        assert_eq!(hid_to_evdev(Key(0x1E)), Some(2)); // 1
        assert_eq!(hid_to_evdev(Key(0x28)), Some(28)); // Enter
        assert_eq!(hid_to_evdev(Key::LEFT_CTRL), Some(29));
        assert_eq!(hid_to_evdev(Key::RIGHT_META), Some(126));
    }

    #[test]
    fn every_modifier_is_mapped() {
        for key in Key::MODIFIERS {
            assert!(hid_to_evdev(key).is_some(), "{key:?} has no keycode");
            assert!(
                hid_to_x11_keycode(key).is_some(),
                "{key:?} has no X keycode"
            );
        }
    }

    #[test]
    fn an_unknown_usage_is_reported_rather_than_guessed() {
        assert_eq!(hid_to_evdev(Key(0xFFFF)), None);
        assert_eq!(hid_to_evdev(Key(0x00)), None);
        assert_eq!(hid_to_evdev(Key(0xA5)), None);
    }

    #[test]
    fn x11_keycodes_round_trip() {
        for (usage, _) in HID_TO_EVDEV {
            let key = Key(usage);
            let Some(code) = hid_to_x11_keycode(key) else {
                continue;
            };
            // Several usages share a keycode, so the reverse may name the
            // other one; what must hold is that it maps back to the same code.
            let back = x11_keycode_to_hid(code).expect("a mapped code reverses");
            assert_eq!(hid_to_evdev(back), hid_to_evdev(key), "usage {usage:#06x}");
        }
    }

    #[test]
    fn every_keycode_fits_in_an_x11_byte() {
        for (usage, code) in HID_TO_EVDEV {
            assert!(
                code + 8 <= 255,
                "usage {usage:#06x} maps to keycode {code}, which X11 cannot express"
            );
        }
    }
}
