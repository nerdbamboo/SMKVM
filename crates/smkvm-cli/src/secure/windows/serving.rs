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

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use smkvm_clipboard::{CatchDrag, Drive, Fetch, Read, Watch, Write};
use smkvm_proto::{ClipFormat, MouseButton};

use crate::secure::windows::clip::{Waiting, PASTE_WITHIN};
use crate::secure::wire::{FromWorker, ToWorker};

/// The worker's end of the clipboard, while it holds one.
pub struct Serving {
    read: Box<dyn Read + Send>,
    write: Box<dyn Write + Send>,
    /// Pastes this process is in the middle of, waiting on the service.
    pastes: Arc<Waiting<Result<Vec<u8>, String>>>,
}

/// Somewhere to put a frame, whichever thread is holding one.
///
/// The worker writes to the service from its main loop, its capture
/// pump, its desktop watch and -- now -- from inside a paste on the
/// clipboard's own thread. One lock rather than a handle each, because
/// a frame torn in half by two writers is a frame the service cannot
/// read.
pub type Speak = Arc<dyn Fn(&FromWorker) -> bool + Send + Sync>;

/// What a paste on this desktop does: ask the service, and wait.
///
/// The service has to go to the machine that did the copying, which is
/// a network round trip, and on Windows this is called inside
/// `WM_RENDERFORMAT` with the pasting application stopped until it
/// returns. That wait is not new -- it was always a round trip -- but
/// it is now bounded, so an application stalls for at most
/// [`PASTE_WITHIN`] rather than for as long as the far machine stays
/// quiet.
struct AskTheService {
    speak: Speak,
    pastes: Arc<Waiting<Result<Vec<u8>, String>>>,
}

impl Fetch for AskTheService {
    fn fetch(&self, format: &ClipFormat) -> smkvm_clipboard::Result<Vec<u8>> {
        let (id, answer) = self.pastes.ask();
        if !(self.speak)(&FromWorker::WantsPaste {
            id,
            format: format.clone(),
        }) {
            self.pastes.forget(id);
            return Err(smkvm_clipboard::ClipboardError::Display(
                "the service is not there to fetch what was copied".into(),
            ));
        }
        match answer.recv_timeout(PASTE_WITHIN) {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) => Err(smkvm_clipboard::ClipboardError::Display(e)),
            Err(_) => {
                self.pastes.forget(id);
                Err(smkvm_clipboard::ClipboardError::Display(format!(
                    "the machine that copied this did not answer within {} s",
                    PASTE_WITHIN.as_secs()
                )))
            }
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

    fn from_parts(
        watch: Box<dyn Watch + Send>,
        read: Box<dyn Read + Send>,
        write: Box<dyn Write + Send>,
        speak: Speak,
    ) -> Result<Serving> {
        let pastes = Arc::new(Waiting::new());
        // Copies are noticed on their own thread, because `next_change`
        // blocks until one happens and the main loop has instructions
        // to be answering meanwhile.
        let mut watch = watch;
        let telling = speak.clone();
        std::thread::Builder::new()
            .name("smkvm-worker-clipboard".into())
            .spawn(move || {
                while let Some(available) = watch.next_change() {
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
        })
    }

    pub fn read(&mut self, format: &ClipFormat) -> Result<Vec<u8>, String> {
        self.read.read(format).map_err(|e| e.to_string())
    }

    pub fn offer(&mut self, formats: &[ClipFormat], speak: Speak) -> Result<(), String> {
        let source = AskTheService {
            speak,
            pastes: self.pastes.clone(),
        };
        self.write
            .offer(formats, Box::new(source))
            .map_err(|e| e.to_string())
    }

    pub fn release(&mut self) -> Result<(), String> {
        self.write.release().map_err(|e| e.to_string())
    }

    /// The service has fetched what a paste was waiting for.
    pub fn pasted(&self, id: u64, bytes: Result<Vec<u8>, String>) {
        self.pastes.answer(id, bytes);
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
                if held.is_none() {
                    match Serving::start(speak.clone()) {
                        Ok(serving) => {
                            tracing::info!("this worker holds the person's clipboard");
                            *held = Some(serving);
                        }
                        // Not fatal. Input is the half that matters at a
                        // consent prompt, and a worker that cannot reach
                        // the clipboard can still type.
                        Err(e) => tracing::warn!("this worker cannot reach the clipboard: {e:#}"),
                    }
                }
            }
            ToWorker::ServeClipboard(false) => {
                if let Some(serving) = held.take() {
                    tracing::info!("this worker is putting the person's clipboard down");
                    serving.nobody_is_answering();
                }
            }
            ToWorker::ReadClipboard { id, format } => {
                let bytes = match held.as_mut() {
                    Some(serving) => serving.read(&format),
                    None => Err("this worker does not hold the clipboard".into()),
                };
                speak(&FromWorker::ClipboardRead { id, bytes });
            }
            ToWorker::OfferClipboard { formats } => {
                if let Some(serving) = held.as_mut() {
                    if let Err(e) = serving.offer(&formats, speak.clone()) {
                        tracing::warn!("could not announce the far clipboard here: {e}");
                    }
                }
            }
            ToWorker::ReleaseClipboard => {
                if let Some(serving) = held.as_mut() {
                    let _ = serving.release();
                }
            }
            ToWorker::Pasted { id, bytes } => {
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
