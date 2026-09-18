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
use std::time::{Duration, Instant};

use smkvm_proto::ClipFormat;
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    PropertyNotifyEvent, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use crate::{Available, ClipboardError, Read, Result, Watch};

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
        })
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
                    // Not what was being waited for, but somebody else's to
                    // handle later.
                    None => self.deferred.push(event),
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
            if matches!(event, Event::XfixesSelectionNotify(_)) {
                return self.available().ok();
            }
            // Requests aimed at us are somebody else's concern here; hold them
            // so whoever is serving the selection still sees them.
            if matches!(
                event,
                Event::SelectionRequest(_) | Event::SelectionClear(_) | Event::PropertyNotify(_)
            ) {
                self.deferred.push(event);
            }
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

        clipboard
            .conn
            .set_selection_owner(clipboard.window, clipboard.atoms.clipboard, CURRENT_TIME)
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
        })
    }

    pub fn owns_clipboard(&self) -> bool {
        self.owns
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
            Event::SelectionClear(_) => {
                // Something else copied; it is theirs now.
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
