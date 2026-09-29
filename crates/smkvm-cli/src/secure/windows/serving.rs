//! The worker's half of the clipboard: the one that is actually the
//! person's.
//!
//! Everything here runs in the interactive session, which is the whole
//! reason it exists. The service's copy of this ran in session 0 and
//! worked perfectly against a clipboard nobody can see.
//!
//! What is here is only the doing. What has been copied, what is
//! announced and what is being fetched all stay in the service with the
//! exchange and the link; this takes instructions and reports what it
//! sees, exactly as the injection side does.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use smkvm_clipboard::{CatchDrag, Drive, Fetch, Read, Watch, Write};
use smkvm_proto::{ClipFormat, MouseButton};

use crate::secure::outbox::{Outbox, Posted};
use crate::secure::windows::clip::{Waiting, PASTE_WITHIN};
use crate::secure::wire::{FromWorker, Level, ToWorker};

/// How many messages may be waiting to go to the service before the
/// rest are refused.
///
/// Generous, because a burst is exactly the moment the messages
/// matter, and bounded, because a queue nobody drains must not grow
/// without limit.
const OUTBOX_ROOM: usize = 256;

/// The one place a thread that must not wait puts something for the
/// service.
///
/// Process-wide, because there is one pipe for the life of the
/// worker, and because the threads that need it -- the clipboard
/// window's and whichever one a render arrives on -- are not the ones
/// that built it.
static OUTBOX: std::sync::OnceLock<Outbox<FromWorker>> = std::sync::OnceLock::new();

/// Whether the outbox above has already been put in place.
static RELAY_INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Say something to the service from a thread that is not allowed to
/// wait, admitting anything lost on the way.
///
/// The return value says whether it got as far as the queue, which is
/// not the same as reaching the service -- and for a diagnostic line
/// that distinction does not matter enough to wait for.
fn say_without_waiting(text: String) -> bool {
    let Some(outbox) = OUTBOX.get() else {
        // Nothing has been set up yet, which on this path means the
        // worker is not serving a clipboard. The local log still has
        // the line; `witness` wrote it there before calling this.
        return false;
    };
    let missed = outbox.missed();
    if missed > 0 {
        // Put back if it cannot be said, so the admission is not
        // itself lost.
        let note = FromWorker::Said {
            level: Level::Warn,
            text: format!(
                "clipboard: {missed} line(s) were dropped rather than made to wait \
                 for the pipe"
            ),
        };
        if !outbox.post(note).arrived() {
            outbox.put_back(missed);
        }
    }
    outbox
        .post(FromWorker::Said {
            level: Level::Info,
            text,
        })
        .arrived()
}

/// The worker's end of the clipboard, while it holds one.
pub struct Serving {
    read: Box<dyn Read + Send>,
    write: Box<dyn Write + Send>,
    /// Pastes this process is in the middle of, waiting on the service.
    pastes: Arc<Waiting<Result<Vec<u8>, String>>>,
    /// What each outstanding question was about, so that an answer
    /// arriving after its asker gave up can still be put somewhere
    /// useful.
    asked: Arc<Mutex<HashMap<u64, ClipFormat>>>,
    /// Contents that arrived too late for the render that wanted them.
    ///
    /// A render has a few seconds; a first fetch across a pipe and a
    /// network may not fit. Rather than make the render wait longer --
    /// which is what left data being handed to a closed clipboard --
    /// the late answer is kept here and the *next* paste is instant.
    /// So the worst case is one paste that does nothing and a second
    /// that works, rather than a minute of nothing and a warning
    /// nobody sees.
    ready: Arc<Mutex<HashMap<ClipFormat, Vec<u8>>>>,
}

/// Somewhere to put a frame, whichever thread is holding one.
///
/// The worker writes to the service from its main loop, its capture
/// pump, its desktop watch and -- now -- from inside a paste on the
/// clipboard's own thread. One lock rather than a handle each, because
/// a frame torn in half by two writers is a frame the service cannot
/// read.
pub type Speak = Arc<dyn Fn(&FromWorker) -> bool + Send + Sync>;

/// Say something, into this process's log *and* the service's.
///
/// Both, on purpose. The worker's own log has been silent across whole
/// deployments for reasons nobody has yet pinned down, while the pipe
/// has demonstrably worked the entire time -- it was carrying every
/// keystroke. Until that is understood, anything worth knowing goes
/// both ways, and which of the two arrives is itself a diagnosis.
pub fn tell(speak: &Speak, level: Level, text: impl Into<String>) {
    let text = text.into();
    match level {
        Level::Debug => tracing::debug!("{text}"),
        Level::Info => tracing::info!("{text}"),
        Level::Warn => tracing::warn!("{text}"),
        Level::Error => tracing::error!("{text}"),
    }
    speak(&FromWorker::Said { level, text });
}

/// What a paste on this desktop does: ask the service, and wait.
///
/// The service has to go to the machine that did the copying, which is
/// a network round trip, and on Windows this is called inside
/// `WM_RENDERFORMAT` with the pasting application stopped until it
/// returns. That wait is not new -- it was always a round trip -- but
/// it is now bounded, so an application stalls for at most
/// [`PASTE_WITHIN`] rather than for as long as the far machine stays
/// quiet.
/// Deliberately holds no `Speak`.
///
/// It used to, and that was the fault: everything reachable from a
/// render has to be incapable of waiting on another thread, and the
/// surest way to say so is to not hand it the thing that can. What
/// goes out from here goes through the outbox. The compiler enforces
/// it now -- put the field back and it is dead code.
struct AskTheService {
    pastes: Arc<Waiting<Result<Vec<u8>, String>>>,
    asked: Arc<Mutex<HashMap<u64, ClipFormat>>>,
    ready: Arc<Mutex<HashMap<ClipFormat, Vec<u8>>>>,
}

impl Fetch for AskTheService {
    fn fetch(&self, format: &ClipFormat) -> smkvm_clipboard::Result<Vec<u8>> {
        // Something an earlier render asked for and did not get in
        // time. No round trip at all.
        if let Some(bytes) = self.ready.lock().expect("not poisoned").remove(format) {
            tracing::debug!(
                ?format,
                "pasting what arrived too late for the last attempt"
            );
            return Ok(bytes);
        }
        let (id, answer) = self.pastes.ask();
        self.asked
            .lock()
            .expect("not poisoned")
            .insert(id, format.clone());
        tracing::debug!("something here is pasting {format:?}; asking the service for it");
        // Through the outbox, not `speak`, and this is the line that
        // matters most in the file.
        //
        // This runs on the clipboard window's thread, inside
        // `WM_RENDERFORMAT`, with the pasting application stopped and
        // a promise outstanding. `speak` takes the lock on the writer
        // every thread shares and then writes to the pipe with no
        // deadline, so calling it here hands the window thread to
        // whoever happens to hold that lock. And a render that does
        // not return has not merely been slow: a handler that returns
        // without setting data has answered the question with
        // nothing, permanently, and Windows never asks again. One
        // contended write and the person's clipboard is empty until
        // something restarts.
        //
        // So the request is posted, never waited on. If the queue is
        // full this gives up at once and says so, which reaches the
        // render as a failure to fetch -- and a failure to fetch is
        // the outcome that renews the promise rather than the one
        // that answers nothing.
        say_without_waiting(format!("something on this desktop is pasting {format:?}"));
        let posted = OUTBOX.get().map(|o| {
            o.post(FromWorker::WantsPaste {
                id,
                format: format.clone(),
            })
        });
        if !posted.map(Posted::arrived).unwrap_or(false) {
            self.pastes.forget(id);
            self.asked.lock().expect("not poisoned").remove(&id);
            return Err(smkvm_clipboard::ClipboardError::Display(format!(
                "could not even ask the service for what was copied ({}), so this \
                 paste produced nothing. The promise is renewed; pasting again \
                 should work",
                match posted {
                    Some(Posted::NoRoom(n)) =>
                        format!("{n} message(s) refused in a row; the pipe is not moving"),
                    Some(Posted::Gone) => "the service is not there".to_string(),
                    _ => "the clipboard is not being served".to_string(),
                }
            )));
        }
        let asked_at = std::time::Instant::now();
        let outcome = answer.recv_timeout(PASTE_WITHIN);
        tracing::info!(
            ?format,
            waited_ms = asked_at.elapsed().as_millis() as u64,
            budget_ms = PASTE_WITHIN.as_millis() as u64,
            "a paste waited on the service"
        );
        match outcome {
            Ok(Ok(bytes)) => {
                self.asked.lock().expect("not poisoned").remove(&id);
                Ok(bytes)
            }
            Ok(Err(e)) => {
                self.asked.lock().expect("not poisoned").remove(&id);
                Err(smkvm_clipboard::ClipboardError::Display(e))
            }
            // Given up on, deliberately and early. The question is left
            // outstanding: whatever comes back goes into `ready`, and
            // the next paste needs no round trip.
            Err(_) => Err(smkvm_clipboard::ClipboardError::Display(format!(
                "the contents did not arrive within {} ms, so this paste produced \
                 nothing. They are still being fetched; pasting again should work",
                PASTE_WITHIN.as_millis()
            ))),
        }
    }
}

impl Serving {
    /// Take up the person's clipboard and start noticing copies.
    pub fn start(speak: Speak) -> Result<Serving> {
        use smkvm_clipboard::platform::windows::WindowsClipboard;
        let clipboard = WindowsClipboard::start().context("watching the clipboard")?;
        let handle = clipboard.handle();
        Self::from_parts(
            Box::new(clipboard),
            Box::new(handle.clone()),
            Box::new(handle),
            speak,
        )
    }

    /// Point the clipboard's witness at the pipe, through a queue it
    /// can never wait on.
    ///
    /// This used to call `speak` directly, and `speak` takes the lock
    /// on the one writer every thread shares and then writes to the
    /// pipe with no deadline. So a diagnostic line could stop the
    /// thread that was emitting it -- and not just that line: every
    /// statement after it in the same function, for as long as the
    /// pipe stayed full or another thread held the lock.
    ///
    /// That is how the one instrument built for this round came to be
    /// the one instrument not reporting. `witness` writes to the log
    /// first and relays second, so a wedged relay leaves exactly the
    /// fingerprint that was seen: the line appears once locally, never
    /// with the `worker:` prefix, and nothing that should have
    /// followed it appears at all.
    ///
    /// A diagnostic that can stop the code it is diagnosing is not a
    /// diagnostic. So the relay hands the line to a bounded queue and
    /// returns immediately; one thread drains it in order. When the
    /// queue is full the line is dropped and counted, and the count is
    /// said with the next line that gets through. Losing a line is a
    /// nuisance; losing the rest of the function is a fault that reads
    /// like a different bug entirely.
    fn speak_for_the_clipboard(speak: &Speak) -> Result<()> {
        // Once per process, and said so here rather than left to
        // `witness_through` quietly ignoring the second call.
        //
        // The clipboard is taken and given back repeatedly in one
        // worker's life -- every `ServeClipboard` does it -- so this
        // runs more than once. `witness_through` keeps the first
        // relay it is given and discards the rest silently, which was
        // survivable only by accident: the first relay happens to
        // stay valid because there is one pipe for the life of the
        // process. Saying no explicitly is the same behaviour without
        // the accident, and without a thread left behind each time.
        if RELAY_INSTALLED.set(()).is_err() {
            return Ok(());
        }
        let speak = speak.clone();
        let (outbox, receive) = Outbox::with_room_for(OUTBOX_ROOM);
        std::thread::Builder::new()
            .name("smkvm-worker-outbox".into())
            .spawn(move || {
                // The one thread allowed to wait on the writer. Every
                // frame still goes out under the shared lock and in
                // order, so nothing is torn in half; what changed is
                // only *who* waits for it.
                while let Ok(message) = receive.recv() {
                    if !speak(&message) {
                        return;
                    }
                }
            })
            .context("starting the worker's outbox")?;
        let _ = OUTBOX.set(outbox);
        smkvm_clipboard::witness_through(Box::new(move |text| {
            // Relayed as given. The `clipboard:` prefix is already on
            // it, put there at the source so that the same search
            // finds these lines on both arrangements; adding another
            // here would make it `worker: clipboard: clipboard: ...`.
            say_without_waiting(text.to_string());
        }));
        Ok(())
    }

    fn from_parts(
        watch: Box<dyn Watch + Send>,
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        speak: Speak,
    ) -> Result<Serving> {
        Self::speak_for_the_clipboard(&speak)?;
        let pastes = Arc::new(Waiting::new());
        let asked: Arc<Mutex<HashMap<u64, ClipFormat>>> = Arc::new(Mutex::new(HashMap::new()));
        let ready: Arc<Mutex<HashMap<ClipFormat, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        // Copies are noticed on their own thread, because `next_change`
        // blocks until one happens and the main loop has instructions
        // to be answering meanwhile.
        let mut watch = watch;
        let telling = speak.clone();
        std::thread::Builder::new()
            .name("smkvm-worker-clipboard".into())
            .spawn(move || {
                while let Some(available) = watch.next_change() {
                    tell(
                        &telling,
                        Level::Debug,
                        format!("somebody copied something here: {:?}", available.formats),
                    );
                    if !telling(&FromWorker::ClipboardChanged(available.formats)) {
                        return;
                    }
                }
            })
            .context("starting the worker's clipboard watch")?;
        Ok(Serving {
            read,
            write,
            pastes,
            asked,
            ready,
        })
    }

    pub fn read(&mut self, format: &ClipFormat) -> Result<Vec<u8>, String> {
        self.read.read(format).map_err(|e| e.to_string())
    }

    pub fn offer(&mut self, formats: &[ClipFormat]) -> Result<(), String> {
        // A new offer is a new set of contents; whatever was kept from
        // the last one is not what this announces.
        self.ready.lock().expect("not poisoned").clear();
        let source = AskTheService {
            pastes: self.pastes.clone(),
            asked: self.asked.clone(),
            ready: self.ready.clone(),
        };
        self.write
            .offer(formats, Box::new(source))
            .map_err(|e| e.to_string())
    }

    pub fn release(&mut self) -> Result<(), String> {
        self.write.release().map_err(|e| e.to_string())
    }

    /// The service has fetched what a paste was waiting for.
    ///
    /// If nothing is waiting any more -- the render gave up, which is
    /// the ordinary case for a first paste of anything large -- the
    /// contents are kept for the next one rather than thrown away. That
    /// is the difference between "paste twice" and "paste, wait a
    /// minute, get nothing".
    pub fn pasted(&self, id: u64, bytes: Result<Vec<u8>, String>) {
        let format = self.asked.lock().expect("not poisoned").remove(&id);
        let ok = bytes.clone().ok();
        if self.pastes.answer(id, bytes) {
            return;
        }
        if let (Some(format), Some(bytes)) = (format, ok) {
            tracing::debug!(
                ?format,
                bytes = bytes.len(),
                "these arrived after the paste that wanted them gave up; keeping them so \
                 the next paste is immediate"
            );
            self.ready
                .lock()
                .expect("not poisoned")
                .insert(format, bytes);
        }
    }

    /// Nothing more will be answered; wake anything mid-paste.
    pub fn nobody_is_answering(&self) {
        self.pastes.nobody_is_answering();
    }
}

/// Catch a drag on this desktop, driving this desktop's pointer.
///
/// All of it local, which is the point. The catcher needs a window
/// stood under the pointer, the pointer nudged so the dragging
/// application notices, and the button released -- and doing that from
/// the service would put a pipe round trip in the middle of each step
/// while an application waits on the other end of them.
pub fn catch_a_drag(
    input: &mut smkvm_input::platform::windows::WindowsInput,
) -> Vec<std::path::PathBuf> {
    use smkvm_input::Inject as _;
    let catcher = smkvm_clipboard::platform::windows_drag::DropCatcher::start();
    let mut catcher = match catcher {
        Ok(catcher) => catcher,
        Err(e) => {
            tracing::warn!("files cannot be dragged off this machine: {e}");
            return Vec::new();
        }
    };
    catcher
        .catch(&mut |drive| match drive {
            Drive::MoveTo(x, y) => {
                let _ = input.move_to(x, y);
                let _ = input.flush();
            }
            Drive::ReleaseLeft => {
                let _ = input.button(MouseButton::Left, false);
                let _ = input.flush();
            }
        })
        .unwrap_or_default()
}

/// Everything the worker does about the clipboard, in one place so the
/// main loop stays a list of instructions.
pub struct Clipboard {
    serving: Mutex<Option<Serving>>,
}

impl Clipboard {
    pub fn new() -> Clipboard {
        Clipboard {
            serving: Mutex::new(None),
        }
    }

    pub fn told(
        &self,
        told: ToWorker,
        speak: &Speak,
        input: &mut smkvm_input::platform::windows::WindowsInput,
    ) -> bool {
        let mut held = self.serving.lock().expect("not poisoned");
        match told {
            ToWorker::ServeClipboard(true) => {
                if held.is_some() {
                    tell(speak, Level::Debug, "already holding the clipboard");
                } else {
                    match Serving::start(speak.clone()) {
                        Ok(serving) => {
                            tell(speak, Level::Info, "holding the person's clipboard");
                            *held = Some(serving);
                        }
                        // Not fatal. Input is the half that matters at a
                        // consent prompt, and a worker that cannot reach
                        // the clipboard can still type.
                        Err(e) => tell(
                            speak,
                            Level::Warn,
                            format!("cannot reach the person's clipboard: {e:#}"),
                        ),
                    }
                }
            }
            ToWorker::ServeClipboard(false) => {
                tell(speak, Level::Info, "putting the person's clipboard down");
                if let Some(serving) = held.take() {
                    serving.nobody_is_answering();
                }
            }
            ToWorker::ReadClipboard { id, format } => {
                let bytes = match held.as_mut() {
                    Some(serving) => serving.read(&format),
                    None => Err("this worker does not hold the clipboard".into()),
                };
                tell(
                    speak,
                    Level::Debug,
                    match &bytes {
                        Ok(b) => format!("read {:?} off the clipboard: {} bytes", format, b.len()),
                        Err(e) => format!("could not read {format:?} off the clipboard: {e}"),
                    },
                );
                speak(&FromWorker::ClipboardRead { id, bytes });
            }
            ToWorker::OfferClipboard { formats } => match held.as_mut() {
                Some(serving) => match serving.offer(&formats) {
                    Ok(()) => tell(
                        speak,
                        Level::Info,
                        format!("announced the far machine's clipboard here: {formats:?}"),
                    ),
                    Err(e) => tell(
                        speak,
                        Level::Warn,
                        format!("could not announce the far machine's clipboard here: {e}"),
                    ),
                },
                // The case that would otherwise be entirely silent: an
                // offer arriving at a worker that never took the
                // clipboard up. The service's log would show the offer
                // being made and nothing would ever be on the person's
                // clipboard.
                None => tell(
                    speak,
                    Level::Warn,
                    "asked to announce the far machine's clipboard, but this worker does                      not hold one -- nothing will appear on the person's clipboard",
                ),
            },
            ToWorker::ReleaseClipboard => {
                tell(speak, Level::Debug, "giving the clipboard back");
                if let Some(serving) = held.as_mut() {
                    let _ = serving.release();
                }
            }
            ToWorker::Pasted { id, bytes } => {
                tell(
                    speak,
                    Level::Debug,
                    match &bytes {
                        Ok(b) => format!("the far machine sent {} bytes to paste", b.len()),
                        Err(e) => format!("the far machine could not supply the paste: {e}"),
                    },
                );
                if let Some(serving) = held.as_ref() {
                    serving.pasted(id, bytes);
                }
            }
            ToWorker::CatchDrag { id } => {
                // Released before the catch, because the catcher has to
                // hold the clipboard lock itself on some paths and this
                // one is not needed for it.
                drop(held);
                let paths = catch_a_drag(input);
                speak(&FromWorker::DragCaught { id, paths });
            }
            ToWorker::DropFiles(paths) => {
                drop(held);
                if let Err(e) = smkvm_clipboard::platform::windows_drag::drop_files(paths) {
                    tracing::warn!("could not start the drop: {e}");
                }
            }
            _ => return false,
        }
        true
    }
}
