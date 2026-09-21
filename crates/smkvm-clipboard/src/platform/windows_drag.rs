//! Files being dragged on Windows: picking them up as the cursor leaves, and
//! dropping them where it arrives.
//!
//! OLE tells only the window under the pointer what a drag carries, so the
//! catcher keeps a tiny, all-but-invisible, topmost window of its own hidden
//! away, and when asked stands it under the pointer for a moment. The dragging
//! application notices the window on the next pointer movement and calls
//! `DragEnter` with its data object; the file list is read then and there,
//! the button is released on the application's behalf so its drag ends on
//! this window, and the drop is answered with "nothing happened" so nothing
//! is moved or copied on this machine.
//!
//! Dropping is the same dance from the other side: a data object naming the
//! files where they have already landed is handed to `DoDragDrop`, and the
//! window under the pointer when the button comes up takes them as any drop.

#![allow(unsafe_code)]

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use windows::core::{implement, PCWSTR};
use windows::Win32::Foundation::{
    BOOL, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, HWND, LPARAM, LRESULT,
    POINT, POINTL, S_OK, WPARAM,
};
use windows::Win32::System::Com::{
    IBindCtx, IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl, OleInitialize,
    OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop, CF_HDROP, DROPEFFECT,
    DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{ILFree, SHCreateDataObject, SHParseDisplayName};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    PostThreadMessageW, RegisterClassW, SetLayeredWindowAttributes, SetWindowPos, ShowWindow,
    TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE,
    WM_APP, WM_QUIT, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

use crate::{files, CatchDrag, ClipboardError, Drive, Result};

/// How long the dragging application gets to notice the window.
///
/// It looks on every pointer movement, and the caller supplies one; the rest
/// is the application's own message loop, which is quick unless it is not.
/// Past this there was no drag, or nothing worth waiting for.
const ENTER_PATIENCE: Duration = Duration::from_millis(250);

/// How long the drop gets to arrive once the button has been released.
const DROP_PATIENCE: Duration = Duration::from_millis(400);

/// Half the side of the catching window, in pixels. Big enough that a
/// pointer nudged by one pixel is still inside it.
const REACH: i32 = 32;

/// Stand the window under the pointer at these coordinates.
const WM_STAND: u32 = WM_APP + 21;
/// Hide it again.
const WM_WITHDRAW: u32 = WM_APP + 22;

fn last_error(what: &str) -> ClipboardError {
    ClipboardError::Display(format!(
        "{what}: {}",
        windows::core::Error::from_win32().message()
    ))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// What the dragging application told our window.
enum Seen {
    Entered(Vec<PathBuf>),
    Dropped,
}

/// The files in a data object, if it carries a file list.
fn paths_in(data: &IDataObject) -> Option<Vec<PathBuf>> {
    let format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    // SAFETY: a well-formed FORMATETC; the medium is released below.
    let mut medium = unsafe { data.GetData(&format) }.ok()?;
    // SAFETY: TYMED_HGLOBAL was asked for, so the union holds a global handle.
    let global = unsafe { medium.u.hGlobal };
    // SAFETY: a memory object the clipboard owner filled.
    let size = unsafe { GlobalSize(global) };
    let bytes = if size == 0 {
        Vec::new()
    } else {
        // SAFETY: balanced by the unlock; valid for `size` bytes while locked.
        let ptr = unsafe { GlobalLock(global) };
        if ptr.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) }.to_vec();
        unsafe {
            let _ = GlobalUnlock(global);
        }
        bytes
    };
    // SAFETY: the medium came from GetData and is not used again.
    unsafe { ReleaseStgMedium(&mut medium) };
    let list = files::hdrop_to_uri_list(&bytes).ok()?;
    Some(files::local_paths(&list))
}

/// The drop target the catching window presents.
#[implement(IDropTarget)]
struct Catcher {
    seen: Sender<Seen>,
}

impl IDropTarget_Impl for Catcher_Impl {
    fn DragEnter(
        &self,
        data: Option<&IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _at: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = data.and_then(paths_in).unwrap_or_default();
        let _ = self.seen.send(Seen::Entered(paths));
        // Say yes, so the application keeps the drag on this window and lets
        // it end here.
        // SAFETY: OLE hands a valid out-parameter.
        unsafe { *effect = DROPEFFECT_COPY };
        Ok(())
    }

    fn DragOver(
        &self,
        _keys: MODIFIERKEYS_FLAGS,
        _at: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // SAFETY: as above.
        unsafe { *effect = DROPEFFECT_COPY };
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn Drop(
        &self,
        _data: Option<&IDataObject>,
        _keys: MODIFIERKEYS_FLAGS,
        _at: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // Nothing happened here, as far as the application is concerned: the
        // files are still where they were, and a "move" moves nothing.
        // SAFETY: as above.
        unsafe { *effect = DROPEFFECT_NONE };
        let _ = self.seen.send(Seen::Dropped);
        Ok(())
    }
}

/// Picks up what is being dragged as the cursor leaves this machine.
pub struct DropCatcher {
    thread_id: u32,
    seen: Receiver<Seen>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DropCatcher {
    /// Make the catching window, on a thread of its own that will hold it
    /// for as long as this lives.
    pub fn start() -> Result<DropCatcher> {
        let (seen_tx, seen) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("smkvm-drag-catcher".into())
            .spawn(move || catcher_thread(seen_tx, ready_tx))
            .map_err(|e| ClipboardError::Display(format!("could not start a thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(DropCatcher {
                thread_id,
                seen,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ClipboardError::Display(
                "the drag catcher thread stopped before it started".into(),
            )),
        }
    }

    fn post(&self, message: u32, w: usize, l: isize) -> Result<()> {
        // SAFETY: posting to a thread id is safe whether or not it is still
        // running.
        unsafe { PostThreadMessageW(self.thread_id, message, WPARAM(w), LPARAM(l)) }
            .map_err(|_| last_error("reaching the drag catcher thread"))
    }
}

impl CatchDrag for DropCatcher {
    fn catch(&mut self, drive: &mut dyn FnMut(Drive)) -> Option<Vec<PathBuf>> {
        // Anything left from an earlier attempt is about that attempt.
        while self.seen.try_recv().is_ok() {}

        let mut at = POINT::default();
        // SAFETY: a valid out-parameter.
        if unsafe { GetCursorPos(&mut at) }.is_err() {
            return None;
        }
        // Coordinates may be negative on a desktop that extends left or up,
        // so they go across as the bits of an i32 and come back the same way.
        self.post(WM_STAND, at.x as u32 as usize, at.y as isize)
            .ok()?;
        // The application looks at what is under the pointer when the pointer
        // moves. Once away and back leaves it exactly where it was.
        drive(Drive::MoveTo(at.x + 1, at.y));
        drive(Drive::MoveTo(at.x, at.y));

        let deadline = Instant::now() + ENTER_PATIENCE;
        let paths = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.seen.recv_timeout(left) {
                Ok(Seen::Entered(paths)) => break paths,
                Ok(Seen::Dropped) => continue,
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                    // Nothing was being dragged, or not by anything that
                    // talks OLE. The button is left exactly as it was.
                    let _ = self.post(WM_WITHDRAW, 0, 0);
                    return None;
                }
            }
        };

        // A drag is in progress and it is on this window. Let go of the
        // button for the application, so its drag ends here and it stops
        // holding the pointer; whether the drop notice actually arrives is
        // then a courtesy, not a need.
        drive(Drive::ReleaseLeft);
        let deadline = Instant::now() + DROP_PATIENCE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.seen.recv_timeout(left) {
                Ok(Seen::Dropped) | Err(_) => break,
                Ok(Seen::Entered(_)) => continue,
            }
        }
        let _ = self.post(WM_WITHDRAW, 0, 0);
        if paths.is_empty() {
            tracing::debug!("a drag was caught, but it carried no files");
            return None;
        }
        Some(paths)
    }
}

impl Drop for DropCatcher {
    fn drop(&mut self) {
        let _ = self.post(WM_QUIT, 0, 0);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

unsafe extern "system" fn catcher_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: handing everything along is what the API requires.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn catcher_thread(seen: Sender<Seen>, ready: Sender<Result<u32>>) {
    // SAFETY: OLE on this thread, balanced by the uninitialise at the end.
    if unsafe { OleInitialize(None) }.is_err() {
        let _ = ready.send(Err(last_error("initialising OLE for the drag catcher")));
        return;
    }
    let class_name = wide("SmkvmDragCatcher");
    // SAFETY: the module handle call takes no pointers.
    let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
        let _ = ready.send(Err(last_error("finding this module")));
        return;
    };
    let class = WNDCLASSW {
        lpfnWndProc: Some(catcher_proc),
        hInstance: instance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // Registering twice is harmless; the second attempt fails and the existing
    // class is used.
    // SAFETY: the class points at a function that lives for the process.
    unsafe { RegisterClassW(&class) };

    let title = wide("smkvm drag catcher");
    // SAFETY: a plain top-level window, created hidden. Tool window so it
    // never appears in the taskbar or Alt+Tab, no-activate so standing it
    // under the pointer takes focus from nothing, layered so it can be made
    // as good as invisible while still being where the pointer is.
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            2 * REACH,
            2 * REACH,
            None,
            None,
            instance,
            None,
        )
    };
    let Ok(window) = window else {
        let _ = ready.send(Err(last_error("making the drag catcher window")));
        return;
    };
    // An alpha of one out of 255: nothing anyone will see, and still a
    // window the pointer is over. Zero would make it pass the pointer through.
    // SAFETY: a window this thread owns.
    unsafe {
        let _ = SetLayeredWindowAttributes(
            window,
            windows::Win32::Foundation::COLORREF(0),
            1,
            LWA_ALPHA,
        );
    }

    let target: IDropTarget = Catcher { seen }.into();
    // SAFETY: a window this thread owns and a live drop target; revoked below.
    if let Err(e) = unsafe { RegisterDragDrop(window, &target) } {
        let _ = ready.send(Err(ClipboardError::Display(format!(
            "registering the drag catcher as a drop target: {e}"
        ))));
        unsafe {
            let _ = DestroyWindow(window);
        }
        return;
    }

    // SAFETY: no pointers involved.
    let thread_id = unsafe { GetCurrentThreadId() };
    let _ = ready.send(Ok(thread_id));

    let mut message = MSG::default();
    // SAFETY: `message` is a valid out-parameter for each call.
    while unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) }.as_bool() {
        match message.message {
            WM_STAND => {
                let x = message.wParam.0 as u32 as i32;
                let y = message.lParam.0 as i32;
                // SAFETY: a window this thread owns.
                unsafe {
                    let _ = SetWindowPos(
                        window,
                        HWND_TOPMOST,
                        x - REACH,
                        y - REACH,
                        2 * REACH,
                        2 * REACH,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                }
            }
            WM_WITHDRAW => {
                // SAFETY: as above.
                unsafe {
                    let _ = ShowWindow(window, SW_HIDE);
                }
            }
            _ => {
                // SAFETY: both take the message just filled in.
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
    }

    // SAFETY: the window came from CreateWindowExW; everything is balanced.
    unsafe {
        let _ = RevokeDragDrop(window);
        let _ = DestroyWindow(window);
        OleUninitialize();
    }
}

/// The drag source for files dropped here on another machine's behalf.
#[implement(IDropSource)]
struct Source;

impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(
        &self,
        escape_pressed: BOOL,
        keys: MODIFIERKEYS_FLAGS,
    ) -> windows::core::HRESULT {
        if escape_pressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        // The button coming up -- which is the server relaying the person's
        // release -- is the drop.
        if keys.0 & MK_LBUTTON.0 == 0 {
            return DRAGDROP_S_DROP;
        }
        S_OK
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> windows::core::HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Drop these files -- already on this machine's disk -- wherever the pointer
/// is when the button comes up.
///
/// Runs on a thread of its own, since the drag holds it until the drop; the
/// outcome goes to the log. The button has to be down when this starts, or
/// the drop happens at once wherever the pointer is.
pub fn drop_files(paths: Vec<PathBuf>) -> Result<()> {
    std::thread::Builder::new()
        .name("smkvm-drag-source".into())
        .spawn(move || match drag_thread(&paths) {
            Ok(effect) if effect == DROPEFFECT_NONE => {
                tracing::info!(
                    files = paths.len(),
                    "the drop was not taken; the files stay where they landed"
                )
            }
            Ok(_) => tracing::info!(files = paths.len(), "the files were dropped"),
            Err(e) => tracing::warn!("the files could not be dropped where the pointer is: {e}"),
        })
        .map_err(|e| ClipboardError::Display(format!("could not start a thread: {e}")))?;
    Ok(())
}

fn drag_thread(paths: &[PathBuf]) -> Result<DROPEFFECT> {
    // SAFETY: OLE on this thread, balanced at the end.
    unsafe { OleInitialize(None) }.map_err(|_| last_error("initialising OLE for the drop"))?;
    let outcome = drag(paths);
    // SAFETY: balanced against the initialise above.
    unsafe { OleUninitialize() };
    outcome
}

fn drag(paths: &[PathBuf]) -> Result<DROPEFFECT> {
    let mut pidls: Vec<*const ITEMIDLIST> = Vec::with_capacity(paths.len());
    for path in paths {
        let name = wide(&path.to_string_lossy());
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        // SAFETY: a valid wide string and a place for the id list; freed below.
        let parsed = unsafe {
            SHParseDisplayName(PCWSTR(name.as_ptr()), None::<&IBindCtx>, &mut pidl, 0, None)
        };
        match parsed {
            Ok(()) if !pidl.is_null() => pidls.push(pidl as *const ITEMIDLIST),
            _ => tracing::warn!(path = %path.display(), "the shell does not know this file"),
        }
    }
    if pidls.is_empty() {
        return Err(ClipboardError::Display(
            "none of the landed files could be named to the shell".into(),
        ));
    }

    let outcome = {
        // SAFETY: absolute id lists, no folder; the shell builds the data object
        // Explorer itself would for these files.
        let data: windows::core::Result<IDataObject> =
            unsafe { SHCreateDataObject(None, Some(&pidls), None::<&IDataObject>) };
        match data {
            Ok(data) => {
                let source: IDropSource = Source.into();
                let mut effect = DROPEFFECT_NONE;
                // SAFETY: live COM objects and a valid out-parameter. Blocks until
                // the drop or the cancel.
                let hr = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
                if hr == DRAGDROP_S_DROP {
                    Ok(effect)
                } else if hr == DRAGDROP_S_CANCEL {
                    Ok(DROPEFFECT_NONE)
                } else {
                    Err(ClipboardError::Display(format!("DoDragDrop: {hr}")))
                }
            }
            Err(e) => Err(ClipboardError::Display(format!(
                "making a data object for the files: {e}"
            ))),
        }
    };
    for pidl in pidls {
        // SAFETY: each came from SHParseDisplayName and is not used again.
        unsafe { ILFree(Some(pidl)) };
    }
    outcome
}
