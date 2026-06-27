//! **Entity-anchored authority** — the world follows the player, not the player the world.
//!
//! The adaptive galaxy ([`crate::galaxy`]) partitions *space*: it cuts the plane into a power-of-two
//! quadtree and hands each rectangle to a node. That is coordinator-free and it scales, but it has one
//! property the design explicitly rejects (`STATE-MODEL.md`: *"I hate sector clamping"*): the cuts are
//! fixed in space, so a pilot loitering on a cell seam crosses a boundary every few seconds and every
//! crossing is a hand-off — a transition the player can feel. A grid can never be seamless *for the
//! owner*, because the owner moves and the grid does not.
//!
//! This module inverts that. Authority is anchored to **entities**, not to coordinates. Each player owns
//! a [`Domain`]: a **recursive AABB** that bounds the things that player is authoritative for — their
//! ship, the ships in their fleet, their faction's structures — and therefore **moves with them**. The
//! player sits at the centre of their own bubble and *never crosses a boundary*: there is no seam to
//! cross, because the seam travels with the ship. "Your own node is the server for you" (`STATE-MODEL.md`)
//! is exactly: your node hosts your domain, your domain is always centred on you, so your authority is
//! always zero-RTT and never handed off.
//!
//! Three nested levels make the AABB *recursive* — each box is the union of the boxes one level finer,
//! so the structure literally "follows the player and its ships and factions":
//!
//! ```text
//!   faction box  ────────────────────────────────────┐  the Domain's outer bound
//!   │   fleet box ───────────────────┐                │  (what others must replicate to see you)
//!   │   │  ship □ (you)              │   ◦ structure  │
//!   │   │     ◦ wingman  ◦ wingman   │     ◦ structure│
//!   │   └────────────────────────────┘                │
//!   └─────────────────────────────────────────────────┘
//! ```
//!
//! **Simulate-yourself-for-others, and your environment too.** Two domains *overlap* exactly when their
//! owners can see each other; overlap is the interest relation (found by a [`DomainTree`] broad-phase,
//! rebuilt each tick from the moving anchors — `O(log n + k)`, never `O(players²)`). The neutral
//! environment near you — asteroids, hazards, wreckage — is claimed by **your** domain (the nearest
//! anchor) and simulated on **your** node *on behalf of everyone who can see it*. So each player brings
//! the compute for itself **and the patch of world around it**, which is why this scales: empty space
//! has no domains and costs nothing; a crowd is its own server farm, each node carrying its own bubble.
//!
//! **Why it is seamless.** The owner's ship is, by construction, always inside its own domain, so the
//! owner is *never* transited. The only thing that ever changes hands is a piece of neutral environment,
//! and that is decided by a **pure, sticky function** ([`DomainField::claim`]) recomputed every tick —
//! not a discrete edge-crossing event — with hysteresis so an asteroid sitting in the overlap of two
//! bubbles does not flap between them. No transition is scheduled, so none can stutter.
//!
//! The module is pure and deterministic (no clock, no mesh, no float-fragile ordering): every node that
//! holds the same entity positions computes the same domains, the same overlaps, and the same
//! environment owner, which is what lets the replicated-authority merge ([`crate::replication`]) agree
//! on *who simulates what* without a coordinator — the same trick `galaxy.rs` uses for cell shape,
//! applied to moving bubbles instead of a fixed grid.

use serde::{Deserialize, Serialize};

/// World units, galaxy scale. Shared with [`crate::galaxy::World`]; `f64` so an Earth-sized plane keeps
/// sub-metre precision far from the origin (an `f32` world would quantise to tens of metres out there).
pub type World = f64;

/// A double-precision axis-aligned box in absolute world coordinates. This is the world-scale analogue
/// of [`crate::aabb::Aabb`] (which is `f32` and *sector-local*, for the per-tick collision broad-phase);
/// domains live in the whole-galaxy frame, so they need the wider type.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub min_x: World,
    pub min_y: World,
    pub max_x: World,
    pub max_y: World,
}

impl Bounds {
    /// A box from explicit corners, normalised so `min <= max` on both axes.
    pub fn new(ax: World, ay: World, bx: World, by: World) -> Self {
        Bounds { min_x: ax.min(bx), min_y: ay.min(by), max_x: ax.max(bx), max_y: ay.max(by) }
    }

    /// A square box of half-extent `r` centred on `(x, y)` — the bound of an entity of radius `r`.
    pub fn around(x: World, y: World, r: World) -> Self {
        Bounds { min_x: x - r, min_y: y - r, max_x: x + r, max_y: y + r }
    }

    /// The smallest box containing both — the recursive step that grows a fleet box to hold a new ship,
    /// and a domain box to hold a new fleet.
    pub fn union(&self, o: &Bounds) -> Bounds {
        Bounds {
            min_x: self.min_x.min(o.min_x),
            min_y: self.min_y.min(o.min_y),
            max_x: self.max_x.max(o.max_x),
            max_y: self.max_y.max(o.max_y),
        }
    }

    /// True if the two boxes overlap (touching edges count). Overlap *is* the interest relation between
    /// two domains.
    pub fn intersects(&self, o: &Bounds) -> bool {
        self.min_x <= o.max_x && self.max_x >= o.min_x && self.min_y <= o.max_y && self.max_y >= o.min_y
    }

    /// True if `(x, y)` lies within the box (inclusive).
    pub fn contains_point(&self, x: World, y: World) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    /// Box centre.
    pub fn center(&self) -> (World, World) {
        ((self.min_x + self.max_x) * 0.5, (self.min_y + self.max_y) * 0.5)
    }

    /// Grow the box by `pad` on every side — how a domain's *footprint* becomes its *interest box*
    /// (footprint + view radius), the region whose contents the owner must receive and serve.
    pub fn expanded(&self, pad: World) -> Bounds {
        Bounds {
            min_x: self.min_x - pad,
            min_y: self.min_y - pad,
            max_x: self.max_x + pad,
            max_y: self.max_y + pad,
        }
    }
}

/// Anything the partition can reason about: it has an owner (whose node simulates it), a stable id, a
/// position, and a radius. Keeping this a trait keeps the module pure and unit-testable without dragging
/// in the whole [`crate::sim`] — exactly like [`crate::partition::Positioned`]. The sim's `Ship`,
/// faction structures, and units all satisfy it.
pub trait Owned {
    /// The owner whose node is authoritative for this entity. Neutral/unowned environment uses
    /// [`UNOWNED`]; it gets *claimed* by the nearest domain (see [`DomainField::claim`]).
    fn owner(&self) -> &str;
    /// A stable id, unique within an owner.
    fn entity_id(&self) -> u64;
    fn pos(&self) -> (World, World);
    /// Half-extent of the entity's own box. A ship is a few units; a faction structure may be larger.
    fn radius(&self) -> World;
}

/// The owner string for neutral environment (asteroids, hazards, wreckage) — entities nobody pilots but
/// somebody must simulate. The field assigns each to the nearest player domain.
pub const UNOWNED: &str = "";

/// One player's authority bubble: the recursive-AABB footprint of everything they own, plus the anchor
/// it follows. Cheap, `Copy`-free but `Clone`, and serde-stamped so it can ride in a snapshot or be
/// gossiped on a `/domains` topic for the live map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Domain {
    /// The owning player (their CE NodeId, as the sim carries it).
    pub owner: String,
    /// The point the bubble tracks — the owner's own ship. Authority is always centred here, so the
    /// owner never crosses a boundary.
    pub anchor: (World, World),
    /// The footprint: the union of every owned entity's box. Follows the entities as they move.
    pub bounds: Bounds,
    /// Footprint grown by the owner's view radius — the region the owner is interested in and serves to
    /// others. Two owners can see each other iff their interest boxes overlap.
    pub interest: Bounds,
    /// How many entities the owner is authoritative for (the load this bubble puts on its host).
    pub entities: u32,
}

impl Domain {
    /// Build a domain from an owner's entities and view radius. `anchor` is the owner's own ship
    /// position; the footprint is the recursive union of all their entity boxes (so it bounds the whole
    /// fleet + faction and moves with them). With no entities the domain is a point at the anchor.
    pub fn build<'a, E: Owned + 'a>(
        owner: impl Into<String>,
        anchor: (World, World),
        owned: impl IntoIterator<Item = &'a E>,
        view_radius: World,
    ) -> Domain {
        let owner = owner.into();
        let mut bounds: Option<Bounds> = None;
        let mut entities = 0u32;
        for e in owned {
            let (x, y) = e.pos();
            let b = Bounds::around(x, y, e.radius());
            bounds = Some(match bounds {
                Some(acc) => acc.union(&b),
                None => b,
            });
            entities += 1;
        }
        // The anchor is always inside the footprint, even before any entity is added — this is what makes
        // the owner un-transitable (the ship is never outside its own domain).
        let seed = Bounds::around(anchor.0, anchor.1, 0.0);
        let bounds = bounds.map(|b| b.union(&seed)).unwrap_or(seed);
        let interest = bounds.expanded(view_radius);
        Domain { owner, anchor, bounds, interest, entities }
    }

    /// Squared distance from this domain's anchor to a point — the deterministic, sqrt-free tiebreaker
    /// for environment ownership.
    fn anchor_d2(&self, x: World, y: World) -> World {
        let dx = self.anchor.0 - x;
        let dy = self.anchor.1 - y;
        dx * dx + dy * dy
    }
}

/// A recursive AABB over the live domains — the broad-phase that turns "which bubbles overlap mine"
/// from an `O(players²)` scan into `O(log n + k)`. It is the *recursive AABB which follows the player
/// and its ships and factions* at the inter-player level: every leaf is a domain whose box tracks its
/// owner, every internal node is the union of its children, and the whole tree is rebuilt each tick from
/// the moving anchors. Median-split on the longer axis keeps it balanced and, because the split key is a
/// deterministic sort, every node builds the byte-identical tree from the same domains.
#[derive(Debug)]
enum TreeNode {
    Leaf(usize),
    Branch { bounds: Bounds, kids: Box<(TreeNode, TreeNode)> },
}

impl TreeNode {
    fn bounds(&self, leaves: &[Bounds]) -> Bounds {
        match self {
            TreeNode::Leaf(i) => leaves[*i],
            TreeNode::Branch { bounds, .. } => *bounds,
        }
    }
}

/// The set of all live domains plus their broad-phase tree. Built once per tick; answers interest and
/// environment-ownership queries.
#[derive(Debug)]
pub struct DomainField {
    domains: Vec<Domain>,
    boxes: Vec<Bounds>,
    root: Option<TreeNode>,
}

impl DomainField {
    /// Assemble the field from already-built domains. Domains are sorted by owner so the tree (and every
    /// query that walks it) is deterministic regardless of insertion order.
    pub fn new(mut domains: Vec<Domain>) -> Self {
        domains.sort_by(|a, b| a.owner.cmp(&b.owner));
        let boxes: Vec<Bounds> = domains.iter().map(|d| d.interest).collect();
        let mut idx: Vec<usize> = (0..boxes.len()).collect();
        let root = build_tree(&mut idx, &boxes);
        DomainField { domains, boxes, root }
    }

    /// Build a field straight from a flat entity list: group entities by owner, give each owner a domain
    /// whose anchor is its own ship (`anchor_of` picks it — typically the owner's player ship), and index
    /// them. `UNOWNED` entities are excluded here; they are *claimed* by [`claim`](Self::claim), not given
    /// their own bubble.
    pub fn from_entities<E: Owned>(
        entities: &[E],
        view_radius: World,
        mut anchor_of: impl FnMut(&str) -> (World, World),
    ) -> Self {
        use std::collections::BTreeMap;
        let mut by_owner: BTreeMap<&str, Vec<&E>> = BTreeMap::new();
        for e in entities {
            if e.owner() == UNOWNED {
                continue;
            }
            by_owner.entry(e.owner()).or_default().push(e);
        }
        let domains = by_owner
            .into_iter()
            .map(|(owner, owned)| Domain::build(owner, anchor_of(owner), owned, view_radius))
            .collect();
        DomainField::new(domains)
    }

    pub fn domains(&self) -> &[Domain] {
        &self.domains
    }

    pub fn get(&self, owner: &str) -> Option<&Domain> {
        self.domains.iter().find(|d| d.owner == owner)
    }

    /// Every domain whose interest box overlaps `query` — the broad-phase descent. Results are the
    /// domains' indices into [`domains`](Self::domains), in deterministic tree order.
    fn query(&self, query: &Bounds) -> Vec<usize> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            self.descend(root, query, &mut out);
        }
        out
    }

    fn descend(&self, node: &TreeNode, query: &Bounds, out: &mut Vec<usize>) {
        if !node.bounds(&self.boxes).intersects(query) {
            return;
        }
        match node {
            TreeNode::Leaf(i) => out.push(*i),
            TreeNode::Branch { kids, .. } => {
                self.descend(&kids.0, query, out);
                self.descend(&kids.1, query, out);
            }
        }
    }

    /// The owners whose bubbles overlap `owner`'s — the players `owner` can see and must simulate-for /
    /// receive-from this tick. Excludes `owner` itself. This is the seamless interest set: continuous
    /// overlap, no grid cell, no subscription churn when you drift (you gain/lose a neighbour smoothly as
    /// boxes start/stop touching, instead of snapping at a seam).
    pub fn interest(&self, owner: &str) -> Vec<&Domain> {
        let Some(me) = self.get(owner) else { return Vec::new() };
        self.query(&me.interest)
            .into_iter()
            .map(|i| &self.domains[i])
            .filter(|d| d.owner != owner)
            .collect()
    }

    /// Claim a neutral environment entity at `(x, y)`: the domain that simulates it on everyone's behalf.
    /// The rule is pure and total — the nearest anchor among the domains whose *footprint* contains the
    /// point; if none contains it, the single nearest domain within `reach`; if the world is empty,
    /// `None` (nobody is here, so the entity is not simulated — empty space costs nothing).
    ///
    /// Ties (equal squared distance) break on `owner` string order, so every node agrees on the owner
    /// without a coordinator.
    pub fn claim(&self, x: World, y: World, reach: World) -> Option<&str> {
        let mut best: Option<(World, bool, &str)> = None; // (d2, contained, owner)
        let reach2 = reach * reach;
        for d in &self.domains {
            let contained = d.bounds.contains_point(x, y);
            let d2 = d.anchor_d2(x, y);
            if !contained && d2 > reach2 {
                continue;
            }
            // Prefer a domain that actually contains the point over one that merely has it in reach;
            // among equals, nearest anchor; among those, lexicographically smallest owner.
            use std::cmp::Reverse;
            let better = match best {
                None => true,
                Some((bd2, bcont, bowner)) => {
                    (contained, Reverse(d2), Reverse(d.owner.as_str()))
                        > (bcont, Reverse(bd2), Reverse(bowner))
                }
            };
            if better {
                best = Some((d2, contained, d.owner.as_str()));
            }
        }
        best.map(|(_, _, o)| o)
    }

    /// Sticky variant for runtime: keep `prev` owner unless another domain is closer by more than
    /// `hysteresis` world-units of squared-distance margin (or `prev` no longer exists / no longer has
    /// the point in its footprint). This is what stops an asteroid in the overlap of two equally close
    /// bubbles from flapping owners every tick — the *reason this can be transition-free*: ownership is a
    /// stable function recomputed continuously, not an event that fires on a crossing.
    pub fn claim_sticky(&self, x: World, y: World, reach: World, prev: &str, hysteresis: World) -> Option<&str> {
        let fresh = self.claim(x, y, reach)?;
        if fresh == prev {
            return Some(fresh);
        }
        // A challenger appeared. Only switch if `prev` is gone or out of reach, or the challenger beats
        // `prev`'s anchor distance by more than the hysteresis margin.
        let Some(prev_d) = self.get(prev) else { return Some(fresh) };
        let prev_d2 = prev_d.anchor_d2(x, y);
        if prev_d2 > reach * reach {
            return Some(fresh);
        }
        let fresh_d2 = self.get(fresh).map(|d| d.anchor_d2(x, y)).unwrap_or(World::INFINITY);
        if prev_d2 - fresh_d2 > hysteresis {
            Some(fresh)
        } else {
            Some(prev_d.owner.as_str()) // keep it — within the dead-band
        }
    }

    /// Total entities under authority across the whole field — the global load, which equals the sum of
    /// the per-player bubbles (no node holds it all).
    pub fn total_entities(&self) -> u64 {
        self.domains.iter().map(|d| d.entities as u64).sum()
    }
}

/// Recursively split `idx` (indices into `boxes`) into a balanced AABB tree. Median-split on the axis
/// the current span is widest along; deterministic because the partition key is a stable sort on the box
/// centre. A single index is a leaf.
fn build_tree(idx: &mut [usize], boxes: &[Bounds]) -> Option<TreeNode> {
    match idx.len() {
        0 => None,
        1 => Some(TreeNode::Leaf(idx[0])),
        _ => {
            // Bounds over this slice, to pick the split axis.
            let mut acc = boxes[idx[0]];
            for &i in idx.iter().skip(1) {
                acc = acc.union(&boxes[i]);
            }
            let wide_x = (acc.max_x - acc.min_x) >= (acc.max_y - acc.min_y);
            idx.sort_by(|&a, &b| {
                let (ca, cb) = (boxes[a].center(), boxes[b].center());
                let (ka, kb) = if wide_x { (ca.0, cb.0) } else { (ca.1, cb.1) };
                // Total order even with the odd NaN-free f64; tiebreak on index for determinism.
                ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal).then(a.cmp(&b))
            });
            let mid = idx.len() / 2;
            let (left, right) = idx.split_at_mut(mid);
            let lo = build_tree(left, boxes)?;
            let ro = build_tree(right, boxes)?;
            let bounds = lo.bounds(boxes).union(&ro.bounds(boxes));
            Some(TreeNode::Branch { bounds, kids: Box::new((lo, ro)) })
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// Bridge to the live sim — turn a sector's authoritative `Sim` into world-framed domains.
//
// The sim computes in **sector-local `f32`** (`0..SECTOR_SIZE`) for precision and determinism; domains
// live in the **absolute galaxy `f64`** frame. This is the one and only coordinate bridge between them,
// so domains built from *adjacent* sectors compose into one seamless field (a ship at the east edge of
// `(0,0)` and one at the west edge of `(1,0)` land right next to each other in world space, and their
// bubbles overlap across the seam exactly as they should).
// ---------------------------------------------------------------------------------------------------

use crate::shard::SectorId;
use crate::sim::{Sim, SECTOR_SIZE, SHIP_R};

/// The world-frame position of a sector-local point.
pub fn world_pos(sector: SectorId, lx: f32, ly: f32) -> (World, World) {
    let s = SECTOR_SIZE as World;
    (sector.sx as World * s + lx as World, sector.sy as World * s + ly as World)
}

impl Bounds {
    /// Every sector this box overlaps. A bubble sitting mid-sector touches **one**; on an edge, **two**;
    /// on a corner, **four** — automatically, with no fixed neighbour ring, and it grows with the bubble.
    /// This is the seamless interest set that replaces `SectorId::neighbors`' fixed 8-ring: crossing a
    /// seam *adds* the neighbour before you reach it and *drops* the old one once your bubble clears it,
    /// so authority slides instead of snapping.
    pub fn sectors(&self) -> Vec<SectorId> {
        let s = SECTOR_SIZE as World;
        let x0 = (self.min_x / s).floor() as i32;
        let x1 = (self.max_x / s).floor() as i32;
        let y0 = (self.min_y / s).floor() as i32;
        let y1 = (self.max_y / s).floor() as i32;
        let mut out = Vec::new();
        for sy in y0..=y1 {
            for sx in x0..=x1 {
                out.push(SectorId::new(sx, sy));
            }
        }
        out
    }
}

impl DomainField {
    /// Build the domain field from a sector's live sim: group every ship by its **authority owner** — a
    /// human ship is owned by its pilot (its id *is* the pilot's NodeId), an NPC fleet unit is owned by
    /// its faction's player (`Ship::owner`) — so a pilot's own ship and their whole faction swarm fold
    /// into **one** domain, anchored on the pilot's own ship. World-framed, so fields from neighbouring
    /// sectors compose seamlessly.
    pub fn from_sim(sim: &Sim, view_radius: World) -> Self {
        use std::collections::BTreeMap;
        struct Acc {
            bounds: Option<Bounds>,
            anchor: Option<(World, World)>,
            n: u32,
        }
        let mut by: BTreeMap<String, Acc> = BTreeMap::new();
        for (id, ship) in &sim.ships {
            // A human's own ship has `owner == None` and is keyed by the pilot id; an NPC carries the
            // faction owner in `Ship::owner`. Either way the authority owner is a player NodeId.
            let owner = ship.owner.clone().unwrap_or_else(|| id.clone());
            let (wx, wy) = world_pos(sim.sector, ship.x, ship.y);
            let b = Bounds::around(wx, wy, SHIP_R as World);
            let acc = by.entry(owner.clone()).or_insert(Acc { bounds: None, anchor: None, n: 0 });
            acc.bounds = Some(match acc.bounds {
                Some(a) => a.union(&b),
                None => b,
            });
            acc.n += 1;
            if *id == owner {
                acc.anchor = Some((wx, wy)); // the pilot's own ship anchors the bubble
            }
        }
        let domains = by
            .into_iter()
            .map(|(owner, acc)| {
                let bounds = acc.bounds.unwrap_or_else(|| Bounds::around(0.0, 0.0, 0.0));
                let anchor = acc.anchor.unwrap_or_else(|| bounds.center());
                let seed = Bounds::around(anchor.0, anchor.1, 0.0);
                let bounds = bounds.union(&seed);
                let interest = bounds.expanded(view_radius);
                Domain { owner, anchor, bounds, interest, entities: acc.n }
            })
            .collect();
        DomainField::new(domains)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal test entity, like `partition::tests::E`.
    struct E {
        owner: &'static str,
        id: u64,
        x: World,
        y: World,
        r: World,
    }
    impl Owned for E {
        fn owner(&self) -> &str {
            self.owner
        }
        fn entity_id(&self) -> u64 {
            self.id
        }
        fn pos(&self) -> (World, World) {
            (self.x, self.y)
        }
        fn radius(&self) -> World {
            self.r
        }
    }

    fn e(owner: &'static str, id: u64, x: World, y: World) -> E {
        E { owner, id, x, y, r: 1.0 }
    }

    #[test]
    fn domain_footprint_follows_the_owned_entities() {
        // A player at the origin with two wingmen out to the east: the footprint must contain all three.
        let owned = [e("p", 0, 0.0, 0.0), e("p", 1, 100.0, 0.0), e("p", 2, 0.0, 50.0)];
        let d = Domain::build("p", (0.0, 0.0), &owned, 200.0);
        assert!(d.bounds.contains_point(0.0, 0.0));
        assert!(d.bounds.contains_point(100.0, 0.0));
        assert!(d.bounds.contains_point(0.0, 50.0));
        assert_eq!(d.entities, 3);
        // Interest is the footprint grown by the view radius. The footprint reaches 101 (the wingman at
        // x=100 plus its radius 1), so interest reaches 301.
        assert!(d.interest.contains_point(101.0 + 199.0, 0.0));
        assert!(!d.interest.contains_point(101.0 + 201.0, 0.0));
    }

    #[test]
    fn the_owner_is_never_outside_its_own_domain() {
        // The seamless property: wherever the anchor is, it is inside the footprint — so the owner is
        // never transited across a boundary, at any position, even with no other owned entities.
        for &(x, y) in &[(0.0, 0.0), (1e9, -7e8), (-3.3e10, 4.4e10)] {
            let d = Domain::build("p", (x, y), std::iter::empty::<&E>(), 10.0);
            assert!(d.bounds.contains_point(x, y), "anchor must lie inside its own domain at ({x},{y})");
        }
    }

    #[test]
    fn overlapping_bubbles_see_each_other_disjoint_ones_do_not() {
        // Two players 100 units apart with view radius 80 -> interest boxes overlap -> mutual interest.
        let near = vec![
            Domain::build("a", (0.0, 0.0), [e("a", 0, 0.0, 0.0)].iter(), 80.0),
            Domain::build("b", (100.0, 0.0), [e("b", 0, 100.0, 0.0)].iter(), 80.0),
        ];
        let field = DomainField::new(near);
        let seen: Vec<&str> = field.interest("a").iter().map(|d| d.owner.as_str()).collect();
        assert_eq!(seen, vec!["b"], "a must see b when their bubbles overlap");
        assert_eq!(field.interest("b").iter().map(|d| d.owner.as_str()).collect::<Vec<_>>(), vec!["a"]);

        // Move b far away: bubbles disjoint -> nobody sees anybody.
        let far = vec![
            Domain::build("a", (0.0, 0.0), [e("a", 0, 0.0, 0.0)].iter(), 80.0),
            Domain::build("b", (1_000_000.0, 0.0), [e("b", 0, 1_000_000.0, 0.0)].iter(), 80.0),
        ];
        let field = DomainField::new(far);
        assert!(field.interest("a").is_empty(), "disjoint bubbles share no interest");
    }

    #[test]
    fn environment_is_claimed_by_the_nearest_domain_and_is_total() {
        // Two players; an asteroid between them is owned by exactly one (the nearer), and the choice is
        // deterministic. Empty space -> None.
        let field = DomainField::new(vec![
            Domain::build("a", (0.0, 0.0), [e("a", 0, 0.0, 0.0)].iter(), 500.0),
            Domain::build("b", (1000.0, 0.0), [e("b", 0, 1000.0, 0.0)].iter(), 500.0),
        ]);
        assert_eq!(field.claim(100.0, 0.0, 10_000.0), Some("a"), "nearer to a");
        assert_eq!(field.claim(900.0, 0.0, 10_000.0), Some("b"), "nearer to b");
        // Exactly-between: deterministic tiebreak on owner string -> "a".
        assert_eq!(field.claim(500.0, 0.0, 10_000.0), Some("a"));
        // Far from everyone, out of reach -> nobody simulates it.
        let lonely = DomainField::new(vec![Domain::build(
            "a",
            (0.0, 0.0),
            [e("a", 0, 0.0, 0.0)].iter(),
            10.0,
        )]);
        assert_eq!(lonely.claim(1e9, 1e9, 100.0), None);
    }

    #[test]
    fn sticky_claim_does_not_flap_in_the_overlap() {
        // An asteroid sitting exactly between two equally-close bubbles must not change owner just
        // because the challenger is marginally closer — only past the hysteresis margin.
        let field = DomainField::new(vec![
            Domain::build("a", (0.0, 0.0), [e("a", 0, 0.0, 0.0)].iter(), 5000.0),
            Domain::build("b", (1000.0, 0.0), [e("b", 0, 1000.0, 0.0)].iter(), 5000.0),
        ]);
        // Point at x=501: b is nearer by a hair, but with a big hysteresis we keep the previous owner a.
        let hysteresis = 1.0e7;
        assert_eq!(field.claim_sticky(501.0, 0.0, 10_000.0, "a", hysteresis), Some("a"));
        // With no hysteresis, it switches to the genuinely-nearest (b).
        assert_eq!(field.claim_sticky(501.0, 0.0, 10_000.0, "a", 0.0), Some("b"));
    }

    #[test]
    fn broadphase_matches_brute_force() {
        // The recursive AABB tree must return exactly the domains a brute-force overlap scan would — the
        // same completeness guarantee aabb.rs holds, at domain scale.
        let mut domains = Vec::new();
        for i in 0..64u64 {
            let x = ((i * 2_654_435_761) % 100_000) as World;
            let y = ((i * 40_503) % 100_000) as World;
            let owner: &'static str = Box::leak(format!("p{i}").into_boxed_str());
            domains.push(Domain::build(owner, (x, y), [e(owner, i, x, y)].iter(), 1500.0));
        }
        let field = DomainField::new(domains);
        for d in field.domains() {
            let mut brute: Vec<&str> = field
                .domains()
                .iter()
                .filter(|o| o.owner != d.owner && o.interest.intersects(&d.interest))
                .map(|o| o.owner.as_str())
                .collect();
            let mut tree: Vec<&str> = field.interest(&d.owner).iter().map(|o| o.owner.as_str()).collect();
            brute.sort();
            tree.sort();
            assert_eq!(tree, brute, "broad-phase interest must equal brute force for {}", d.owner);
        }
    }

    #[test]
    fn bounds_resolve_to_the_sectors_they_touch() {
        let s = SECTOR_SIZE as World;
        // A small box well inside sector (0,0) touches only it.
        assert_eq!(Bounds::around(s * 0.5, s * 0.5, 100.0).sectors(), vec![SectorId::new(0, 0)]);
        // A box straddling the (0,0)|(1,0) seam touches both.
        let edge = Bounds::around(s - 1.0, s * 0.5, 50.0).sectors();
        assert!(edge.contains(&SectorId::new(0, 0)) && edge.contains(&SectorId::new(1, 0)));
        assert_eq!(edge.len(), 2, "an east-edge bubble touches exactly two sectors");
        // A box on the corner touches four.
        assert_eq!(Bounds::around(s - 1.0, s - 1.0, 50.0).sectors().len(), 4);
    }

    #[test]
    fn from_sim_builds_a_world_framed_domain_per_pilot_and_folds_in_their_fleet() {
        // A pilot's own ship plus their NPC fleet units must collapse into ONE domain, anchored on the
        // pilot, positioned in the absolute world frame of the sim's sector.
        let mut sim = Sim::for_sector(SectorId::new(1, 0), std::sync::Arc::new(crate::ruleset::Ruleset::builtin()));
        sim.factions.clear(); // start clean, then add exactly what we assert on
        sim.join("pilot", "Ace", 0);
        let field = DomainField::from_sim(&sim, 1000.0);
        let d = field.get("pilot").expect("the pilot has a domain");
        // Anchor is in sector (1,0)'s world frame: x >= SECTOR_SIZE.
        assert!(d.anchor.0 >= SECTOR_SIZE as World, "domain anchored in the sim's world-frame sector");
        assert!(d.bounds.contains_point(d.anchor.0, d.anchor.1), "pilot inside its own domain (seamless)");
        assert!(d.entities >= 1);
    }

    #[test]
    fn from_entities_groups_by_owner() {
        let entities = vec![
            e("a", 0, 0.0, 0.0),
            e("a", 1, 10.0, 0.0),
            e("b", 0, 5000.0, 0.0),
            E { owner: UNOWNED, id: 99, x: 1.0, y: 1.0, r: 1.0 }, // a rock — no bubble of its own
        ];
        let field = DomainField::from_entities(&entities, 100.0, |o| match o {
            "a" => (0.0, 0.0),
            "b" => (5000.0, 0.0),
            _ => (0.0, 0.0),
        });
        assert_eq!(field.domains().len(), 2, "two players, the rock gets no domain");
        assert_eq!(field.get("a").unwrap().entities, 2);
        // The rock is claimed by a (nearest), i.e. simulated on a's node for everyone.
        assert_eq!(field.claim(1.0, 1.0, 1e6), Some("a"));
        assert_eq!(field.total_entities(), 3);
    }
}
