//! The client: a machine that receives the cursor rather than owning it.
//!
//! Its job is narrow. It puts input where it is told, reports its displays,
//! and — the part that matters most — lets go of everything the moment it
//! stops being in charge. A key still held on a machine the user has walked
//! away from is the failure this is built around.

use smkvm_input::{Inject, Tracked};
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
}

impl<I: Inject> Client<I> {
    pub fn new(input: I) -> Self {
        Self {
            input: Tracked::new(input),
            active: false,
            suspended: None,
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
    }

    pub fn handle(&mut self, msg: ServerControl) -> Vec<ClientAction> {
        match msg {
            ServerControl::Enter {
                at,
                pressed,
                buttons,
            } => {
                self.active = true;
                // Position first, then state: a press that lands before the
                // pointer has arrived goes to whatever was under it.
                let _ = self.input.move_to(at.x, at.y);
                let _ = self.input.sync(&pressed, &buttons);
                Vec::new()
            }
            ServerControl::Leave => {
                self.active = false;
                let _ = self.input.release_all();
                Vec::new()
            }
            ServerControl::MoveTo { x, y } => {
                if self.deliverable() {
                    let _ = self.input.move_to(x, y);
                    let _ = self.input.flush();
                }
                Vec::new()
            }
            ServerControl::Button { button, down } => {
                if self.deliverable() {
                    let _ = self.input.button(button, down);
                    let _ = self.input.flush();
                }
                Vec::new()
            }
            ServerControl::Wheel(scroll) => {
                if self.deliverable() {
                    let _ = self.input.wheel(scroll);
                    let _ = self.input.flush();
                }
                Vec::new()
            }
            ServerControl::KeyEvent { key, down, repeat } => {
                if self.deliverable() {
                    // A repeat is the same physical key going down again; the
                    // held set does not change.
                    let _ = if repeat && down {
                        self.input.inner_mut().key(key, true)
                    } else {
                        self.input.key(key, down)
                    };
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
            // Handshake messages are the session layer's business.
            ServerControl::Hello(_) | ServerControl::Rejected { .. } => Vec::new(),
        }
    }

    /// Should input actually be put on this screen right now?
    fn deliverable(&self) -> bool {
        self.active && self.suspended.is_none()
    }
}
