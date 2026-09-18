//! Injecting input into Windows, and reading its displays.
//!
//! Keys are sent as scan codes rather than virtual keys, so the key that
//! arrives is the key at that physical position whatever layout the machine
//! has loaded. Virtual keys would be translated through the active layout and
//! two machines with different ones would disagree about what was pressed.

#![allow(unsafe_code)]

use smkvm_layout::{Monitor, Rect};
use smkvm_proto::{Key, MouseButton, Scroll};
use windows::Win32::Foundation::{BOOL, LPARAM, POINT, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, DISPLAY_DEVICEW, HDC, HMONITOR,
    MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, EDD_GET_DEVICE_INTERFACE_NAME, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

use crate::keymap::hid_to_scancode;
use crate::{Inject, InputError, Monitors, Parked, Result};

pub mod capture;
pub mod desktop;

/// Stamped on every event this process injects.
///
/// The server's hook sees everything the system delivers, including what this
/// very process sent. Without a marker it would treat its own injections as
/// fresh input and feed them round again.
pub const INJECTED_MARKER: usize = 0x534D_4B56; // "SMKV"

/// Is this event one we sent?
pub fn is_ours(extra_info: usize) -> bool {
    extra_info == INJECTED_MARKER
}

/// The rectangle covering every display.
fn virtual_desktop() -> Rect {
    // SAFETY: reading system metrics takes no pointers and cannot fail.
    unsafe {
        Rect::new(
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

pub struct WindowsInput {
    /// Scroll finer than one notch, kept until it adds up to one.
    wheel_remainder: (i32, i32),
    /// Where the pointer was before it was moved out of the way.
    parked: Parked,
}

impl Default for WindowsInput {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsInput {
    pub fn new() -> Self {
        Self {
            wheel_remainder: (0, 0),
            parked: Parked::default(),
        }
    }

    fn send(&self, inputs: &[INPUT]) -> Result<()> {
        // SAFETY: the slice is valid for the call and the size matches the
        // struct the API expects.
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            return Err(InputError::Display(
                "the system refused the input, which usually means a window of \
                 higher privilege has the foreground"
                    .into(),
            ));
        }
        Ok(())
    }

    fn mouse(&self, dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        // The field is unsigned, but a wheel delta is signed and travels in
        // its bits, which is what the API expects.
        let data = data as u32;
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTED_MARKER,
                },
            },
        }
    }
}

impl WindowsInput {
    /// Put the pointer at a desktop position, without touching the record of
    /// where it is owed back to.
    ///
    /// Injected rather than set with `SetCursorPos`, so it carries
    /// [`INJECTED_MARKER`] and the capture hook knows not to read this
    /// program's own placements back as though the person had moved the mouse.
    fn place(&self, x: i32, y: i32) -> Result<()> {
        let desk = virtual_desktop();
        if desk.w <= 1 || desk.h <= 1 {
            return Err(InputError::Display("no usable display".into()));
        }
        // Absolute mouse positions are given as a fraction of the virtual
        // desktop, scaled to 16 bits, rather than in pixels.
        let nx = ((x - desk.x) as i64 * 65535) / (desk.w - 1) as i64;
        let ny = ((y - desk.y) as i64 * 65535) / (desk.h - 1) as i64;
        let input = self.mouse(
            nx.clamp(0, 65535) as i32,
            ny.clamp(0, 65535) as i32,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        );
        self.send(&[input])
    }
}

impl Inject for WindowsInput {
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        self.place(x, y)?;
        // Being told where the pointer goes settles any debt from parking it:
        // putting it back afterwards would undo the position just asked for.
        self.parked.placed();
        Ok(())
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<()> {
        let (flags, data) = match (button, down) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, i32::from(XBUTTON1)),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, i32::from(XBUTTON1)),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, i32::from(XBUTTON2)),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, i32::from(XBUTTON2)),
            // Windows names only five buttons; anything further has nowhere to
            // go, and pressing the wrong one is worse than pressing none.
            (MouseButton::Other(_), _) => return Err(InputError::UnmappedButton(button)),
        };
        let input = self.mouse(0, 0, data, flags);
        self.send(&[input])
    }

    fn wheel(&mut self, scroll: Scroll) -> Result<()> {
        // Windows measures wheel motion in the same units the protocol does,
        // one notch being 120, so whole notches pass straight through and only
        // the remainder needs carrying.
        let dx = self.wheel_remainder.0.saturating_add(scroll.dx);
        let dy = self.wheel_remainder.1.saturating_add(scroll.dy);
        let (whole_x, whole_y) = (dx / Scroll::NOTCH, dy / Scroll::NOTCH);
        self.wheel_remainder = (dx % Scroll::NOTCH, dy % Scroll::NOTCH);

        let mut inputs = Vec::new();
        if whole_y != 0 {
            inputs.push(self.mouse(0, 0, whole_y * Scroll::NOTCH, MOUSEEVENTF_WHEEL));
        }
        if whole_x != 0 {
            inputs.push(self.mouse(0, 0, whole_x * Scroll::NOTCH, MOUSEEVENTF_HWHEEL));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        self.send(&inputs)
    }

    fn key(&mut self, key: Key, down: bool) -> Result<()> {
        let scan = hid_to_scancode(key).ok_or(InputError::UnmappedKey(key.0))?;
        let mut flags = KEYEVENTF_SCANCODE;
        if scan & 0xFF00 == 0xE000 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan & 0x00FF,
                    dwFlags: KEYBD_EVENT_FLAGS(flags.0),
                    time: 0,
                    dwExtraInfo: INJECTED_MARKER,
                },
            },
        };
        self.send(&[input])
    }

    fn flush(&mut self) -> Result<()> {
        // SendInput has already delivered; there is nothing held back.
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<()> {
        // Windows offers no way to hide the pointer for the whole desktop from
        // outside the application drawing it, so it goes to the very corner
        // instead of sitting in the middle of whatever is being read.
        //
        // It is deliberately not confined there. Confining it added nothing --
        // the hook already swallows movement while the cursor belongs to
        // another machine, so the pointer does not drift anyway -- and it
        // could strand someone with no pointer at all if the far machine
        // turned out not to take the cursor. Parking is recoverable; a cage is
        // not.
        //
        // Recoverable only if the position it was taken from is kept, though,
        // which is what `parked` is for: the corner is where the pointer goes,
        // not where it belongs.
        let desk = virtual_desktop();
        if desk.is_empty() {
            return Ok(());
        }
        if !self.parked.park(cursor_position()?) {
            return Ok(());
        }
        let corner = (desk.right() - 1, desk.bottom() - 1);
        if let Err(e) = self.place(corner.0, corner.1) {
            // Nothing was moved, so nothing is owed back.
            self.parked.placed();
            return Err(e);
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        // Only the parking is undone. Windows has no desktop-wide hiding to
        // reverse -- and a pointer hidden by whatever application is in front
        // is that application's to show again, not this program's to wrestle
        // for.
        let Some((x, y)) = self.parked.restore() else {
            return Ok(());
        };
        self.place(x, y)
    }
}

/// Collects displays during enumeration.
struct Collector {
    monitors: Vec<Monitor>,
}

unsafe extern "system" fn collect(
    handle: HMONITOR,
    _dc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the pointer handed to EnumDisplayMonitors below, which
    // outlives the enumeration.
    let collector = unsafe { &mut *(data.0 as *mut Collector) };

    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `info` is correctly sized and lives for the call.
    let ok = unsafe { GetMonitorInfoW(handle, &mut info.monitorInfo as *mut _) };
    if !ok.as_bool() {
        return TRUE;
    }

    let r = info.monitorInfo.rcMonitor;
    let adapter = String::from_utf16_lossy(&info.szDevice)
        .trim_end_matches('\0')
        .to_string();
    collector.monitors.push(Monitor {
        id: stable_id(&info.szDevice).unwrap_or(adapter).into(),
        local: Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top),
        scale: 1.0,
        primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        label: None,
    });
    TRUE
}

impl Monitors for WindowsInput {
    fn monitors(&mut self) -> Result<Vec<Monitor>> {
        let mut collector = Collector {
            monitors: Vec::new(),
        };
        // SAFETY: the callback matches the expected signature and the pointer
        // stays valid for the duration of the call.
        unsafe {
            let _ = EnumDisplayMonitors(
                None,
                None,
                Some(collect),
                LPARAM(&mut collector as *mut Collector as isize),
            );
        }
        if collector.monitors.is_empty() {
            let desk = virtual_desktop();
            collector.monitors.push(Monitor {
                id: "display-0".into(),
                local: desk,
                scale: 1.0,
                primary: true,
                label: None,
            });
        }
        Ok(collector.monitors)
    }
}

/// A name for a monitor that survives being unplugged and plugged back in.
///
/// The obvious name, `\\.\DISPLAY3`, is a slot number: it depends on the order
/// displays were detected and shifts when one is added or removed. Placements
/// are keyed on this, so using it would quietly move a monitor's position on
/// the global desktop whenever Windows renumbered it.
///
/// The device interface path contains the monitor's own hardware identity and
/// the port it is on, so it stays put.
fn stable_id(adapter: &[u16; 32]) -> Option<String> {
    let mut device = DISPLAY_DEVICEW {
        cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    // SAFETY: both arguments are correctly sized and live for the call.
    let ok = unsafe {
        EnumDisplayDevicesW(
            windows::core::PCWSTR(adapter.as_ptr()),
            0,
            &mut device,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
    };
    if !ok.as_bool() {
        return None;
    }
    let id = String::from_utf16_lossy(&device.DeviceID);
    let id = id.trim_end_matches('\0').trim();
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

/// Where the pointer is now, in desktop coordinates.
pub fn cursor_position() -> Result<(i32, i32)> {
    let mut point = POINT::default();
    // SAFETY: `point` is a valid out-parameter for the call.
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point)
            .map_err(|e| InputError::Display(e.to_string()))?;
    }
    Ok((point.x, point.y))
}
