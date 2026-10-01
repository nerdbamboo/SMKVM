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
use windows::Win32::Foundation::{
    SetLastError, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::Com::{CoTaskMemFree, DATADIR_GET, FORMATETC};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, EnumClipboardFormats,
    GetClipboardData, GetClipboardFormatNameW, GetClipboardOwner, GetClipboardSequenceNumber,
    GetOpenClipboardWindow, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::{
    OleGetClipboard, OleInitialize, OleUninitialize, CF_BITMAP, CF_DIB, CF_DIBV5, CF_HDROP,
    CF_UNICODETEXT,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetMessageW, GetWindowThreadProcessId, KillTimer, PostMessageW, PostThreadMessageW,
    RegisterClassW, SetTimer, TranslateMessage, HWND_MESSAGE, MSG, MSGFLT_ALLOW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_CLIPBOARDUPDATE, WM_DESTROYCLIPBOARD, WM_QUIT, WM_RENDERALLFORMATS,
    WM_RENDERFORMAT, WM_TIMER, WNDCLASSW,
};

use crate::{files, html, image, Available, ClipboardError, Fetch, Result, Write};
use crate::{step, witness, Rendered, RENDER_BUDGET};

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

/// Asks the window thread to start or stop being told about copies.
///
/// Separate from giving the clipboard back, and that separation is
/// the whole point of it. Watching and offering are two jobs on one
/// window: noticing what the person copied, and holding out what
/// another machine copied. On a machine with a reader, the first of
/// those is known to be wrong by construction -- a process running
/// as the system account cannot see what the person copied -- while
/// the second works and must keep working.
///
/// Before this there was no way to stop one without stopping the
/// other, so the worker went on looking, settling, concluding and
/// acting on an answer that was wrong every time, arriving after the
/// reader's correct one and overwriting it. A wrong answer that
/// arrives second is worse than no answer.
const WM_WATCH: u32 = WM_APP + 4;

/// How many times a promise is renewed before the clipboard is given
/// back altogether.
///
/// Holding a promise that cannot be kept is worse for the person than
/// holding nothing: their own last copy is gone and every paste yields
/// emptiness. After this many failures the clipboard is released, so
/// copying and pasting on the machine itself starts working again even
/// though what the other machine offered is lost.
const RENEWALS_BEFORE_GIVING_UP: u32 = 3;

/// How many times `smkvm status` looks to see whether the clipboard is
/// held open, and how long it leaves between looks.
///
/// Spread across roughly a second, which is long enough that an
/// application merely copying or pasting will have let go, and short
/// enough that nobody minds the command taking it.
const HELD_OPEN_SAMPLES: usize = 5;
const HELD_OPEN_GAP: Duration = Duration::from_millis(200);

/// How many times `smkvm status` will wait out `OPEN_PATIENCE` before
/// reporting that the clipboard could not be opened.
const OPEN_ATTEMPTS_FOR_STATUS: u32 = 4;

/// Why the clipboard is being given back, carried in `WM_RELEASE`'s
/// `wparam`.
///
/// Every release used to look the same in the log, and releasing is
/// the act that ends a person's ability to paste what the other
/// machine copied. Two of these mean something is wrong here and one
/// is ordinary housekeeping; a single line saying "giving the
/// clipboard back" cannot be read as either.
const WITHDRAWN: usize = 0;
const RENDERS_KEPT_FAILING: usize = 1;
const OFFER_VANISHED: usize = 2;

fn why_released(reason: usize) -> &'static str {
    match reason {
        RENDERS_KEPT_FAILING => "because renders kept failing",
        OFFER_VANISHED => "because the offer was gone before the render finished",
        // The ordinary one: the service says the far machine's
        // clipboard is no longer on offer.
        _ => "because the offer was withdrawn",
    }
}

/// How long to wait before looking at a copy again, and how many
/// times.
///
/// A file copy does not arrive on the clipboard all at once. The
/// shell puts its OLE data object there first and the standard
/// formats the object stands for -- `CF_HDROP` among them -- appear
/// around it afterwards. Looking once, sixty milliseconds after the
/// first notice, caught a clipboard holding nothing but
/// `49161=DataObject`: a copy still being assembled, which is
/// indistinguishable from a copy of something we do not share unless
/// you look again.
///
/// Eight looks a quarter-second apart is two seconds, which is what
/// a probe in the session needed to see all five formats. It costs
/// nothing when the copy is complete first time, because the first
/// look ends it.
const LOOK_AGAIN_AFTER: u32 = 250;
const LOOK_AT_MOST: u32 = 8;

/// How many `WM_CLIPBOARDUPDATE` messages this process has been sent.
///
/// Process-wide rather than per window, because there is one
/// clipboard window per process and the question being answered is
/// "does this process get told at all".
pub static NOTICED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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

/// The number the last failed call left behind.
fn last_win32() -> u32 {
    windows::core::Error::from_win32().code().0 as u32 & 0xFFFF
}

fn last_error(what: &str) -> ClipboardError {
    ClipboardError::Display(format!("{what}: {}", describe_win32(last_win32())))
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

/// A question for the thread that has a COM apartment.
///
/// Deliberately a message rather than a call. Everything here can
/// wait -- `OleGetClipboard` talks to the process that did the
/// copying, and `GetData` may make it produce the data -- and this
/// file has produced three separate faults from giving the clipboard
/// window's thread something that can wait. So the window thread
/// posts and never looks back; this thread does the waiting, and
/// reports what it found down the same channel a copy would have
/// gone down anyway.
enum Ask {
    /// Nothing we share was visible the ordinary way. Look with an
    /// apartment, and say what each way of looking saw.
    WhatIsOnIt(Formats),
    /// Read one format, from a thread that has an apartment.
    Read(Formats, ClipFormat, Sender<Result<Vec<u8>>>),
}

/// How long a read on the OLE thread may take.
///
/// Inside the five seconds the service allows for a read, with room
/// for the answer to get back, so a reader is told something rather
/// than left to the outer deadline.
const OLE_READ_WITHIN: Duration = Duration::from_millis(3000);

/// The one thread in this process with a COM apartment.
///
/// It exists because of a measurement. A copy of a file arrives on
/// the clipboard as an OLE data object; a process in the session sees
/// `CF_HDROP` beside it and can fetch the file list, and the worker
/// -- running as the system account, on the same desktop, at the same
/// instant -- enumerated the data object alone, eight times across
/// two seconds. Whether that is the account or the missing apartment
/// could not be told apart from outside, because everything available
/// to run in that session initialises COM whether it is asked to or
/// not.
///
/// So this thread is both the experiment and, if the experiment comes
/// out the way the evidence points, the fix. It calls `OleInitialize`
/// and then looks in the two ways that can disagree: plain
/// `EnumClipboardFormats`, which is exactly what the window thread
/// already does and differs only in having an apartment behind it,
/// and `IDataObject::EnumFormatEtc`, which asks the data object what
/// it can supply rather than what is registered. Both answers are
/// said. If the first is full, an apartment was all that was wanted.
/// If only the second is full, the formats live in the object and
/// never on the clipboard. If both are bare, it is the account, and
/// no amount of COM will help.
fn ole_thread(asks: Receiver<Ask>, changes: Sender<Available>) {
    // SAFETY: once, on this thread, which exists for nothing else.
    let apartment = unsafe { OleInitialize(None) };
    match &apartment {
        Ok(()) => witness("the clipboard has a thread with a COM apartment"),
        Err(e) => witness(&format!(
            "the clipboard's COM thread could not get an apartment ({e}); it will still \
             look the plain way"
        )),
    }
    while let Ok(ask) = asks.recv() {
        match ask {
            Ask::WhatIsOnIt(formats) => look_with_an_apartment(formats, &changes),
            Ask::Read(formats, format, back) => {
                let _ = back.send(read_format(formats, &format));
            }
        }
    }
    if apartment.is_ok() {
        // SAFETY: balanced against the initialise above.
        unsafe { OleUninitialize() };
    }
}

/// Look at the clipboard both ways, say both answers, and report a
/// copy if either of them found one.
fn look_with_an_apartment(formats: Formats, changes: &Sender<Available>) {
    let plainly = match Opened::take(HWND::default()) {
        Ok(_open) => Ok((available_now(formats), everything_on_it())),
        Err(e) => Err(e),
    };
    let offered = what_the_data_object_offers();

    witness(&format!(
        "looked again from a thread with a COM apartment. Plain Win32 sees: {}. The data \
         object offers: {}",
        match &plainly {
            Ok((_, on_it)) => format!("[{}]", on_it.join(", ")),
            Err(e) => format!("(the clipboard would not open: {e})"),
        },
        match &offered {
            Ok(ids) => format!(
                "[{}]",
                ids.iter()
                    .map(|(_, named)| named.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(e) => format!("(it could not be asked: {e})"),
        }
    ));

    // Whichever way saw something, that is what was copied. Both use
    // the same rule for deciding which formats are ours.
    let found = match &plainly {
        Ok((found, _)) if !found.formats.is_empty() => found.clone(),
        _ => match &offered {
            Ok(ids) => which_of_ours(formats, |f| ids.iter().any(|(id, _)| *id == f)),
            Err(_) => Available {
                formats: Vec::new(),
            },
        },
    };
    if found.formats.is_empty() {
        witness(
            "neither way of looking found a format we share, so the other machines were \
             not told. If both lists above are bare, this process cannot see what a \
             process in the session can, and an apartment is not the difference",
        );
        return;
    }
    let announced = format!("{:?}", found.formats);
    match changes.send(found) {
        Ok(()) => witness(&format!(
            "something was copied here: {announced}; telling whoever is listening (found \
             only from the thread with an apartment)"
        )),
        Err(_) => witness(&format!(
            "something was copied here: {announced}, but NOBODY IS LISTENING for copies \
             any more, so no other machine will be told"
        )),
    }
}

/// What the clipboard's data object says it can supply.
///
/// A different question from "what formats are on the clipboard", and
/// the two can disagree: `OleSetClipboard` puts the object there, and
/// what the object advertises through `EnumFormatEtc` is not
/// necessarily registered with `SetClipboardData` for everyone to
/// enumerate.
fn what_the_data_object_offers() -> Result<Vec<(u32, String)>> {
    // SAFETY: takes no pointers; the interface is reference counted
    // by the binding.
    let object = unsafe { OleGetClipboard() }
        .map_err(|e| ClipboardError::Display(format!("asking OLE for the clipboard: {e}")))?;
    // SAFETY: a live interface, and the direction is the documented
    // constant for "what can be read out of it".
    let list = unsafe { object.EnumFormatEtc(DATADIR_GET.0 as u32) }
        .map_err(|e| ClipboardError::Display(format!("asking what it can supply: {e}")))?;
    let mut out = Vec::new();
    loop {
        let mut one = [FORMATETC::default(); 1];
        let mut got = 0u32;
        // SAFETY: `one` is valid for the single element asked for,
        // and `got` for the count written back.
        let more = unsafe { list.Next(&mut one, Some(&mut got)) };
        if more.is_err() || got == 0 {
            break;
        }
        let id = one[0].cfFormat as u32;
        out.push((id, name_of(id)));
        // The enumerator allocates a target device for some entries
        // and hands ownership over with it.
        if !one[0].ptd.is_null() {
            // SAFETY: allocated by COM, released once, and not read
            // again.
            unsafe { CoTaskMemFree(Some(one[0].ptd as *const _)) };
        }
        if out.len() > 32 {
            break;
        }
    }
    Ok(out)
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

/// How many times the clipboard has changed since the machine
/// started.
///
/// A second, independent way of noticing a copy, and the reason it
/// is here is that the first one stopped working for one process and
/// not another. The reader's window registers as a format listener,
/// the registration is accepted, the window pumps -- and
/// `WM_CLIPBOARDUPDATE` never arrives, while the worker's window in
/// the same session on the same clipboard receives every one.
///
/// This asks nobody's permission and needs no window, no message
/// queue and no notification. If the number moves while no message
/// arrives, the notification path is broken for this process and the
/// clipboard itself is fine -- which is a diagnosis and a way to
/// carry on, in the same call.
pub fn sequence_number() -> u32 {
    // SAFETY: takes no pointers.
    unsafe { GetClipboardSequenceNumber() }
}

/// Is anybody holding the clipboard open right now?
///
/// Only one process may have it open at a time, so a window left in
/// here does not merely stop this program working -- it stops
/// *everything* in that session copying and pasting, for as long as it
/// lasts. There is no timeout and no recovery but the holder letting
/// go or dying.
///
/// That happened, held by this program's own window, and it is the
/// reason the whole of this file is careful about what may run between
/// an open and a close.
pub fn held_open_by() -> Option<isize> {
    // SAFETY: takes no pointers.
    let window = unsafe { GetOpenClipboardWindow() };
    match window {
        Ok(w) if !w.0.is_null() => Some(w.0 as isize),
        _ => None,
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
    which_of_ours(formats, |format| {
        unsafe { IsClipboardFormatAvailable(format) }.is_ok()
    })
}

/// Which of the formats we share are among these.
///
/// Split out so that the two ways of finding out what has been copied
/// -- asking Windows which formats are on the clipboard, and asking a
/// data object which it can supply -- decide by the same rule. There
/// is one list of formats we share, and keeping it one list is the
/// reason an earlier round could rule out a whole class of
/// explanation in a sentence.
fn which_of_ours(formats: Formats, has: impl Fn(u32) -> bool) -> Available {
    let mut out = Vec::new();
    if has(CF_UNICODETEXT.0 as u32) {
        out.push(ClipFormat::Text);
    }
    if has(formats.html) {
        out.push(ClipFormat::Html);
    }
    // Every way an application may offer a picture, not only the two
    // that were here. `CF_DIBV5` carries alpha and some applications
    // offer it alone; `CF_BITMAP` is what the oldest ones offer, and
    // Windows synthesises the rest from it only once something asks.
    // An image offered in a way this list does not name is an image
    // nobody is told about, and the failure is silent at both ends.
    if has(formats.png) || has(CF_DIB.0 as u32) || has(CF_DIBV5.0 as u32) || has(CF_BITMAP.0 as u32)
    {
        out.push(ClipFormat::Png);
    }
    if has(CF_HDROP.0 as u32) {
        out.push(ClipFormat::Uris);
    }
    Available { formats: out }
}

/// Which native identifiers [`available_now`] looks at, so that
/// finding none of them can say which it was looking for.
fn what_we_look_for(formats: Formats) -> Vec<u32> {
    vec![
        CF_UNICODETEXT.0 as u32,
        formats.html,
        formats.png,
        CF_DIB.0 as u32,
        CF_DIBV5.0 as u32,
        CF_BITMAP.0 as u32,
        CF_HDROP.0 as u32,
    ]
}

/// Everything on the clipboard right now, with whatever name Windows
/// has for each. The clipboard must already be open.
///
/// For one line in one place: a copy noticed here with none of the
/// formats we share on it. "None of the formats we share were on it"
/// is a true statement that cost a round, because it named neither
/// what was there nor what was wanted, and settling that took
/// somebody sitting in the session with a Win32 enumerator. A
/// diagnostic that reports a mismatch should name both sides of it.
fn everything_on_it() -> Vec<String> {
    let mut out = Vec::new();
    let mut next = 0u32;
    loop {
        // Cleared first, because the return value means two things.
        //
        // `EnumClipboardFormats` answers zero both for "that was the
        // last one" and for "that did not work", and tells them
        // apart only by the error it leaves behind: clear for the
        // end of the list, something else for a failure. Read
        // naively it reports every failure as an empty clipboard --
        // which is what `the clipboard now holds []` was, printed
        // one line above a list of formats that something else had
        // just found on the same clipboard.
        //
        // This is the second call in this file with that shape. The
        // first was `SetClipboardData`, whose null return meant both
        // "promised" and "refused", and which reported every promise
        // it ever made as a refusal until the error was cleared
        // before it. A zero that means two things will be read as
        // the wrong one.
        // SAFETY: clearing the thread's last-error value.
        unsafe { SetLastError(WIN32_ERROR(0)) };
        // SAFETY: the clipboard is open, which is what this requires.
        next = unsafe { EnumClipboardFormats(next) };
        if next == 0 {
            let why = last_win32();
            if why != 0 {
                out.push(format!(
                    "(the list could not be read: {})",
                    describe_win32(why)
                ));
            }
            break;
        }
        out.push(name_of(next));
        if out.len() > 32 {
            out.push("(and more)".to_string());
            break;
        }
    }
    out
}

/// One format, with whatever name Windows has for it.
///
/// Needs no clipboard open, which is why it can be used from the
/// data object's side as well.
fn name_of(format: u32) -> String {
    let mut name = [0u16; 128];
    // SAFETY: the buffer is valid for its own length. A standard
    // format has no registered name and returns zero, which is not an
    // error.
    let len = unsafe { GetClipboardFormatNameW(format, &mut name) };
    if len > 0 {
        format!(
            "{format}={}",
            String::from_utf16_lossy(&name[..len as usize])
        )
    } else {
        format!("{format}=(standard)")
    }
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
    offer: Option<Offer>,
    /// Anything already fetched, so a second paste does not fetch again.
    cache: HashMap<ClipFormat, Vec<u8>>,
    /// Whether this window is registered to be told about copies.
    ///
    /// Tracked because the two calls are not idempotent: registering
    /// twice fails, and so does unregistering what was never
    /// registered.
    watching: bool,
    /// How many times the copy now settling has been looked at.
    looks: u32,
    /// What the clipboard held the first time this copy was looked
    /// at, kept so that giving up can show the before and the after
    /// rather than only the after.
    first_look: Option<String>,
    /// Where to send a question for the thread that has a COM
    /// apartment. Posting on this cannot wait: the channel is
    /// unbounded, and that is the point.
    asks: Sender<Ask>,
}

/// What the far machine has copied, and how many chances are left to
/// hand it over.
///
/// The count lives here rather than beside the offer, and that is the
/// whole reason this is a struct. It used to sit in `State`, reset
/// only by a render that succeeded -- so once a promise had been
/// given up on, the counter stayed above its limit for the life of
/// the window, and the *next* offer was released after its first
/// unsuccessful render. One clipboard that could not be served
/// poisoned every clipboard after it, and from outside that reads
/// exactly like the worker announcing something it does not hold.
///
/// A count that can outlive what it counts will eventually be applied
/// to something else. Keeping it inside the offer makes that
/// impossible: letting the offer go takes the count with it, and a
/// new offer starts from zero because it is a new value.
struct Offer {
    formats: Vec<ClipFormat>,
    source: Box<dyn Fetch>,
    /// Renders of this offer that could not be served, and what the
    /// last of them said.
    ///
    /// Only renders, and only ones that failed. A limit is a
    /// statement about one particular kind of event, and counting
    /// anything else against it means giving up on something that
    /// never went wrong -- which reads, from outside, as the program
    /// withdrawing an offer for no reason at all.
    ///
    /// The reason is kept alongside the count because the count on
    /// its own says only that a limit was reached. `4 of 3` is a
    /// limit working perfectly and tells nobody what it was working
    /// against.
    failures: u32,
    last_failure: Option<String>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Produce one promised format, now that something has asked for it.
/// Supply one promised format, and say how it went.
///
/// Returns the outcome rather than acting on it: whether to promise
/// again is `Rendered::needs_renewing`, which lives beside the
/// outcomes and is tested. The shape before this threaded a `bool`
/// through the body, and nothing at all stopped the success path
/// setting it -- which would re-take the clipboard, emptying it, and
/// throw away the data handed over a moment earlier.
fn render(format_id: u32) -> Rendered {
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else {
            return Rendered::NoState;
        };
        let formats = state.formats;
        let Some(offer) = state.offer.as_ref() else {
            // The one outcome that is about this program's own state
            // rather than about Windows, so it says what that state
            // was. Reaching here means the promise outlived the thing
            // it was a promise for.
            witness(&format!(
                "asked to produce format {format_id}, but nothing is recorded to fetch \
                 (nothing has been announced to this window, or it has been let go)"
            ));
            return Rendered::NothingOffered;
        };
        let wanted = offer
            .formats
            .iter()
            .find(|f| native_ids(formats, f).contains(&format_id));
        let Some(wanted) = wanted.cloned() else {
            return Rendered::NotOurFormat;
        };

        let asked_at = Instant::now();
        let bytes = match state.cache.get(&wanted) {
            Some(cached) => cached.clone(),
            None => match offer.source.fetch(&wanted) {
                Ok(bytes) => {
                    state.cache.insert(wanted.clone(), bytes.clone());
                    bytes
                }
                Err(e) => return Rendered::CouldNotFetch(e.to_string()),
            },
        };

        // Kept even when it is too late to use, because the next paste
        // will find it here and be instant. A fetch that arrives after
        // its render is not wasted, only mistimed.
        let took = asked_at.elapsed();
        if took > RENDER_BUDGET {
            return Rendered::TooLate(took.as_millis() as u64);
        }

        match write_native(formats, &wanted, format_id, &bytes) {
            Ok(()) => {
                if let Some(offer) = state.offer.as_mut() {
                    offer.failures = 0;
                    offer.last_failure = None;
                }
                Rendered::Served(bytes.len())
            }
            Err(e) => Rendered::CouldNotHandOver(e.to_string()),
        }
    })
}

/// Promise again, or give the clipboard back, after a render that kept
/// nothing.
fn after_a_render_that_kept_nothing(window: HWND, format_id: u32, outcome: &Rendered) {
    let why = outcome.said();
    let counted = STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        // No state, or no offer, means there is nothing to renew
        // *and* nothing worth holding the clipboard for.
        let offer = slot.as_mut()?.offer.as_mut()?;
        offer.failures += 1;
        offer.last_failure = Some(why.clone());
        Some(offer.failures)
    });
    let Some(failures) = counted else {
        witness(&format!(
            "the offer was gone by the time the render of format {format_id} finished, \
             so the clipboard is being given back"
        ));
        // SAFETY: posting to this window, which is this thread's own.
        let _ = unsafe { PostMessageW(window, WM_RELEASE, WPARAM(OFFER_VANISHED), LPARAM(0)) };
        return;
    };
    if failures > RENEWALS_BEFORE_GIVING_UP {
        witness(&format!(
            "{failures} renders of format {format_id} could not be served, which is past \
             the limit of {RENEWALS_BEFORE_GIVING_UP}, so the clipboard is being given \
             back. The last one said: {why}. Copying and pasting on this machine will \
             work again; what the other machine copied is not available here"
        ));
        // SAFETY: posting to this window, which is this thread's own.
        let _ =
            unsafe { PostMessageW(window, WM_RELEASE, WPARAM(RENDERS_KEPT_FAILING), LPARAM(0)) };
        return;
    }
    witness(&format!(
        "render {failures} of {RENEWALS_BEFORE_GIVING_UP} for format {format_id} could not \
         be served ({why}), so the promise is being made again"
    ));
    // SAFETY: posting to this window, which is this thread's own. Posted
    // rather than done here because the clipboard is still open for the
    // length of the handler this was called from.
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
            // Counted, and said with the count.
            //
            // "No message arrived" and "a message arrived and the
            // handler did nothing" shared a symptom for a round, and
            // an absence cannot be told from a dead instrument by
            // looking at an absence. The number makes the first one
            // a thing the log says rather than a thing it omits.
            let seen = NOTICED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            step(&format!(
                "a clipboard change was announced to this window (number {seen} since it \
                 opened; the clipboard is at sequence {})",
                sequence_number()
            ));
            // Whose change is this? Asked of the system rather than
            // remembered, and that distinction is the whole of this
            // fix.
            //
            // It used to be a flag set when we took the clipboard and
            // cleared by the first notice that arrived afterwards. The
            // comment immediately below says why that could not work,
            // and has said so the whole time: one copy arrives as
            // *several* of these. The first consumed the flag and every
            // one after it was taken for somebody else copying -- which
            // threw away our own offer and our own cache, told the far
            // machine that this machine had copied something, and left
            // the worker being asked to read a clipboard it was itself
            // in the middle of promising. The log went round that loop
            // for an entire run.
            //
            // The X11 side has always compared against its own owner
            // window rather than remembering; this is the same thing,
            // and it does not care how many notices one copy produces.
            // SAFETY: reading the owner takes no pointers.
            let owner = unsafe { GetClipboardOwner() }.map(|o| o.0).ok();
            let ours = owner == Some(window.0);
            if ours {
                // Our own offer coming back round. Announcing it would
                // send the far machine's clipboard straight back to it.
                return LRESULT(0);
            }
            // Somebody else's copy replaces ours, which is correct --
            // and it is also one of only two places the offer is ever
            // discarded, so it says so with what it discarded.
            let was = on_offer_now();
            STATE.with(|cell| {
                let mut slot = cell.borrow_mut();
                let Some(state) = slot.as_mut() else {
                    return;
                };
                state.offer = None;
                state.cache.clear();
                // A fresh notice is a fresh copy to be looked at, and
                // whatever we were part-way through examining is no
                // longer the thing in front of us.
                state.looks = 0;
                state.first_look = None;
            });
            witness(&format!(
                "somebody else copied something (owner is {:?}, this window is {:?}), so the \
                 far machine's offer of {was} was let go",
                owner.unwrap_or(std::ptr::null_mut()),
                window.0
            ));
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
            let Some((formats, sender)) = report else {
                witness(
                    "a copy settled, but there is no state on this thread to report it with, \
                     so nothing was told about it",
                );
                return LRESULT(0);
            };
            {
                // `if let`, not `if ... .is_ok()`. The condition of an
                // `if` is a terminating scope, so a temporary created
                // there is dropped *before* the block runs -- which
                // closed the clipboard and left the body reading a
                // clipboard it did not have open. Binding the guard is
                // what keeps it alive for the block.
                //
                // Everything below this point used to be silent. This
                // is the whole of the outbound path inside this file
                // -- a copy made here, on its way to the other
                // machines -- and it could fail to open the
                // clipboard, find nothing, or send into a channel
                // nobody holds, without leaving a single line. The
                // inbound half has been instrumented for four rounds
                // and this half for none, which is most of why "the
                // client copies and nobody hears" looked like a
                // missing feature rather than a broken one.
                let (found, on_it) = match Opened::take(HWND::default()) {
                    // Not a conclusion. A clipboard that will not
                    // open is a clipboard that has not been looked
                    // at, and "the other machines were not told" is
                    // a decision drawn from a failure to look. With
                    // two halves on one machine -- one holding the
                    // clipboard out, one trying to see it -- a
                    // refusal is ordinary and momentary, and the
                    // right answer is to look again.
                    Err(e) => {
                        let again = STATE.with(|cell| {
                            let mut slot = cell.borrow_mut();
                            match slot.as_mut() {
                                Some(state) => {
                                    state.looks += 1;
                                    state.looks < LOOK_AT_MOST
                                }
                                None => false,
                            }
                        });
                        if again {
                            step(&format!(
                                "a copy settled here and the clipboard would not open \
                                 ({e}); looking again in {LOOK_AGAIN_AFTER} ms"
                            ));
                            // SAFETY: a timer on this thread's own
                            // window; re-setting restarts it.
                            unsafe {
                                SetTimer(window, SETTLE_TIMER, LOOK_AGAIN_AFTER, None);
                            }
                        } else {
                            witness(&format!(
                                "a copy settled here and the clipboard would not open \
                                 ({e}) in any of {LOOK_AT_MOST} attempts, so what was \
                                 copied here is not known -- which is not the same as \
                                 nothing having been copied"
                            ));
                        }
                        return LRESULT(0);
                    }
                    // Both gathered inside the arm: the guard lives
                    // only as long as it, and both need the clipboard
                    // open.
                    Ok(_open) => (available_now(formats), everything_on_it()),
                };
                if found.formats.is_empty() {
                    // Not a conclusion yet. A copy still being
                    // assembled looks exactly like a copy of
                    // something we do not share, and the only way to
                    // tell them apart is to look again.
                    let (looks, first) = STATE.with(|cell| {
                        let mut slot = cell.borrow_mut();
                        let Some(state) = slot.as_mut() else {
                            return (LOOK_AT_MOST, None);
                        };
                        state.looks += 1;
                        if state.first_look.is_none() {
                            state.first_look = Some(on_it.join(", "));
                        }
                        (state.looks, state.first_look.clone())
                    });
                    if looks < LOOK_AT_MOST {
                        step(&format!(
                            "look {looks} of {LOOK_AT_MOST} at this copy found only [{}]; \
                             looking again in {LOOK_AGAIN_AFTER} ms",
                            on_it.join(", ")
                        ));
                        // SAFETY: a timer on this thread's own window;
                        // re-setting an existing timer restarts it.
                        unsafe {
                            SetTimer(window, SETTLE_TIMER, LOOK_AGAIN_AFTER, None);
                        }
                        return LRESULT(0);
                    }
                    // Both enumerations, because which of them is
                    // bare is the whole question: a full second one
                    // means the first was early, and two bare ones
                    // mean this process cannot see what a process in
                    // the session can.
                    witness(&format!(
                        "a copy settled here and none of the formats we share were on it \
                         after {looks} looks over {} ms. At first: [{}]. At last: [{}]. \
                         Looking for: {:?}. Asking the thread with a COM apartment to \
                         look as well",
                        LOOK_AGAIN_AFTER * (LOOK_AT_MOST - 1),
                        first.unwrap_or_default(),
                        on_it.join(", "),
                        what_we_look_for(formats)
                    ));
                    // Posted, never waited on. Everything that thread
                    // does can block, and this is the thread that
                    // must not.
                    STATE.with(|cell| {
                        if let Some(state) = cell.borrow().as_ref() {
                            let _ = state.asks.send(Ask::WhatIsOnIt(formats));
                        }
                    });
                    return LRESULT(0);
                }
                // Found. Say how long it took to appear, because "the
                // first look was enough" and "it took seven more"
                // are different facts about the shell, and only one
                // of them means this re-looking is load-bearing.
                let looks = STATE.with(|cell| {
                    let mut slot = cell.borrow_mut();
                    match slot.as_mut() {
                        Some(state) => {
                            let looks = state.looks + 1;
                            state.looks = 0;
                            state.first_look = None;
                            looks
                        }
                        None => 1,
                    }
                });
                if looks > 1 {
                    witness(&format!(
                        "the copy was still being assembled and took {looks} looks over \
                         {} ms to show what it was",
                        LOOK_AGAIN_AFTER * (looks - 1)
                    ));
                }
                // `witness`, not `step`, while this disagrees with
                // itself: an empty list beside a found format is the
                // open question, and it should not be sitting at
                // debug while it is.
                if on_it.is_empty() {
                    witness(&format!(
                        "the enumeration found nothing on the clipboard, while the \
                         formats we share were found to be {:?} on the same clipboard a \
                         moment earlier. Those two disagree",
                        found.formats
                    ));
                } else {
                    step(&format!("the clipboard now holds [{}]", on_it.join(", ")));
                }
                let announced = format!("{:?}", found.formats);
                match sender.send(found) {
                    Ok(()) => witness(&format!(
                        "something was copied here: {announced}; telling whoever is listening"
                    )),
                    // The receiver is the watch, which lives on
                    // another thread in this process. If it has gone,
                    // nothing this machine copies will ever reach
                    // another one again, and that is worth more than
                    // a discarded `Result`.
                    Err(_) => witness(&format!(
                        "something was copied here: {announced}, but NOBODY IS LISTENING \
                         for copies any more, so no other machine will be told. Nothing \
                         copied on this machine will reach another until it is restarted"
                    )),
                }
            }
            LRESULT(0)
        }
        WM_RENDERFORMAT => {
            // First thing, before anything is looked at. Whether this
            // line appears is the whole of the difference between "the
            // handler did not run" and "the handler ran and said
            // nothing", and telling those apart has cost three rounds.
            // Entered, and -- the line that was missing -- returned.
            // Knowing it was entered while not knowing whether it came
            // back left a hang with nowhere to look: the absence of a
            // line was doing the work of evidence, and only worked at
            // all because somebody could see the log carry on with
            // other traffic. Both ends are said now, and the outcome
            // between them, whichever way it went.
            let format_id = wparam.0 as u32;
            step(&format!("WM_RENDERFORMAT entered for format {format_id}"));
            // Said by a guard rather than by a statement after the
            // call, so that it is said whatever happens in between.
            //
            // This is the single most important line in the file and
            // it had never once appeared. Every reason it went
            // missing was a different one -- a filtered message, a
            // wedged relay, a blocking write -- and in each case the
            // statement that would have reported it was simply never
            // reached. A statement can be skipped; a drop cannot.
            let mut done = Returning::from(format_id);
            let outcome = render(format_id);
            done.was(&outcome);
            drop(done);
            if outcome.needs_renewing() {
                after_a_render_that_kept_nothing(window, format_id, &outcome);
            }
            LRESULT(0)
        }
        WM_RENEW => {
            witness("promising the far machine's formats again");
            if let Err(e) = take_clipboard(
                window,
                "renewing after a render that \
                 could not be served",
            ) {
                tracing::warn!("could not promise the far machine's clipboard again: {e}");
            }
            LRESULT(0)
        }
        WM_RENDERALLFORMATS => {
            step("WM_RENDERALLFORMATS entered; everything promised is wanted now");
            // The process is going away; anything promised has to be made real
            // now or it vanishes with us.
            let mut said = Vec::new();
            STATE.with(|cell| {
                let offered = cell
                    .borrow()
                    .as_ref()
                    .and_then(|s| s.offer.as_ref().map(|o| o.formats.clone()))
                    .unwrap_or_default();
                let formats = cell.borrow().as_ref().map(|s| s.formats);
                if let (Some(formats), false) = (formats, offered.is_empty()) {
                    // Bound, for the reason above: as
                    // `if ... .is_ok()` this ran every `SetClipboardData`
                    // with the clipboard already closed again, which is
                    // where an earlier round's `ERROR_CLIPBOARD_NOT_OPEN`
                    // came from.
                    if let Ok(_open) = Opened::take(window) {
                        for format in &offered {
                            for id in native_ids(formats, format) {
                                // Not said here: this runs with the
                                // clipboard open, and saying anything
                                // can block. Collected and said after.
                                said.push(format!("format {id}: {}", render(id).said()));
                            }
                        }
                    }
                }
            });
            // Said out here, after the guard above has gone and the
            // clipboard is closed again. Collecting these and then
            // never saying them is how this handler stood until now:
            // the one message that can quietly overwrite every promise
            // with real data was also the one that reported nothing.
            for line in said {
                step(&format!("WM_RENDERALLFORMATS {line}"));
            }
            LRESULT(0)
        }
        WM_WATCH => {
            let wanted = wparam.0 != 0;
            let change = STATE.with(|cell| {
                let mut slot = cell.borrow_mut();
                match slot.as_mut() {
                    Some(state) if state.watching != wanted => {
                        state.watching = wanted;
                        true
                    }
                    _ => false,
                }
            });
            if !change {
                return LRESULT(0);
            }
            let outcome = if wanted {
                // SAFETY: this thread's own window.
                unsafe { AddClipboardFormatListener(window) }
            } else {
                // Any settle in flight belongs to a question nobody
                // is asking any more.
                // SAFETY: killing a timer that may not exist is not
                // an error worth acting on.
                unsafe {
                    let _ = KillTimer(window, SETTLE_TIMER);
                }
                // SAFETY: this thread's own window.
                unsafe { RemoveClipboardFormatListener(window) }
            };
            match outcome {
                Ok(()) if wanted => witness("told to watch for copies here again"),
                Ok(()) => witness(
                    "told to stop watching for copies here; something better placed is \
                     doing it, and this window will go on offering what other machines \
                     copy",
                ),
                Err(e) => witness(&format!(
                    "could not change whether copies are noticed here (wanted {wanted}): {e}"
                )),
            }
            LRESULT(0)
        }
        WM_OFFER => {
            let _ = take_clipboard(window, "a fresh announcement from the far machine");
            LRESULT(0)
        }
        WM_RELEASE => {
            // The other of the two places the offer is let go.
            let was = on_offer_now();
            STATE.with(|cell| {
                if let Some(state) = cell.borrow_mut().as_mut() {
                    state.offer = None;
                    state.cache.clear();
                }
            });
            witness(&format!(
                "giving the clipboard back {}; {was} was on offer",
                why_released(wparam.0)
            ));
            LRESULT(0)
        }
        // SAFETY: handing anything else along is what the API requires.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// What is on offer right now, in a form fit to put in a log line.
///
/// The offer is the one piece of ordinary program state in this file,
/// and it is held in a thread-local reachable only from the window's
/// own thread while being *set* from another thread through a
/// channel. Two halves of one act, arriving separately. So every
/// place that sets it, clears it or finds it missing says what it saw
/// -- "announced" and "nothing is on offer" were two statements about
/// the same thing that never once appeared together with their
/// contents.
fn on_offer_now() -> String {
    STATE.with(|cell| match cell.borrow().as_ref() {
        None => "no state at all on this thread".to_string(),
        Some(state) => match state.offer.as_ref() {
            None => "nothing".to_string(),
            Some(offer) => format!(
                "{:?} ({} of {RENEWALS_BEFORE_GIVING_UP} renders could not be served{})",
                offer.formats,
                offer.failures,
                match offer.last_failure.as_deref() {
                    Some(why) => format!("; the last said: {why}"),
                    None => String::new(),
                }
            ),
        },
    })
}

/// Says how a render ended, when it ends, however it ends.
///
/// Held across the render and dropped afterwards. If the render
/// returns normally the outcome is recorded first and reported; if it
/// unwinds, or if anything is ever added between the call and the
/// report that can leave early, the line still goes out -- saying
/// that the render did not come back, which is precisely the fact
/// that took several rounds to establish by other means.
struct Returning {
    format_id: u32,
    began: Instant,
    outcome: Option<String>,
    served: bool,
}

impl Returning {
    fn from(format_id: u32) -> Returning {
        Returning {
            format_id,
            began: Instant::now(),
            outcome: None,
            served: false,
        }
    }

    fn was(&mut self, outcome: &Rendered) {
        self.outcome = Some(outcome.said());
        self.served = matches!(outcome, Rendered::Served(_));
    }
}

impl Drop for Returning {
    fn drop(&mut self) {
        let said = self.outcome.as_deref().unwrap_or(
            "DID NOT COME BACK -- the handler was left before it could say how it went, \
             so the promise has been answered with nothing and Windows will not ask again",
        );
        let line = format!(
            "WM_RENDERFORMAT for format {} returned after {} ms: {said}",
            self.format_id,
            self.began.elapsed().as_millis()
        );
        // A render that worked is a machine step and repeats as fast
        // as anything cares to paste. A render that did not is the
        // thing somebody is looking for, and there is one of them per
        // attempt.
        if self.served {
            step(&line)
        } else {
            witness(&line)
        }
    }
}

/// Take the clipboard, promising the offered formats without supplying them.
fn take_clipboard(window: HWND, asked_by: &str) -> Result<()> {
    let (formats, offered) = STATE.with(|cell| {
        let slot = cell.borrow();
        let state = slot.as_ref();
        (
            state.map(|s| s.formats),
            state
                .and_then(|s| s.offer.as_ref().map(|o| o.formats.clone()))
                .unwrap_or_default(),
        )
    });
    let (Some(formats), false) = (formats, offered.is_empty()) else {
        // Said, rather than returned from in silence. A promise that
        // is never made looks identical from outside to one that is
        // made and then lost, and telling those apart is the whole of
        // the current question.
        witness(&format!(
            "asked to take the clipboard ({asked_by}), but on offer is {}, so nothing \
             was promised",
            on_offer_now()
        ));
        return Ok(());
    };
    step(&format!(
        "taking the clipboard to promise {offered:?} ({asked_by})"
    ));

    let _open = Opened::take(window)?;
    // SAFETY: the clipboard is open and owned by this window.
    unsafe { EmptyClipboard() }.map_err(|_| last_error("clearing the clipboard"))?;

    // What the system says about each promise, gathered here and said
    // after the close.
    //
    // The result of `SetClipboardData` used to be discarded. A promise
    // nobody has ever inspected is not evidence of anything, and this
    // one sits at the very start of the chain: if it fails, every
    // symptom downstream -- the format missing, the format listed but
    // empty, a paste answered with nothing -- looks like a fault
    // somewhere else entirely.
    let mut promises = Vec::new();
    for format in &offered {
        for id in native_ids(formats, format) {
            // A null handle is the promise: the data is produced only if
            // something pastes it.
            // SAFETY: promising a format takes no memory.
            //
            // The error is cleared first, and that is not a
            // formality. `SetClipboardData` returns the handle it was
            // given; a promise gives it null, so on success it returns
            // null, which the Rust binding cannot tell from failure --
            // it reports an error carrying whatever `GetLastError`
            // last held. Reported naively that reads as a refusal on
            // every promise ever made, and it did: a stale
            // `ERROR_INVALID_HANDLE` from some earlier call was taken
            // for the promise being rejected. Clearing the error first
            // is what Win32 has always documented for a call that can
            // return null on success.
            // SAFETY: clearing the thread's last-error value.
            unsafe { SetLastError(WIN32_ERROR(0)) };
            let set = unsafe { SetClipboardData(id, HANDLE::default()) };
            let last = last_win32();
            // Asked of the system rather than assumed. `IsClipboardFormatAvailable`
            // is the only read that is safe from this side: it answers
            // from the list of formats without touching the data.
            //
            // `GetClipboardData` is deliberately *not* called here,
            // however much it would say. Asking for our own promised
            // format makes Windows send this thread `WM_RENDERFORMAT`,
            // synchronously, from inside the handler that is still
            // making the promise -- and a render that returns without
            // calling `SetClipboardData` has answered the question with
            // nothing, permanently. Reading the promise that way would
            // destroy it. What `GetClipboardData` returns is worth
            // knowing, but it has to be asked from a process that is
            // pasting, which is what `smkvm status` now does.
            // SAFETY: asking whether a format is listed takes no pointers.
            let listed = unsafe { IsClipboardFormatAvailable(id) }.is_ok();
            promises.push(format!(
                "{id}: {}",
                crate::promise_said(set.is_ok(), last, listed)
            ));
        }
    }
    // Who the system thinks owns it, rather than who we asked it to.
    // Read here because it takes no pointers and cannot block; *said*
    // further down, after the clipboard has been closed.
    // SAFETY: reading the owner takes no pointers.
    let owner = unsafe { GetClipboardOwner() }.map(|o| o.0).ok();
    // Closed here, deliberately and by name, before anything is said.
    //
    // This is the whole lesson of the worst fault in this file. Saying
    // something reaches `witness`, which in the worker relays down a
    // pipe -- a blocking write, with no deadline, on a pipe that also
    // carries every captured pointer movement. Called between the open
    // above and the close below, one such write that did not return
    // left this window holding the clipboard open *indefinitely*, and
    // only one process in a session may have it open at a time. So the
    // person's own copy and paste stopped working, not only ours, with
    // `ERROR_ACCESS_DENIED` for everybody and no way out but killing
    // the process.
    //
    // The rule that follows is short and has no exceptions: **between
    // an open and a close, do nothing that can wait on anything.** Not
    // a log line, not a lock, not a channel, not a pipe. Gather what is
    // needed, close, then speak.
    drop(_open);

    if let Some(holder) = held_open_by() {
        // Said loudly because of what it costs everybody else. If this
        // ever appears, the session's clipboard is unusable until this
        // process lets go.
        witness(&format!(
            "THE CLIPBOARD IS STILL OPEN, held by window {holder:#x}, after this \
             process should have closed it. Nothing in this session can copy or paste \
             while that is true"
        ));
    }
    // The thread that made the promise, against the thread that owns
    // the window it was made for.
    //
    // `SetClipboardData` requires the calling thread to be the one
    // holding the clipboard open, on a window belonging to that
    // thread, and nothing in the original single-process code ever had
    // to name a thread. Splitting the work across a service and a
    // worker made that assumption worth checking rather than assuming,
    // so both numbers are said and compared. If they ever differ, the
    // fix is to do the work on the thread that owns the window rather
    // than wherever the instruction happened to arrive.
    // SAFETY: both take no pointers beyond the place for the id.
    let (here, owns) = unsafe {
        let mut owning = 0u32;
        let owns = GetWindowThreadProcessId(window, Some(&mut owning));
        (GetCurrentThreadId(), owns)
    };
    step(&format!(
        "promising on thread {here}; the window belongs to thread {owns} ({})",
        if here == owns {
            "the same, which is what SetClipboardData requires"
        } else {
            "NOT the same -- SetClipboardData will refuse"
        }
    ));
    step(&format!(
        "what the system says it now holds: [{}]",
        promises.join("; ")
    ));
    step(&format!(
        "promised {:?}; the owner is now window={:?}, and this window is {:?} ({})",
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

/// Who, if anybody, is holding the clipboard open, and what that
/// process is called.
///
/// For `smkvm status`, so that somebody whose copy and paste has
/// stopped can find out in one command whether this program is the
/// reason. Only one process in a session may have the clipboard open,
/// and a window left in there takes it away from everybody.
pub fn who_is_holding_it_open() -> Option<(isize, u32)> {
    let window = held_open_by()?;
    let mut pid = 0u32;
    // SAFETY: a window handle the system just gave us, and a place for
    // the process id.
    unsafe {
        GetWindowThreadProcessId(HWND(window as *mut _), Some(&mut pid));
    }
    Some((window, pid))
}

/// The four values that settle an argument about the clipboard.
///
/// Who owns it, which formats it lists, whether anything is holding it
/// open, and -- the one that matters most and was hardest to get --
/// what a paste actually returns. Every round of this bug so far has
/// been six commands and a guess, because those four had to be
/// gathered from four different places, none of them this program.
/// They are one command now.
///
/// This *performs a paste*. Asking a delayed-rendering owner for data
/// makes it produce the data, so running this is not free of effect
/// and the caller should say so. That is the point: the question is
/// what a paste does, and nothing short of pasting answers it.
pub fn verdict() -> Vec<String> {
    let mut lines = Vec::new();

    // SAFETY: reading the owner takes no pointers.
    let owner = unsafe { GetClipboardOwner() }.map(|o| o.0).ok();
    match owner {
        None => lines.push("owner           nobody owns it".to_string()),
        Some(w) => {
            let mut pid = 0u32;
            // SAFETY: a window handle the system just gave us.
            unsafe {
                GetWindowThreadProcessId(HWND(w), Some(&mut pid));
            }
            lines.push(format!(
                "owner           window {:#x} of process {pid}",
                w as isize
            ));
        }
    }

    // Sampled repeatedly, because one sample cannot tell a passer-by
    // from a squatter and the difference is the whole meaning of the
    // line.
    //
    // Every application that copies or pastes holds the clipboard for
    // an instant, and on these machines Windows Defender's session
    // helper does it often. The first version of this line caught one
    // of those and announced that nothing in the session could copy or
    // paste -- true of that instant, false of the situation, and
    // alarming. A diagnostic exists to be trusted, so an alarming
    // sentence about an ordinary event is worse than no sentence. Only
    // a holder present in every sample gets the verdict; anything else
    // is named without one.
    let mut samples = Vec::new();
    for i in 0..HELD_OPEN_SAMPLES {
        samples.push(who_is_holding_it_open());
        if i + 1 < HELD_OPEN_SAMPLES {
            std::thread::sleep(HELD_OPEN_GAP);
        }
    }
    let held = samples.iter().filter_map(|s| *s).count();
    let ever = samples.iter().find_map(|s| *s);
    match (held, ever) {
        (0, _) | (_, None) => lines.push(format!(
            "held open       no -- free in all {HELD_OPEN_SAMPLES} samples over the last second"
        )),
        (n, Some((window, pid))) if n == HELD_OPEN_SAMPLES => lines.push(format!(
            "held open       YES, by window {window:#x} of process {pid}, in all {n} samples \
             over the last second.\n\x20               Nothing in this session can copy or \
             paste until that lets go"
        )),
        (n, Some((window, pid))) => lines.push(format!(
            "held open       briefly, by window {window:#x} of process {pid}, in {n} of \
             {HELD_OPEN_SAMPLES} samples.\n\x20               That is what an application \
             copying or pasting looks like, and is normal"
        )),
    }

    // Patient, because the value behind this open is the one the
    // reader most needs and giving up leaves the four values as three.
    // `Opened::take` waits `OPEN_PATIENCE`; this waits several of
    // those, which is still under three seconds and well within what
    // somebody typing a command will wait for an answer.
    let mut opened = Err(ClipboardError::Busy);
    for _ in 0..OPEN_ATTEMPTS_FOR_STATUS {
        opened = Opened::take(HWND::default());
        if opened.is_ok() {
            break;
        }
    }
    let Ok(_open) = opened else {
        lines.push(format!(
            "formats         could not be read: the clipboard would not open in \
             {OPEN_ATTEMPTS_FOR_STATUS} tries over \
             {} s, so something is holding it",
            (OPEN_PATIENCE * OPEN_ATTEMPTS_FOR_STATUS).as_secs_f32()
        ));
        return lines;
    };

    let mut listed = Vec::new();
    let mut next = 0u32;
    loop {
        // SAFETY: the clipboard is open, which is what this requires.
        next = unsafe { EnumClipboardFormats(next) };
        if next == 0 {
            break;
        }
        listed.push(next);
        if listed.len() > 64 {
            break;
        }
    }
    if listed.is_empty() {
        lines.push("formats         none are listed".to_string());
        return lines;
    }
    lines.push(format!("formats         {listed:?}"));

    for id in listed {
        // The whole question, asked the way a pasting application asks
        // it. A delayed-render promise answers by rendering; a promise
        // that has been overwritten, answered once with nothing, or
        // whose owner cannot be reached answers with zero, instantly,
        // and that difference is what the timing here is for.
        let began = Instant::now();
        // SAFETY: the clipboard is open.
        let got = unsafe { GetClipboardData(id) };
        let took = began.elapsed().as_millis();
        let said = match got {
            Ok(h) if !h.0.is_null() => {
                // SAFETY: clipboard data is a memory object and stays
                // valid while the clipboard is open.
                let size = unsafe { GlobalSize(HGLOBAL(h.0)) };
                format!("{size} bytes")
            }
            Ok(_) => "nothing (a null handle, and no error)".to_string(),
            Err(_) => format!("nothing ({})", describe_win32(last_win32())),
        };
        lines.push(format!("  format {id:<6} {said}, after {took} ms"));
    }
    lines
}

/// A handle to the thread that owns the clipboard window.
pub struct WindowsClipboard {
    formats: Formats,
    changes: Receiver<Available>,
    offers: Sender<(Vec<ClipFormat>, Box<dyn Fetch>)>,
    asks: Sender<Ask>,
    thread_id: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WindowsClipboard {
    /// Start watching the clipboard.
    pub fn start() -> Result<WindowsClipboard> {
        let (changes_tx, changes_rx) = mpsc::channel();
        let (offers_tx, offers_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (asks_tx, asks_rx) = mpsc::channel();

        // Its own thread, owning nothing else, because everything it
        // does can wait and the window thread may not.
        let apartment = changes_tx.clone();
        std::thread::Builder::new()
            .name("smkvm-clipboard-com".into())
            .spawn(move || ole_thread(asks_rx, apartment))
            .map_err(|e| ClipboardError::Display(format!("could not start the COM thread: {e}")))?;

        let asking = asks_tx.clone();
        let handle = std::thread::Builder::new()
            .name("smkvm-clipboard".into())
            .spawn(move || clipboard_thread(changes_tx, offers_rx, ready_tx, asking))
            .map_err(|e| ClipboardError::Display(format!("could not start a thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok((thread_id, formats))) => Ok(WindowsClipboard {
                formats,
                changes: changes_rx,
                offers: offers_tx,
                asks: asks_tx,
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
            asks: self.asks.clone(),
            thread_id: self.thread_id,
        }
    }
}

/// Offers to and reads from the clipboard, from any thread.
#[derive(Clone)]
pub struct WindowsHandle {
    formats: Formats,
    offers: Sender<(Vec<ClipFormat>, Box<dyn Fetch>)>,
    asks: Sender<Ask>,
    thread_id: u32,
}

impl WindowsHandle {
    /// Hand an offer to the window thread.
    ///
    /// In two parts, and that is worth saying out loud because it is
    /// the only place in this file where one act is split across two
    /// mechanisms. The formats and where to fetch them go down a
    /// channel; a thread message tells the window thread to look.
    /// Neither half is any use without the other, and they are sent
    /// from a different thread than the one that will read them, so
    /// both ends say what they saw.
    pub fn offer(&self, formats: &[ClipFormat], source: Box<dyn Fetch>) -> Result<()> {
        // SAFETY: takes no pointers.
        let from = unsafe { GetCurrentThreadId() };
        witness(&format!(
            "handing {formats:?} to the clipboard thread {} from thread {from}",
            self.thread_id
        ));
        self.offers
            .send((formats.to_vec(), source))
            .map_err(|_| ClipboardError::Display("the clipboard thread has stopped".into()))?;
        let posted = self.post(WM_OFFER, 0);
        if let Err(e) = &posted {
            // The channel now holds an offer nobody will collect, and
            // the next message to arrive would take *this* one. Said
            // loudly for that reason.
            witness(&format!(
                "the offer is on the channel but the clipboard thread could not be told \
                 to look ({e}); it will be picked up by whatever is announced next"
            ));
        }
        posted
    }

    pub fn release(&self) -> Result<()> {
        witness("asking the clipboard thread to give the clipboard back");
        self.post(WM_RELEASE, WITHDRAWN)
    }

    /// Start or stop being told about copies made here.
    ///
    /// Does not touch what is being offered: a window that has
    /// stopped watching still holds out another machine's clipboard
    /// and still serves renders for it.
    pub fn watch_for_copies(&self, wanted: bool) -> Result<()> {
        self.post(WM_WATCH, usize::from(wanted))
    }

    fn post(&self, message: u32, reason: usize) -> Result<()> {
        // SAFETY: posting to a thread id is safe whether or not it is still
        // running.
        unsafe { PostThreadMessageW(self.thread_id, message, WPARAM(reason), LPARAM(0)) }
            .map_err(|_| last_error("reaching the clipboard thread"))
    }
}

impl WindowsHandle {
    /// What is on the clipboard now, asked from a handle rather than
    /// from the watcher.
    ///
    /// The watcher blocks on the next change, so whoever has to
    /// answer "what is on it right now" cannot be holding it.
    pub fn available(&self) -> Result<Available> {
        let _open = Opened::take(HWND::default())?;
        Ok(available_now(self.formats))
    }
}

impl crate::Read for WindowsHandle {
    /// Read through the thread that has a COM apartment.
    ///
    /// The same reason as the enumeration: a file copy is an OLE data
    /// object, and if this process cannot see `CF_HDROP` without an
    /// apartment then it cannot fetch it either -- one cause, both
    /// halves. `GetClipboardData` returning nothing for an image is
    /// the same symptom from the other end.
    ///
    /// This waits, which is allowed here: it is called from the
    /// worker's main loop answering a request, not from the clipboard
    /// window's thread. It is bounded, so a reader is told something
    /// rather than left to an outer deadline, and it falls back to
    /// reading here if that thread has gone -- a clipboard that still
    /// half works being better than one that does not work at all.
    fn read(&mut self, format: &ClipFormat) -> Result<Vec<u8>> {
        // Here first, always. Under the scheduled task this path
        // works today on three machines, and a change that routes a
        // working read through a new thread to fix a different
        // arrangement is a change that can only lose. So the
        // apartment is a second chance after a failure, never a
        // detour before one.
        let here = read_format(self.formats, format);
        let Err(why) = here else {
            return here;
        };
        let (back, answer) = mpsc::channel();
        if self
            .asks
            .send(Ask::Read(self.formats, format.clone(), back))
            .is_err()
        {
            return Err(why);
        }
        witness(&format!(
            "could not read {format:?} on this thread ({why}); asking the thread with a \
             COM apartment"
        ));
        match answer.recv_timeout(OLE_READ_WITHIN) {
            Ok(Ok(bytes)) => {
                witness(&format!(
                    "the thread with a COM apartment read {format:?}, {} bytes, where this \
                     one could not",
                    bytes.len()
                ));
                Ok(bytes)
            }
            // Both reasons, because which of them failed is the
            // difference between an apartment being the answer and
            // the account being the answer.
            Ok(Err(there)) => Err(ClipboardError::Display(format!(
                "{why}; and from a thread with a COM apartment: {there}"
            ))),
            Err(_) => Err(ClipboardError::Display(format!(
                "{why}; and a thread with a COM apartment took longer than {} ms",
                OLE_READ_WITHIN.as_millis()
            ))),
        }
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
        let _ = self.handle().post(WM_QUIT, 0);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn clipboard_thread(
    changes: Sender<Available>,
    offers: Receiver<(Vec<ClipFormat>, Box<dyn Fetch>)>,
    ready: Sender<Result<(u32, Formats)>>,
    asks: Sender<Ask>,
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
            watching: true,
            looks: 0,
            first_look: None,
            asks,
            formats,
            changes,
            offer: None,
            cache: HashMap::new(),
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
        "window is up: window={:?} thread={thread_id} \
         listening-for-copies=accepted sequence-now={} \
         messages-from-less-privileged[{}]",
        window.0,
        sequence_number(),
        allowed.join(" ")
    ));
    let _ = ready.send(Ok((thread_id, formats)));

    let mut message = MSG::default();
    // SAFETY: `message` is a valid out-parameter for each call.
    while unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) }.as_bool() {
        // An offer posted from another thread arrives as a message with the
        // data waiting on the channel.
        if message.message == WM_OFFER {
            // The announcement and the thing announced travel
            // separately: the formats and where to fetch them come
            // down this channel, the nudge to look comes as a
            // message. Two halves of one act, and if they ever come
            // apart the clipboard promises something nothing can
            // supply. So both halves are said here, including the
            // case where the message arrives with no offer behind it.
            match offers.try_recv() {
                Ok((formats, source)) => {
                    let replacing = on_offer_now();
                    let announced = formats.clone();
                    STATE.with(|cell| {
                        if let Some(state) = cell.borrow_mut().as_mut() {
                            // A new value, so the renewal count is
                            // new too. That is the point of it living
                            // in here.
                            state.offer = Some(Offer {
                                formats,
                                source,
                                failures: 0,
                                last_failure: None,
                            });
                            state.cache.clear();
                        }
                    });
                    witness(&format!(
                        "announcing {announced:?} from the far machine, replacing {replacing}"
                    ));
                }
                Err(e) => witness(&format!(
                    "asked to announce an offer, but none came with the message ({e}); \
                     on offer is still {}",
                    on_offer_now()
                )),
            }
            // Thread messages have no window, so the procedure is called here.
            // SAFETY: the window is this thread's own.
            unsafe { window_proc(window, WM_OFFER, WPARAM(0), LPARAM(0)) };
            continue;
        }
        if message.message == WM_WATCH {
            // A thread message has no window to be dispatched to, so
            // it is handed to the procedure here, with its argument.
            // SAFETY: the window is this thread's own.
            unsafe { window_proc(window, WM_WATCH, WPARAM(message.wParam.0), LPARAM(0)) };
            continue;
        }
        if message.message == WM_RELEASE {
            // The reason travels in `wparam` and has to be carried
            // across by hand here, because a thread message has no
            // window to be dispatched to. Dropping it on the floor --
            // which this did -- makes every release look like the
            // ordinary one.
            // SAFETY: as above.
            unsafe { window_proc(window, WM_RELEASE, WPARAM(message.wParam.0), LPARAM(0)) };
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
