//! The client: a machine that receives the cursor rather than owning it.
//!
//! Its job is narrow. It puts input where it is told, reports its displays,
//! and — the part that matters most — lets go of everything the moment it
//! stops being in charge. A key still held on a machine the user has walked
//! away from is the failure this is built around.

use smkvm_input::{Inject, InputError, Tracked};
use smkvm_layout::Monitor;
use smkvm_proto::{ClientControl, ServerControl, SuspendReason};

/// What the client should send back.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientAction {
    Send(ClientControl),
}

pub struct Client<I: Inject> {
    input: Tracked<I>,
    /// Whether this machine currently has the cursor.
    active: bool,
    suspended: Option<SuspendReason>,
    /// The last injection that the platform refused while the cursor was
    /// here, kept until somebody asks. A refusal means nothing sent is
    /// landing, which from the person's side is a pointer that has stopped;
    /// throwing the error away is how that went unexplained.
    refused: Option<InputError>,
}

impl<I: Inject> Client<I> {
    pub fn new(input: I) -> Self {
        Self {
            input: Tracked::new(input),
            active: false,
            suspended: None,
            refused: None,
        }
    }

    /// The most recent injection the platform refused, if any since the last
    /// time this was asked. The caller decides what that means here -- on
    /// Windows, most often a window of higher privilege in front.
    pub fn take_refusal(&mut self) -> Option<InputError> {
        self.refused.take()
    }

    fn note(&mut self, result: smkvm_input::Result<()>) {
        if let Err(e) = result {
            self.refused = Some(e);
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended.is_some()
    }

    pub fn input(&self) -> &Tracked<I> {
        &self.input
    }

    pub fn input_mut(&mut self) -> &mut Tracked<I> {
        &mut self.input
    }

    /// Tell the server what displays this machine has.
    pub fn report_monitors(&self, monitors: Vec<Monitor>) -> ClientAction {
        ClientAction::Send(ClientControl::Monitors { monitors })
    }

    /// Injection has stopped reaching the screen.
    ///
    /// On Windows this is a UAC prompt or the lock screen taking over: the
    /// session is fine, but nothing sent will land. Saying so lets the server
    /// keep the cursor rather than the pointer fighting an invisible wall.
    pub fn suspend(&mut self, reason: SuspendReason) -> Vec<ClientAction> {
        if self.suspended == Some(reason) {
            return Vec::new();
        }
        self.suspended = Some(reason);
        // Anything held would otherwise stay held for as long as the prompt is
        // up, and possibly past it.
        let _ = self.input.release_all();
        self.active = false;
        vec![ClientAction::Send(ClientControl::Suspended { reason })]
    }

    pub fn resume(&mut self) -> Vec<ClientAction> {
        if self.suspended.take().is_none() {
            return Vec::new();
        }
        vec![ClientAction::Send(ClientControl::Resumed)]
    }

    /// The link went away.
    ///
    /// Nothing can arrive to release what is held, so it is released here. A
    /// dropped connection in the middle of a chord is the commonest way a key
    /// gets stranded.
    pub fn disconnected(&mut self) {
        self.active = false;
        let _ = self.input.release_all();
        // Whatever happened to the link, this machine's own pointer is its own
        // again: leaving it out of the way would strand the person without one,
        // and this is the last moment anything will be done about it.
        if let Err(e) = self.input.inner_mut().show_cursor() {
            tracing::warn!("the pointer could not be brought back: {e}");
        }
    }

    pub fn handle(&mut self, msg: ServerControl) -> Vec<ClientAction> {
        match msg {
            ServerControl::Enter {
                at,
                pressed,
                buttons,
            } => {
                self.active = true;
                tracing::info!(at = ?(at.x, at.y), "the cursor arrived");
                // Position first, then state: a press that lands before the
                // pointer has arrived goes to whatever was under it.
                if let Err(e) = self.input.move_to(at.x, at.y) {
                    // Worth saying: this is the person watching a machine they
                    // have just been given not show them a pointer.
                    tracing::warn!("the pointer could not be put where it arrived: {e}");
                    self.refused = Some(e);
                }
                // After the placement, not before. Anything owed from being
                // parked is settled by having been put somewhere on purpose --
                // unless the placement failed, in which case going back to
                // where it was parked from beats leaving it in a corner.
                if let Err(e) = self.input.inner_mut().show_cursor() {
                    tracing::warn!("the pointer could not be brought back: {e}");
                }
                let _ = self.input.sync(&pressed, &buttons);
                Vec::new()
            }
            ServerControl::Leave => {
                self.active = false;
                let _ = self.input.release_all();
                tracing::info!("the cursor left");
                // Out of sight, so it stops looking like a pointer the person
                // could still move.
                if let Err(e) = self.input.inner_mut().hide_cursor() {
                    tracing::warn!("the pointer could not be put out of the way: {e}");
                }
                Vec::new()
            }
            ServerControl::MoveTo { x, y } => {
                if self.deliverable() {
                    let moved = self.input.move_to(x, y);
                    self.note(moved);
                    let _ = self.input.flush();
                }
                Vec::new()
            }
            ServerControl::Button { button, down } => {
                if self.deliverable() {
                    let pressed = self.input.button(button, down);
                    self.note(pressed);
                    let _ = self.input.flush();
                }
                Vec::new()
            }
            ServerControl::Wheel(scroll) => {
                if self.deliverable() {
                    let scrolled = self.input.wheel(scroll);
                    self.note(scrolled);
                    let _ = self.input.flush();
                }
                Vec::new()
            }
            ServerControl::KeyEvent { key, down, repeat } => {
                if self.deliverable() {
                    // A repeat is the same physical key going down again; the
                    // held set does not change.
                    let typed = if repeat && down {
                        self.input.inner_mut().key(key, true)
                    } else {
                        self.input.key(key, down)
                    };
                    // A key this platform has no way to send is not a refusal;
                    // it is reported once by the capture side already.
                    if !matches!(typed, Err(InputError::UnmappedKey(_))) {
                        self.note(typed);
                    }
                    let _ = self.input.flush();
                }
                Vec::new()
            }
            ServerControl::SyncKeys { pressed, buttons } => {
                let _ = self.input.sync(&pressed, &buttons);
                Vec::new()
            }
            // Always obeyed, active or not, suspended or not: being told to
            // let go is never something to second-guess.
            ServerControl::ReleaseAll => {
                let _ = self.input.release_all();
                Vec::new()
            }
            ServerControl::Ping { id } => vec![ClientAction::Send(ClientControl::Pong { id })],
            ServerControl::Pong { .. } => Vec::new(),
            ServerControl::Goodbye => {
                self.disconnected();
                Vec::new()
            }
            // Handshake messages are the session layer's business, and the
            // clipboard is the exchange's: both are dealt with before anything
            // reaches here.
            ServerControl::Hello(_) | ServerControl::Rejected { .. } | ServerControl::Bulk(_) => {
                Vec::new()
            }
        }
    }

    /// Should input actually be put on this screen right now?
    fn deliverable(&self) -> bool {
        self.active && self.suspended.is_none()
    }
}
