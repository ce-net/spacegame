//! Player-centric, seamless world partitioning — the evolution of the fixed sector grid.
//!
//! The original galaxy is a grid of fixed `3000 x 3000` [`crate::shard::SectorId`] cells, each
//! rendezvous-hashed onto a node. That model has two hard ceilings the game has outgrown:
//!
//! 1. **Transitions.** A ship crossing a sector edge is *handed off* between two unrelated hosts.
//!    Even done well that is a discrete boundary — a place where state migrates and a hitch can show.
//!    A seamless, real-Earth-sized open world that you walk around in as a *human* cannot have visible
//!    seams every 3 km.
//! 2. **Range & precision.** Sector-local `f32` coordinates run `0..3000`. An Earth-sized world
//!    (~4·10⁷ units around) in raw `f32` has metre-plus quantisation — useless at human scale.
//!
//! This module replaces the *grid* with the thing the design actually wants: **each player simulates
//! itself and the patch of world around it, for everyone else.** Authority is not a fixed tile owned by
//! a hashed stranger; it is a **bubble that follows you** — a recursive AABB tightly bounding *your
//! ship, your fleet and your faction* — and your own node is its primary. Bubbles overlap and slide
//! continuously, so there is **no grid line to cross and therefore no transition**: as you fly, the set
//! of peers you exchange state with changes smoothly, and authority for any patch of space migrates with
//! **hysteresis** (a dead-band) so it never flaps at a border. The same bubble AABBs are the
//! broad-phase the physics later queries.
//!
//! Two layers, both pure (no clock, no mesh) and unit-tested:
//!
//! * **Floating-origin coordinates** ([`Wpos`], [`Frame`]): the canonical world position is `f64`
//!   (≈15–16 significant digits — sub-millimetre anywhere on an Earth-sized map). The hot simulation
//!   still runs in `f32`, but **local to a [`Frame`] origin near the action**, so the bits that matter
//!   are always near zero where `f32` is dense. This is the standard floating-origin technique, and it
//!   keeps the deterministic-replica property (the `f32` kernels are unchanged) while lifting the range
//!   ceiling to planetary scale.
//!
//! * **Bubbles & the world index** ([`Bubble`], [`World`], [`BubbleTree`]): a [`Bubble`] is one
//!   player's authority footprint; [`World`] holds the live bubbles and answers the three questions the
//!   mesh layer asks every tick — *who is authoritative for this point?* ([`World::authority_for`]),
//!   *whose bubbles overlap my view?* ([`World::interest`], the seamless replacement for the fixed
//!   nine-neighbour set), and *should this body's authority migrate?* ([`World::handoff`], with
//!   hysteresis). The overlap query is served by [`BubbleTree`], a recursive `f64` AABB tree — the
//!   "recursive AABB that holds recursive AABBs and follows objects", at the granularity of whole
//!   players.

use serde::{Deserialize, Serialize};

/// A node id (CE NodeId hex). Authority and interest are keyed by it, exactly as the mesh layer keys
/// pubsub by the authenticated sender.
pub type NodeId = String;

// ---------------------------------------------------------------------------------------------------
// Floating-origin coordinates
// ---------------------------------------------------------------------------------------------------

/// An absolute position in the seamless world, in world units (1 unit ≈ 1 metre by convention). Stored
/// as `f64` so the whole of an Earth-sized map (~4·10⁷ units across) is representable to far better than
/// a millimetre — there is no global `f32` precision cliff. The realtime sim never works in these
/// directly; it works in a [`Frame`]'s local `f32` space (see [`Frame::local`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Wpos {
    pub x: f64,
    pub y: f64,
}

impl Wpos {
    pub const fn new(x: f64, y: f64) -> Self {
        Wpos { x, y }
    }

    pub const ORIGIN: Wpos = Wpos { x: 0.0, y: 0.0 };

    /// Squared distance — cheaper when you only need to compare.
    pub fn dist2(&self, o: &Wpos) -> f64 {
        let dx = self.x - o.x;
        let dy = self.y - o.y;
        dx * dx + dy * dy
    }

    pub fn dist(&self, o: &Wpos) -> f64 {
        self.dist2(o).sqrt()
    }
}

/// A rebasing frame: a world-space origin that a node keeps **near the local action** so that the
/// simulation can run in `f32` offsets from it without losing precision. When the focus drifts too far
/// from the origin, the node [`rebased_to`](Frame::rebased_to) a fresh origin and shifts every local
/// coordinate by the (exactly representable, see [`REBASE_QUANTUM`]) delta — invisible to gameplay
/// because the relative geometry is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub origin: Wpos,
}

/// Origins are snapped to a multiple of this many units. A power of two so the `f64`→`f32` shift on a
/// rebase is exact (no rounding error injected into live state) and so two nodes that independently
/// rebase near the same place pick the *same* origin — important for the deterministic replicas.
pub const REBASE_QUANTUM: f64 = 1024.0;

/// How far (in `f32` local units) the focus may drift from the frame origin before a rebase is due.
/// Kept well inside the region where `f32` is still dense (|x| ≲ 2¹⁶ has < 0.004-unit spacing).
pub const REBASE_LIMIT: f32 = 16_384.0;

impl Frame {
    /// A frame whose origin is the [`REBASE_QUANTUM`]-snapped point nearest `focus`.
    pub fn around(focus: Wpos) -> Self {
        Frame { origin: snap(focus) }
    }

    pub const fn at(origin: Wpos) -> Self {
        Frame { origin }
    }

    /// World position → local `f32` offset from this frame's origin. This is what the realtime sim and
    /// the `f32` AABB trees consume; near the origin the result is dense and the determinism of the
    /// `f32` kernels is preserved.
    pub fn local(&self, p: Wpos) -> (f32, f32) {
        ((p.x - self.origin.x) as f32, (p.y - self.origin.y) as f32)
    }

    /// Local `f32` offset → absolute world position. Inverse of [`local`](Frame::local).
    pub fn world(&self, lx: f32, ly: f32) -> Wpos {
        Wpos { x: self.origin.x + lx as f64, y: self.origin.y + ly as f64 }
    }

    /// True if a local offset has drifted past [`REBASE_LIMIT`] on either axis and a rebase is due.
    pub fn needs_rebase(&self, lx: f32, ly: f32) -> bool {
        lx.abs() >= REBASE_LIMIT || ly.abs() >= REBASE_LIMIT
    }

    /// Re-centre the frame on `focus` (snapped). Returns the new frame and the **exact** local shift
    /// `(dx, dy)` to add to every existing local coordinate so the world is unmoved. The shift is exact
    /// because both origins are [`REBASE_QUANTUM`] multiples, so their difference is an integer multiple
    /// representable in `f32` for any in-range world.
    pub fn rebased_to(&self, focus: Wpos) -> (Frame, f32, f32) {
        let next = snap(focus);
        let dx = (self.origin.x - next.x) as f32;
        let dy = (self.origin.y - next.y) as f32;
        (Frame { origin: next }, dx, dy)
    }
}

/// Snap a world position to the nearest [`REBASE_QUANTUM`] lattice point.
fn snap(p: Wpos) -> Wpos {
    Wpos { x: (p.x / REBASE_QUANTUM).round() * REBASE_QUANTUM, y: (p.y / REBASE_QUANTUM).round() * REBASE_QUANTUM }
}

// ---------------------------------------------------------------------------------------------------
// f64 world-space AABB (the cell / bubble box)
// ---------------------------------------------------------------------------------------------------

/// An axis-aligned box in absolute world space (`f64`). The coarse, planet-scale counterpart to the
/// `f32` [`crate::aabb::Aabb`]: it bounds whole *bubbles* (a player + fleet + faction), not individual
/// ships, so `f64` is both necessary (range) and cheap (few of them per node).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WAabb {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl WAabb {
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        WAabb { min_x: min_x.min(max_x), min_y: min_y.min(max_y), max_x: min_x.max(max_x), max_y: min_y.max(max_y) }
    }

    /// A square box of half-extent `r` centred on `c`.
    pub fn around(c: Wpos, r: f64) -> Self {
        WAabb { min_x: c.x - r, min_y: c.y - r, max_x: c.x + r, max_y: c.y + r }
    }

    pub fn center(&self) -> Wpos {
        Wpos { x: (self.min_x + self.max_x) * 0.5, y: (self.min_y + self.max_y) * 0.5 }
    }

    /// Overlap (touching edges count), the broad-phase predicate.
    pub fn intersects(&self, o: &WAabb) -> bool {
        self.min_x <= o.max_x && self.max_x >= o.min_x && self.min_y <= o.max_y && self.max_y >= o.min_y
    }

    pub fn contains_point(&self, p: Wpos) -> bool {
        p.x >= self.min_x && p.x <= self.max_x && p.y >= self.min_y && p.y <= self.max_y
    }

    /// The tightest box containing both.
    pub fn union(&self, o: &WAabb) -> WAabb {
        WAabb {
            min_x: self.min_x.min(o.min_x),
            min_y: self.min_y.min(o.min_y),
            max_x: self.max_x.max(o.max_x),
            max_y: self.max_y.max(o.max_y),
        }
    }

    /// Grow by `pad` on every side (e.g. to fatten a bubble for the dead-band).
    pub fn expanded(&self, pad: f64) -> WAabb {
        WAabb { min_x: self.min_x - pad, min_y: self.min_y - pad, max_x: self.max_x + pad, max_y: self.max_y + pad }
    }
}

// ---------------------------------------------------------------------------------------------------
// Bubbles: one player's authority footprint
// ---------------------------------------------------------------------------------------------------

/// One player's **authority bubble** — the recursive-AABB region that player's node simulates for
/// everyone. `focus` is the owner's own position (the high-precision centre of attention); `bounds` is
/// the box that follows the owner's ship, fleet and faction, fattened by a dead-band so it does not
/// re-fit every tick. `load` is a cheap fullness metric (entities the owner is simulating) the placement
/// layer can balance on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bubble {
    pub owner: NodeId,
    pub focus: Wpos,
    pub bounds: WAabb,
    pub load: u32,
}

impl Bubble {
    /// Build the bubble that *follows* an owner: the union of boxes around every member entity
    /// (`(position, radius)`) — the ship, each fleet unit, each faction structure — plus a baseline
    /// `slack` so a lone player still owns a usable patch of space and small movements don't refit it.
    /// The owner's own position is `focus`.
    pub fn following(
        owner: impl Into<NodeId>,
        focus: Wpos,
        members: impl IntoIterator<Item = (Wpos, f64)>,
        slack: f64,
    ) -> Self {
        let mut b = WAabb::around(focus, slack.max(1.0));
        let mut load = 1u32;
        for (p, r) in members {
            b = b.union(&WAabb::around(p, r.max(0.0)));
            load += 1;
        }
        Bubble { owner: owner.into(), focus, bounds: b.expanded(slack * 0.5), load }
    }

    /// A minimal bubble: a single player at a point with the default slack and no fleet.
    pub fn solo(owner: impl Into<NodeId>, focus: Wpos, slack: f64) -> Self {
        Bubble::following(owner, focus, std::iter::empty(), slack)
    }
}

// ---------------------------------------------------------------------------------------------------
// The world: live bubbles + the queries the mesh layer asks each tick
// ---------------------------------------------------------------------------------------------------

/// The set of live authority bubbles a node knows about (its own plus the neighbours it has heard
/// announce), and the pure decision layer over them. No mesh I/O — the host loop gathers bubble
/// announcements off a pubsub topic and feeds them here, exactly as it gathers replica heartbeats.
#[derive(Debug, Clone, Default)]
pub struct World {
    bubbles: Vec<Bubble>,
}

impl World {
    pub fn new() -> Self {
        World { bubbles: Vec::new() }
    }

    pub fn from_bubbles(bubbles: impl IntoIterator<Item = Bubble>) -> Self {
        let mut w = World { bubbles: bubbles.into_iter().collect() };
        w.bubbles.sort_by(|a, b| a.owner.cmp(&b.owner));
        w
    }

    /// Insert or replace a bubble (keyed by owner). Kept sorted by owner so every node iterates in the
    /// same order and the queries below are deterministic across replicas.
    pub fn upsert(&mut self, bubble: Bubble) {
        match self.bubbles.binary_search_by(|b| b.owner.cmp(&bubble.owner)) {
            Ok(i) => self.bubbles[i] = bubble,
            Err(i) => self.bubbles.insert(i, bubble),
        }
    }

    /// Drop a bubble whose owner has gone (left, or expired off the bubble topic).
    pub fn remove(&mut self, owner: &str) {
        if let Ok(i) = self.bubbles.binary_search_by(|b| b.owner.as_str().cmp(owner)) {
            self.bubbles.remove(i);
        }
    }

    pub fn len(&self) -> usize {
        self.bubbles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bubbles.is_empty()
    }

    pub fn bubbles(&self) -> &[Bubble] {
        &self.bubbles
    }

    pub fn get(&self, owner: &str) -> Option<&Bubble> {
        self.bubbles.binary_search_by(|b| b.owner.as_str().cmp(owner)).ok().map(|i| &self.bubbles[i])
    }

    /// **Who simulates this point?** The owner whose bubble contains `p`, choosing the one whose
    /// `focus` is *closest* to `p` when several overlap (you are most authoritatively simulated by the
    /// player you are nearest — typically yourself). `None` if `p` lies in no bubble (deep empty space,
    /// which no one needs to simulate). Deterministic: ties broken by the smaller owner id.
    pub fn authority_for(&self, p: Wpos) -> Option<&NodeId> {
        self.bubbles
            .iter()
            .filter(|b| b.bounds.contains_point(p))
            .min_by(|a, b| {
                a.focus
                    .dist2(&p)
                    .total_cmp(&b.focus.dist2(&p))
                    .then_with(|| a.owner.cmp(&b.owner))
            })
            .map(|b| &b.owner)
    }

    /// **Whose state must I receive?** Every owner whose bubble overlaps the query box `view` (a
    /// player's viewport, or a host's simulated region). This is the seamless replacement for
    /// [`crate::shard::SectorId::neighbors`]: instead of a fixed 3×3 of grid tiles, you subscribe to
    /// exactly the bubbles your interest box touches, and that set slides continuously as you move — no
    /// edge to cross. Returned sorted & deduped for determinism.
    pub fn interest(&self, view: &WAabb) -> Vec<&NodeId> {
        let tree = BubbleTree::build(&self.bubbles);
        let mut out: Vec<&NodeId> = tree.query(view).into_iter().map(|i| &self.bubbles[i].owner).collect();
        out.sort();
        out.dedup();
        out
    }

    /// The neighbours of `owner`: the other players whose bubbles overlap `owner`'s, i.e. exactly the
    /// peers it must reconcile shared physics and hand-offs with. Excludes `owner` itself.
    pub fn neighbors(&self, owner: &str) -> Vec<&NodeId> {
        let Some(me) = self.get(owner) else { return Vec::new() };
        let tree = BubbleTree::build(&self.bubbles);
        let mut out: Vec<&NodeId> = tree
            .query(&me.bounds)
            .into_iter()
            .map(|i| &self.bubbles[i].owner)
            .filter(|o| o.as_str() != owner)
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// **Seamless authority migration.** A body at `at` is currently simulated by `current`. Return the
    /// owner it should migrate to, or `None` to keep it where it is. The decision has a **dead-band**
    /// (`hysteresis`, in world units): a challenger only wins if its focus is closer than the incumbent's
    /// by more than that margin, so a body sitting on a boundary between two equidistant players does not
    /// flap back and forth every tick. This dead-band — not a grid line — is what makes hand-off
    /// continuous and invisible. `None` `current` (or a `current` that no longer exists) is treated as
    /// "unowned", so any containing bubble may claim it.
    pub fn handoff(&self, at: Wpos, current: Option<&str>, hysteresis: f64) -> Option<&NodeId> {
        // Best challenger: nearest focus among bubbles that actually contain the body.
        let best = self
            .bubbles
            .iter()
            .filter(|b| b.bounds.contains_point(at))
            .min_by(|a, b| {
                a.focus.dist2(&at).total_cmp(&b.focus.dist2(&at)).then_with(|| a.owner.cmp(&b.owner))
            })?;

        match current.and_then(|c| self.get(c)) {
            // Incumbent still simulating it: only migrate if the challenger's focus is closer than the
            // incumbent's by more than the dead-band. This is symmetric — switching back also needs a
            // full band of advantage — so a body parked near the midline cannot oscillate.
            Some(inc) => {
                if inc.owner == best.owner {
                    return None;
                }
                let inc_d = inc.focus.dist(&at);
                let best_d = best.focus.dist(&at);
                if best_d + hysteresis < inc_d {
                    Some(&best.owner)
                } else {
                    None
                }
            }
            // No live incumbent: hand straight to the best container.
            None => Some(&best.owner),
        }
    }

    /// Total simulated load across all known bubbles — a coarse galaxy-occupancy gauge for telemetry
    /// and for deciding when to ask the mesh for help.
    pub fn total_load(&self) -> u64 {
        self.bubbles.iter().map(|b| b.load as u64).sum()
    }
}

// ---------------------------------------------------------------------------------------------------
// BubbleTree: a recursive f64 AABB tree over bubbles (the world broad-phase)
// ---------------------------------------------------------------------------------------------------

enum BNode {
    Leaf(Vec<usize>),
    Branch(Box<[BNode; 4]>),
}

/// A recursively subdivided `f64` AABB tree (loose quadtree) over a slice of [`Bubble`]s — the
/// planet-scale analogue of [`crate::aabb::AabbTree`]. It answers "which bubbles overlap this box?" in
/// `~O(log n + k)` instead of scanning every player, which is what keeps [`World::interest`] /
/// [`World::neighbors`] cheap as the galaxy fills, and is the same structure the physics broad-phase
/// reuses once bubbles carry their nested per-cell trees. Items are bucketed by box **centre** (loose
/// quadtree), so the query pads each region by the largest half-extent before pruning — provably
/// lossless (see the brute-force cross-check test).
pub struct BubbleTree {
    boxes: Vec<WAabb>,
    bounds: WAabb,
    root: BNode,
    max_half: f64,
}

impl BubbleTree {
    const LEAF_CAP: usize = 8;
    const MAX_DEPTH: u32 = 16;

    pub fn build(bubbles: &[Bubble]) -> Self {
        let boxes: Vec<WAabb> = bubbles.iter().map(|b| b.bounds).collect();
        Self::build_boxes(boxes)
    }

    fn build_boxes(boxes: Vec<WAabb>) -> Self {
        if boxes.is_empty() {
            return BubbleTree {
                boxes,
                bounds: WAabb::new(0.0, 0.0, 0.0, 0.0),
                root: BNode::Leaf(Vec::new()),
                max_half: 0.0,
            };
        }
        let mut bounds = boxes[0];
        for b in &boxes[1..] {
            bounds = bounds.union(b);
        }
        let max_half = boxes
            .iter()
            .map(|b| ((b.max_x - b.min_x) * 0.5).max((b.max_y - b.min_y) * 0.5))
            .fold(0.0_f64, f64::max);
        let all: Vec<usize> = (0..boxes.len()).collect();
        let root = Self::subdivide(bounds, &all, &boxes, 0);
        BubbleTree { boxes, bounds, root, max_half }
    }

    fn subdivide(region: WAabb, idxs: &[usize], boxes: &[WAabb], depth: u32) -> BNode {
        if idxs.len() <= Self::LEAF_CAP || depth >= Self::MAX_DEPTH {
            return BNode::Leaf(idxs.to_vec());
        }
        let c = region.center();
        let quads = Self::quadrants(region);
        let mut buckets: [Vec<usize>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for &i in idxs {
            let bc = boxes[i].center();
            let q = match (bc.y >= c.y, bc.x >= c.x) {
                (false, false) => 0,
                (false, true) => 1,
                (true, false) => 2,
                (true, true) => 3,
            };
            buckets[q].push(i);
        }
        if buckets.iter().filter(|b| !b.is_empty()).count() <= 1 && depth + 1 >= Self::MAX_DEPTH {
            return BNode::Leaf(idxs.to_vec());
        }
        BNode::Branch(Box::new([
            Self::subdivide(quads[0], &buckets[0], boxes, depth + 1),
            Self::subdivide(quads[1], &buckets[1], boxes, depth + 1),
            Self::subdivide(quads[2], &buckets[2], boxes, depth + 1),
            Self::subdivide(quads[3], &buckets[3], boxes, depth + 1),
        ]))
    }

    fn quadrants(region: WAabb) -> [WAabb; 4] {
        let c = region.center();
        [
            WAabb::new(region.min_x, region.min_y, c.x, c.y),
            WAabb::new(c.x, region.min_y, region.max_x, c.y),
            WAabb::new(region.min_x, c.y, c.x, region.max_y),
            WAabb::new(c.x, c.y, region.max_x, region.max_y),
        ]
    }

    /// Indices of every bubble box overlapping `q`, in deterministic build order.
    pub fn query(&self, q: &WAabb) -> Vec<usize> {
        let mut out = Vec::new();
        if self.boxes.is_empty() {
            return out;
        }
        self.query_into(&self.root, self.bounds, q, &mut out);
        out
    }

    fn query_into(&self, node: &BNode, region: WAabb, q: &WAabb, out: &mut Vec<usize>) {
        if !region.expanded(self.max_half).intersects(q) {
            return;
        }
        match node {
            BNode::Leaf(items) => {
                for &i in items {
                    if self.boxes[i].intersects(q) {
                        out.push(i);
                    }
                }
            }
            BNode::Branch(children) => {
                let quads = Self::quadrants(region);
                for (child, quad) in children.iter().zip(quads.iter()) {
                    self.query_into(child, *quad, q, out);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Wpos {
        Wpos::new(x, y)
    }

    #[test]
    fn floating_origin_keeps_sub_mm_precision_at_planet_scale() {
        // A point ~1.2 * 10^7 units out (well past the far side of an Earth-sized map). A frame placed
        // near it round-trips through f32 locals to sub-millimetre — the whole point of f64 canonical +
        // f32-local.
        let far = p(12_345_678.5, -9_876_543.25);
        let frame = Frame::around(far);
        let (lx, ly) = frame.local(far);
        assert!(lx.abs() < REBASE_LIMIT && ly.abs() < REBASE_LIMIT, "focus sits inside the dense f32 window");
        let back = frame.world(lx, ly);
        assert!(back.dist(&far) < 1e-3, "round-trip error {} too large", back.dist(&far));
    }

    #[test]
    fn raw_f32_would_lose_that_precision() {
        // Demonstrate why the frame is necessary: storing the same coordinate directly in f32 quantises
        // to metre scale, which the floating origin avoids.
        let v: f64 = 12_345_678.5;
        let naive = v as f32 as f64;
        assert!((naive - v).abs() > 0.4, "raw f32 quantisation is coarse here ({} off)", (naive - v).abs());
    }

    #[test]
    fn rebase_shift_is_exact_and_world_stable() {
        let frame = Frame::around(p(1000.0, 1000.0));
        // A body at some world point, expressed locally.
        let body = p(1500.0, 800.0);
        let (lx, ly) = frame.local(body);
        // Drift far enough to need a rebase, then rebase onto a new focus.
        let focus = p(20_000.0, -5_000.0);
        assert!(frame.needs_rebase(frame.local(focus).0, frame.local(focus).1));
        let (next, dx, dy) = frame.rebased_to(focus);
        // The body's NEW local coords are the old ones plus the exact shift, and map back to the SAME world point.
        let world_again = next.world(lx + dx, ly + dy);
        assert!(world_again.dist(&body) < 1e-6, "rebase moved the world by {}", world_again.dist(&body));
    }

    #[test]
    fn bubble_follows_ship_and_fleet() {
        // A player with a fleet spread out to the east — the bubble must grow to bound all of it.
        let me = p(0.0, 0.0);
        let fleet = vec![(p(400.0, 0.0), 20.0), (p(800.0, 50.0), 20.0), (p(-100.0, -100.0), 20.0)];
        let b = Bubble::following("me", me, fleet, 200.0);
        assert!(b.bounds.contains_point(p(800.0, 50.0)), "bubble bounds the far fleet unit");
        assert!(b.bounds.contains_point(me), "and the owner");
        assert_eq!(b.load, 4, "owner + three fleet members");
    }

    #[test]
    fn authority_is_the_nearest_owning_player() {
        // Two overlapping bubbles; a point inside both is simulated by whoever is nearer.
        let a = Bubble::solo("alice", p(0.0, 0.0), 500.0);
        let b = Bubble::solo("bob", p(600.0, 0.0), 500.0);
        let w = World::from_bubbles([a, b]);
        assert_eq!(w.authority_for(p(100.0, 0.0)).map(String::as_str), Some("alice"));
        assert_eq!(w.authority_for(p(500.0, 0.0)).map(String::as_str), Some("bob"));
        // Deep empty space belongs to no one.
        assert!(w.authority_for(p(100_000.0, 0.0)).is_none());
    }

    #[test]
    fn interest_is_the_seamless_neighbour_set() {
        // Five players strung along x. A viewport in the middle sees only the bubbles it overlaps —
        // not the whole line, and with no fixed grid tiles involved.
        let bubbles: Vec<Bubble> =
            (0..5).map(|i| Bubble::solo(format!("p{i}"), p(i as f64 * 1000.0, 0.0), 300.0)).collect();
        let w = World::from_bubbles(bubbles);
        let view = WAabb::around(p(2000.0, 0.0), 350.0); // around p2, just brushing p1/p3 fat boxes
        let seen = w.interest(&view);
        assert!(seen.iter().any(|o| o.as_str() == "p2"), "sees the player it is on top of");
        assert!(!seen.iter().any(|o| o.as_str() == "p0"), "does NOT see the far player p0");
        assert!(!seen.iter().any(|o| o.as_str() == "p4"), "does NOT see the far player p4");
    }

    #[test]
    fn handoff_has_a_dead_band_so_it_does_not_flap() {
        // Two equal players; a body drifting across the midline must NOT switch authority until it is
        // decisively into the other's half (beyond the hysteresis band).
        let a = Bubble::solo("a", p(0.0, 0.0), 1000.0);
        let b = Bubble::solo("b", p(1000.0, 0.0), 1000.0);
        let w = World::from_bubbles([a, b]);
        let hys = 100.0;

        // Just past the midline toward b, but inside the dead-band: keep current owner 'a'.
        assert_eq!(w.handoff(p(520.0, 0.0), Some("a"), hys), None, "small overshoot does not migrate");
        // Decisively into b's half and out of a's comfortable interior: migrate.
        assert_eq!(
            w.handoff(p(950.0, 0.0), Some("a"), hys).map(String::as_str),
            Some("b"),
            "a body deep in b's half migrates to b"
        );
        // An unowned body is claimed immediately by its nearest container.
        assert_eq!(w.handoff(p(100.0, 0.0), None, hys).map(String::as_str), Some("a"));
    }

    #[test]
    fn neighbors_are_overlapping_players_only() {
        let a = Bubble::solo("a", p(0.0, 0.0), 400.0);
        let b = Bubble::solo("b", p(500.0, 0.0), 400.0); // overlaps a
        let c = Bubble::solo("c", p(10_000.0, 0.0), 400.0); // far away
        let w = World::from_bubbles([a, b, c]);
        let n = w.neighbors("a");
        assert_eq!(n.iter().map(|s| s.as_str()).collect::<Vec<_>>(), vec!["b"]);
    }

    #[test]
    fn bubble_tree_query_is_complete_against_brute_force() {
        // The recursive f64 tree must never miss an overlap. Grid of bubbles + many query boxes,
        // compared to the linear scan. Deterministic (no rng).
        let mut bubbles = Vec::new();
        for gx in 0..30 {
            for gy in 0..30 {
                let c = p(gx as f64 * 800.0, gy as f64 * 800.0);
                bubbles.push(Bubble::solo(format!("{gx}_{gy}"), c, 150.0));
            }
        }
        let tree = BubbleTree::build(&bubbles);
        let boxes: Vec<WAabb> = bubbles.iter().map(|b| b.bounds).collect();
        for k in 0..300i64 {
            let qx = (k * 137 % 24000) as f64;
            let qy = (k * 211 % 24000) as f64;
            let qr = (100 + (k * 17) % 1200) as f64;
            let q = WAabb::around(p(qx, qy), qr);
            let mut got = tree.query(&q);
            let mut brute: Vec<usize> =
                (0..boxes.len()).filter(|&i| boxes[i].intersects(&q)).collect();
            got.sort();
            brute.sort();
            assert_eq!(got, brute, "tree query mismatch at k={k}");
        }
    }

    #[test]
    fn upsert_and_remove_keep_owner_order() {
        let mut w = World::new();
        w.upsert(Bubble::solo("zed", p(0.0, 0.0), 100.0));
        w.upsert(Bubble::solo("amy", p(0.0, 0.0), 100.0));
        w.upsert(Bubble::solo("amy", p(50.0, 0.0), 100.0)); // replace, not duplicate
        assert_eq!(w.len(), 2);
        assert_eq!(w.bubbles()[0].owner, "amy");
        assert_eq!(w.get("amy").unwrap().focus, p(50.0, 0.0));
        w.remove("amy");
        assert_eq!(w.len(), 1);
        assert_eq!(w.bubbles()[0].owner, "zed");
    }
}
