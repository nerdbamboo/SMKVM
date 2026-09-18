//! Capturing the keyboard and mouse on the machine that owns them.
//!
//! Two sources, because neither alone is enough.
//!
//! Low-level hooks report every key and button, and can swallow them, which is
//! what stops the local machine acting on input meant for another. But the
//! pointer position a hook reports has already been clamped to the desktop, so
//! once the cursor is pressed against an edge it stops changing however hard
//! the user pushes — and that push is exactly the thing that needs noticing.
//!
//! Raw input reports the mouse's own motion, before any of that. It cannot
//! swallow anything, but it can still see movement when the position has
//! stopped moving. So hooks decide what the local machine sees, raw input
//! decides where the cursor goes, and the two run together.
//!
//! Both must live on one thread with a message loop: Windows delivers hook
//! callbacks and raw input to the thread that asked for them, and only while
//! that thread is pumping messages.

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

use smkvm_proto::{Key, MouseButton, Scroll};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP, MSG,
    MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1,
};

use crate::keymap::scancode_to_hid;
use crate::platform::windows::is_ours;
use crate::{InputError, Result};

/// Something the local keyboard or mouse did.
#[derive(Debug, Clone, PartialEq)]
pub enum Captured {
    /// Where the pointer now is, in desktop coordinates. Already clamped by
    /// the system, so it says where the cursor is but not where it was pushed.
    PointerAt {
        x: i32,
        y: i32,
    },
    /// How far the mouse itself moved, before any clamping.
    PointerBy {
        dx: i32,
        dy: i32,
    },
    Button {
        button: MouseButton,
        down: bool,
    },
    Wheel(Scroll),
    Key {
        key: Key,
        down: bool,
        repeat: bool,
    },
}

/// Whether the hooks should swallow what they see.
///
/// Read from the hook callbacks, which run on the capture thread, and written
/// by whoever decides the cursor has left. An atomic keeps that from needing a
/// lock on a path that runs for every mouse movement.
static SWALLOW: AtomicBool = AtomicBool::new(false);

/// Where captured events are delivered.
///
/// Hook callbacks are plain functions the system calls with no room for a
/// context pointer, so the destination has to be reachable from anywhere. It
/// is written twice in the life of a capture and read from one thread, so the
/// lock is never contended.
static SINK: Mutex<Option<Sender<Captured>>> = Mutex::new(None);

fn emit(event: Captured) {
    if let Ok(sink) = SINK.lock() {
        if let Some(tx) = sink.as_ref() {
            let _ = tx.send(event);
        }
    }
}

fn set_sink(sink: Option<Sender<Captured>>) {
    if let Ok(mut slot) = SINK.lock() {
        *slot = sink;
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        // SAFETY: passing the call along unchanged is what the API requires
        // when it says not to process the event.
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    // SAFETY: for a non-negative code the system guarantees lparam points at
    // one of these for the duration of the callback.
    let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };

    // Our own injections come back round through here; acting on them would
    // send every keystroke twice and, on the server, in a loop.
    if is_ours(info.dwExtraInfo) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let down = info.flags.0 & LLKHF_UP.0 == 0;
    let extended = info.flags.0 & LLKHF_EXTENDED.0 != 0;
    let scan = if extended {
        0xE000 | (info.scanCode & 0xFF)
    } else {
        info.scanCode & 0xFF
    };

    if let Some(key) = scancode_to_hid(scan as u16) {
        emit(Captured::Key {
            key,
            down,
            // The hook gives no repeat flag, so a press of a key already held
            // is what a repeat looks like. Working that out belongs with the
            // state that knows what is held, not here.
            repeat: false,
        });
    }

    if SWALLOW.load(Ordering::Relaxed) {
        // Stop it reaching this machine: the key belongs to another screen.
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    // SAFETY: as above, the system guarantees this for the callback's duration.
    let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    if is_ours(info.dwExtraInfo) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let high = ((info.mouseData >> 16) & 0xFFFF) as u16;
    match wparam.0 as u32 {
        WM_MOUSEMOVE => emit(Captured::PointerAt {
            x: info.pt.x,
            y: info.pt.y,
        }),
        WM_LBUTTONDOWN => button(MouseButton::Left, true),
        WM_LBUTTONUP => button(MouseButton::Left, false),
        WM_RBUTTONDOWN => button(MouseButton::Right, true),
        WM_RBUTTONUP => button(MouseButton::Right, false),
        WM_MBUTTONDOWN => button(MouseButton::Middle, true),
        WM_MBUTTONUP => button(MouseButton::Middle, false),
        WM_XBUTTONDOWN => button(xbutton(high), true),
        WM_XBUTTONUP => button(xbutton(high), false),
        // The delta is signed and arrives in the high half.
        WM_MOUSEWHEEL => emit(Captured::Wheel(Scroll::new(0, high as i16 as i32))),
        WM_MOUSEHWHEEL => emit(Captured::Wheel(Scroll::new(high as i16 as i32, 0))),
        _ => {}
    }

    if SWALLOW.load(Ordering::Relaxed) {
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn xbutton(high: u16) -> MouseButton {
    if high == XBUTTON1 {
        MouseButton::Back
    } else {
        MouseButton::Forward
    }
}

fn button(button: MouseButton, down: bool) {
    emit(Captured::Button { button, down });
}

/// A running capture.
///
/// The events it produces come back on a separate channel rather than through
/// here, so the thread that consumes them and the code that decides whether to
/// swallow input need not be the same one.
pub struct Capture {
    thread_id: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Install the hooks on a thread of their own and start delivering events.
    pub fn start() -> Result<(Capture, Receiver<Captured>)> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        let handle = std::thread::Builder::new()
            .name("smkvm-capture".into())
            .spawn(move || capture_thread(tx, ready_tx))
            .map_err(|e| InputError::Display(format!("could not start the capture thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok((
                Capture {
                    thread_id,
                    handle: Some(handle),
                },
                rx,
            )),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(InputError::Display(
                "the capture thread stopped before it started".into(),
            )),
        }
    }

    /// Start or stop swallowing local input.
    ///
    /// Swallowing is what makes the difference between the cursor being here
    /// and being somewhere else: the events still arrive, but this machine no
    /// longer acts on them.
    pub fn set_swallow(&self, swallow: bool) {
        SWALLOW.store(swallow, Ordering::Relaxed);
    }

    /// Which mouse buttons the system says are down.
    ///
    /// Used to reconcile after a spell of not seeing events, such as while
    /// another desktop had the input.
    pub fn buttons_down() -> Vec<MouseButton> {
        let pairs = [
            (VK_LBUTTON, MouseButton::Left),
            (VK_RBUTTON, MouseButton::Right),
            (VK_MBUTTON, MouseButton::Middle),
            (VK_XBUTTON1, MouseButton::Back),
            (VK_XBUTTON2, MouseButton::Forward),
        ];
        pairs
            .into_iter()
            .filter(|(vk, _)| pressed(*vk))
            .map(|(_, button)| button)
            .collect()
    }
}

fn pressed(vk: VIRTUAL_KEY) -> bool {
    // SAFETY: reading key state takes no pointers.
    (unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000) != 0
}

impl Drop for Capture {
    fn drop(&mut self) {
        SWALLOW.store(false, Ordering::Relaxed);
        // SAFETY: posting a quit to a thread id is safe whether or not the
        // thread is still there.
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn capture_thread(tx: Sender<Captured>, ready: Sender<Result<u32>>) {
    set_sink(Some(tx));

    // SAFETY: both callbacks have the signature the API requires and outlive
    // the hooks, being ordinary functions.
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) };
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) };

    let (keyboard, mouse) = match (keyboard, mouse) {
        (Ok(k), Ok(m)) => (k, m),
        _ => {
            let _ = ready.send(Err(InputError::Display(
                "the system would not install the input hooks".into(),
            )));
            set_sink(None);
            return;
        }
    };

    // Raw input is what still sees the mouse moving once the pointer has
    // stopped: hooks report a position the system has already clamped.
    let window = match raw_input::start() {
        Ok(window) => window,
        Err(e) => {
            // SAFETY: the handles came from SetWindowsHookExW.
            unsafe {
                let _ = UnhookWindowsHookEx(keyboard);
                let _ = UnhookWindowsHookEx(mouse);
            }
            let _ = ready.send(Err(e));
            set_sink(None);
            return;
        }
    };

    // SAFETY: no pointers involved.
    let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    let _ = ready.send(Ok(thread_id));

    // Hooks are only called while this thread pumps messages.
    let mut message = MSG::default();
    // SAFETY: `message` is a valid out-parameter for each call.
    while unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    raw_input::stop(window);
    // SAFETY: both handles came from SetWindowsHookExW and are not used again.
    unsafe {
        let _ = UnhookWindowsHookEx(keyboard);
        let _ = UnhookWindowsHookEx(mouse);
    }
    set_sink(None);
}

/// The mouse's own motion, before the system clamps the pointer.
///
/// Raw input needs somewhere to deliver to, so this keeps a window that is
/// never shown and exists only to receive it. A pointer pinned against a
/// screen edge reports the same position however hard it is pushed, and that
/// push is the whole signal for leaving the screen, so nothing else will do.
mod raw_input {
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::{
        GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
        RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, HWND_MESSAGE,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WNDCLASSW,
    };

    use super::{emit, Captured};
    use crate::{InputError, Result};

    unsafe extern "system" fn wndproc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_INPUT {
            // SAFETY: for WM_INPUT the system guarantees lparam is a raw input
            // handle valid for the duration of the message.
            unsafe { handle(HRAWINPUT(lparam.0 as *mut _)) };
        }
        // SAFETY: passing anything else along is what the API requires.
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    unsafe fn handle(handle: HRAWINPUT) {
        let mut size = 0u32;
        let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        // SAFETY: asking for the size writes only to `size`.
        unsafe { GetRawInputData(handle, RID_INPUT, None, &mut size, header) };
        if size == 0 || size as usize > std::mem::size_of::<RAWINPUT>() * 4 {
            return;
        }

        let mut buffer = vec![0u8; size as usize];
        // SAFETY: the buffer is at least `size` bytes, which is what the
        // previous call asked for.
        let got = unsafe {
            GetRawInputData(
                handle,
                RID_INPUT,
                Some(buffer.as_mut_ptr() as *mut _),
                &mut size,
                header,
            )
        };
        if got != size || (got as usize) < std::mem::size_of::<RAWINPUTHEADER>() {
            return;
        }

        // SAFETY: the system filled the buffer with a RAWINPUT, and the length
        // was checked against the header above.
        let input = unsafe { &*(buffer.as_ptr() as *const RAWINPUT) };
        if input.header.dwType != RIM_TYPEMOUSE.0 {
            return;
        }
        // SAFETY: the type says the union holds the mouse variant.
        let mouse = unsafe { &input.data.mouse };
        // A tablet or remote desktop reports where the pointer is rather than
        // how far it moved, which is the clamped figure this exists to avoid.
        if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0 {
            return;
        }
        if mouse.lLastX != 0 || mouse.lLastY != 0 {
            emit(Captured::PointerBy {
                dx: mouse.lLastX,
                dy: mouse.lLastY,
            });
        }
    }

    /// Create the receiving window and ask for mouse input.
    pub(super) fn start() -> Result<HWND> {
        // SAFETY: the module handle call takes no pointers.
        let instance =
            unsafe { GetModuleHandleW(None) }.map_err(|e| InputError::Display(e.to_string()))?;

        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("SmkvmRawInput"),
            ..Default::default()
        };
        // Registering twice is harmless; the second attempt simply fails and
        // the existing class is used.
        // SAFETY: the class points at a function that lives for the process.
        unsafe { RegisterClassW(&class) };

        // SAFETY: a message-only window needs no geometry and is never shown.
        let window = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("SmkvmRawInput"),
                w!("smkvm"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                instance,
                None,
            )
        }
        .map_err(|e| InputError::Display(format!("no window for raw input: {e}")))?;

        let device = RAWINPUTDEVICE {
            // Generic desktop page, mouse.
            usUsagePage: 0x01,
            usUsage: 0x02,
            // Keep receiving even when this window is not in the foreground,
            // which it never is.
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: window,
        };
        // SAFETY: the slice is valid for the call and the size matches.
        unsafe { RegisterRawInputDevices(&[device], std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
            .map_err(|e| InputError::Display(format!("the system refused raw mouse input: {e}")))?;

        Ok(window)
    }

    pub(super) fn stop(window: HWND) {
        // SAFETY: the handle came from CreateWindowExW and is not used again.
        unsafe {
            let _ = DestroyWindow(window);
        }
    }
}
