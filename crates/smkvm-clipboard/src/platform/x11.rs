//! The X11 clipboard.
//!
//! X11 has no clipboard as such. It has selections, and the application that
//! last copied something keeps holding it, answering requests from whoever
//! wants to paste. Reading therefore means asking that application and waiting
//! for it to answer, and offering means becoming the one who answers.
//!
//! Large contents arrive in pieces, through what the protocol calls INCR: the
//! owner replies with a size instead of the data, then sends it a property at
//! a time. Skipping that is why some tools cannot paste anything sizeable out
//! of a browser, since browsers use it readily.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smkvm_proto::ClipFormat;
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    PropertyNotifyEvent, SelectionNotifyEvent, SelectionRequestEvent, Timestamp, Window,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use crate::{
    files, Available, CatchDrag, ClipboardError, Drive, Fetch, Read, Result, Watch, Write,
};

/// How long to wait for the application holding the selection to answer.
///
/// It is another program, and it may be busy or wedged. Waiting forever would
/// make one unresponsive application freeze the whole link.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

fn display_err(e: impl std::fmt::Display) -> ClipboardError {
    ClipboardError::Display(e.to_string())
}

/// The atoms this needs, resolved once.
struct Atoms {
    clipboard: Atom,
    targets: Atom,
    incr: Atom,
    utf8_string: Atom,
    text_plain_utf8: Atom,
    text_html: Atom,
    image_png: Atom,
    uri_list: Atom,
    /// Where replies to our own requests are placed.
    transfer: Atom,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Atoms> {
        let get = |name: &str| -> Result<Atom> {
            Ok(conn
                .intern_atom(false, name.as_bytes())
                .map_err(display_err)?
                .reply()
                .map_err(display_err)?
                .atom)
        };
        Ok(Atoms {
            clipboard: get("CLIPBOARD")?,
            targets: get("TARGETS")?,
            incr: get("INCR")?,
            utf8_string: get("UTF8_STRING")?,
            text_plain_utf8: get("text/plain;charset=utf-8")?,
            text_html: get("text/html")?,
            image_png: get("image/png")?,
            uri_list: get("text/uri-list")?,
            transfer: get("SMKVM_TRANSFER")?,
        })
    }

    /// Which of our formats an X11 target stands for.
    fn format_of(&self, target: Atom) -> Option<ClipFormat> {
        if target == self.utf8_string || target == self.text_plain_utf8 {
            Some(ClipFormat::Text)
        } else if target == self.text_html {
            Some(ClipFormat::Html)
        } else if target == self.image_png {
            Some(ClipFormat::Png)
        } else if target == self.uri_list {
            Some(ClipFormat::Uris)
        } else {
            None
        }
    }

    /// The targets to ask for, in order of preference.
    ///
    /// Text is offered as both names for it: applications disagree about which
    /// to advertise, and asking for only one loses to whichever disagrees.
    fn targets_for(&self, format: &ClipFormat) -> Vec<Atom> {
        match format {
            ClipFormat::Text => vec![self.utf8_string, self.text_plain_utf8],
            ClipFormat::Html => vec![self.text_html],
            ClipFormat::Png => vec![self.image_png],
            ClipFormat::Uris => vec![self.uri_list],
            ClipFormat::Other(_) => Vec::new(),
        }
    }
}

/// A connection to X11 dedicated to the clipboard.
pub struct X11Clipboard {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
    /// Events seen while waiting for something else, kept so nothing is lost.
    deferred: Vec<Event>,
    /// A window whose taking of the clipboard is not news: this program's own
    /// offer, made through another connection. Zero for none.
    ignore_owner: Option<Arc<AtomicU32>>,
}

impl X11Clipboard {
    pub fn open() -> Result<X11Clipboard> {
        Self::open_display(None)
    }

    pub fn open_display(display: Option<&str>) -> Result<X11Clipboard> {
        let (conn, screen_num) = x11rb::connect(display).map_err(display_err)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        // An unmapped window of its own: selections are addressed to a window,
        // and using someone else's would put this in their event stream.
        let window = conn.generate_id().map_err(display_err)?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(display_err)?;

        let atoms = Atoms::intern(&conn)?;
        conn.flush().map_err(display_err)?;

        Ok(X11Clipboard {
            conn,
            window,
            atoms,
            deferred: Vec::new(),
            ignore_owner: None,
        })
    }

    /// Do not report a change when the clipboard is taken by this window.
    ///
    /// An offer made on another machine's behalf is a change of owner like
    /// any other, and reporting it would send that machine's clipboard
    /// straight back to it. The writer publishes the window it owns through,
    /// and the watcher is told to look away from it.
    pub fn ignore_changes_by(&mut self, owner: Arc<AtomicU32>) {
        self.ignore_owner = Some(owner);
    }

    /// Ask to be told whenever the clipboard changes hands.
    pub fn watch(&mut self) -> Result<()> {
        self.conn
            .xfixes_query_version(5, 0)
            .map_err(display_err)?
            .reply()
            .map_err(display_err)?;
        self.conn
            .xfixes_select_selection_input(
                self.window,
                self.atoms.clipboard,
                xfixes::SelectionEventMask::SET_SELECTION_OWNER
                    | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE
                    | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY,
            )
            .map_err(display_err)?;
        self.conn.flush().map_err(display_err)?;
        Ok(())
    }

    /// Take the next event, preferring any held back from earlier.
    fn next_event(&mut self) -> Result<Event> {
        if !self.deferred.is_empty() {
            return Ok(self.deferred.remove(0));
        }
        self.conn.wait_for_event().map_err(display_err)
    }

    /// Take an event if one is waiting, and return straight away if none is.
    fn poll_event(&mut self) -> Result<Option<Event>> {
        if !self.deferred.is_empty() {
            return Ok(Some(self.deferred.remove(0)));
        }
        self.conn.poll_for_event().map_err(display_err)
    }

    /// Wait for an event matching `want`, keeping anything else for later.
    ///
    /// Polls rather than blocking on the display, because the thing being
    /// waited for is another application's reply and it may never come. A
    /// blocking wait honours a deadline only between events, so one wedged
    /// application would hold this up for ever -- which is the outcome the
    /// deadline exists to prevent.
    fn wait_for<T>(
        &mut self,
        deadline: Instant,
        mut want: impl FnMut(&Event) -> Option<T>,
    ) -> Result<T> {
        loop {
            if let Some(index) = self.deferred.iter().position(|e| want(e).is_some()) {
                let event = self.deferred.remove(index);
                return Ok(want(&event).expect("just matched"));
            }
            match self.conn.poll_for_event().map_err(display_err)? {
                Some(event) => match want(&event) {
                    Some(value) => return Ok(value),
                    // A change of owner seen mid-conversion is news the watcher
                    // still has to hear, and a request or a clearing is the
                    // owner's to deal with once it is done asking. Property
                    // notices are the trail of our own asking -- written and
                    // deleted on our window -- and matter to nobody; kept,
                    // they were once fed back into the watcher's loop for ever.
                    None if matches!(
                        event,
                        Event::XfixesSelectionNotify(_)
                            | Event::SelectionRequest(_)
                            | Event::SelectionClear(_)
                    ) =>
                    {
                        self.deferred.push(event)
                    }
                    None => {}
                },
                None => {
                    if Instant::now() >= deadline {
                        return Err(ClipboardError::Display(
                            "the application holding the clipboard did not answer".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    /// Ask the selection's owner to convert it to `target` and hand it over.
    fn convert(&mut self, target: Atom) -> Result<Vec<u8>> {
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        // Clear the landing property so a stale value cannot be mistaken for
        // an answer.
        self.conn
            .delete_property(self.window, self.atoms.transfer)
            .map_err(display_err)?;
        self.conn
            .convert_selection(
                self.window,
                self.atoms.clipboard,
                target,
                self.atoms.transfer,
                CURRENT_TIME,
            )
            .map_err(display_err)?;
        self.conn.flush().map_err(display_err)?;

        let window = self.window;
        let transfer = self.atoms.transfer;
        let notify: SelectionNotifyEvent = self.wait_for(deadline, |event| match event {
            Event::SelectionNotify(e) if e.requestor == window && e.property != NONE => Some(*e),
            // A property of None means the owner declined to convert.
            Event::SelectionNotify(e) if e.requestor == window => Some(*e),
            _ => None,
        })?;
        if notify.property == NONE {
            return Err(ClipboardError::Refused(ClipFormat::Other(format!(
                "target {target}"
            ))));
        }

        let reply = self
            .conn
            .get_property(false, window, transfer, AtomEnum::ANY, 0, u32::MAX / 4)
            .map_err(display_err)?
            .reply()
            .map_err(display_err)?;

        if reply.type_ != self.atoms.incr {
            self.conn
                .delete_property(window, transfer)
                .map_err(display_err)?;
            self.conn.flush().map_err(display_err)?;
            return Ok(reply.value);
        }

        // An INCR reply carries only the expected size. Deleting the property
        // tells the owner to send the first piece; each further deletion asks
        // for the next, until one arrives empty.
        let mut collected = Vec::new();
        loop {
            self.conn
                .delete_property(window, transfer)
                .map_err(display_err)?;
            self.conn.flush().map_err(display_err)?;

            let _: PropertyNotifyEvent = self.wait_for(deadline, |event| match event {
                Event::PropertyNotify(e)
                    if e.window == window
                        && e.atom == transfer
                        && e.state == Property::NEW_VALUE =>
                {
                    Some(*e)
                }
                _ => None,
            })?;

            let piece = self
                .conn
                .get_property(false, window, transfer, AtomEnum::ANY, 0, u32::MAX / 4)
                .map_err(display_err)?
                .reply()
                .map_err(display_err)?;
            if piece.value.is_empty() {
                self.conn
                    .delete_property(window, transfer)
                    .map_err(display_err)?;
                self.conn.flush().map_err(display_err)?;
                return Ok(collected);
            }
            collected.extend_from_slice(&piece.value);
        }
    }

    /// The server's idea of now.
    ///
    /// X11 has no request for the time; the way to learn it is to change a
    /// property on a window of one's own and read the time off the notice.
    fn server_time(&mut self) -> Result<Timestamp> {
        let window = self.window;
        let atom = self.atoms.transfer;
        self.conn
            .change_property8(PropMode::APPEND, window, atom, AtomEnum::STRING, &[])
            .map_err(display_err)?;
        self.conn.flush().map_err(display_err)?;
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        self.wait_for(deadline, |event| match event {
            Event::PropertyNotify(e) if e.window == window && e.atom == atom => Some(e.time),
            _ => None,
        })
    }

    /// What the current owner says it can provide.
    pub fn available(&mut self) -> Result<Available> {
        let targets = match self.convert(self.atoms.targets) {
            Ok(bytes) => bytes,
            // Nothing owns the clipboard, or it will not say.
            Err(_) => return Ok(Available::default()),
        };
        let mut formats = Vec::new();
        for chunk in targets.chunks_exact(4) {
            let atom = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if let Some(format) = self.atoms.format_of(atom) {
                if !formats.contains(&format) {
                    formats.push(format);
                }
            }
        }
        Ok(Available { formats })
    }
}

impl Read for X11Clipboard {
    fn read(&mut self, format: &ClipFormat) -> Result<Vec<u8>> {
        let targets = self.atoms.targets_for(format);
        if targets.is_empty() {
            return Err(ClipboardError::Refused(format.clone()));
        }
        let mut last = None;
        for target in targets {
            match self.convert(target) {
                Ok(bytes) => return Ok(bytes),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or(ClipboardError::Refused(format.clone())))
    }
}

impl Watch for X11Clipboard {
    fn next_change(&mut self) -> Option<Available> {
        loop {
            let event = self.next_event().ok()?;
            if let Event::XfixesSelectionNotify(notice) = &event {
                let ours = self
                    .ignore_owner
                    .as_ref()
                    .is_some_and(|w| w.load(Ordering::Relaxed) == notice.owner);
                if ours {
                    continue;
                }
                return self.available().ok();
            }
            // Anything else here is left over from looking at the last owner,
            // and is dropped. It must never go back into `deferred`: this
            // loop takes from there first, and would spin on it for ever,
            // never reaching the display again -- which is how the watcher
            // once went deaf after its first look.
        }
    }
}

/// A transfer being sent a piece at a time.
struct Incremental {
    requestor: Window,
    property: Atom,
    data: Vec<u8>,
    sent: usize,
}

/// Holds the clipboard on this machine on another machine's behalf.
///
/// Owning a selection in X11 means answering requests for it, so this has to
/// keep running for as long as the offer stands. Contents are fetched only
/// when something actually asks, which is what lets an offer be made without
/// knowing how large it is or whether it will ever be wanted.
pub struct X11Owner {
    clipboard: X11Clipboard,
    offered: Vec<ClipFormat>,
    source: Box<dyn crate::Fetch>,
    /// Anything already fetched, so a second paste does not fetch again.
    cached: HashMap<ClipFormat, Vec<u8>>,
    in_flight: Vec<Incremental>,
    /// Largest property this server will accept in one go.
    chunk: usize,
    owns: bool,
    /// The server's time when the selection was taken. Giving a selection up
    /// sends its owner a SelectionClear -- including when the owner is this
    /// very window letting go before taking it again -- and one from before
    /// this moment is about an ownership that is already over.
    since: Timestamp,
}

impl X11Owner {
    /// Take the clipboard, offering these formats.
    pub fn take(
        clipboard: X11Clipboard,
        formats: &[ClipFormat],
        source: Box<dyn crate::Fetch>,
    ) -> Result<X11Owner> {
        // A request must fit in one of the server's messages, with room for
        // its header; going over is a protocol error rather than a short read.
        let chunk = (clipboard.conn.maximum_request_bytes() / 2).max(4096);

        let mut clipboard = clipboard;
        let since = clipboard.server_time()?;
        clipboard
            .conn
            .set_selection_owner(clipboard.window, clipboard.atoms.clipboard, since)
            .map_err(display_err)?;
        clipboard.conn.flush().map_err(display_err)?;

        let held = clipboard
            .conn
            .get_selection_owner(clipboard.atoms.clipboard)
            .map_err(display_err)?
            .reply()
            .map_err(display_err)?;
        if held.owner != clipboard.window {
            return Err(ClipboardError::Busy);
        }

        Ok(X11Owner {
            clipboard,
            offered: formats.to_vec(),
            source,
            cached: HashMap::new(),
            in_flight: Vec::new(),
            chunk,
            owns: true,
            since,
        })
    }

    /// Offer something else through the selection this already holds.
    ///
    /// Letting go and taking again would work, but the letting go sends
    /// this window a SelectionClear that arrives after the taking, and
    /// looks exactly like somebody else copying. Ownership simply continues,
    /// and requests from here on are answered from the new source.
    pub fn reoffer(&mut self, formats: &[ClipFormat], source: Box<dyn crate::Fetch>) {
        self.offered = formats.to_vec();
        self.source = source;
        self.cached.clear();
    }

    pub fn owns_clipboard(&self) -> bool {
        self.owns
    }

    /// Whether the server still has this window down as the owner.
    fn still_owner(&self) -> Result<bool> {
        let held = self
            .clipboard
            .conn
            .get_selection_owner(self.clipboard.atoms.clipboard)
            .map_err(display_err)?
            .reply()
            .map_err(display_err)?;
        Ok(held.owner == self.clipboard.window)
    }

    /// The window the clipboard is held through.
    pub fn window(&self) -> Window {
        self.clipboard.window
    }

    /// Deal with one event, waiting until one arrives.
    ///
    /// Returns `false` once the clipboard has been taken by something else,
    /// at which point there is nothing left to answer.
    pub fn serve_one(&mut self) -> Result<bool> {
        let event = self.clipboard.next_event()?;
        self.dispatch(event)
    }

    /// Deal with whatever is waiting, and return straight away if nothing is.
    ///
    /// An owner has other things to attend to: a link to watch, a decision to
    /// stop. Once the last request has been answered no further event arrives,
    /// so waiting on the display would mean never getting back to them.
    pub fn serve_pending(&mut self) -> Result<bool> {
        while let Some(event) = self.clipboard.poll_event()? {
            if !self.dispatch(event)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn dispatch(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::SelectionRequest(request) => {
                self.answer(request)?;
                Ok(true)
            }
            Event::SelectionClear(cleared) => {
                // Either something else copied, or this is the echo of this
                // window's own earlier letting-go arriving after it took the
                // selection again. The two can carry the same timestamp, so
                // the server is asked who owns it now rather than guessing.
                if cleared.time < self.since || self.still_owner()? {
                    return Ok(true);
                }
                self.owns = false;
                Ok(false)
            }
            Event::PropertyNotify(notify) if notify.state == Property::DELETE => {
                self.continue_incremental(notify.window, notify.atom)?;
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    fn atoms_for_offer(&self) -> Vec<Atom> {
        let atoms = &self.clipboard.atoms;
        let mut out = vec![atoms.targets];
        for format in &self.offered {
            out.extend(atoms.targets_for(format));
        }
        out
    }

    fn data_for(&mut self, format: &ClipFormat) -> Result<Vec<u8>> {
        if let Some(cached) = self.cached.get(format) {
            return Ok(cached.clone());
        }
        let bytes = self.source.fetch(format)?;
        self.cached.insert(format.clone(), bytes.clone());
        Ok(bytes)
    }

    fn answer(&mut self, request: SelectionRequestEvent) -> Result<()> {
        // A requestor from an old protocol may leave the property unset, in
        // which case the convention is to use the target as the property.
        let property = if request.property == NONE {
            request.target
        } else {
            request.property
        };

        let refuse = |owner: &X11Owner| -> Result<()> {
            owner.notify(request, NONE)?;
            Ok(())
        };

        tracing::debug!(
            target = request.target,
            format = ?self.clipboard.atoms.format_of(request.target),
            requestor = request.requestor,
            "something here is asking the clipboard"
        );
        if request.target == self.clipboard.atoms.targets {
            let atoms = self.atoms_for_offer();
            let bytes: Vec<u8> = atoms.iter().flat_map(|a| a.to_ne_bytes()).collect();
            self.clipboard
                .conn
                .change_property(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    32,
                    atoms.len() as u32,
                    &bytes,
                )
                .map_err(display_err)?;
            return self.notify(request, property);
        }

        let Some(format) = self.clipboard.atoms.format_of(request.target) else {
            return refuse(self);
        };
        if !self.offered.contains(&format) {
            return refuse(self);
        }
        let data = match self.data_for(&format) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(?format, "could not supply the clipboard: {e}");
                return refuse(self);
            }
        };

        if data.len() <= self.chunk {
            self.clipboard
                .conn
                .change_property(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    request.target,
                    8,
                    data.len() as u32,
                    &data,
                )
                .map_err(display_err)?;
            return self.notify(request, property);
        }

        // Too large for one property. Announce the size, then feed it a piece
        // at a time as the requestor consumes them.
        self.clipboard
            .conn
            .change_property32(
                PropMode::REPLACE,
                request.requestor,
                property,
                self.clipboard.atoms.incr,
                &[data.len() as u32],
            )
            .map_err(display_err)?;
        self.clipboard
            .conn
            .change_window_attributes(
                request.requestor,
                &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::PROPERTY_CHANGE),
            )
            .map_err(display_err)?;
        self.in_flight.push(Incremental {
            requestor: request.requestor,
            property,
            data,
            sent: 0,
        });
        self.notify(request, property)
    }

    fn continue_incremental(&mut self, window: Window, property: Atom) -> Result<()> {
        let Some(index) = self
            .in_flight
            .iter()
            .position(|t| t.requestor == window && t.property == property)
        else {
            return Ok(());
        };

        let (chunk, finished) = {
            let transfer = &mut self.in_flight[index];
            let end = (transfer.sent + self.chunk).min(transfer.data.len());
            let chunk = transfer.data[transfer.sent..end].to_vec();
            transfer.sent = end;
            (chunk, end >= transfer.data.len())
        };

        self.clipboard
            .conn
            .change_property(
                PropMode::REPLACE,
                window,
                property,
                AtomEnum::STRING,
                8,
                chunk.len() as u32,
                &chunk,
            )
            .map_err(display_err)?;
        self.clipboard.conn.flush().map_err(display_err)?;

        // A zero-length write is how the end is signalled, so the last real
        // piece is followed by one more round.
        if finished && chunk.is_empty() {
            self.in_flight.remove(index);
        }
        Ok(())
    }

    fn notify(&self, request: SelectionRequestEvent, property: Atom) -> Result<()> {
        let event = SelectionNotifyEvent {
            response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property,
        };
        self.clipboard
            .conn
            .send_event(false, request.requestor, EventMask::NO_EVENT, event)
            .map_err(display_err)?;
        self.clipboard.conn.flush().map_err(display_err)?;
        Ok(())
    }

    /// Give the clipboard back.
    pub fn release(mut self) -> Result<X11Clipboard> {
        if self.owns {
            self.clipboard
                .conn
                .set_selection_owner(NONE, self.clipboard.atoms.clipboard, CURRENT_TIME)
                .map_err(display_err)?;
            self.clipboard.conn.flush().map_err(display_err)?;
            self.owns = false;
        }
        Ok(self.clipboard)
    }
}

/// Something to do on the owner's thread.
enum Command {
    Offer(Vec<ClipFormat>, Box<dyn Fetch>),
    Release,
}

/// Holds the clipboard on other machines' behalf, from a thread of its own.
///
/// An X11 owner has to keep answering for as long as its offer stands, and an
/// answer may mean fetching from another machine, which takes as long as it
/// takes. None of that belongs on the thread deciding where the cursor is, so
/// the owner lives here and is spoken to through a channel.
pub struct X11Writer {
    commands: mpsc::Sender<Command>,
    owner_window: Arc<AtomicU32>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl X11Writer {
    /// Open a connection of its own and stand ready to take the clipboard.
    pub fn open() -> Result<X11Writer> {
        Self::open_display(None)
    }

    pub fn open_display(display: Option<&str>) -> Result<X11Writer> {
        let display = display.map(str::to_string);
        // Connected here rather than on the thread, so a display that cannot
        // be reached is reported to the caller instead of to a log nobody is
        // watching yet.
        let clipboard = X11Clipboard::open_display(display.as_deref())?;
        let (commands, inbox) = mpsc::channel();
        let owner_window = Arc::new(AtomicU32::new(0));
        let published = owner_window.clone();
        let thread = std::thread::Builder::new()
            .name("smkvm-clipboard-owner".into())
            .spawn(move || owner_thread(clipboard, display, inbox, published))
            .map_err(|e| ClipboardError::Display(format!("could not start a thread: {e}")))?;
        Ok(X11Writer {
            commands,
            owner_window,
            thread: Some(thread),
        })
    }

    /// The window this program holds the clipboard through, or zero when it
    /// holds nothing. For a watcher to look away from.
    pub fn owner_window(&self) -> Arc<AtomicU32> {
        self.owner_window.clone()
    }

    fn send(&self, command: Command) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| ClipboardError::Display("the clipboard owner thread has stopped".into()))
    }
}

impl Write for X11Writer {
    fn offer(&mut self, formats: &[ClipFormat], source: Box<dyn Fetch>) -> Result<()> {
        self.send(Command::Offer(formats.to_vec(), source))
    }

    fn release(&mut self) -> Result<()> {
        self.send(Command::Release)
    }
}

impl Drop for X11Writer {
    fn drop(&mut self) {
        // Closing the channel is the signal to stop; the owner lets go of the
        // clipboard on its way out.
        let (stop, _) = mpsc::channel();
        self.commands = stop;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How long the owner thread waits for an instruction before looking after the
/// clipboard again. Short, because a paste is somebody waiting.
const OWNER_TICK: Duration = Duration::from_millis(5);

fn owner_thread(
    clipboard: X11Clipboard,
    display: Option<String>,
    inbox: mpsc::Receiver<Command>,
    published: Arc<AtomicU32>,
) {
    let mut owner: Option<X11Owner> = None;
    // The connection an offer is made through, between offers.
    let mut idle: Option<X11Clipboard> = Some(clipboard);
    let reconnect = || X11Clipboard::open_display(display.as_deref()).ok();

    // Let go of whatever is held, and get the connection back for next time.
    let take_back = |owner: &mut Option<X11Owner>, idle: &mut Option<X11Clipboard>| {
        if let Some(held) = owner.take() {
            published.store(0, Ordering::Relaxed);
            match held.release() {
                Ok(conn) => *idle = Some(conn),
                Err(e) => {
                    tracing::warn!("could not give the clipboard back: {e}");
                    *idle = reconnect();
                }
            }
        }
    };

    loop {
        let command = match inbox.recv_timeout(OWNER_TICK) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        match command {
            Some(Command::Offer(formats, source)) => {
                if let Some(held) = owner.as_mut().filter(|h| h.owns_clipboard()) {
                    held.reoffer(&formats, source);
                    continue;
                }
                take_back(&mut owner, &mut idle);
                let Some(conn) = idle.take().or_else(reconnect) else {
                    tracing::warn!("cannot reach the display to offer the clipboard");
                    continue;
                };
                match X11Owner::take(conn, &formats, source) {
                    Ok(taken) => {
                        published.store(taken.window(), Ordering::Relaxed);
                        owner = Some(taken);
                    }
                    Err(e) => {
                        tracing::warn!("could not take the clipboard: {e}");
                        idle = reconnect();
                    }
                }
            }
            Some(Command::Release) => take_back(&mut owner, &mut idle),
            None => {}
        }

        if let Some(held) = owner.as_mut() {
            match held.serve_pending() {
                Ok(true) => {}
                // Something else copied; the clipboard is theirs now and there
                // is nothing left to answer.
                Ok(false) => take_back(&mut owner, &mut idle),
                Err(e) => {
                    tracing::warn!("serving the clipboard failed: {e}");
                    published.store(0, Ordering::Relaxed);
                    owner = None;
                    idle = reconnect();
                }
            }
        }
    }
    take_back(&mut owner, &mut idle);
}

// ---------------------------------------------------------------------------
// Drag and drop: picking up what is being dragged as the cursor leaves.
// ---------------------------------------------------------------------------

/// The atoms the XDND protocol is spoken in.
struct DndAtoms {
    aware: Atom,
    enter: Atom,
    position: Atom,
    status: Atom,
    drop: Atom,
    finished: Atom,
    selection: Atom,
    type_list: Atom,
    action_copy: Atom,
    uri_list: Atom,
    /// Where the file list is asked to be written on our window.
    transfer: Atom,
}

impl DndAtoms {
    fn intern(conn: &RustConnection) -> Result<DndAtoms> {
        let get = |name: &str| -> Result<Atom> {
            Ok(conn
                .intern_atom(false, name.as_bytes())
                .map_err(display_err)?
                .reply()
                .map_err(display_err)?
                .atom)
        };
        Ok(DndAtoms {
            aware: get("XdndAware")?,
            enter: get("XdndEnter")?,
            position: get("XdndPosition")?,
            status: get("XdndStatus")?,
            drop: get("XdndDrop")?,
            finished: get("XdndFinished")?,
            selection: get("XdndSelection")?,
            type_list: get("XdndTypeList")?,
            action_copy: get("XdndActionCopy")?,
            uri_list: get("text/uri-list")?,
            transfer: get("SMKVM_DND_TRANSFER")?,
        })
    }
}

/// The XDND protocol version this speaks. Five is what every toolkit has
/// spoken for twenty years.
const XDND_VERSION: u32 = 5;

/// How long the dragging application gets to notice the window.
const ENTER_PATIENCE: Duration = Duration::from_millis(250);
/// How long the drop gets to arrive once the button has been released, and
/// then the file list once the drop has.
const DROP_PATIENCE: Duration = Duration::from_millis(500);
/// Half the side of the catching window. Big enough that a pointer nudged
/// by one pixel is still inside it.
const REACH: i16 = 32;

/// Picks up what is being dragged as the cursor leaves this machine.
///
/// XDND tells only the window under the pointer what a drag carries, so this
/// keeps a small override-redirect window of its own unmapped, and when asked
/// maps it under the pointer for a moment. The dragging application notices
/// it on the next pointer movement and sends `XdndEnter`; the drag is then
/// accepted, the button released on the application's behalf so it drops
/// here, the file list read out of the drag's selection, and the application
/// told the drop is finished -- with nothing moved or copied on this machine,
/// since a copy is what was declared.
pub struct DndCatcher {
    conn: RustConnection,
    root: Window,
    window: Window,
    atoms: DndAtoms,
}

impl DndCatcher {
    pub fn open() -> Result<DndCatcher> {
        Self::open_display(None)
    }

    pub fn open_display(display: Option<&str>) -> Result<DndCatcher> {
        let (conn, screen_num) = x11rb::connect(display).map_err(display_err)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let window = conn.generate_id().map_err(display_err)?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            (2 * REACH) as u16,
            (2 * REACH) as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::PROPERTY_CHANGE)
                .background_pixel(screen.black_pixel),
        )
        .map_err(display_err)?;
        let atoms = DndAtoms::intern(&conn)?;
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atoms.aware,
            AtomEnum::ATOM,
            &[XDND_VERSION],
        )
        .map_err(display_err)?;
        conn.flush().map_err(display_err)?;
        Ok(DndCatcher {
            conn,
            root,
            window,
            atoms,
        })
    }

    /// Wait for an event matching `want`, dropping everything else.
    fn wait_for<T>(
        &self,
        deadline: Instant,
        mut want: impl FnMut(&Event) -> Option<T>,
    ) -> Option<T> {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => {
                    if let Some(value) = want(&event) {
                        return Some(value);
                    }
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return None,
            }
        }
    }

    fn tell(&self, to: Window, type_: Atom, data: [u32; 5]) -> Result<()> {
        let event = x11rb::protocol::xproto::ClientMessageEvent {
            response_type: x11rb::protocol::xproto::CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: to,
            type_,
            data: x11rb::protocol::xproto::ClientMessageData::from(data),
        };
        self.conn
            .send_event(false, to, EventMask::NO_EVENT, event)
            .map_err(display_err)?;
        self.conn.flush().map_err(display_err)?;
        Ok(())
    }

    fn unmap(&self) {
        let _ = self.conn.unmap_window(self.window);
        let _ = self.conn.flush();
    }

    /// The types a drag offers: three in the message, or a list on the
    /// source's window when there are more.
    fn types_of(&self, source: Window, data: &[u32; 5]) -> Vec<Atom> {
        if data[1] & 1 == 0 {
            return data[2..5].iter().copied().filter(|a| *a != 0).collect();
        }
        self.conn
            .get_property(false, source, self.atoms.type_list, AtomEnum::ATOM, 0, 1024)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|reply| reply.value32().map(|v| v.collect()))
            .unwrap_or_default()
    }

    /// Read the drag's file list out of its selection, as of `time`.
    fn file_list(&self, time: Timestamp) -> Option<Vec<u8>> {
        self.conn
            .delete_property(self.window, self.atoms.transfer)
            .ok()?;
        self.conn
            .convert_selection(
                self.window,
                self.atoms.selection,
                self.atoms.uri_list,
                self.atoms.transfer,
                time,
            )
            .ok()?;
        self.conn.flush().ok()?;
        let window = self.window;
        let deadline = Instant::now() + DROP_PATIENCE;
        let notify: SelectionNotifyEvent = self.wait_for(deadline, |event| match event {
            Event::SelectionNotify(e) if e.requestor == window => Some(*e),
            _ => None,
        })?;
        if notify.property == NONE {
            return None;
        }
        let reply = self
            .conn
            .get_property(
                false,
                window,
                self.atoms.transfer,
                AtomEnum::ANY,
                0,
                u32::MAX / 4,
            )
            .ok()?
            .reply()
            .ok()?;
        let _ = self.conn.delete_property(window, self.atoms.transfer);
        let _ = self.conn.flush();
        // A file list is a few hundred bytes; one that arrives in pieces is
        // not one this expects, and is not worth the machinery.
        (reply.format == 8 && !reply.value.is_empty()).then_some(reply.value)
    }
}

impl CatchDrag for DndCatcher {
    fn catch(&mut self, drive: &mut dyn FnMut(Drive)) -> Option<Vec<std::path::PathBuf>> {
        // Anything left over from an earlier attempt is about that attempt.
        while let Ok(Some(_)) = self.conn.poll_for_event() {}

        let pointer = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        let (x, y) = (pointer.root_x, pointer.root_y);
        self.conn
            .configure_window(
                self.window,
                &x11rb::protocol::xproto::ConfigureWindowAux::new()
                    .x(i32::from(x - REACH))
                    .y(i32::from(y - REACH))
                    .stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
            )
            .ok()?;
        self.conn.map_window(self.window).ok()?;
        self.conn.flush().ok()?;
        // The application looks at what is under the pointer when the pointer
        // moves. Once away and back leaves it exactly where it was.
        drive(Drive::MoveTo(i32::from(x) + 1, i32::from(y)));
        drive(Drive::MoveTo(i32::from(x), i32::from(y)));

        let (enter, position) = (self.atoms.enter, self.atoms.position);
        let window = self.window;
        let deadline = Instant::now() + ENTER_PATIENCE;
        let Some((source, data)) = self.wait_for(deadline, |event| match event {
            Event::ClientMessage(m) if m.window == window && m.type_ == enter => {
                let data = m.data.as_data32();
                Some((data[0], data))
            }
            _ => None,
        }) else {
            // Nothing was being dragged, or not by anything that speaks XDND.
            // The button is left exactly as it was.
            self.unmap();
            return None;
        };
        let types = self.types_of(source, &data);
        if !types.contains(&self.atoms.uri_list) {
            tracing::debug!("a drag was caught, but it carries no files");
            self.unmap();
            return None;
        }

        // The application sends where the pointer is once we have said we
        // are interested; we are, so accept whatever it asks about.
        let accept = |me: &DndCatcher| {
            me.tell(
                source,
                me.atoms.status,
                [me.window, 1, 0, 0, me.atoms.action_copy],
            )
        };
        let deadline = Instant::now() + ENTER_PATIENCE;
        let _ = self.wait_for(deadline, |event| match event {
            Event::ClientMessage(m) if m.window == window && m.type_ == position => Some(()),
            _ => None,
        });
        if accept(self).is_err() {
            self.unmap();
            return None;
        }

        // Let go of the button on the application's behalf, so its drag ends
        // on this window.
        drive(Drive::ReleaseLeft);
        let (drop, position) = (self.atoms.drop, self.atoms.position);
        let deadline = Instant::now() + DROP_PATIENCE;
        let mut pending_position = false;
        let dropped_at = self.wait_for(deadline, |event| match event {
            Event::ClientMessage(m) if m.window == window && m.type_ == drop => {
                Some(m.data.as_data32()[2])
            }
            Event::ClientMessage(m) if m.window == window && m.type_ == position => {
                pending_position = true;
                None
            }
            _ => None,
        });
        if pending_position {
            let _ = accept(self);
        }
        let Some(time) = dropped_at else {
            tracing::debug!("the drag was accepted but the drop never came");
            self.unmap();
            return None;
        };
        let list = self.file_list(if time == 0 { CURRENT_TIME } else { time });
        // Finished, and nothing was taken: the files are still where they
        // were, whatever the application thought it was doing.
        let _ = self.tell(
            source,
            self.atoms.finished,
            [self.window, 1, self.atoms.action_copy, 0, 0],
        );
        self.unmap();
        let paths = files::local_paths(&list?);
        (!paths.is_empty()).then_some(paths)
    }
}
