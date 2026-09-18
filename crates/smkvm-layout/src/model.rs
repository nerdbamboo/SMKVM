//! Devices, monitors, and where they sit on the global virtual desktop.
//!
//! Nothing here is specific to any particular set of machines. A layout holds
//! N devices, each reporting N monitors, placed anywhere. Adding, removing, or
//! replacing a computer is a data change, never a code change.
//!
//! Two identities matter and they are deliberately separate:
//!
//! * [`DeviceId`] is derived from the device's key material and never changes,
//!   so renaming a machine keeps its layout.
//! * [`MonitorId`] is stable per device (an X11 output name, a Windows display
//!   device path). Placements are keyed on the pair, so unplugging a monitor
//!   and plugging it back in restores where it was.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::geom::{Point, Rect};

/// Stable identity of a machine, derived from its device certificate.
///
/// Held as raw bytes but serialized as lowercase hex so config files stay
/// readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId([u8; 32]);

impl DeviceId {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            use fmt::Write as _;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    pub fn from_hex(s: &str) -> Result<Self, ParseDeviceIdError> {
        if s.len() != 64 {
            return Err(ParseDeviceIdError);
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| ParseDeviceIdError)?;
        }
        Ok(Self(out))
    }

    /// Short form for logs and UI. Not for equality checks.
    pub fn short(self) -> String {
        self.to_hex()[..12].to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseDeviceIdError;

impl fmt::Display for ParseDeviceIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("device id must be 64 hex characters")
    }
}

impl std::error::Error for ParseDeviceIdError {}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for DeviceId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for DeviceId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = DeviceId;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a 64-character hex device id")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<DeviceId, E> {
                DeviceId::from_hex(v).map_err(E::custom)
            }
        }
        d.deserialize_str(V)
    }
}

/// Stable identifier for one monitor attached to one device.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MonitorId(pub String);

impl MonitorId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MonitorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One monitor as the owning device reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub id: MonitorId,
    /// Position and size in the device's own OS coordinate space.
    pub local: Rect,
    /// The OS-reported scale factor. Advisory: it informs automatic placement
    /// and the editor, but cursor mapping is driven purely by rect sizes.
    #[serde(default = "one")]
    pub scale: f32,
    #[serde(default)]
    pub primary: bool,
    /// Human-readable name, e.g. a monitor model from EDID.
    ///
    /// Note for anyone adding fields here: this type crosses the wire in a
    /// non-self-describing encoding, so `skip_serializing_if` must not be used.
    /// Omitting a field on write while expecting it on read desynchronises the
    /// stream. `default` alone is fine, and is what lets older config files
    /// load.
    #[serde(default)]
    pub label: Option<String>,
}

fn one() -> f32 {
    1.0
}

impl Monitor {
    pub fn new(id: impl Into<String>, local: Rect) -> Self {
        Self {
            id: MonitorId::new(id),
            local,
            scale: 1.0,
            primary: false,
            label: None,
        }
    }
}

/// A computer participating in the layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: DeviceId,
    /// Display name. Free to change without disturbing placements.
    pub name: String,
    /// Monitors as last reported by the device.
    pub monitors: Vec<Monitor>,
    /// Whether the device currently has a live session. Offline devices keep
    /// their placements but never receive the cursor.
    #[serde(skip, default)]
    pub online: bool,
}

impl Device {
    pub fn new(id: DeviceId, name: impl Into<String>, monitors: Vec<Monitor>) -> Self {
        Self {
            id,
            name: name.into(),
            monitors,
            online: false,
        }
    }

    pub fn monitor(&self, id: &MonitorId) -> Option<&Monitor> {
        self.monitors.iter().find(|m| &m.id == id)
    }

    /// Bounding box of the device's monitors in its own coordinate space.
    pub fn local_bounds(&self) -> Rect {
        self.monitors
            .iter()
            .fold(Rect::new(0, 0, 0, 0), |acc, m| acc.union(&m.local))
    }
}

/// What to do when the cursor is driven into empty space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeOverflow {
    /// Slide onto the nearest monitor in the direction of travel, even when it
    /// does not line up with the exit point. Keeps every edge usable.
    #[default]
    Clamp,
    /// Stop at the edge. The cursor stays on the current monitor.
    Block,
}

/// A monitor that is both placed and online: the unit cursor resolution runs on.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub device: DeviceId,
    pub monitor: MonitorId,
    /// Position on the global virtual desktop.
    pub global: Rect,
    /// Position in the owning device's coordinate space.
    pub local: Rect,
}

/// Where a global point lands.
#[derive(Debug, Clone, PartialEq)]
pub struct Located {
    pub device: DeviceId,
    pub monitor: MonitorId,
    /// The point in the owning device's coordinate space.
    pub local: Point,
}

/// What changed when a device reported its monitors.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reconcile {
    pub added: Vec<MonitorId>,
    pub removed: Vec<MonitorId>,
    /// Monitors whose local geometry changed (resolution or arrangement).
    pub changed: Vec<MonitorId>,
}

impl Reconcile {
    pub fn is_noop(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// The full virtual desktop: which devices exist, and where their monitors sit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    #[serde(default)]
    pub edge_overflow: EdgeOverflow,
    #[serde(default)]
    devices: Vec<Device>,
    /// Global placement per `(device, monitor)`. Kept separate from `devices`
    /// so a machine can go offline, or drop and re-attach a monitor, without
    /// losing where the user put it.
    #[serde(default)]
    placements: BTreeMap<String, Rect>,
}

fn key(device: DeviceId, monitor: &MonitorId) -> String {
    format!("{}/{}", device.to_hex(), monitor.as_str())
}

impl Layout {
    pub fn new(edge_overflow: EdgeOverflow) -> Self {
        Self {
            edge_overflow,
            devices: Vec::new(),
            placements: BTreeMap::new(),
        }
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn device(&self, id: DeviceId) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn device_mut(&mut self, id: DeviceId) -> Option<&mut Device> {
        self.devices.iter_mut().find(|d| d.id == id)
    }

    pub fn set_online(&mut self, id: DeviceId, online: bool) {
        if let Some(d) = self.device_mut(id) {
            d.online = online;
        }
    }

    /// Record what a device says its monitors are, reporting what changed.
    ///
    /// This is the entry point for both first contact and later display
    /// changes. Existing placements survive; monitors that are new or whose
    /// geometry changed are left unplaced for the caller to position (see
    /// [`Layout::auto_place`]).
    pub fn report_monitors(
        &mut self,
        id: DeviceId,
        name: impl Into<String>,
        monitors: Vec<Monitor>,
    ) -> Reconcile {
        let name = name.into();
        let Some(idx) = self.devices.iter().position(|d| d.id == id) else {
            let added = monitors.iter().map(|m| m.id.clone()).collect();
            let mut device = Device::new(id, name, monitors);
            device.online = true;
            self.devices.push(device);
            return Reconcile {
                added,
                ..Default::default()
            };
        };

        let mut rec = Reconcile::default();
        let old = std::mem::take(&mut self.devices[idx].monitors);

        for m in &monitors {
            match old.iter().find(|o| o.id == m.id) {
                None => rec.added.push(m.id.clone()),
                Some(prev) if prev.local != m.local => {
                    rec.changed.push(m.id.clone());
                    // The monitor's size changed, so a placement sized to the
                    // old geometry no longer maps 1:1. Drop it and re-place.
                    if prev.local.w != m.local.w || prev.local.h != m.local.h {
                        self.placements.remove(&key(id, &m.id));
                    }
                }
                Some(_) => {}
            }
        }
        for o in &old {
            if !monitors.iter().any(|m| m.id == o.id) {
                rec.removed.push(o.id.clone());
                // Keep the placement: re-attaching the monitor restores it.
            }
        }

        self.devices[idx].name = name;
        self.devices[idx].monitors = monitors;
        self.devices[idx].online = true;
        rec
    }

    /// Forget a device entirely, including its placements.
    pub fn remove_device(&mut self, id: DeviceId) {
        self.devices.retain(|d| d.id != id);
        let prefix = format!("{}/", id.to_hex());
        self.placements.retain(|k, _| !k.starts_with(&prefix));
    }

    pub fn placement(&self, device: DeviceId, monitor: &MonitorId) -> Option<Rect> {
        self.placements.get(&key(device, monitor)).copied()
    }

    /// Put a monitor at `origin` on the global desktop, sized to its native
    /// pixels so the mapping stays an exact translation.
    pub fn place(&mut self, device: DeviceId, monitor: &MonitorId, origin: Point) -> bool {
        let Some(local) = self
            .device(device)
            .and_then(|d| d.monitor(monitor))
            .map(|m| m.local)
        else {
            return false;
        };
        self.placements.insert(
            key(device, monitor),
            Rect::new(origin.x, origin.y, local.w, local.h),
        );
        true
    }

    /// Place a monitor into an explicit global rect. A rect whose size differs
    /// from the monitor's native pixels scales the mapping proportionally.
    pub fn place_rect(&mut self, device: DeviceId, monitor: &MonitorId, global: Rect) -> bool {
        if global.is_empty() {
            return false;
        }
        if self
            .device(device)
            .and_then(|d| d.monitor(monitor))
            .is_none()
        {
            return false;
        }
        self.placements.insert(key(device, monitor), global);
        true
    }

    pub fn unplace(&mut self, device: DeviceId, monitor: &MonitorId) {
        self.placements.remove(&key(device, monitor));
    }

    /// Every monitor that exists but has not been given a spot.
    pub fn unplaced(&self) -> Vec<(DeviceId, MonitorId)> {
        let mut out = Vec::new();
        for d in &self.devices {
            for m in &d.monitors {
                if !self.placements.contains_key(&key(d.id, &m.id)) {
                    out.push((d.id, m.id.clone()));
                }
            }
        }
        out
    }

    /// Monitors that are placed and whose device is online.
    pub fn cells(&self) -> Vec<Cell> {
        let mut out = Vec::new();
        for d in &self.devices {
            if !d.online {
                continue;
            }
            for m in &d.monitors {
                if let Some(global) = self.placements.get(&key(d.id, &m.id)) {
                    if global.is_empty() || m.local.is_empty() {
                        continue;
                    }
                    out.push(Cell {
                        device: d.id,
                        monitor: m.id.clone(),
                        global: *global,
                        local: m.local,
                    });
                }
            }
        }
        out
    }

    /// Bounding box of all placements, online or not.
    pub fn bounds(&self) -> Rect {
        self.placements
            .values()
            .fold(Rect::new(0, 0, 0, 0), |acc, r| acc.union(r))
    }

    /// Give every unplaced monitor a reasonable spot so the system works
    /// without anyone opening the editor.
    ///
    /// A monitor joining a device that already has placements keeps its local
    /// offset relative to its siblings, so plugging a second screen in to the
    /// right of the first puts it to the right globally too. A device with no
    /// placements at all is appended as a block to the right of everything
    /// else, preserving its internal arrangement.
    pub fn auto_place(&mut self) {
        // Collected up front, but each origin is computed against the
        // placements made so far, so siblings placed in this pass can act as
        // anchors for the ones after them.
        for (device, monitor) in self.unplaced() {
            let origin = self.auto_origin(device, &monitor);
            self.place(device, &monitor, origin);
        }
    }

    fn auto_origin(&self, device: DeviceId, monitor: &MonitorId) -> Point {
        let Some(dev) = self.device(device) else {
            return Point::new(0, 0);
        };
        let Some(mon) = dev.monitor(monitor) else {
            return Point::new(0, 0);
        };

        // Anchor on a sibling that is already placed: reproduce this monitor's
        // local offset from that sibling in global space.
        let anchor = dev.monitors.iter().find_map(|sib| {
            self.placement(device, &sib.id)
                .map(|global| (sib.local, global))
        });
        if let Some((sib_local, sib_global)) = anchor {
            let candidate = Rect::new(
                sib_global.x + (mon.local.x - sib_local.x),
                sib_global.y + (mon.local.y - sib_local.y),
                mon.local.w,
                mon.local.h,
            );
            let clashes = self
                .placements
                .iter()
                .any(|(k, r)| *k != key(device, monitor) && r.overlaps(&candidate));
            if !clashes {
                return candidate.origin();
            }
        }

        // Otherwise append to the right of everything placed so far, keeping
        // this device's monitors in their local arrangement.
        let bounds = self.bounds();
        let base_x = if bounds.is_empty() { 0 } else { bounds.right() };
        let dev_local = dev.local_bounds();
        Point::new(
            base_x + (mon.local.x - dev_local.x),
            mon.local.y - dev_local.y,
        )
    }
}
