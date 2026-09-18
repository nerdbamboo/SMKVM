//! Injecting input into an X11 session, and reading its monitors.
//!
//! Injection goes through the XTEST extension, which is what lets one client
//! synthesise events the rest of the session treats as real. Keys are placed by
//! keycode, computed from the HID usage rather than looked up by keysym, so the
//! layout the X server happens to have loaded never enters into it.
//!
//! Only injection lives here. The machine that owns the physical keyboard and
//! mouse is the one that needs to capture, and that is a separate backend; a
//! client is told where the pointer is and puts it there.

use smkvm_layout::{Monitor, Rect};
use smkvm_proto::{Key, MouseButton, Scroll};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    ConnectionExt as _, Window, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::keymap::hid_to_x11_keycode;
use crate::{Inject, InputError, Monitors, Result};

/// X11 numbers wheel motion as button presses.
const BUTTON_WHEEL_UP: u8 = 4;
const BUTTON_WHEEL_DOWN: u8 = 5;
const BUTTON_WHEEL_LEFT: u8 = 6;
const BUTTON_WHEEL_RIGHT: u8 = 7;
const BUTTON_BACK: u8 = 8;
const BUTTON_FORWARD: u8 = 9;

/// XTEST takes absolute pointer positions when `detail` is zero.
const MOTION_ABSOLUTE: u8 = 0;

/// The most wheel clicks one event may turn into.
///
/// X11 has no way to say "scrolled a lot"; it can only repeat a click, so a
/// large delta becomes that many requests to the server. Since the delta
/// arrives over the network, leaving it unbounded would let one message tie up
/// the display for minutes. No real wheel or touchpad produces anything near
/// this in a single event, so clamping costs nothing real.
const MAX_CLICKS_PER_EVENT: i32 = 64;

fn display_err(e: impl std::fmt::Display) -> InputError {
    InputError::Display(e.to_string())
}

pub struct X11Input {
    conn: RustConnection,
    root: Window,
    screen_num: usize,
    /// Wheel motion finer than one click, kept until it adds up to one.
    ///
    /// X11 has no way to express a partial click, so discarding the remainder
    /// would make a touchpad or a high-resolution wheel scroll short.
    wheel_remainder: (i32, i32),
    has_randr_monitors: bool,
    has_xfixes: bool,
    cursor_hidden: bool,
}

impl X11Input {
    /// Connect to the session named by `$DISPLAY`.
    pub fn open() -> Result<Self> {
        Self::open_display(None)
    }

    pub fn open_display(display: Option<&str>) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(display).map_err(display_err)?;
        let root = conn.setup().roots[screen_num].root;

        if conn
            .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
            .map_err(display_err)?
            .is_none()
        {
            return Err(InputError::Display(
                "the X server has no XTEST extension, so input cannot be injected".into(),
            ));
        }

        // Hiding the pointer needs XFixes. Without it the pointer simply
        // stays where it was, which is worse than hiding it but better than
        // refusing to run.
        let has_xfixes = match conn.xfixes_query_version(4, 0) {
            Ok(cookie) => cookie.reply().is_ok(),
            Err(_) => false,
        };

        // RandR 1.5 introduced monitors, which is the view that matches what
        // the user sees: one entry per panel, already combined.
        let has_randr_monitors = match conn.randr_query_version(1, 5) {
            Ok(cookie) => match cookie.reply() {
                Ok(v) => (v.major_version, v.minor_version) >= (1, 5),
                Err(_) => false,
            },
            Err(_) => false,
        };

        Ok(Self {
            conn,
            root,
            screen_num,
            wheel_remainder: (0, 0),
            has_randr_monitors,
            has_xfixes,
            cursor_hidden: false,
        })
    }

    fn fake(&self, type_: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(type_, detail, 0, self.root, x, y, 0)
            .map_err(display_err)?;
        Ok(())
    }

    /// Wait until the server has processed everything sent so far.
    ///
    /// [`Inject::flush`] deliberately does not wait: injection is on the path
    /// every pointer movement takes, and a round trip per movement would show
    /// up as lag. This is for the few places that need to know the server has
    /// caught up, such as confirming state before reporting it.
    pub fn sync(&self) -> Result<()> {
        self.conn
            .get_input_focus()
            .map_err(display_err)?
            .reply()
            .map_err(display_err)?;
        Ok(())
    }

    fn click(&self, button: u8, times: i32) -> Result<()> {
        for _ in 0..times {
            self.fake(BUTTON_PRESS_EVENT, button, 0, 0)?;
            self.fake(BUTTON_RELEASE_EVENT, button, 0, 0)?;
        }
        Ok(())
    }
}

fn button_number(button: MouseButton) -> Result<u8> {
    Ok(match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => BUTTON_BACK,
        MouseButton::Forward => BUTTON_FORWARD,
        // Anything past the named buttons is passed through as-is, but the
        // wheel's numbers are not free to reuse.
        MouseButton::Other(n) => {
            let n = n.max(1);
            if (BUTTON_WHEEL_UP..=BUTTON_WHEEL_RIGHT).contains(&n) {
                return Err(InputError::UnmappedButton(button));
            }
            n
        }
    })
}

impl Inject for X11Input {
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        // X11 carries pointer positions as 16-bit values; a desktop larger than
        // that cannot be addressed, so clamp rather than wrap.
        let x = x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let y = y.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        self.fake(MOTION_NOTIFY_EVENT, MOTION_ABSOLUTE, x, y)
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<()> {
        let detail = button_number(button)?;
        let type_ = if down {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake(type_, detail, 0, 0)
    }

    fn wheel(&mut self, scroll: Scroll) -> Result<()> {
        let limit = MAX_CLICKS_PER_EVENT * Scroll::NOTCH;
        let dx = self.wheel_remainder.0 + scroll.dx.clamp(-limit, limit);
        let dy = self.wheel_remainder.1 + scroll.dy.clamp(-limit, limit);
        let (clicks_x, clicks_y) = (
            (dx / Scroll::NOTCH).clamp(-MAX_CLICKS_PER_EVENT, MAX_CLICKS_PER_EVENT),
            (dy / Scroll::NOTCH).clamp(-MAX_CLICKS_PER_EVENT, MAX_CLICKS_PER_EVENT),
        );
        // Only the sub-click part is carried forward. Keeping more would let
        // repeated large deltas build a backlog that scrolls long after the
        // user stopped.
        self.wheel_remainder = (dx % Scroll::NOTCH, dy % Scroll::NOTCH);

        if clicks_y > 0 {
            self.click(BUTTON_WHEEL_UP, clicks_y)?;
        } else if clicks_y < 0 {
            self.click(BUTTON_WHEEL_DOWN, -clicks_y)?;
        }
        if clicks_x > 0 {
            self.click(BUTTON_WHEEL_RIGHT, clicks_x)?;
        } else if clicks_x < 0 {
            self.click(BUTTON_WHEEL_LEFT, -clicks_x)?;
        }
        Ok(())
    }

    fn key(&mut self, key: Key, down: bool) -> Result<()> {
        let keycode = hid_to_x11_keycode(key).ok_or(InputError::UnmappedKey(key.0))?;
        let type_ = if down {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.fake(type_, keycode, 0, 0)
    }

    fn flush(&mut self) -> Result<()> {
        self.conn.flush().map_err(display_err)?;
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<()> {
        // XFixes hides it for the whole screen while this client asks, and
        // puts it back the moment the request is withdrawn -- so nothing is
        // left in a strange state if this process disappears.
        if !self.has_xfixes {
            return Ok(());
        }
        if !self.cursor_hidden {
            self.conn
                .xfixes_hide_cursor(self.root)
                .map_err(display_err)?;
            self.cursor_hidden = true;
            self.conn.flush().map_err(display_err)?;
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        if !self.has_xfixes || !self.cursor_hidden {
            return Ok(());
        }
        // Hiding is counted, so every hide needs exactly one show.
        self.conn
            .xfixes_show_cursor(self.root)
            .map_err(display_err)?;
        self.cursor_hidden = false;
        self.conn.flush().map_err(display_err)?;
        Ok(())
    }
}

impl X11Input {
    /// The whole screen as a single monitor.
    ///
    /// Used when the server will not describe individual panels, either
    /// because it predates RandR 1.5 or because it defines no monitors at all,
    /// as a bare virtual server does. One rectangle is then the truest picture
    /// available: the arrangement cannot be split finer than it is described.
    fn whole_screen(&self) -> Monitor {
        let screen = &self.conn.setup().roots[self.screen_num];
        Monitor {
            id: "screen-0".into(),
            local: Rect::new(
                0,
                0,
                i32::from(screen.width_in_pixels),
                i32::from(screen.height_in_pixels),
            ),
            scale: 1.0,
            primary: true,
            label: None,
        }
    }
}

impl Monitors for X11Input {
    fn monitors(&mut self) -> Result<Vec<Monitor>> {
        if !self.has_randr_monitors {
            return Ok(vec![self.whole_screen()]);
        }

        let reply = self
            .conn
            .randr_get_monitors(self.root, true)
            .map_err(display_err)?
            .reply()
            .map_err(display_err)?;
        if reply.monitors.is_empty() {
            return Ok(vec![self.whole_screen()]);
        }

        let mut out = Vec::with_capacity(reply.monitors.len());
        for m in reply.monitors {
            let name = self
                .conn
                .get_atom_name(m.name)
                .map_err(display_err)?
                .reply()
                .map_err(display_err)?;
            out.push(Monitor {
                id: String::from_utf8_lossy(&name.name).into_owned().into(),
                local: Rect::new(
                    i32::from(m.x),
                    i32::from(m.y),
                    i32::from(m.width),
                    i32::from(m.height),
                ),
                scale: 1.0,
                primary: m.primary,
                label: None,
            });
        }
        Ok(out)
    }
}
