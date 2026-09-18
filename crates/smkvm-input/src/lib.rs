//! Putting input into a machine, and reading its monitors.
//!
//! Platforms provide the raw primitives through [`Inject`]. Everything above
//! that — in particular remembering what is currently held down — lives in
//! [`Tracked`], in one place, so the behaviour is identical on every platform
//! and can be tested without a display server.
//!
//! That bookkeeping is the whole answer to keys sticking. A modifier held while
//! the cursor moves to another machine, or a link that drops mid-chord, leaves
//! a key down forever unless something knows what it pressed and lets go.

// Talking to a platform's input APIs means calling into them, which is the one
// place unsafe is unavoidable. It is confined to the platform backends; every
// other module is held to the same rule this crate started with.
#![deny(unsafe_code)]

pub mod keymap;
pub mod platform;

use std::collections::BTreeSet;

use smkvm_proto::{Key, MouseButton, Scroll};

pub type Result<T> = std::result::Result<T, InputError>;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("HID usage {0:#06x} has no key on this platform")]
    UnmappedKey(u16),
    #[error("mouse button {0:?} has no equivalent on this platform")]
    UnmappedButton(MouseButton),
    #[error("display server: {0}")]
    Display(String),
    #[error("{0} is not available on this platform")]
    Unsupported(&'static str),
}

/// The raw input primitives a platform must provide.
///
/// Implementations do exactly what they are told and keep no state; deciding
/// what to press and what to let go of is [`Tracked`]'s job.
pub trait Inject {
    /// Place the pointer at a position in this machine's own coordinates.
    fn move_to(&mut self, x: i32, y: i32) -> Result<()>;
    fn button(&mut self, button: MouseButton, down: bool) -> Result<()>;
    fn wheel(&mut self, scroll: Scroll) -> Result<()>;
    fn key(&mut self, key: Key, down: bool) -> Result<()>;
    /// Push anything buffered to the display server.
    fn flush(&mut self) -> Result<()>;

    /// Take the pointer out of sight, because this machine no longer has the
    /// cursor.
    ///
    /// Left where it was, it sits in the middle of whatever the person is
    /// reading, looking for all the world as though it were still theirs to
    /// move. The default does nothing, for backends with no way to do it.
    fn hide_cursor(&mut self) -> Result<()> {
        Ok(())
    }

    /// Put the pointer back, because the cursor has returned.
    fn show_cursor(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Reports the monitors attached to this machine.
pub trait Monitors {
    fn monitors(&mut self) -> Result<Vec<smkvm_layout::Monitor>>;
}

// A backend chosen at run time arrives in a box, and a box should be as usable
// as the thing inside it.
impl<T: Inject + ?Sized> Inject for Box<T> {
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        (**self).move_to(x, y)
    }
    fn button(&mut self, button: MouseButton, down: bool) -> Result<()> {
        (**self).button(button, down)
    }
    fn wheel(&mut self, scroll: Scroll) -> Result<()> {
        (**self).wheel(scroll)
    }
    fn key(&mut self, key: Key, down: bool) -> Result<()> {
        (**self).key(key, down)
    }
    fn flush(&mut self) -> Result<()> {
        (**self).flush()
    }
    fn hide_cursor(&mut self) -> Result<()> {
        (**self).hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<()> {
        (**self).show_cursor()
    }
}

impl<T: Monitors + ?Sized> Monitors for Box<T> {
    fn monitors(&mut self) -> Result<Vec<smkvm_layout::Monitor>> {
        (**self).monitors()
    }
}

/// An injector that remembers what it has pressed.
///
/// Every press and release goes through here, so at any moment the set of keys
/// and buttons this machine is holding on the local user's behalf is known
/// exactly. That makes releasing them a matter of fact rather than of guessing.
#[derive(Debug)]
pub struct Tracked<I> {
    inner: I,
    keys: BTreeSet<Key>,
    buttons: BTreeSet<MouseButton>,
}

impl<I: Inject> Tracked<I> {
    pub fn new(inner: I) -> Self {
        Self {
            inner,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
        }
    }

    pub fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        self.inner.move_to(x, y)
    }

    pub fn wheel(&mut self, scroll: Scroll) -> Result<()> {
        self.inner.wheel(scroll)
    }

    pub fn key(&mut self, key: Key, down: bool) -> Result<()> {
        self.inner.key(key, down)?;
        if down {
            self.keys.insert(key);
        } else {
            self.keys.remove(&key);
        }
        Ok(())
    }

    pub fn button(&mut self, button: MouseButton, down: bool) -> Result<()> {
        self.inner.button(button, down)?;
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        Ok(())
    }

    /// Make what is held match `keys` and `buttons` exactly.
    ///
    /// Sent on every arrival, so a chord that began on another machine
    /// continues here and nothing left over from last time survives.
    pub fn sync(&mut self, keys: &[Key], buttons: &[MouseButton]) -> Result<()> {
        let wanted: BTreeSet<Key> = keys.iter().copied().collect();
        let wanted_buttons: BTreeSet<MouseButton> = buttons.iter().copied().collect();

        let mut first_error = None;
        // Let go before taking hold, so a key moving from one state to the
        // other never appears held twice.
        for key in release_order(self.keys.difference(&wanted).copied()) {
            keep_first(&mut first_error, self.key(key, false));
        }
        for button in self
            .buttons
            .difference(&wanted_buttons)
            .copied()
            .collect::<Vec<_>>()
        {
            keep_first(&mut first_error, self.button(button, false));
        }
        for key in press_order(wanted.difference(&self.keys.clone()).copied()) {
            keep_first(&mut first_error, self.key(key, true));
        }
        for button in wanted_buttons
            .difference(&self.buttons.clone())
            .copied()
            .collect::<Vec<_>>()
        {
            keep_first(&mut first_error, self.button(button, true));
        }
        keep_first(&mut first_error, self.inner.flush());
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Let go of everything.
    ///
    /// Every key is attempted even if one fails, because giving up partway is
    /// precisely the outcome this exists to prevent: one failed release would
    /// otherwise leave the rest held down with nothing to clear them.
    pub fn release_all(&mut self) -> Result<()> {
        let mut first_error = None;
        for key in release_order(self.keys.iter().copied()) {
            keep_first(&mut first_error, self.key(key, false));
        }
        for button in self.buttons.iter().copied().collect::<Vec<_>>() {
            keep_first(&mut first_error, self.button(button, false));
        }
        // Whatever happened above, this machine is no longer holding anything
        // on anyone's behalf, so the record says so.
        self.keys.clear();
        self.buttons.clear();
        keep_first(&mut first_error, self.inner.flush());
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }

    pub fn held_keys(&self) -> Vec<Key> {
        self.keys.iter().copied().collect()
    }

    pub fn held_buttons(&self) -> Vec<MouseButton> {
        self.buttons.iter().copied().collect()
    }

    pub fn is_holding_anything(&self) -> bool {
        !self.keys.is_empty() || !self.buttons.is_empty()
    }

    pub fn inner(&self) -> &I {
        &self.inner
    }

    /// The underlying injector.
    ///
    /// Reaching past the tracking is for platform-specific extras and for
    /// tests; presses and releases must still go through [`Tracked`] or the
    /// record of what is held stops matching reality.
    pub fn inner_mut(&mut self) -> &mut I {
        &mut self.inner
    }

    pub fn into_inner(self) -> I {
        self.inner
    }
}

/// Ordinary keys before modifiers, the way a hand lets go of a chord.
///
/// Releasing Ctrl first would momentarily present the other key as unmodified,
/// which some applications act on.
fn release_order(keys: impl Iterator<Item = Key>) -> Vec<Key> {
    let (modifiers, rest): (Vec<_>, Vec<_>) = keys.partition(|k| k.is_modifier());
    rest.into_iter().chain(modifiers).collect()
}

/// Modifiers before ordinary keys, so a chord arrives already modified.
fn press_order(keys: impl Iterator<Item = Key>) -> Vec<Key> {
    let (modifiers, rest): (Vec<_>, Vec<_>) = keys.partition(|k| k.is_modifier());
    modifiers.into_iter().chain(rest).collect()
}

fn keep_first(slot: &mut Option<InputError>, result: Result<()>) {
    if let (None, Err(e)) = (&slot, result) {
        *slot = Some(e);
    }
}
