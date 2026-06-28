//! **Anchored / floating-origin galaxy coordinates** — the precision foundation for a 1:1-scale galaxy.
//!
//! A flat coordinate cannot span a galaxy. A real galaxy is ~1e21 m across; an `f32` has ~7 significant
//! digits (hopeless past a few km from the origin) and even an `f64`'s ~15-16 digits resolve to ~1 km at
//! galactic radius — useless for atmospheric flight. So a single absolute number is physically incapable
//! of holding sub-metre precision *and* galaxy-spanning range at once (~21 orders of magnitude).
//!
//! The fix is **anchored** coordinates: a position is a coarse integer **[`Anchor`]** (which cell of a
//! galaxy-scale grid you are in, `i64` so the grid spans ~8e22 m — far beyond any galaxy) plus a small
//! **local** `f32` offset within that anchor's span. Precision is spent *where you are*, never wasted on
//! absolute galactic offset — a ship a hundred thousand light-years out still has the same sub-millimetre
//! local precision as one at the origin. This is the classic "64-bit grid + local float" / floating-origin
//! technique, made a first-class type.
//!
//! ## The one invariant that makes it safe in a distributed deterministic sim
//!
//! Two honest hosts may anchor the *same* physical point at *different* origins (a ship near a sector seam
//! is `(sector=(1,0), local=(0,y))` to one host and `(sector=(0,0), local=(9000,y))` to its neighbour).
//! The simulation's canonical, hashable form of a position **must be identical regardless of which origin
//! a host chose** — otherwise floating origin would manufacture false replica disagreements. That is
//! [`GalaxyPos::fixed8`]: it [`normalize`](GalaxyPos::normalize)s to the unique canonical `(anchor, local)`
//! decomposition (local forced into `[0, ANCHOR_SPAN)`, so the anchor is determined by the point alone)
//! and quantises the local to 1/8 unit. Same physical point ⇒ same `fixed8`, whatever origin produced it.
//!
//! Note the canonical key is **not** collapsed to one global integer: at galactic scale 1/8-unit fixed
//! point overflows `i64` (1e21 m → 8e21 eighths > 9.2e18). Keeping it as `(i64 anchor, i32 local-eighths)`
//! both avoids overflow and *is* the origin-invariance guarantee.
//!
//! Pure, `Copy`, and unit-tested. Pairs with [`crate::sim::Sim::state_hash`] (which folds `fixed8`) and the
//! seamless transit re-anchoring in [`crate::sim::Sim::tick`].

use serde::{Deserialize, Serialize};

use crate::shard::SectorId;

/// Side length, in world units (~metres), of one [`Anchor`] cell. Equal to [`crate::sim::SECTOR_SIZE`] so
/// an anchor *is* a galaxy-scale generalisation of the sector grid: `Anchor{ax,ay}` is the same patch of
/// space as `SectorId{sx:ax, sy:ay}`, just addressed with `i64` so it reaches the whole galaxy.
pub const ANCHOR_SPAN: f32 = 9000.0;

/// Sub-unit resolution of the canonical key — 1/8 world unit, matching the quantisation the authoritative
/// [`crate::sim::Sim::state_hash`] has always used, so honest replicas agree despite sub-unit float noise.
pub const QUANT: i64 = 8;

/// `ANCHOR_SPAN` measured in canonical 1/8-unit eighths. `ANCHOR_SPAN` is an exact integer so this is exact.
pub const SPAN8: i64 = ANCHOR_SPAN as i64 * QUANT;

/// A coarse, galaxy-scale grid cell — the *anchor* (origin) a [`GalaxyPos`]'s local offset is measured from.
/// `i64` axes span `±9.2e18` cells × `ANCHOR_SPAN` ≈ `±8.3e22` m, comfortably past a ~1e21 m galaxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Anchor {
    pub ax: i64,
    pub ay: i64,
}

impl Anchor {
    pub const ORIGIN: Anchor = Anchor { ax: 0, ay: 0 };

    pub fn new(ax: i64, ay: i64) -> Self {
        Anchor { ax, ay }
    }

    /// The galaxy-scale anchor for a legacy [`SectorId`]. The sector grid is the `i32` sub-range of the
    /// anchor grid, so this is exact and lossless.
    pub fn from_sector(s: SectorId) -> Anchor {
        Anchor { ax: s.sx as i64, ay: s.sy as i64 }
    }

    /// The legacy `i32` [`SectorId`] for this anchor. **Lossy** beyond the `i32` range (`±2.1e9` cells,
    /// ~`±1.9e13` m) — fine for today's sector grid, saturating past it until the sim's anchor is widened.
    pub fn to_sector(self) -> SectorId {
        SectorId::new(
            self.ax.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            self.ay.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        )
    }

    /// Shift the anchor by an integer number of cells (the seam crossing a transit performs).
    pub fn offset(self, dx: i64, dy: i64) -> Anchor {
        Anchor { ax: self.ax + dx, ay: self.ay + dy }
    }

    /// The anchor's world-space origin as `f64`. **Coarse / lossy** at galactic extremes (that is the whole
    /// reason anchored coordinates exist) — use it for maps and debugging, never for canonical state.
    pub fn origin_f64(self) -> (f64, f64) {
        (self.ax as f64 * ANCHOR_SPAN as f64, self.ay as f64 * ANCHOR_SPAN as f64)
    }
}

/// A position: a coarse [`Anchor`] plus a small local `f32` offset within that anchor's span. The same
/// physical point has many `(anchor, local)` spellings; [`normalize`](Self::normalize) /
/// [`fixed8`](Self::fixed8) reduce them all to one canonical form.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GalaxyPos {
    pub anchor: Anchor,
    pub x: f32,
    pub y: f32,
}

impl GalaxyPos {
    pub fn new(anchor: Anchor, x: f32, y: f32) -> Self {
        GalaxyPos { anchor, x, y }
    }

    /// A position at an anchor's origin.
    pub fn at(anchor: Anchor) -> Self {
        GalaxyPos { anchor, x: 0.0, y: 0.0 }
    }

    /// The unique **canonical decomposition** of this point: the local offset carried into `[0, ANCHOR_SPAN)`
    /// on each axis, with the integer carry folded into the anchor. Because local is forced into one span,
    /// the anchor is determined by the point alone — so any two spellings of the same point normalize equal.
    ///
    /// Precision note: this is exact when local is within a few spans of zero (floating origin keeps it so).
    /// A pathologically huge local (millions of spans) loses `f32` precision *before* this is ever called —
    /// the contract is "keep your local small", which the sim does by re-anchoring on transit.
    pub fn normalize(self) -> GalaxyPos {
        let span = ANCHOR_SPAN;
        let cx = (self.x / span).floor();
        let cy = (self.y / span).floor();
        GalaxyPos {
            anchor: self.anchor.offset(cx as i64, cy as i64),
            x: self.x - cx * span,
            y: self.y - cy * span,
        }
    }

    /// The **origin-invariant canonical key**: `(anchor.ax, anchor.ay, local_x_eighths, local_y_eighths)`
    /// after normalisation, with each local quantised to 1/8 unit and held in `[0, SPAN8)`. This is the
    /// authoritative fingerprint of a position — equal for the same physical point regardless of which
    /// origin produced it, and free of the `i64` overflow a single global fixed-point key would hit at
    /// galactic scale. [`crate::sim::Sim::state_hash`] folds exactly this.
    pub fn fixed8(self) -> (i64, i64, i32, i32) {
        let n = self.normalize();
        let mut ax = n.anchor.ax;
        let mut ay = n.anchor.ay;
        let mut lx = (n.x * QUANT as f32).round() as i64;
        let mut ly = (n.y * QUANT as f32).round() as i64;
        // Rounding can push a local that sits a hair under the span up to exactly SPAN8 — carry it.
        if lx >= SPAN8 {
            lx -= SPAN8;
            ax += 1;
        }
        if ly >= SPAN8 {
            ly -= SPAN8;
            ay += 1;
        }
        if lx < 0 {
            lx += SPAN8;
            ax -= 1;
        }
        if ly < 0 {
            ly += SPAN8;
            ay -= 1;
        }
        (ax, ay, lx as i32, ly as i32)
    }

    /// This point re-expressed as a local `(x, y)` offset relative to a different `frame` anchor. Exact for
    /// nearby frames (the common case — you read the world in your own host's frame); the integer-cell
    /// difference is computed in `f64` then narrowed, so it only loses precision once `frame` is galactically
    /// far from the point (where an `f32` local is meaningless anyway).
    pub fn to_frame(self, frame: Anchor) -> (f32, f32) {
        let dx = (self.anchor.ax - frame.ax) as f64 * ANCHOR_SPAN as f64 + self.x as f64;
        let dy = (self.anchor.ay - frame.ay) as f64 * ANCHOR_SPAN as f64 + self.y as f64;
        (dx as f32, dy as f32)
    }

    /// The relative vector from `other` to `self` (`self - other`), in world units. Valid (sub-unit precise)
    /// for points in the same neighbourhood — which is the only case a relative vector is meaningful.
    pub fn delta(self, other: GalaxyPos) -> (f32, f32) {
        let (sx, sy) = self.to_frame(other.anchor);
        (sx - other.x, sy - other.y)
    }

    /// Coarse `f64` global position. **Lossy** past ~`±1.1e15` units (`f64` ⁄ 8) — debug/maps only.
    pub fn global_f64(self) -> (f64, f64) {
        let (ox, oy) = self.anchor.origin_f64();
        (ox + self.x as f64, oy + self.y as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ANCHOR_SPAN` must be an exact integer (so `SPAN8` is exact) and must match the sim's sector size,
    /// or the anchor grid and the sector grid would silently disagree.
    #[test]
    fn span_constants_are_consistent() {
        assert_eq!(ANCHOR_SPAN.fract(), 0.0, "ANCHOR_SPAN must be integral for exact fixed-point");
        assert_eq!(SPAN8, 72_000);
        assert_eq!(ANCHOR_SPAN, crate::sim::SECTOR_SIZE, "anchor grid must equal the sector grid");
    }

    /// Normalising forces local into `[0, ANCHOR_SPAN)` while keeping the same physical point.
    #[test]
    fn normalize_keeps_point_and_bounds_local() {
        let p = GalaxyPos::new(Anchor::new(3, -2), 9000.0 + 12.5, -7.5);
        let n = p.normalize();
        assert!(n.x >= 0.0 && n.x < ANCHOR_SPAN, "x in [0,span): {}", n.x);
        assert!(n.y >= 0.0 && n.y < ANCHOR_SPAN, "y in [0,span): {}", n.y);
        // Same physical point: anchor absorbed the carry.
        assert_eq!(n.anchor, Anchor::new(4, -3));
        let (gx, gy) = n.global_f64();
        let (px, py) = p.global_f64();
        assert!((gx - px).abs() < 1e-3 && (gy - py).abs() < 1e-3);
    }

    /// THE core property. The same physical point, spelled at different anchors, has one canonical key.
    #[test]
    fn fixed8_is_origin_invariant() {
        // Point P, expressed three ways.
        let a = GalaxyPos::new(Anchor::new(1, 0), 0.0, 1500.0); // sector (1,0), at its west edge
        let b = GalaxyPos::new(Anchor::new(0, 0), 9000.0, 1500.0); // sector (0,0), at its east edge
        let c = GalaxyPos::new(Anchor::new(2, 0), -9000.0, 1500.0); // sector (2,0), two spans back
        assert_eq!(a.fixed8(), b.fixed8());
        assert_eq!(a.fixed8(), c.fixed8());

        // And it survives a whole grid of points re-anchored within the floating-origin regime. The
        // shifts are small (a few cells) on purpose: invariance holds for *nearby* re-anchorings — the
        // transit seam moves ±1 cell, domain rebasing keeps you local. Re-anchoring a point thousands of
        // cells away while keeping it as an `f32` local would push the local past `f32`'s exact-integer
        // range (~16M) and lose the 1/8 unit — which is precisely the regime anchored coordinates forbid
        // ("keep your local small"). The sim never does that; it re-derives a local from the new frame.
        for gx in [-3.0, 0.0, 17.25, 4500.0, 8999.5] {
            for gy in [-9001.0, -0.5, 123.0, 9000.0, 18000.0] {
                let base = GalaxyPos::new(Anchor::ORIGIN, gx, gy);
                let key = base.fixed8();
                for (sx, sy) in [(1i64, 0i64), (-2, 3), (2, -1), (-1, -1)] {
                    // Re-anchor by (sx,sy) cells, compensating the local so the physical point is unchanged.
                    let shifted = GalaxyPos::new(
                        Anchor::new(sx, sy),
                        gx - sx as f32 * ANCHOR_SPAN,
                        gy - sy as f32 * ANCHOR_SPAN,
                    );
                    assert_eq!(shifted.fixed8(), key, "anchor shift ({sx},{sy}) changed the key");
                }
            }
        }
    }

    /// Galaxy-scale precision: sub-unit offsets are preserved at an anchor a hundred thousand light-years
    /// out, where a flat `f64` global coordinate cannot tell them apart at all.
    #[test]
    fn galaxy_scale_keeps_sub_unit_precision() {
        // ax = 1e17 cells × 9000 m ≈ 9e20 m ≈ ~95,000 light-years — galactic radius.
        let far = Anchor::new(100_000_000_000_000_000, 0);
        let p1 = GalaxyPos::new(far, 100.000, 1500.0);
        let p2 = GalaxyPos::new(far, 100.125, 1500.0); // 1/8 unit apart — one quantum
        assert_ne!(p1.fixed8(), p2.fixed8(), "anchored coords resolve 1/8 unit at galactic distance");

        // Prove the flat-f64 approach genuinely fails here, so the anchor is doing real work.
        let span = ANCHOR_SPAN as f64;
        let g1 = far.ax as f64 * span + 100.000;
        let g2 = far.ax as f64 * span + 100.125;
        assert_eq!(g1, g2, "a flat f64 global cannot distinguish 1/8 unit at 9e20 m — hence anchoring");
    }

    /// Round-trip: a position read in another frame, then differenced, recovers the true separation.
    #[test]
    fn delta_recovers_separation_across_frames() {
        let a = GalaxyPos::new(Anchor::new(5, 5), 100.0, 200.0);
        let b = GalaxyPos::new(Anchor::new(6, 5), 50.0, 260.0); // one cell east of a's anchor
        let (dx, dy) = b.delta(a);
        // b is (9000+50 - 100) east and (260-200) north of a.
        assert!((dx - 8950.0).abs() < 1e-2, "dx={dx}");
        assert!((dy - 60.0).abs() < 1e-2, "dy={dy}");
    }

    /// Modelling the seamless-transit re-anchoring: a ship leaving a sector's east edge and entering the
    /// neighbour's west edge is the *same* physical point — so its canonical key does not change.
    #[test]
    fn transit_reanchoring_preserves_canonical_key() {
        // Before: sector (4,1), local x just past the east edge (the tick that triggers handoff).
        let before = GalaxyPos::new(Anchor::new(4, 1), ANCHOR_SPAN + 3.0, 1500.0);
        // After: the sim subtracts ANCHOR_SPAN and bumps the anchor east — exactly Sim::tick's transit math.
        let after = GalaxyPos::new(Anchor::new(5, 1), 3.0, 1500.0);
        assert_eq!(before.fixed8(), after.fixed8());
    }

    /// Negative-side carry: points west/south of an anchor normalise correctly (floor handles the sign).
    #[test]
    fn negative_local_carries_down() {
        let p = GalaxyPos::new(Anchor::new(0, 0), -1.0, -0.125);
        let (ax, ay, lx, ly) = p.fixed8();
        assert_eq!((ax, ay), (-1, -1), "a negative local rolls the anchor back one cell");
        assert_eq!(lx, SPAN8 as i32 - QUANT as i32, "-1.0 unit ⇒ SPAN8-8 eighths");
        assert_eq!(ly, SPAN8 as i32 - 1, "-0.125 unit ⇒ SPAN8-1 eighths");
    }
}
