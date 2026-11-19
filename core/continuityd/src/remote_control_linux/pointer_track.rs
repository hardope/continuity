//! Where the pointer is thought to be while another computer shares its
//! keyboard and mouse with this one (an input-only session): the portal
//! gives no way to read the pointer back, so it's worked out from what's
//! injected. Arithmetic only, so it's tested anywhere.
//!
//! With the desktop's extent known (`Desktop`, from GDK), the controller
//! places the pointer (`MouseMove`, which the host carries out as a pin to
//! the top-left corner and a move from there) and from then on every move
//! is added up and kept on the desktop, as the compositor keeps the real
//! pointer — so the edges are exact, and so is where along one the pointer
//! leaves. Without it, the controller pins the pointer to an edge with one
//! move far longer than any screen (`PIN`), and only how far in from that
//! edge the moves since have taken it is known.
//!
//! Neither accounts for the person here moving the pointer meanwhile, nor
//! for monitors that don't line up into a rectangle; the escape chord on
//! the controlling computer is the sure way back.

use continuity_proto::ScreenEdge;

/// The bounding box of the monitors, in the compositor's logical pixels —
/// what the portal's relative moves are in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Desktop {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The bounding box of monitors given as `(x, y, width, height)`.
pub fn union(monitors: impl IntoIterator<Item = (i32, i32, i32, i32)>) -> Option<Desktop> {
    let mut extent: Option<(i32, i32, i32, i32)> = None;
    for (x, y, width, height) in monitors.into_iter().filter(|m| m.2 > 0 && m.3 > 0) {
        let (left, top, right, bottom) = extent.unwrap_or((x, y, x + width, y + height));
        extent = Some((left.min(x), top.min(y), right.max(x + width), bottom.max(y + height)));
    }
    extent.map(|(left, top, right, bottom)| Desktop { x: f64::from(left), y: f64::from(top), width: f64::from(right - left), height: f64::from(bottom - top) })
}

/// A relative move at least this long is a pin to an edge.
pub const PIN: f64 = 100_000.0;

/// How far a pin moves the pointer — further than any desktop is wide, so
/// it ends up against the edge wherever it was.
const PIN_MOVE: f64 = 1_000_000.0;

/// The relative moves that put the pointer at `(x, y)` (fractions of the
/// desktop) from wherever it is, there being no way to place it directly
/// without a screen stream: pressed against the bottom (or the right)
/// first, moved along to the point's height (or column), then pressed
/// against the edge nearest the point and moved in from it to the point.
/// Never by way of the top-left corner — GNOME's Activities hot corner,
/// which a pin into it would open — unless the point is there.
pub fn placing_moves(desktop: Desktop, x: f64, y: f64) -> Vec<(f64, f64)> {
    let (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
    let (right, bottom) = (desktop.width - 1.0, desktop.height - 1.0);
    let (to_x, to_y) = (x * right, y * bottom);
    let moves = match nearest_edge(x, y).0 {
        ScreenEdge::Left => [(0.0, PIN_MOVE), (0.0, to_y - bottom), (-PIN_MOVE, 0.0), (to_x, 0.0)],
        ScreenEdge::Right => [(0.0, PIN_MOVE), (0.0, to_y - bottom), (PIN_MOVE, 0.0), (to_x - right, 0.0)],
        ScreenEdge::Top => [(PIN_MOVE, 0.0), (to_x - right, 0.0), (0.0, -PIN_MOVE), (0.0, to_y)],
        ScreenEdge::Bottom => [(PIN_MOVE, 0.0), (to_x - right, 0.0), (0.0, PIN_MOVE), (0.0, to_y - bottom)],
    };
    moves.into_iter().filter(|&(dx, dy)| dx != 0.0 || dy != 0.0).collect()
}

#[derive(Default, Debug)]
pub struct PointerTrack {
    /// Where the pointer is, once placed, in the desktop's coordinates.
    at: Option<(f64, f64)>,
    /// With the desktop unknown: the edge a pin pressed the pointer
    /// against, and how far in from it the moves since have taken it.
    pinned: Option<(ScreenEdge, f64)>,
    /// The edge the pointer arrived at, and where along it — reported
    /// while input can't be injected yet, so the controller takes its
    /// cursor straight back rather than sending it into nothing.
    arrived: Option<(ScreenEdge, f64)>,
}

impl PointerTrack {
    /// The controller placed the pointer at `(x, y)` (0.0..=1.0 across the
    /// desktop each).
    pub fn placed(&mut self, desktop: Option<Desktop>, x: f64, y: f64) {
        let Some(desktop) = desktop else { return };
        let (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        self.at = Some((desktop.x + x * (desktop.width - 1.0), desktop.y + y * (desktop.height - 1.0)));
        self.pinned = None;
        self.arrived = Some(nearest_edge(x, y));
    }

    pub fn moved(&mut self, desktop: Option<Desktop>, dx: f64, dy: f64) {
        if let (Some(desktop), Some((x, y))) = (desktop, self.at) {
            // Kept on the desktop like the real pointer; a pin is just a
            // long move to an edge.
            let right = desktop.x + desktop.width - 1.0;
            let bottom = desktop.y + desktop.height - 1.0;
            self.at = Some(((x + dx).clamp(desktop.x, right), (y + dy).clamp(desktop.y, bottom)));
            return;
        }
        let pin = if dx <= -PIN {
            Some(ScreenEdge::Left)
        } else if dx >= PIN {
            Some(ScreenEdge::Right)
        } else if dy <= -PIN {
            Some(ScreenEdge::Top)
        } else if dy >= PIN {
            Some(ScreenEdge::Bottom)
        } else {
            None
        };
        if let Some(edge) = pin {
            self.pinned = Some((edge, 0.0));
            self.arrived = Some((edge, 0.5));
        } else if let Some((edge, inward)) = self.pinned {
            let step = match edge {
                ScreenEdge::Left => dx,
                ScreenEdge::Right => -dx,
                ScreenEdge::Top => dy,
                ScreenEdge::Bottom => -dy,
            };
            self.pinned = Some((edge, (inward + step).max(0.0)));
        }
    }

    /// The edge the pointer is pressed against right now, and how far
    /// along it (0.0 at the top or left), if any.
    pub fn at_edge(&self, desktop: Option<Desktop>) -> Option<(ScreenEdge, f64)> {
        if let (Some(desktop), Some((x, y))) = (desktop, self.at) {
            return edge_of(desktop, x, y);
        }
        self.pinned.filter(|(_, inward)| *inward <= 0.0).map(|(edge, _)| (edge, 0.5))
    }

    /// The edge the pointer arrived at, and where along it.
    pub fn arrived(&self) -> Option<(ScreenEdge, f64)> {
        self.arrived
    }
}

/// The edge nearest a point given as fractions of the desktop, and how far
/// along it the point is.
fn nearest_edge(x: f64, y: f64) -> (ScreenEdge, f64) {
    let candidates = [(x, ScreenEdge::Left, y), (1.0 - x, ScreenEdge::Right, y), (y, ScreenEdge::Top, x), (1.0 - y, ScreenEdge::Bottom, x)];
    let (_, edge, along) = candidates.into_iter().fold(candidates[0], |best, next| if next.0 < best.0 { next } else { best });
    (edge, along)
}

/// Which edge of `desktop` the point is pressed against (within a pixel),
/// and how far along it.
fn edge_of(desktop: Desktop, x: f64, y: f64) -> Option<(ScreenEdge, f64)> {
    if desktop.width <= 1.0 || desktop.height <= 1.0 {
        return None;
    }
    let (left, top) = (desktop.x, desktop.y);
    let (right, bottom) = (left + desktop.width - 1.0, top + desktop.height - 1.0);
    let along_y = ((y - top) / (bottom - top)).clamp(0.0, 1.0);
    let along_x = ((x - left) / (right - left)).clamp(0.0, 1.0);
    if x <= left + 1.0 {
        Some((ScreenEdge::Left, along_y))
    } else if x >= right - 1.0 {
        Some((ScreenEdge::Right, along_y))
    } else if y <= top + 1.0 {
        Some((ScreenEdge::Top, along_x))
    } else if y >= bottom - 1.0 {
        Some((ScreenEdge::Bottom, along_x))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESKTOP: Desktop = Desktop { x: 0.0, y: 0.0, width: 1921.0, height: 1081.0 };

    #[test]
    fn the_desktop_is_the_monitors_bounding_box() {
        assert_eq!(union([(0, 0, 1920, 1080), (1920, -200, 1280, 1024)]), Some(Desktop { x: 0.0, y: -200.0, width: 3200.0, height: 1280.0 }));
        assert_eq!(union([(0, 0, 0, 0)]), None, "a monitor with no size isn't one");
        assert_eq!(union([]), None);
    }

    #[test]
    fn a_placed_pointer_is_followed_exactly_and_kept_on_the_desktop() {
        let mut track = PointerTrack::default();
        // Arrives 4 px in from the left edge, a third of the way down.
        track.placed(Some(DESKTOP), 4.0 / 1920.0, 1.0 / 3.0);
        assert_eq!(track.at_edge(Some(DESKTOP)), None, "a few pixels in isn't at the edge");
        assert_eq!(track.arrived(), Some((ScreenEdge::Left, 1.0 / 3.0)));
        track.moved(Some(DESKTOP), 500.0, 0.0);
        track.moved(Some(DESKTOP), 5000.0, 0.0);
        assert_eq!(track.at_edge(Some(DESKTOP)).map(|(edge, _)| edge), Some(ScreenEdge::Right), "pushed past the right edge, it stops there");
        // ...and coming back takes only the desktop's width, not 5,500.
        track.moved(Some(DESKTOP), -1900.0, 0.0);
        assert_eq!(track.at_edge(Some(DESKTOP)), None);
        track.moved(Some(DESKTOP), -30.0, 0.0);
        assert_eq!(track.at_edge(Some(DESKTOP)), Some((ScreenEdge::Left, 1.0 / 3.0)));
        track.moved(Some(DESKTOP), 0.0, 1080.0 / 6.0);
        assert_eq!(track.at_edge(Some(DESKTOP)), Some((ScreenEdge::Left, 0.5)), "along the edge: where it reaches it");
    }

    #[test]
    fn without_the_desktop_only_the_pinned_edge_is_known() {
        let mut track = PointerTrack::default();
        track.moved(None, 0.0, -PIN);
        track.moved(None, -PIN, 0.0);
        assert_eq!(track.at_edge(None), Some((ScreenEdge::Left, 0.5)), "pinned there");
        assert_eq!(track.arrived(), Some((ScreenEdge::Left, 0.5)));
        track.moved(None, 300.0, 0.0);
        assert_eq!(track.at_edge(None), None);
        track.moved(None, 0.0, 40.0);
        assert_eq!(track.at_edge(None), None, "along the edge doesn't bring it back");
        track.moved(None, -300.0, 0.0);
        assert_eq!(track.at_edge(None), Some((ScreenEdge::Left, 0.5)));
    }

    #[test]
    fn nothing_is_known_before_anything_was_injected() {
        let track = PointerTrack::default();
        assert_eq!(track.at_edge(Some(DESKTOP)), None);
        assert_eq!(track.arrived(), None);
    }

    /// Where `moves` take a pointer starting at `(x, y)`, kept on the
    /// desktop as the compositor keeps it — and every point it stops at.
    fn walk(desktop: Desktop, start: (f64, f64), moves: &[(f64, f64)]) -> Vec<(f64, f64)> {
        let mut at = start;
        let mut stops = Vec::new();
        for &(dx, dy) in moves {
            at = ((at.0 + dx).clamp(0.0, desktop.width - 1.0), (at.1 + dy).clamp(0.0, desktop.height - 1.0));
            stops.push(at);
        }
        stops
    }

    #[test]
    fn placing_the_pointer_lands_on_the_point_from_anywhere() {
        for (x, y) in [(4.0 / 1920.0, 0.25), (1.0 - 4.0 / 1920.0, 0.9), (0.5, 4.0 / 1080.0), (0.3, 1.0 - 4.0 / 1080.0)] {
            for start in [(0.0, 0.0), (1920.0, 1080.0), (700.0, 3.0), (1919.0, 500.0)] {
                let stops = walk(DESKTOP, start, &placing_moves(DESKTOP, x, y));
                let end = *stops.last().unwrap();
                assert!((end.0 - x * 1920.0).abs() < 1e-6 && (end.1 - y * 1080.0).abs() < 1e-6, "({x}, {y}) from {start:?}: {end:?}");
                // Through the top-left corner only when starting there.
                if start != (0.0, 0.0) {
                    assert!(!stops.contains(&(0.0, 0.0)), "({x}, {y}) from {start:?} went by the hot corner: {stops:?}");
                }
            }
        }
    }

    #[test]
    fn the_nearest_edge_of_a_point() {
        assert_eq!(nearest_edge(0.99, 0.5), (ScreenEdge::Right, 0.5));
        assert_eq!(nearest_edge(0.5, 0.0), (ScreenEdge::Top, 0.5));
        assert_eq!(nearest_edge(0.3, 0.98), (ScreenEdge::Bottom, 0.3));
    }
}
