//! Keyboard and pointer vocabulary.
//!
//! Keys travel as USB HID usage IDs from the keyboard/keypad page, the same
//! identifiers a physical keyboard reports. That choice is what keeps layouts
//! from mattering: a Windows hook gives a scancode, an X11 event gives a
//! keycode, and both convert to the same usage without either side needing to
//! agree on a keyboard layout, a locale, or a character.
//!
//! Sending characters instead — as some sharing tools do — is where mismatched
//! layouts and dead keys start producing the wrong letters.

use serde::{Deserialize, Serialize};

/// A physical key, as a USB HID usage ID on page 0x07.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Key(pub u16);

impl Key {
    /// Usage IDs 0xE0..=0xE7 are the eight modifier keys.
    pub const fn is_modifier(self) -> bool {
        self.0 >= 0xE0 && self.0 <= 0xE7
    }

    pub const LEFT_CTRL: Key = Key(0xE0);
    pub const LEFT_SHIFT: Key = Key(0xE1);
    pub const LEFT_ALT: Key = Key(0xE2);
    pub const LEFT_META: Key = Key(0xE3);
    pub const RIGHT_CTRL: Key = Key(0xE4);
    pub const RIGHT_SHIFT: Key = Key(0xE5);
    pub const RIGHT_ALT: Key = Key(0xE6);
    pub const RIGHT_META: Key = Key(0xE7);

    /// Every modifier, in usage order. Used when force-releasing state.
    pub const MODIFIERS: [Key; 8] = [
        Key::LEFT_CTRL,
        Key::LEFT_SHIFT,
        Key::LEFT_ALT,
        Key::LEFT_META,
        Key::RIGHT_CTRL,
        Key::RIGHT_SHIFT,
        Key::RIGHT_ALT,
        Key::RIGHT_META,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// The "back" thumb button.
    Back,
    /// The "forward" thumb button.
    Forward,
    Other(u8),
}

/// A scroll amount.
///
/// Units are 1/120 of a notch, matching what both Windows and modern Linux
/// input stacks report for high-resolution wheels. A classic notchy wheel
/// sends ±120; a touchpad or free-spinning wheel sends finer values, which is
/// what makes smooth scrolling possible rather than quantised jumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Scroll {
    pub dx: i32,
    pub dy: i32,
}

impl Scroll {
    /// One notch is 120 units.
    pub const NOTCH: i32 = 120;

    pub const fn new(dx: i32, dy: i32) -> Self {
        Self { dx, dy }
    }
}
