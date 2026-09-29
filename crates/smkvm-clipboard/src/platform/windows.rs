//! The Windows clipboard.
//!
//! Three things here matter more than the rest.
//!
//! Change notification uses the format listener rather than the older viewer
//! chain. The chain is a linked list every watcher splices itself into, and one
//! badly behaved link breaks it for everyone downstream -- silently, with no
//! error and no way to notice. That is the likeliest reason a shared clipboard
//! "sometimes" stops working until something is restarted.
//!
//! The clipboard can be held by only one process at a time, and applications
//! open it constantly. A single refusal means nothing, so attempts are repeated
//! briefly rather than reported as failure.
//!
//! Contents are offered without being supplied. Windows lets a format be
//! promised and produced only if something actually pastes it, which is what
//! allows a large image to be shared without reading it first.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use smkvm_proto::ClipFormat;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, KillTimer, PostMessageW, PostThreadMessageW, RegisterClassW, SetTimer,
    TranslateMessage, HWND_MESSAGE, MSG, MSGFLT_ALLOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLIPBOARDUPDATE, WM_DESTROYCLIPBOARD, WM_QUIT, WM_RENDERALLFORMATS, WM_RENDERFORMAT,
    WM_TIMER, WNDCLASSW,
};

use crate::{files, html, image, Available, ClipboardError, Fetch, Result, Write};
use crate::{witness, RENDER_BUDGET};

/// How long to keep trying to open the clipboard.
///
/// Every application that copies or pastes holds it for a moment, so a refusal
/// is ordinary rather than a failure. Giving up at once would make sharing
/// unreliable in precisely the way this exists to fix.
const OPEN_PATIENCE: Duration = Duration::from_millis(500);

/// Asks the window thread to take the clipboard and offer these formats.
const WM_OFFER: u32 = WM_APP + 1;
/// Asks it to give the clipboard back.
const WM_RELEASE: u32 = WM_APP + 2;

/// Promise the offered formats again, after a render could not keep the
/// last promise.
///
/// This exists because of the way delayed rendering fails. A handler
/// that returns from `WM_RENDERFORMAT` *without* calling
/// `SetClipboardData` has not deferred the question -- it has answered
/// it, with nothing. Windows records empty data for that format and
/// never asks again, so one render that could not be served in time
/// turns into a clipboard that is permanently empty while still listing
/// the formats and still owned by us. That is precisely the state a
/// real machine was left in, and it is what "fail fast" bought when it
/// replaced a render that merely took too long: the slow version at
/// least got asked again.
///
/// So a render that cannot supply the data renews the promise instead
/// of abandoning it. Posted rather than done inline because the
/// clipboard is open for the length of the handler and re-taking it has
/// to happen after that returns.
const WM_RENEW: u32 = WM_APP + 3;

/// How many times a promise is renewed before the clipboard is given
/// back altogether.
///
/// Holding a promise that cannot be kept is worse for the person than
/// holding nothing: their own last copy is gone and every paste yields
/// emptiness. After this many failures the clipboard is released, so
/// copying and pasting on the machine itself starts working again even
/// though what the other machine offered is lost.
const RENEWALS_BEFORE_GIVING_UP: u32 = 3;

/// The timer that lets a burst of change notices settle into one report.
const SETTLE_TIMER: usize = 1;
/// How long to wait for the burst to end. The steps of one copy are a few
/// milliseconds apart; two copies by a person are never this close.
const SETTLE_MS: u32 = 60;

/// The system's own words for the last error, and its number.
///
/// The number matters as much as the words. These machines are Korean,
/// so every message the system produces comes back in Korean -- and if
/// anything about the decoding is wrong it arrives as mojibake, which
/// is unreadable and unsearchable both. A bare number can always be
/// looked up. `FormatMessageW` is used explicitly rather than left to
/// anything that might reach for the ANSI form, and the text is
/// trimmed because the system ends its messages with a newline.
fn describe_win32(code: u32) -> String {
    use windows::Win32::System::Diagnostics::Debug::{
        FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
    };
    let mut buffer = [0u16; 512];
    // SAFETY: the buffer is valid for its own length, and no inserts are
    // read because the flag says to ignore them.
    let len = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            None,
            code,
            0,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            buffer.len() as u32,
            None,
        )
    };
    let said = String::from_utf16_lossy(&buffer[..len as usize]);
    let said = said.trim();
    if said.is_empty() {
        format!("error {code}")
    } else {
        format!("{said} (error {code})")
    }
}

fn last_error(what: &str) -> ClipboardError {
    let code = windows::core::Error::from_win32().code().0 as u32 & 0xFFFF;
    ClipboardError::Display(format!("{what}: {}", describe_win32(code)))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The clipboard format numbers that have to be registered by name.
#[derive(Clone, Copy)]
struct Formats {
    html: u32,
    /// Some applications offer PNG directly, which spares a conversion.
    png: u32,
}

impl Formats {
    fn register() -> Formats {
        let (html, png) = (wide("HTML Format"), wide("PNG"));
        // SAFETY: both names are valid wide strings for the call's duration.
        unsafe {
            Formats {
                html: RegisterClipboardFormatW(PCWSTR(html.as_ptr())),
                png: RegisterClipboardFormatW(PCWSTR(png.as_ptr())),
            }
        }
    }
}

/// Holds the clipboard open for as long as it is alive.
struct Opened;

impl Opened {
    /// `window` is the owner, or the null handle for none.
    fn take(window: HWND) -> Result<Opened> {
        let deadline = Instant::now() + OPEN_PATIENCE;
        loop {
            // SAFETY: the handle, when given, is a window this crate made.
            if unsafe { OpenClipboard(window) }.is_ok() {
                return Ok(Opened);
            }
            if Instant::now() >= deadline {
                return Err(ClipboardError::Busy);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        // SAFETY: balanced against the open that produced this.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Read one raw clipboard format. The clipboard must already be open.
fn read_raw(format: u32) -> Result<Vec<u8>> {
    // SAFETY: the handle belongs to the clipboard and stays valid while it is
    // open, which the caller's guard guarantees.
    let handle: HANDLE = unsafe { GetClipboardData(format) }
        .map_err(|_| ClipboardError::Refused(ClipFormat::Other(format.to_string())))?;
    let global = HGLOBAL(handle.0);
    // SAFETY: clipboard data is a memory object.
    let size = unsafe { GlobalSize(global) };
    if size == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: balanced by the unlock below.
    let ptr = unsafe { GlobalLock(global) };
    if ptr.is_null() {
        return Err(last_error("locking the clipboard's memory"));
    }
    // SAFETY: valid for `size` bytes while locked.
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) }.to_vec();
    // SAFETY: balanced against the lock.
    unsafe {
        let _ = GlobalUnlock(global);
    }
    Ok(bytes)
}

/// Hand bytes to the clipboard, which takes ownership of them.
fn write_raw(format: u32, bytes: &[u8]) -> Result<()> {
    // SAFETY: a plain allocation of a known size.
    let global = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) }
        .map_err(|_| last_error("reserving memory for the clipboard"))?;
    // SAFETY: the allocation just succeeded.
    let ptr = unsafe { GlobalLock(global) };
    if ptr.is_null() {
        return Err(last_error("locking memory for the clipboard"));
    }
    // SAFETY: the allocation is at least this long, and the regions are
    // distinct.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        let _ = GlobalUnlock(global);
    }
    // SAFETY: a fresh handle, not touched again here: the clipboard frees it.
    unsafe { SetClipboardData(format, HANDLE(global.0)) }
        .map_err(|_| last_error("handing the data to the clipboard"))?;
    Ok(())
}

fn available_now(formats: Formats) -> Available {
    // SAFETY: asking whether a format is present takes no pointers.
    let has = |format: u32| unsafe { IsClipboardFormatAvailable(format) }.is_ok();
    let mut out = Vec::new();
    if has(CF_UNICODETEXT.0 as u32) {
        out.push(ClipFormat::Text);
    }
    if has(formats.html) {
        out.push(ClipFormat::Html);
    }
    if has(formats.png) || has(CF_DIB.0 as u32) {
        out.push(ClipFormat::Png);
    }
    if has(CF_HDROP.0 as u32) {
        out.push(ClipFormat::Uris);
    }
    Available { formats: out }
}

/// Read one of our formats, converting from whatever Windows keeps it as.
fn read_format(formats: Formats, format: &ClipFormat) -> Result<Vec<u8>> {
    let _open = Opened::take(HWND::default())?;
    match format {
        ClipFormat::Text => {
            let bytes = read_raw(CF_UNICODETEXT.0 as u32)?;
            let utf16: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|c| *c != 0)
                .collect();
            Ok(String::from_utf16_lossy(&utf16).into_bytes())
        }
        ClipFormat::Html => Ok(html::unwrap(&read_raw(formats.html)?)),
        ClipFormat::Png => {
            // Prefer the form that needs no conversion.
            if let Ok(png) = read_raw(formats.png) {
                if !png.is_empty() {
                    return Ok(png);
                }
            }
            image::dib_to_png(&read_raw(CF_DIB.0 as u32)?)
        }
        ClipFormat::Uris => files::hdrop_to_uri_list(&read_raw(CF_HDROP.0 as u32)?),
        ClipFormat::Other(_) => Err(ClipboardError::Refused(format.clone())),
    }
}

/// What the window procedure needs, reachable from a callback that has nowhere
/// to carry context. Only ever touched on the window's own thread.
struct State {
    formats: Formats,
    changes: Sender<Available>,
    /// What is currently being offered, and where to get it.
    offer: Option<(Vec<ClipFormat>, Box<dyn Fetch>)>,
    /// Anything already fetched, so a second paste does not fetch again.
    cache: HashMap<ClipFormat, Vec<u8>>,
    /// Set while this process is the one putting data on the clipboard, so its
    /// own change notification is not mistaken for someone else copying.
    ours: bool,
    /// How many times running the promise has been renewed without a
    /// paste ever being served.
    renewals: u32,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Produce one promised format, now that something has asked for it.
fn render(window: HWND, format_id: u32) {
    // Every way out of this says why, including the ones that cannot
    // happen. A render that gives up in silence is indistinguishable
    // from a render that was never asked for, and telling those two
    // apart has now cost this project three separate rounds.
    let mut renew = false;
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else {
            tracing::warn!(
                format_id,
                "asked to render, but this thread holds no clipboard state at all"
            );
            return;
        };
        let formats = state.formats;
        let Some((offered, source)) = state.offer.as_ref() else {
            tracing::debug!(format_id, "asked to render, but nothing is on offer");
            return;
        };
        let wanted = offered
            .iter()
            .find(|f| native_ids(formats, f).contains(&format_id));
        let Some(wanted) = wanted.cloned() else {
            tracing::debug!(
                format_id,
                ?offered,
                "asked to render a form that was not offered"
            );
            return;
        };
        tracing::debug!(format_id, ?wanted, "something here is pasting");

        let asked_at = Instant::now();
        let bytes = match state.cache.get(&wanted) {
            Some(cached) => cached.clone(),
            None => match source.fetch(&wanted) {
                Ok(bytes) => {
                    state.cache.insert(wanted.clone(), bytes.clone());
                    bytes
                }
                Err(e) => {
                    tracing::warn!(?wanted, "could not supply the clipboard: {e}");
                    renew = true;
                    return;
                }
            },
        };

        // Kept even when it is too late to use, because the next paste
        // will find it here and be instant. A fetch that arrives after
        // its render is not wasted, only mistimed.
        let took = asked_at.elapsed();
        if took > RENDER_BUDGET {
            tracing::warn!(
                ?wanted,
                took_ms = took.as_millis() as u64,
                "the contents arrived too late to hand over -- the clipboard closed while \
                 they were being fetched, so this paste produced nothing. They are kept, \
                 so pasting again will be immediate"
            );
            renew = true;
            return;
        }

        match write_native(formats, &wanted, format_id, &bytes) {
            Ok(()) => {
                state.renewals = 0;
                tracing::debug!(format_id, ?wanted, bytes = bytes.len(), "handed over");
            }
            Err(e) => {
                tracing::warn!(?wanted, "could not put the data on the clipboard: {e}");
                renew = true;
            }
        }
    });

    if !renew {
        return;
    }
    // Nothing was handed over, so Windows now holds empty data for this
    // format and will not ask again. The promise has to be made afresh
    // or the clipboard stays permanently empty -- which is the fault
    // this whole branch exists to fix.
    let give_up = STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return true;
        };
        state.renewals += 1;
        state.renewals > RENEWALS_BEFORE_GIVING_UP
    });
    if give_up {
        tracing::warn!(
            format_id,
            "this machine has promised the far machine's clipboard {RENEWALS_BEFORE_GIVING_UP} \
             times and served none of them, so it is giving the clipboard back. Copying and \
             pasting on this machine will work again; what the other machine copied is not \
             available here"
        );
        // SAFETY: posting to this window, which is this thread's own.
        let _ = unsafe { PostMessageW(window, WM_RELEASE, WPARAM(0), LPARAM(0)) };
        return;
    }
    tracing::info!(
        format_id,
        "this paste produced nothing; promising it again so the next one can be served"
    );
    // SAFETY: posting to this window, which is this thread's own. Posted
    // rather than done here because the clipboard is still open for the
    // length of this handler.
    let _ = unsafe { PostMessageW(window, WM_RENEW, WPARAM(0), LPARAM(0)) };
}

/// The Windows format numbers one of our formats can be supplied as.
fn native_ids(formats: Formats, format: &ClipFormat) -> Vec<u32> {
    match format {
        ClipFormat::Text => vec![CF_UNICODETEXT.0 as u32],
        ClipFormat::Html => vec![formats.html],
        // Offered every way applications ask: the plain bitmap is the one
        // Windows will turn into a `CF_BITMAP`, the v5 one carries alpha for
        // those that read it, and PNG spares the conversion for the rest.
        ClipFormat::Png => vec![formats.png, CF_DIB.0 as u32, CF_DIBV5.0 as u32],
        ClipFormat::Uris => vec![CF_HDROP.0 as u32],
        ClipFormat::Other(_) => Vec::new(),
    }
}

fn write_native(formats: Formats, format: &ClipFormat, as_id: u32, bytes: &[u8]) -> Result<()> {
    match format {
        ClipFormat::Text => {
            let text = String::from_utf8_lossy(bytes);
            let utf16 = wide(&text);
            let raw: Vec<u8> = utf16.iter().flat_map(|c| c.to_le_bytes()).collect();
            write_raw(CF_UNICODETEXT.0 as u32, &raw)
        }
        ClipFormat::Html => write_raw(formats.html, &html::wrap(bytes)),
        ClipFormat::Png if as_id == formats.png => write_raw(formats.png, bytes),
        ClipFormat::Png if as_id == CF_DIBV5.0 as u32 => {
            write_raw(CF_DIBV5.0 as u32, &image::png_to_dibv5(bytes)?)
        }
        ClipFormat::Png => write_raw(CF_DIB.0 as u32, &image::png_to_dib(bytes)?),
        ClipFormat::Uris => write_raw(CF_HDROP.0 as u32, &files::uri_list_to_hdrop(bytes)?),
        ClipFormat::Other(_) => Err(ClipboardError::Refused(format.clone())),
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CLIPBOARDUPDATE => {
            STATE.with(|cell| {
                let mut slot = cell.borrow_mut();
                let Some(state) = slot.as_mut() else {
                    return;
                };
                if state.ours {
                    // Our own offer coming back round; announcing it would send
                    // the far machine's clipboard straight back to it.
                    state.ours = false;
                    return;
                }
                state.offer = None;
                state.cache.clear();
            });
            // One copy arrives as several of these: an application that sets
            // the clipboard through OLE empties it and fills it in more than
            // one step, and each step is a notice, a few milliseconds apart.
            // Reporting each made every copy two offers, and a paste that had
            // asked for the first was refused as stale when the second
            // replaced it. So the report waits a moment, and a burst becomes
            // one report of how the clipboard was finally left.
            // SAFETY: a timer on this thread's own window; re-setting an
            // existing timer restarts it.
            unsafe {
                SetTimer(window, SETTLE_TIMER, SETTLE_MS, None);
            }
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == SETTLE_TIMER => {
            // SAFETY: balanced against the SetTimer above.
            unsafe {
                let _ = KillTimer(window, SETTLE_TIMER);
            }
            let report = STATE.with(|cell| {
                let slot = cell.borrow();
                slot.as_ref().map(|s| (s.formats, s.changes.clone()))
            });
            if let Some((formats, sender)) = report {
                if Opened::take(HWND::default()).is_ok() {
                    let _ = sender.send(available_now(formats));
                }
            }
            LRESULT(0)
        }
        WM_RENDERFORMAT => {
            // First thing, before anything is looked at. Whether this
            // line appears is the whole of the difference between "the
            // handler did not run" and "the handler ran and said
            // nothing", and telling those apart has cost three rounds.
            witness(&format!("WM_RENDERFORMAT entered for format {}", wparam.0));
            render(window, wparam.0 as u32);
            LRESULT(0)
        }
        WM_RENEW => {
            witness("promising the far machine's clipboard again");
            if let Err(e) = take_clipboard(window) {
                tracing::warn!("could not promise the far machine's clipboard again: {e}");
            }
            LRESULT(0)
        }
        WM_RENDERALLFORMATS => {
            witness("WM_RENDERALLFORMATS entered; everything promised is wanted now");
            // The process is going away; anything promised has to be made real
            // now or it vanishes with us.
            STATE.with(|cell| {
                let offered = cell
                    .borrow()
                    .as_ref()
                    .and_then(|s| s.offer.as_ref().map(|(f, _)| f.clone()))
                    .unwrap_or_default();
                let formats = cell.borrow().as_ref().map(|s| s.formats);
                if let (Some(formats), false) = (formats, offered.is_empty()) {
                    if Opened::take(window).is_ok() {
                        for format in &offered {
                            for id in native_ids(formats, format) {
                                render(window, id);
                            }
                        }
                    }
                }
            });
            LRESULT(0)
        }
        WM_OFFER => {
            let _ = take_clipboard(window);
            LRESULT(0)
        }
        WM_RELEASE => {
            STATE.with(|cell| {
                if let Some(state) = cell.borrow_mut().as_mut() {
                    state.offer = None;
                    state.cache.clear();
                }
            });
            LRESULT(0)
        }
        // SAFETY: handing anything else along is what the API requires.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// Take the clipboard, promising the offered formats without supplying them.
fn take_clipboard(window: HWND) -> Result<()> {
    let (formats, offered) = STATE.with(|cell| {
        let slot = cell.borrow();
        let state = slot.as_ref();
        (
            state.map(|s| s.formats),
            state
                .and_then(|s| s.offer.as_ref().map(|(f, _)| f.clone()))
                .unwrap_or_default(),
        )
    });
    let (Some(formats), false) = (formats, offered.is_empty()) else {
        return Ok(());
    };

    let _open = Opened::take(window)?;
    // SAFETY: the clipboard is open and owned by this window.
    unsafe { EmptyClipboard() }.map_err(|_| last_error("clearing the clipboard"))?;

    STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.ours = true;
        }
    });
    for format in &offered {
        for id in native_ids(formats, format) {
            // A null handle is the promise: the data is produced only if
            // something pastes it.
            // SAFETY: promising a format takes no memory.
            let _ = unsafe { SetClipboardData(id, HANDLE::default()) };
        }
    }
    // Who the system thinks owns it, rather than who we asked it to.
    // If these ever differ, nothing inside the render matters and the
    // question is which window holds the promise -- which is the first
    // thing to establish and was, for three rounds, unestablished.
    // SAFETY: reading the owner takes no pointers.
    let owner = unsafe { GetClipboardOwner() }.map(|o| o.0).ok();
    witness(&format!(
        "promised {:?}; the clipboard's owner is now window={:?}, and this window is \
         {:?} ({})",
        offered,
        owner.unwrap_or(std::ptr::null_mut()),
        window.0,
        if owner == Some(window.0) {
            "the same"
        } else {
            "NOT the same -- renders will go elsewhere"
        }
    ));
    Ok(())
}

/// A handle to the thread that owns the clipboard window.
pub struct WindowsClipboard {
    formats: Formats,
    changes: Receiver<Available>,
    offers: Sender<(Vec<ClipFormat>, Box<dyn Fetch>)>,
    thread_id: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WindowsClipboard {
    /// Start watching the clipboard.
    pub fn start() -> Result<WindowsClipboard> {
        let (changes_tx, changes_rx) = mpsc::channel();
        let (offers_tx, offers_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        let handle = std::thread::Builder::new()
            .name("smkvm-clipboard".into())
            .spawn(move || clipboard_thread(changes_tx, offers_rx, ready_tx))
            .map_err(|e| ClipboardError::Display(format!("could not start a thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok((thread_id, formats))) => Ok(WindowsClipboard {
                formats,
                changes: changes_rx,
                offers: offers_tx,
                thread_id,
                handle: Some(handle),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ClipboardError::Display(
                "the clipboard thread stopped before it started".into(),
            )),
        }
    }

    /// What is on the clipboard now.
    pub fn available(&mut self) -> Result<Available> {
        let _open = Opened::take(HWND::default())?;
        Ok(available_now(self.formats))
    }

    /// Offer another machine's clipboard here.
    pub fn offer(&self, formats: &[ClipFormat], source: Box<dyn Fetch>) -> Result<()> {
        self.handle().offer(formats, source)
    }

    /// Stop offering, leaving the clipboard to whatever takes it next.
    pub fn release(&self) -> Result<()> {
        self.handle().release()
    }

    /// A way to offer and read from another thread, while this one watches.
    ///
    /// Watching blocks on the next change, so whoever needs to read the
    /// clipboard or put an offer on it cannot be the same holder. The handle
    /// reaches the same window thread and the same clipboard.
    pub fn handle(&self) -> WindowsHandle {
        WindowsHandle {
            formats: self.formats,
            offers: self.offers.clone(),
            thread_id: self.thread_id,
        }
    }
}

/// Offers to and reads from the clipboard, from any thread.
#[derive(Clone)]
pub struct WindowsHandle {
    formats: Formats,
    offers: Sender<(Vec<ClipFormat>, Box<dyn Fetch>)>,
    thread_id: u32,
}

impl WindowsHandle {
    pub fn offer(&self, formats: &[ClipFormat], source: Box<dyn Fetch>) -> Result<()> {
        self.offers
            .send((formats.to_vec(), source))
            .map_err(|_| ClipboardError::Display("the clipboard thread has stopped".into()))?;
        self.post(WM_OFFER)
    }

    pub fn release(&self) -> Result<()> {
        self.post(WM_RELEASE)
    }

    fn post(&self, message: u32) -> Result<()> {
        // SAFETY: posting to a thread id is safe whether or not it is still
        // running.
        unsafe { PostThreadMessageW(self.thread_id, message, WPARAM(0), LPARAM(0)) }
            .map_err(|_| last_error("reaching the clipboard thread"))
    }
}

impl crate::Read for WindowsHandle {
    fn read(&mut self, format: &ClipFormat) -> Result<Vec<u8>> {
        read_format(self.formats, format)
    }
}

impl Write for WindowsHandle {
    fn offer(&mut self, formats: &[ClipFormat], source: Box<dyn Fetch>) -> Result<()> {
        WindowsHandle::offer(self, formats, source)
    }

    fn release(&mut self) -> Result<()> {
        WindowsHandle::release(self)
    }
}

impl crate::Read for WindowsClipboard {
    fn read(&mut self, format: &ClipFormat) -> Result<Vec<u8>> {
        read_format(self.formats, format)
    }
}

impl crate::Watch for WindowsClipboard {
    fn next_change(&mut self) -> Option<Available> {
        self.changes.recv().ok()
    }
}

/// Watches without being able to offer, for a caller that only wants changes.
pub struct WindowsWatcher(pub WindowsClipboard);

impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        let _ = self.handle().post(WM_QUIT);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn clipboard_thread(
    changes: Sender<Available>,
    offers: Receiver<(Vec<ClipFormat>, Box<dyn Fetch>)>,
    ready: Sender<Result<(u32, Formats)>>,
) {
    let formats = Formats::register();
    let class_name = wide("SmkvmClipboard");

    // SAFETY: the module handle call takes no pointers.
    let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
        let _ = ready.send(Err(last_error("finding this module")));
        return;
    };
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    // Registering twice is harmless; the second attempt fails and the existing
    // class is used.
    // SAFETY: the class points at a function that lives for the process.
    unsafe { RegisterClassW(&class) };

    let title = wide("smkvm clipboard");
    // SAFETY: a message-only window needs no geometry and is never shown.
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
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
    };
    let Ok(window) = window else {
        let _ = ready.send(Err(last_error("making a window for the clipboard")));
        return;
    };

    STATE.with(|cell| {
        *cell.borrow_mut() = Some(State {
            renewals: 0,
            formats,
            changes,
            offer: None,
            cache: HashMap::new(),
            ours: false,
        });
    });

    // The format listener, rather than the viewer chain it replaced: a chain
    // is only as reliable as every other watcher in it.
    // SAFETY: the window was just created and outlives the registration.
    if unsafe { AddClipboardFormatListener(window) }.is_err() {
        let _ = ready.send(Err(last_error("asking to be told of clipboard changes")));
        // SAFETY: the window came from CreateWindowExW.
        unsafe {
            let _ = DestroyWindow(window);
        }
        return;
    }

    // SAFETY: no pointers involved.
    let thread_id = unsafe { GetCurrentThreadId() };

    // A window of a higher integrity level than the process asking it
    // for something does not, by default, receive that ask. Windows
    // drops the message and tells nobody -- which is the same rule,
    // with the same silence, as the very first fault this program ever
    // had: `SendInput` refused by UIPI, returning zero and saying
    // nothing.
    //
    // It matters here and nowhere else in this program's history
    // because the worker runs as the system account, and the
    // application the person is pasting into does not. Delayed
    // rendering works by the system sending `WM_RENDERFORMAT` to the
    // clipboard's owner on behalf of whoever is pasting; from a medium
    // integrity application to a system integrity window, that send is
    // exactly what UIPI exists to stop. The clipboard then lists the
    // formats, names us as the owner, and yields nothing -- with no
    // handler ever entered, which is precisely what a real machine
    // showed.
    //
    // Under the scheduled task none of this arises: that daemon runs at
    // high integrity but in the person's own session, and the same code
    // pastes correctly there. The control is what makes this the
    // explanation rather than a guess.
    //
    // Asking for each message by name rather than turning the filter
    // off wholesale: these three are what a clipboard owner must
    // receive, and nothing else needs to reach a window running as the
    // system account.
    let mut allowed = Vec::new();
    for message in [WM_RENDERFORMAT, WM_RENDERALLFORMATS, WM_DESTROYCLIPBOARD] {
        // SAFETY: a window this thread made, and no filter structure.
        let asked = unsafe { ChangeWindowMessageFilterEx(window, message, MSGFLT_ALLOW, None) };
        // Both outcomes are said. A call that quietly succeeded and a
        // call that was never made look identical afterwards, and
        // telling those apart is the entire subject of this round.
        allowed.push(format!(
            "{message}={}",
            if asked.is_ok() { "allowed" } else { "refused" }
        ));
    }

    witness(&format!(
        "the clipboard window is up: window={:?} thread={thread_id} \
         messages-from-less-privileged[{}]",
        window.0,
        allowed.join(" ")
    ));
    let _ = ready.send(Ok((thread_id, formats)));

    let mut message = MSG::default();
    // SAFETY: `message` is a valid out-parameter for each call.
    while unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) }.as_bool() {
        // An offer posted from another thread arrives as a message with the
        // data waiting on the channel.
        if message.message == WM_OFFER {
            if let Ok(offer) = offers.try_recv() {
                STATE.with(|cell| {
                    if let Some(state) = cell.borrow_mut().as_mut() {
                        state.offer = Some(offer);
                        state.cache.clear();
                    }
                });
            }
            // Thread messages have no window, so the procedure is called here.
            // SAFETY: the window is this thread's own.
            unsafe { window_proc(window, WM_OFFER, WPARAM(0), LPARAM(0)) };
            continue;
        }
        if message.message == WM_RELEASE {
            // SAFETY: as above.
            unsafe { window_proc(window, WM_RELEASE, WPARAM(0), LPARAM(0)) };
            continue;
        }
        // SAFETY: both take the message just filled in.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    // SAFETY: the window came from CreateWindowExW and is not used again.
    unsafe {
        let _ = RemoveClipboardFormatListener(window);
        let _ = DestroyWindow(window);
    }
    STATE.with(|cell| *cell.borrow_mut() = None);
}
