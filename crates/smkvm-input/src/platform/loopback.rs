//! An injector that records instead of injecting.
//!
//! Lets the client and server be driven end to end in tests, on a machine with
//! no display server, and makes the exact sequence of presses and releases
//! something a test can assert on rather than something a human has to watch
//! for.

use std::collections::BTreeSet;

use smkvm_layout::{Monitor, Rect};
use smkvm_proto::{Key, MouseButton, Scroll};

use crate::{Inject, InputError, Monitors, Parked, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    MoveTo { x: i32, y: i32 },
    Button { button: MouseButton, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { key: Key, down: bool },
    Flush,
}

/// Records what it is asked to do.
///
/// It also stands in for a backend that cannot hide the pointer and can only
/// move it out of the way, which is what Windows is. That behaviour is the one
/// part of injection with no second chance -- nothing arrives later to correct
/// a pointer left in a corner -- so it is worth being able to exercise here
/// rather than only on the machine it happens on.
#[derive(Debug, Default)]
pub struct Loopback {
    events: Vec<Event>,
    monitors: Vec<Monitor>,
    /// Keys whose injection fails, so error handling can be exercised.
    failing: BTreeSet<Key>,
    /// Where the pointer has been put. A recording backend has no pointer of
    /// its own, but parking one needs somewhere to put it back.
    at: (i32, i32),
    parked: Parked,
}

impl Loopback {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_monitors(monitors: Vec<Monitor>) -> Self {
        Self {
            monitors,
            ..Self::default()
        }
    }

    /// One 1920x1080 monitor, for tests that do not care about geometry.
    pub fn single_screen() -> Self {
        Self::with_monitors(vec![Monitor::new(
            "loopback-0",
            Rect::new(0, 0, 1920, 1080),
        )])
    }

    /// Make injecting this key fail from now on.
    pub fn fail_key(&mut self, key: Key) {
        self.failing.insert(key);
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// The recorded events with flushes removed, which is usually what a test
    /// is actually asserting about.
    pub fn actions(&self) -> Vec<Event> {
        self.events
            .iter()
            .filter(|e| **e != Event::Flush)
            .cloned()
            .collect()
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Where the pointer has been put.
    pub fn pointer(&self) -> (i32, i32) {
        self.at
    }

    /// Whether the pointer is currently out of the way.
    pub fn is_parked(&self) -> bool {
        self.parked.is_parked()
    }

    /// The far corner of everything this machine can display, which is where a
    /// parked pointer goes.
    fn corner(&self) -> Option<(i32, i32)> {
        let bounds = self
            .monitors
            .iter()
            .map(|m| m.local)
            .reduce(|acc, r| acc.union(&r))?;
        if bounds.is_empty() {
            return None;
        }
        Some((bounds.right() - 1, bounds.bottom() - 1))
    }

    /// Put the pointer somewhere without touching what is owed back.
    fn place(&mut self, x: i32, y: i32) {
        self.events.push(Event::MoveTo { x, y });
        self.at = (x, y);
    }
}

impl Inject for Loopback {
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        self.place(x, y);
        self.parked.placed();
        Ok(())
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<()> {
        self.events.push(Event::Button { button, down });
        Ok(())
    }

    fn wheel(&mut self, scroll: Scroll) -> Result<()> {
        self.events.push(Event::Wheel {
            dx: scroll.dx,
            dy: scroll.dy,
        });
        Ok(())
    }

    fn key(&mut self, key: Key, down: bool) -> Result<()> {
        if self.failing.contains(&key) {
            return Err(InputError::UnmappedKey(key.0));
        }
        self.events.push(Event::Key { key, down });
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.events.push(Event::Flush);
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<()> {
        let Some(corner) = self.corner() else {
            return Ok(());
        };
        if !self.parked.park(self.at) {
            return Ok(());
        }
        self.place(corner.0, corner.1);
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<()> {
        let Some((x, y)) = self.parked.restore() else {
            return Ok(());
        };
        self.place(x, y);
        Ok(())
    }
}

impl Monitors for Loopback {
    fn monitors(&mut self) -> Result<Vec<Monitor>> {
        Ok(self.monitors.clone())
    }
}
