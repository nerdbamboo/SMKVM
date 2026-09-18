//! The server: the machine holding the keyboard and mouse.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use smkvm_layout::{DeviceId, EdgeOverflow, Layout, Located, Monitor, MonitorId, Point, Rect};
use smkvm_proto::{Key, MouseButton, Scroll, ServerControl, SuspendReason};

/// How the server should be reading the local pointer.
///
/// While the cursor is on the server's own screen the operating system moves
/// it and the server simply follows along. Once it leaves, local input is
/// swallowed and the pointer is driven by relative motion instead, because
/// there is no longer any local position for it to have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerMode {
    /// The cursor is here. Let input through and follow the pointer.
    Local,
    /// The cursor is on another machine. Swallow local input, confine the
    /// pointer, and report relative motion.
    Captured,
}

/// Things that happen to the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Where the real local pointer is, in the server's own coordinates.
    ///
    /// Only meaningful while the cursor is here, and only ever used to keep
    /// the tracked position in step with the one the operating system is
    /// drawing. Pointer acceleration means raw motion and actual movement
    /// differ, so without this they would slowly drift apart.
    PointerAt {
        x: i32,
        y: i32,
    },
    /// Raw pointer motion, before the operating system clamps it.
    ///
    /// This is what decides crossings, in both modes. Once the real pointer is
    /// pinned against a screen edge its position stops changing however hard
    /// the user pushes, so the position alone can never reveal the intent to
    /// leave. The raw delta still does.
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

    /// A machine finished its handshake.
    ClientUp {
        device: DeviceId,
        name: String,
    },
    /// A machine reported its displays, on arrival or when they changed.
    ClientMonitors {
        device: DeviceId,
        monitors: Vec<Monitor>,
    },
    /// A machine's link went away.
    ClientDown {
        device: DeviceId,
    },
    /// A machine cannot accept input for the moment.
    ClientSuspended {
        device: DeviceId,
        reason: SuspendReason,
    },
    ClientResumed {
        device: DeviceId,
    },
    /// Nothing happened, but time passed. Drives the edge-hold timing.
    Tick,
}

/// What the server's own machine should do.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalAction {
    /// Start or stop swallowing input and confining the pointer.
    SetPointerMode(PointerMode),
    /// Put the local pointer here, in the server's own coordinates.
    WarpCursor { x: i32, y: i32 },
    /// Release everything held locally.
    ReleaseAll,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Send { to: DeviceId, msg: ServerControl },
    Local(LocalAction),
}

/// Where the configuration says a monitor belongs.
///
/// Machines are named rather than identified by key, because a layout is
/// written before anything has been paired and has to survive a machine being
/// replaced. A monitor is named by its own identifier, or by `primary` for the
/// common case of a machine with one screen, whose identifier is a long device
/// path nobody wants to type.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub machine: String,
    pub monitor: String,
    pub global: Rect,
}

impl Placement {
    /// The name that stands for whichever monitor the system calls primary.
    pub const PRIMARY: &'static str = "primary";
}

/// Switching behaviour, from the configuration file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// How long the pointer must rest against an edge before it crosses.
    pub switch_delay: Duration,
    /// When set, an edge must be struck twice within this window to cross.
    pub switch_double_tap: Duration,
    pub edge_overflow: EdgeOverflow,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            switch_delay: Duration::ZERO,
            switch_double_tap: Duration::ZERO,
            edge_overflow: EdgeOverflow::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Ready,
    /// Connected but unable to act on input, so the cursor must stay away.
    Suspended,
}

/// A pointer pressed against an edge that it has not yet been allowed through.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    target: DeviceId,
    since: Instant,
    /// Where the cursor would land once it is let through.
    at: Point,
}

pub struct Server {
    local: DeviceId,
    layout: Layout,
    settings: Settings,

    /// The authoritative cursor position on the global desktop.
    cursor: Point,
    /// Which machine currently has the cursor.
    active: DeviceId,
    mode: PointerMode,

    clients: BTreeMap<DeviceId, Health>,
    keys: BTreeSet<Key>,
    buttons: BTreeSet<MouseButton>,

    pending: Option<Pending>,
    /// When the edge was last struck, for double-tap.
    last_tap: Option<(Instant, DeviceId)>,
    /// The arrangement the configuration asks for. Applied as each machine
    /// reports what it has; anything not mentioned is placed automatically.
    placements: Vec<Placement>,
}

impl Server {
    /// Start a server whose own machine is `local`.
    pub fn new(
        local: DeviceId,
        name: impl Into<String>,
        layout: Layout,
        settings: Settings,
    ) -> Self {
        let mut layout = layout;
        // Seed the machine's own entry so it is known by its name from the
        // first log line, rather than by an identifier nobody can read.
        if layout.device(local).is_none() {
            layout.report_monitors(local, name, Vec::new());
        }
        layout.edge_overflow = settings.edge_overflow;
        layout.set_online(local, true);
        let cursor = layout
            .cells()
            .iter()
            .find(|c| c.device == local)
            .map(|c| c.global.origin())
            .unwrap_or(Point::new(0, 0));
        Self {
            local,
            layout,
            settings,
            cursor,
            active: local,
            mode: PointerMode::Local,
            clients: BTreeMap::new(),
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            pending: None,
            last_tap: None,
            placements: Vec::new(),
        }
    }

    /// Use the arrangement from the configuration.
    ///
    /// Takes effect as machines report their monitors, so it can name a
    /// machine that has not connected yet, or one that never does.
    pub fn set_placements(&mut self, placements: Vec<Placement>) {
        self.placements = placements;
        let known: Vec<DeviceId> = self.layout.devices().iter().map(|d| d.id).collect();
        for device in known {
            self.apply_placements(device);
        }
        self.layout.auto_place();
    }

    /// Put this machine's monitors where the configuration says.
    fn apply_placements(&mut self, device: DeviceId) {
        let Some(dev) = self.layout.device(device) else {
            return;
        };
        let name = dev.name.clone();
        let monitors: Vec<(MonitorId, bool)> = dev
            .monitors
            .iter()
            .map(|m| (m.id.clone(), m.primary))
            .collect();

        for placement in self.placements.clone() {
            if placement.machine != name {
                continue;
            }
            let wanted = monitors.iter().find(|(id, primary)| {
                id.as_str() == placement.monitor
                    || (placement.monitor == Placement::PRIMARY && *primary)
            });
            let Some((id, _)) = wanted else {
                tracing::warn!(
                    machine = %name,
                    monitor = %placement.monitor,
                    "the configuration places a monitor this machine does not have"
                );
                continue;
            };
            self.layout.place_rect(device, id, placement.global);
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn layout_mut(&mut self) -> &mut Layout {
        &mut self.layout
    }

    /// Which machine currently has the cursor.
    pub fn active(&self) -> DeviceId {
        self.active
    }

    pub fn cursor(&self) -> Point {
        self.cursor
    }

    pub fn pointer_mode(&self) -> PointerMode {
        self.mode
    }

    pub fn held_keys(&self) -> Vec<Key> {
        self.keys.iter().copied().collect()
    }

    pub fn held_buttons(&self) -> Vec<MouseButton> {
        self.buttons.iter().copied().collect()
    }

    fn is_usable(&self, device: DeviceId) -> bool {
        if device == self.local {
            return true;
        }
        matches!(self.clients.get(&device), Some(Health::Ready))
    }

    pub fn handle(&mut self, event: Event, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        match event {
            Event::PointerAt { x, y } => self.pointer_absolute(x, y, now, &mut out),

            Event::PointerBy { dx, dy } => self.pointer_relative(dx, dy, now, &mut out),
            Event::Button { button, down } => self.button(button, down, &mut out),
            Event::Wheel(scroll) => self.wheel(scroll, &mut out),
            Event::Key { key, down, repeat } => self.key(key, down, repeat, &mut out),
            Event::ClientUp { device, name } => self.client_up(device, name, &mut out),
            Event::ClientMonitors { device, monitors } => {
                self.client_monitors(device, monitors, &mut out)
            }
            Event::ClientDown { device } => self.client_down(device, &mut out),
            Event::ClientSuspended { device, reason } => {
                self.client_suspended(device, reason, &mut out)
            }
            Event::ClientResumed { device } => self.client_resumed(device, &mut out),
            Event::Tick => self.settle_pending(now, &mut out),
        }
        out
    }

    // --- pointer ------------------------------------------------------

    /// Keep the tracked cursor in step with the real one. Never crosses:
    /// a clamped pointer reports the same position forever, so treating it as
    /// motion would both miss the crossing and double-count the movement that
    /// [`Event::PointerBy`] already accounted for.
    fn pointer_absolute(&mut self, x: i32, y: i32, _now: Instant, _out: &mut [Action]) {
        if self.mode != PointerMode::Local || self.active != self.local {
            return;
        }
        if let Some(global) = self.local_to_global(x, y) {
            self.cursor = global;
        }
    }

    fn pointer_relative(&mut self, dx: i32, dy: i32, now: Instant, out: &mut Vec<Action>) {
        self.advance(dx, dy, now, out);
    }

    /// Move the authoritative cursor and deal with whatever that implies.
    fn advance(&mut self, dx: i32, dy: i32, now: Instant, out: &mut Vec<Action>) {
        let Some(motion) = self.layout.resolve(self.cursor, dx, dy) else {
            return;
        };
        let target = motion.located.device;

        if target == self.active {
            self.pending = None;
            self.cursor = motion.global;
            self.deliver_position(&motion.located, out);
            return;
        }

        if !self.is_usable(target) {
            // The machine beyond that edge cannot take the cursor, so the
            // cursor does not go there. Better a wall than a pointer that
            // disappears onto a screen that will not move it.
            self.pending = None;
            self.stop_at_edge(motion.global, out);
            return;
        }

        if self.may_cross(target, now) {
            self.cross_to(target, motion.global, motion.located, out);
        } else {
            self.hold_at_edge(target, motion.global, now);
            self.stop_at_edge(motion.global, out);
        }
    }

    /// Put the cursor as close to `beyond` as the current screen allows.
    ///
    /// A cursor that is not being let through still travels to the edge and
    /// waits there. Leaving it where it was would make the screen feel like it
    /// ended early.
    fn stop_at_edge(&mut self, beyond: Point, out: &mut Vec<Action>) {
        let cells = self.layout.cells();
        let mine: Vec<_> = cells.iter().filter(|c| c.device == self.active).collect();
        let Some(cell) = mine
            .iter()
            .find(|c| c.global.contains(self.cursor))
            .or(mine.first())
        else {
            return;
        };
        let at = cell.global.clamp_point(beyond);
        if at == self.cursor {
            return;
        }
        self.cursor = at;
        if let Some(located) = self.layout.locate(at) {
            self.deliver_position(&located, out);
        }
    }

    /// Is the cursor allowed through the edge it is pressed against?
    fn may_cross(&mut self, target: DeviceId, now: Instant) -> bool {
        if !self.settings.switch_double_tap.is_zero() {
            // A second strike on the same edge within the window goes straight
            // through; otherwise the strike is remembered and the cursor waits.
            let repeat = matches!(
                self.last_tap,
                Some((when, dev)) if dev == target && now.duration_since(when) <= self.settings.switch_double_tap
            );
            self.last_tap = Some((now, target));
            if repeat {
                return true;
            }
            if self.settings.switch_delay.is_zero() {
                return false;
            }
        }
        if self.settings.switch_delay.is_zero() {
            return true;
        }
        match &self.pending {
            Some(p) if p.target == target => {
                now.duration_since(p.since) >= self.settings.switch_delay
            }
            _ => false,
        }
    }

    fn hold_at_edge(&mut self, target: DeviceId, at: Point, now: Instant) {
        match &self.pending {
            Some(p) if p.target == target => {}
            _ => {
                self.pending = Some(Pending {
                    target,
                    since: now,
                    at,
                });
            }
        }
        if let Some(p) = &mut self.pending {
            p.at = at;
        }
    }

    /// Let a waiting cursor through once its edge has been held long enough.
    fn settle_pending(&mut self, now: Instant, out: &mut Vec<Action>) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        if self.settings.switch_delay.is_zero()
            || now.duration_since(pending.since) < self.settings.switch_delay
        {
            return;
        }
        if !self.is_usable(pending.target) {
            self.pending = None;
            return;
        }
        let Some(located) = self.layout.locate(pending.at) else {
            self.pending = None;
            return;
        };
        self.cross_to(pending.target, pending.at, located, out);
    }

    /// Hand the cursor to another machine.
    fn cross_to(
        &mut self,
        target: DeviceId,
        global: Point,
        located: Located,
        out: &mut Vec<Action>,
    ) {
        let leaving = self.active;
        self.pending = None;
        self.last_tap = None;
        self.cursor = global;
        self.active = target;

        // Tell the machine being left that it no longer has the cursor, so it
        // lets go of anything it is holding on the user's behalf.
        if leaving == self.local {
            out.push(Action::Local(LocalAction::SetPointerMode(
                PointerMode::Captured,
            )));
        } else {
            out.push(Action::Send {
                to: leaving,
                msg: ServerControl::Leave,
            });
        }

        // The arriving machine is given the input state as it stands, so a
        // chord begun elsewhere continues here and nothing stale survives.
        if target == self.local {
            self.mode = PointerMode::Local;
            out.push(Action::Local(LocalAction::SetPointerMode(
                PointerMode::Local,
            )));
            out.push(Action::Local(LocalAction::WarpCursor {
                x: located.local.x,
                y: located.local.y,
            }));
        } else {
            self.mode = PointerMode::Captured;
            out.push(Action::Send {
                to: target,
                msg: ServerControl::Enter {
                    at: located.local,
                    pressed: self.held_keys(),
                    buttons: self.held_buttons(),
                },
            });
        }
    }

    /// Send the cursor's new position to whoever currently has it.
    fn deliver_position(&mut self, located: &Located, out: &mut Vec<Action>) {
        if located.device == self.local {
            // The operating system is already moving the pointer; warping it to
            // where it already is would fight the user.
            if self.mode == PointerMode::Captured {
                out.push(Action::Local(LocalAction::WarpCursor {
                    x: located.local.x,
                    y: located.local.y,
                }));
            }
            return;
        }
        out.push(Action::Send {
            to: located.device,
            msg: ServerControl::MoveTo {
                x: located.local.x,
                y: located.local.y,
            },
        });
    }

    // --- buttons and keys --------------------------------------------

    fn button(&mut self, button: MouseButton, down: bool, out: &mut Vec<Action>) {
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        if self.active != self.local {
            out.push(Action::Send {
                to: self.active,
                msg: ServerControl::Button { button, down },
            });
        }
    }

    fn wheel(&mut self, scroll: Scroll, out: &mut Vec<Action>) {
        if self.active != self.local {
            out.push(Action::Send {
                to: self.active,
                msg: ServerControl::Wheel(scroll),
            });
        }
    }

    fn key(&mut self, key: Key, down: bool, repeat: bool, out: &mut Vec<Action>) {
        if down {
            self.keys.insert(key);
        } else {
            self.keys.remove(&key);
        }
        if self.active != self.local {
            out.push(Action::Send {
                to: self.active,
                msg: ServerControl::KeyEvent { key, down, repeat },
            });
        }
    }

    // --- machines coming and going ------------------------------------

    fn client_up(&mut self, device: DeviceId, name: String, out: &mut Vec<Action>) {
        self.clients.insert(device, Health::Ready);
        match self.layout.device_mut(device) {
            Some(existing) => {
                existing.name = name;
                existing.online = true;
            }
            // Record the name now, before any monitors arrive. The
            // configuration places machines by name, so a machine whose name
            // is not yet known would be arranged automatically instead and
            // then never corrected.
            None => {
                self.layout.report_monitors(device, name, Vec::new());
            }
        }
        // A machine that has just arrived may be holding keys from a previous
        // session; start it from a known state.
        out.push(Action::Send {
            to: device,
            msg: ServerControl::ReleaseAll,
        });
    }

    fn client_monitors(&mut self, device: DeviceId, monitors: Vec<Monitor>, out: &mut Vec<Action>) {
        let name = self
            .layout
            .device(device)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| device.short());
        self.layout.report_monitors(device, name, monitors);
        self.apply_placements(device);
        self.layout.auto_place();
        for cell in self.layout.cells() {
            tracing::info!(
                machine = %self.name_of(cell.device),
                monitor = %cell.monitor,
                at = ?(cell.global.x, cell.global.y, cell.global.w, cell.global.h),
                "placed on the desktop"
            );
        }
        self.clients.entry(device).or_insert(Health::Ready);
        // The arrangement just changed under the cursor; make sure it is still
        // somewhere real.
        self.resettle(out);
    }

    fn client_down(&mut self, device: DeviceId, out: &mut Vec<Action>) {
        self.clients.remove(&device);
        self.layout.set_online(device, false);
        if let Some(p) = &self.pending {
            if p.target == device {
                self.pending = None;
            }
        }
        if self.active == device {
            self.recall_cursor(out);
        }
    }

    fn client_suspended(
        &mut self,
        device: DeviceId,
        _reason: SuspendReason,
        out: &mut Vec<Action>,
    ) {
        self.clients.insert(device, Health::Suspended);
        if let Some(p) = &self.pending {
            if p.target == device {
                self.pending = None;
            }
        }
        if self.active == device {
            // The machine cannot move its pointer, so continuing to send it
            // positions produces the cursor fighting itself. Take the cursor
            // back until it says it can act again.
            self.recall_cursor(out);
        }
    }

    fn client_resumed(&mut self, device: DeviceId, out: &mut Vec<Action>) {
        if self.clients.contains_key(&device) {
            self.clients.insert(device, Health::Ready);
        }
        let _ = out;
    }

    fn name_of(&self, device: DeviceId) -> String {
        self.layout
            .device(device)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| device.short())
    }

    /// Bring the cursor back to the server's own screen.
    fn recall_cursor(&mut self, out: &mut Vec<Action>) {
        self.pending = None;
        self.last_tap = None;
        self.active = self.local;
        self.mode = PointerMode::Local;

        let home = self
            .layout
            .cells()
            .into_iter()
            .find(|c| c.device == self.local);
        out.push(Action::Local(LocalAction::SetPointerMode(
            PointerMode::Local,
        )));
        if let Some(cell) = home {
            let centre = Point::new(
                cell.global.x + cell.global.w / 2,
                cell.global.y + cell.global.h / 2,
            );
            self.cursor = centre;
            if let Some(located) = self.layout.locate(centre) {
                out.push(Action::Local(LocalAction::WarpCursor {
                    x: located.local.x,
                    y: located.local.y,
                }));
            }
        }
    }

    /// Put the cursor somewhere valid after the layout changed beneath it.
    fn resettle(&mut self, out: &mut Vec<Action>) {
        let Some((global, located)) = self.layout.snap(self.cursor) else {
            return;
        };
        if global == self.cursor && located.device == self.active {
            return;
        }
        self.cursor = global;
        if located.device != self.active {
            self.cross_to(located.device, global, located, out);
        }
    }

    /// Where a position in the server's own coordinates sits globally.
    fn local_to_global(&self, x: i32, y: i32) -> Option<Point> {
        let cells = self.layout.cells();
        let cell = cells
            .iter()
            .filter(|c| c.device == self.local)
            .find(|c| c.local.contains(Point::new(x, y)))?;
        Some(smkvm_layout::map_point(
            &cell.local,
            &cell.global,
            Point::new(x, y),
        ))
    }
}
