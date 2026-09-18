//! An injector that records instead of injecting.
//!
//! Lets the client and server be driven end to end in tests, on a machine with
//! no display server, and makes the exact sequence of presses and releases
//! something a test can assert on rather than something a human has to watch
//! for.

use std::collections::BTreeSet;

use smkvm_layout::{Monitor, Rect};
use smkvm_proto::{Key, MouseButton, Scroll};

use crate::{Inject, InputError, Monitors, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    MoveTo { x: i32, y: i32 },
    Button { button: MouseButton, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { key: Key, down: bool },
    Flush,
}

/// Records what it is asked to do.
#[derive(Debug, Default)]
pub struct Loopback {
    events: Vec<Event>,
    monitors: Vec<Monitor>,
    /// Keys whose injection fails, so error handling can be exercised.
    failing: BTreeSet<Key>,
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
}

impl Inject for Loopback {
    fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
        self.events.push(Event::MoveTo { x, y });
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
}

impl Monitors for Loopback {
    fn monitors(&mut self) -> Result<Vec<Monitor>> {
        Ok(self.monitors.clone())
    }
}
