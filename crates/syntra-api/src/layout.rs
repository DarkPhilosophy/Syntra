//! Screen layout: every device's monitors placed on one shared plane, and the
//! crossings that follow from where they touch.
//!
//! The user edits placements; crossings are derived, never stored. Two
//! monitors of different devices that share part of an edge form a crossing
//! in each direction, and a pointer leaving one at a given height enters the
//! other at the same height on the plane. A monitor beside a taller one
//! therefore only covers the part of the edge it actually faces.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Edges closer than this many pixels count as touching, so a placement off
/// by a rounding pixel still connects.
pub const TOUCH_TOLERANCE: i64 = 2;

/// One monitor of one device, placed on the shared plane in pixels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monitor {
    /// The device it belongs to (its certificate fingerprint or local id).
    pub device: String,
    /// Stable per-device monitor identifier (connector name, index...).
    pub id: String,
    /// Left edge on the plane.
    pub x: i64,
    /// Top edge on the plane.
    pub y: i64,
    /// Width in pixels, at least one.
    pub width: u32,
    /// Height in pixels, at least one.
    pub height: u32,
}

impl Monitor {
    fn right(&self) -> i64 {
        self.x + i64::from(self.width)
    }
    fn bottom(&self) -> i64 {
        self.y + i64::from(self.height)
    }
}

/// Which side of a monitor the pointer leaves through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    /// The left side.
    Left,
    /// The right side.
    Right,
    /// The top side.
    Top,
    /// The bottom side.
    Bottom,
}

impl Edge {
    /// The side the pointer arrives through on the monitor it enters.
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }
}

/// A monitor named by device and id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MonitorRef {
    /// Owning device.
    pub device: String,
    /// Monitor id within that device.
    pub id: String,
}

impl From<&Monitor> for MonitorRef {
    fn from(monitor: &Monitor) -> Self {
        Self {
            device: monitor.device.clone(),
            id: monitor.id.clone(),
        }
    }
}

/// Leaving `from` through `edge` within `from_span` enters `to` within
/// `to_span`. Spans are fractions `[start, end]` along the edge, measured
/// from the top (vertical edges) or the left (horizontal edges).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crossing {
    /// Monitor the pointer leaves.
    pub from: MonitorRef,
    /// Its edge the pointer leaves through.
    pub edge: Edge,
    /// Part of that edge that faces `to`.
    pub from_span: (f64, f64),
    /// Monitor the pointer enters, through `edge.opposite()`.
    pub to: MonitorRef,
    /// Part of the entered edge that faces `from`.
    pub to_span: (f64, f64),
}

impl Crossing {
    /// Where on the entered edge a pointer leaving at `along` arrives, or
    /// `None` when `along` is outside the part of the edge facing `to`.
    pub fn map(&self, along: f64) -> Option<f64> {
        let (start, end) = self.from_span;
        if !(start..=end).contains(&along) || end <= start {
            return None;
        }
        let t = (along - start) / (end - start);
        let (to_start, to_end) = self.to_span;
        Some(to_start + t * (to_end - to_start))
    }
}

/// Every crossing between monitors of different devices.
pub fn crossings(monitors: &[Monitor]) -> Vec<Crossing> {
    let mut found = Vec::new();
    for (index, a) in monitors.iter().enumerate() {
        for b in &monitors[index + 1..] {
            if a.device == b.device {
                continue;
            }
            for (from, to) in [(a, b), (b, a)] {
                found.extend(crossing(from, to));
            }
        }
    }
    found
}

/// The crossing from `from` into `to`, if they share part of an edge.
fn crossing(from: &Monitor, to: &Monitor) -> Option<Crossing> {
    let touches = |a: i64, b: i64| (a - b).abs() <= TOUCH_TOLERANCE;
    let edge = if touches(from.right(), to.x) {
        Edge::Right
    } else if touches(from.x, to.right()) {
        Edge::Left
    } else if touches(from.bottom(), to.y) {
        Edge::Bottom
    } else if touches(from.y, to.bottom()) {
        Edge::Top
    } else {
        return None;
    };
    let ((from_start, from_len), (to_start, to_len)) = match edge {
        Edge::Left | Edge::Right => (
            (from.y, i64::from(from.height)),
            (to.y, i64::from(to.height)),
        ),
        Edge::Top | Edge::Bottom => ((from.x, i64::from(from.width)), (to.x, i64::from(to.width))),
    };
    let start = from_start.max(to_start);
    let end = (from_start + from_len).min(to_start + to_len);
    if end <= start {
        // Corners touching, or edges on the same line without overlap.
        return None;
    }
    let fraction = |at: i64, origin: i64, len: i64| (at - origin) as f64 / len as f64;
    Some(Crossing {
        from: from.into(),
        edge,
        from_span: (
            fraction(start, from_start, from_len),
            fraction(end, from_start, from_len),
        ),
        to: to.into(),
        to_span: (
            fraction(start, to_start, to_len),
            fraction(end, to_start, to_len),
        ),
    })
}

/// Why a layout cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum LayoutError {
    /// A monitor has no area.
    #[error("monitor {0:?} has no area")]
    Empty(MonitorRef),
    /// Two monitors cover the same place; the pointer could be on both.
    #[error("monitors {0:?} and {1:?} overlap")]
    Overlap(MonitorRef, MonitorRef),
    /// A device touches no other device, so the pointer can never reach it.
    #[error("device {0} touches no other device")]
    Detached(String),
}

/// Checks a layout: monitors have area, none overlap, and with more than one
/// device every device touches another.
pub fn validate(monitors: &[Monitor]) -> Result<(), LayoutError> {
    for monitor in monitors {
        if monitor.width == 0 || monitor.height == 0 {
            return Err(LayoutError::Empty(monitor.into()));
        }
    }
    for (index, a) in monitors.iter().enumerate() {
        for b in &monitors[index + 1..] {
            let overlap_x = a.right().min(b.right()) - a.x.max(b.x);
            let overlap_y = a.bottom().min(b.bottom()) - a.y.max(b.y);
            // A tolerance-sized sliver is how touching edges look once rounded.
            if overlap_x > TOUCH_TOLERANCE && overlap_y > TOUCH_TOLERANCE {
                return Err(LayoutError::Overlap(a.into(), b.into()));
            }
        }
    }
    let mut devices: Vec<&str> = monitors.iter().map(|m| m.device.as_str()).collect();
    devices.sort_unstable();
    devices.dedup();
    if devices.len() > 1 {
        let linked = crossings(monitors);
        if let Some(lonely) = devices
            .iter()
            .find(|device| !linked.iter().any(|c| c.from.device == **device))
        {
            return Err(LayoutError::Detached((*lonely).to_owned()));
        }
    }
    Ok(())
}

/// Where to put `moving` (wanted at `x`, `y`) so an edge lines up with a
/// monitor of another device when it is within `threshold` pixels: sides
/// meet sides and tops or bottoms align, each axis on its own.
pub fn snap(moving: &Monitor, x: i64, y: i64, others: &[Monitor], threshold: i64) -> (i64, i64) {
    let width = i64::from(moving.width);
    let height = i64::from(moving.height);
    let mut best_x: Option<(i64, i64)> = None;
    let mut best_y: Option<(i64, i64)> = None;
    let consider = |best: &mut Option<(i64, i64)>, wanted: i64, candidate: i64| {
        let distance = (candidate - wanted).abs();
        if distance <= threshold && best.is_none_or(|(_, d)| distance < d) {
            *best = Some((candidate, distance));
        }
    };
    for other in others.iter().filter(|m| m.device != moving.device) {
        for candidate in [
            other.right(),
            other.x - width,
            other.x,
            other.right() - width,
        ] {
            consider(&mut best_x, x, candidate);
        }
        for candidate in [
            other.bottom(),
            other.y - height,
            other.y,
            other.bottom() - height,
        ] {
            consider(&mut best_y, y, candidate);
        }
    }
    (
        best_x.map_or(x, |(at, _)| at),
        best_y.map_or(y, |(at, _)| at),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(device: &str, x: i64, y: i64, width: u32, height: u32) -> Monitor {
        Monitor {
            device: device.into(),
            id: "0".into(),
            x,
            y,
            width,
            height,
        }
    }

    /// A smaller screen placed lower on the left only covers the part of the
    /// edge it faces, and heights map through the shared plane both ways.
    #[test]
    fn offset_neighbours_cross_where_they_face_each_other() {
        let msi = monitor("msi", 0, 0, 2560, 1600);
        let hp = monitor("hp", -1920, 200, 1920, 1080);
        let all = crossings(&[msi, hp]);
        assert_eq!(all.len(), 2);
        let out = all.iter().find(|c| c.from.device == "msi").unwrap();
        assert_eq!(out.edge, Edge::Left);
        assert_eq!(out.from_span, (0.125, 0.8));
        assert_eq!(out.to_span, (0.0, 1.0));
        // Half way down the MSI screen is 800 px on the plane, 600 px into HP.
        let entered = out.map(0.5).unwrap();
        assert!((entered - 600.0 / 1080.0).abs() < 1e-9);
        // Above the HP screen the MSI edge leads nowhere.
        assert_eq!(out.map(0.05), None);
        let back = all.iter().find(|c| c.from.device == "hp").unwrap();
        assert_eq!(back.edge, Edge::Right);
        assert!((back.map(entered).unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_rounding_gap_still_touches_but_corners_do_not() {
        let a = monitor("a", 0, 0, 100, 100);
        assert_eq!(
            crossings(&[a.clone(), monitor("b", 102, 0, 100, 100)]).len(),
            2
        );
        assert!(crossings(&[a.clone(), monitor("b", 103, 0, 100, 100)]).is_empty());
        assert!(
            crossings(&[a, monitor("b", 100, 100, 100, 100)]).is_empty(),
            "corner"
        );
    }

    #[test]
    fn monitors_of_one_device_never_cross_to_each_other() {
        let left = monitor("msi", 0, 0, 100, 100);
        let mut right = monitor("msi", 100, 0, 100, 100);
        right.id = "1".into();
        assert!(crossings(&[left, right]).is_empty());
    }

    #[test]
    fn vertical_neighbours_cross_through_top_and_bottom() {
        let desk = monitor("desk", 0, 0, 1000, 500);
        let laptop = monitor("laptop", 250, 500, 500, 300);
        let down = crossings(&[desk, laptop])
            .into_iter()
            .find(|c| c.from.device == "desk")
            .unwrap();
        assert_eq!(down.edge, Edge::Bottom);
        assert_eq!(down.from_span, (0.25, 0.75));
        assert_eq!(down.map(0.5), Some(0.5));
    }

    #[test]
    fn validation_refuses_overlaps_and_unreachable_devices() {
        let a = monitor("a", 0, 0, 100, 100);
        assert!(matches!(
            validate(&[a.clone(), monitor("b", 50, 50, 100, 100)]),
            Err(LayoutError::Overlap(..))
        ));
        assert_eq!(
            validate(&[a.clone(), monitor("b", 500, 0, 100, 100)]),
            Err(LayoutError::Detached("a".into()))
        );
        assert!(matches!(
            validate(&[monitor("a", 0, 0, 0, 100)]),
            Err(LayoutError::Empty(_))
        ));
        assert_eq!(validate(&[a, monitor("b", 101, 30, 100, 100)]), Ok(()));
    }

    #[test]
    fn dragging_near_an_edge_snaps_to_it() {
        let fixed = monitor("msi", 0, 0, 2560, 1600);
        let moving = monitor("hp", 0, 0, 1920, 1080);
        // Dropped 6 px short of touching and 4 px below the top: both snap.
        assert_eq!(
            snap(&moving, -1926, 4, std::slice::from_ref(&fixed), 10),
            (-1920, 0)
        );
        // Far from everything: left where it was dropped.
        assert_eq!(snap(&moving, -2500, 300, &[fixed], 10), (-2500, 300));
    }
}
