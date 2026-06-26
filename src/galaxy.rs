//! The **adaptive galaxy** — an infinite quadtree of authoritative cells that subdivides where the
//! crowds are and merges where they aren't, so compute follows population automatically.
//!
//! This is the heart of planet-scale play. Space is not a fixed grid of sectors; it is a quadtree.
//! Each **leaf cell** is one authoritative simulation running on one mesh node, sized so its load fits
//! a single node's tick budget. When a cell gets hot — too many players, too many entities, tick time
//! creeping toward the frame deadline — it **splits into four children**, each handed to a (possibly
//! different) node, and the load halves. When four children all go cold, they **merge** back into one.
//!
//! So an empty galaxy is a single enormous cell on a single node; a 10,000-ship battle is a deep funnel
//! of subdivision spread across dozens of nodes — and everything in between, continuously, with no
//! coordinator. The split/merge decision is **deterministic** from the cell's own measured load plus the
//! chain beacon epoch, so every replica computes the *same* verdict at the *same* tick — exactly like
//! deterministic failover takeover ([`crate::replication`]) — and no two nodes ever disagree about the
//! shape of the galaxy.
//!
//! ```text
//!                       depth 0  (whole galaxy, ~empty space)
//!                          │  population rises ──► SPLIT
//!         ┌──────────┬─────┴────┬──────────┐
//!      depth1 NW   depth1 NE  depth1 SW   depth1 SE        each on its own node
//!                    │ a battle forms here ──► SPLIT again
//!            ┌────┬──┴─┬────┐
//!          d2   d2   d2   d2   ...recurse until each leaf fits one node's budget
//! ```

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// World units. The galaxy plane is signed and effectively unbounded; the root cell covers the whole
/// addressable plane and subdivides down toward the players. One unit ≈ one in-sim metre.
pub type World = f64;

/// The side length of the **root** cell (depth 0). Astronomically large on purpose: the playable galaxy
/// is carved out of it by subdivision, never by a fixed border. ~1.1e15 units across.
pub const ROOT_SPAN: World = 1_125_899_906_842_624.0; // 2^50, so depth d cells are clean powers of two.

/// The deepest we ever subdivide. Capped at 30 so a cell's per-axis index stays within [`CellId`]'s
/// `u32` coordinates (a depth-d axis has up to 2^d cells); at depth 30 a cell is ~1 million units down to
/// ~2^20 across — already far finer than any battle needs (real play lives at depths 0-8). The cap just
/// bounds the address space and keeps [`CellId::morton`] inside a u128.
pub const MAX_DEPTH: u8 = 30;

/// An axis-aligned world rectangle (a cell's footprint).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldRect {
    pub x: World,
    pub y: World,
    pub span: World,
}

impl WorldRect {
    pub fn contains(&self, wx: World, wy: World) -> bool {
        wx >= self.x && wx < self.x + self.span && wy >= self.y && wy < self.y + self.span
    }
    pub fn center(&self) -> (World, World) {
        (self.x + self.span * 0.5, self.y + self.span * 0.5)
    }
    /// Grow the rect by `pad` on every side (used for interest/AOI queries that bleed across edges).
    pub fn expanded(&self, pad: World) -> WorldRect {
        WorldRect { x: self.x - pad, y: self.y - pad, span: self.span + pad * 2.0 }
    }
}

/// A cell's address in the quadtree: a depth plus integer (x, y) at that depth. Depth 0 is the root;
/// each step down doubles the resolution. This is a *stable name* — the same patch of space always has
/// the same `CellId` at a given depth, so any node can name, hash, route to, and reason about a cell
/// without asking anyone. The galaxy's shape is just "which CellIds are currently leaves".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CellId {
    pub depth: u8,
    pub x: u32,
    pub y: u32,
}

impl CellId {
    pub const ROOT: CellId = CellId { depth: 0, x: 0, y: 0 };

    /// The four children (NW, NE, SW, SE) one level finer. Empty at [`MAX_DEPTH`].
    pub fn children(&self) -> Vec<CellId> {
        if self.depth >= MAX_DEPTH {
            return Vec::new();
        }
        let (d, x, y) = (self.depth + 1, self.x * 2, self.y * 2);
        vec![
            CellId { depth: d, x, y },
            CellId { depth: d, x: x + 1, y },
            CellId { depth: d, x, y: y + 1 },
            CellId { depth: d, x: x + 1, y: y + 1 },
        ]
    }

    /// The parent one level coarser (`None` for the root). Four siblings share a parent and merge into it.
    pub fn parent(&self) -> Option<CellId> {
        (self.depth > 0).then(|| CellId { depth: self.depth - 1, x: self.x / 2, y: self.y / 2 })
    }

    /// The three siblings that merge with this cell.
    pub fn siblings(&self) -> Vec<CellId> {
        match self.parent() {
            Some(p) => p.children().into_iter().filter(|c| c != self).collect(),
            None => Vec::new(),
        }
    }

    /// The world footprint of this cell.
    pub fn rect(&self) -> WorldRect {
        let span = ROOT_SPAN / (1u64 << self.depth) as World;
        // Center the addressable plane on the origin so the galaxy grows symmetrically around (0,0).
        let half = ROOT_SPAN * 0.5;
        WorldRect { x: self.x as World * span - half, y: self.y as World * span - half, span }
    }

    /// Which leaf-or-not cell at `depth` covers a world point. The galaxy resolves a *point* to its
    /// *current* authoritative leaf by walking [`Galaxy::leaf_at`] from here.
    pub fn at_depth_for(depth: u8, wx: World, wy: World) -> CellId {
        let span = ROOT_SPAN / (1u64 << depth) as World;
        let half = ROOT_SPAN * 0.5;
        let x = (((wx + half) / span).floor() as i64).clamp(0, (1i64 << depth) - 1) as u32;
        let y = (((wy + half) / span).floor() as i64).clamp(0, (1i64 << depth) - 1) as u32;
        CellId { depth, x, y }
    }

    /// Same-depth neighbours (the up-to-8 ring), for cross-cell interest and seamless transit. Cells on
    /// the addressable edge simply have fewer neighbours.
    pub fn ring(&self) -> Vec<CellId> {
        let max = 1u32 << self.depth;
        let mut out = Vec::with_capacity(8);
        for dy in [-1i64, 0, 1] {
            for dx in [-1i64, 0, 1] {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let nx = self.x as i64 + dx;
                let ny = self.y as i64 + dy;
                if nx >= 0 && ny >= 0 && (nx as u32) < max && (ny as u32) < max {
                    out.push(CellId { depth: self.depth, x: nx as u32, y: ny as u32 });
                }
            }
        }
        out
    }

    /// A dense, sortable key interleaving depth + position — handy for rendezvous hashing, range scans,
    /// and stable iteration order across the whole mesh.
    pub fn morton(&self) -> u128 {
        let mut key = 0u128;
        for i in 0..32 {
            key |= ((self.x as u128 >> i) & 1) << (2 * i);
            key |= ((self.y as u128 >> i) & 1) << (2 * i + 1);
        }
        (key << 8) | self.depth as u128
    }

    /// The pubsub topic token for this cell (replaces the old fixed `sector` token end to end).
    pub fn token(&self) -> String {
        format!("{}.{}.{}", self.depth, self.x, self.y)
    }

    /// Parse a token produced by [`token`](Self::token) (`"depth.x.y"`). Inverse of `token`; `None` on
    /// malformed input or an out-of-range depth.
    pub fn parse(token: &str) -> Option<CellId> {
        let mut it = token.split('.');
        let depth: u8 = it.next()?.parse().ok()?;
        let x: u32 = it.next()?.parse().ok()?;
        let y: u32 = it.next()?.parse().ok()?;
        if it.next().is_some() || depth > MAX_DEPTH {
            return None;
        }
        Some(CellId { depth, x, y })
    }
}

/// A cell's live load, measured by its host every tick and gossiped on the cell's `/load` topic. This is
/// the *only* input to the split/merge decision, so the decision is reproducible by anyone holding the
/// same `CellLoad` — which is what keeps the galaxy's shape coordinator-free.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CellLoad {
    pub players: u32,
    pub entities: u32,
    /// 99th-percentile tick time in microseconds — the real "am I keeping up?" signal.
    pub tick_p99_us: u32,
    /// Outbound snapshot bandwidth, bytes/sec — the other thing that pins a host.
    pub bandwidth_bps: u64,
    /// The beacon epoch this measurement belongs to, so split/merge is decided on a shared clock.
    pub epoch: u64,
}

/// Tunables for when a cell splits or merges. Hysteresis (split high, merge low) prevents flapping at
/// the boundary; the same numbers on every node make every node agree.
#[derive(Debug, Clone, Copy)]
pub struct ScalePolicy {
    pub split_players: u32,
    pub split_entities: u32,
    pub split_tick_us: u32,
    pub split_bandwidth_bps: u64,
    /// A leaf merges only when ALL four siblings are this far below the split thresholds.
    pub merge_fraction: f32,
    /// Consecutive agreeing epochs required before acting — debounces a single noisy tick.
    pub dwell_epochs: u8,
}

impl Default for ScalePolicy {
    fn default() -> Self {
        // Calibrated for a commodity donor core hosting a 20 Hz cell with viewport-scoped snapshots.
        ScalePolicy {
            split_players: 150,
            split_entities: 4_000,
            split_tick_us: 35_000, // 35 ms of a 50 ms budget → split before we miss frames
            split_bandwidth_bps: 12_000_000, // ~12 MB/s up
            merge_fraction: 0.30,
            dwell_epochs: 3,
        }
    }
}

/// What a cell should do about its current load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Hold,
    Split,
    /// Merge this cell and its three siblings back into the parent.
    Merge,
}

impl CellLoad {
    /// The deterministic verdict for a leaf under `policy`. Pure — same inputs, same answer, everywhere.
    pub fn verdict(&self, depth: u8, policy: &ScalePolicy) -> Verdict {
        let hot = self.players >= policy.split_players
            || self.entities >= policy.split_entities
            || self.tick_p99_us >= policy.split_tick_us
            || self.bandwidth_bps >= policy.split_bandwidth_bps;
        if hot && depth < MAX_DEPTH {
            return Verdict::Split;
        }
        let f = policy.merge_fraction;
        let cold = (self.players as f32) < policy.split_players as f32 * f
            && (self.entities as f32) < policy.split_entities as f32 * f
            && (self.tick_p99_us as f32) < policy.split_tick_us as f32 * f;
        if cold && depth > 0 {
            Verdict::Merge
        } else {
            Verdict::Hold
        }
    }
}

/// A node's local view of the **galaxy shape** — the current set of leaf cells — assembled from the
/// `/galaxy` gossip topic. It is eventually-consistent and self-healing: every host advertises the
/// leaves it runs, every controller advertises the splits/merges it commits, and `leaf_at` always
/// resolves a point to *some* live leaf even mid-reshape (it walks down to the finest advertised leaf
/// covering the point, and up to the nearest live ancestor if a child hasn't been claimed yet).
#[derive(Debug, Default, Clone)]
pub struct Galaxy {
    leaves: BTreeSet<CellId>,
}

impl Galaxy {
    /// A brand-new galaxy: one root leaf covering everything. The first player spawns into it; it
    /// subdivides from there.
    pub fn genesis() -> Self {
        let mut g = Galaxy::default();
        g.leaves.insert(CellId::ROOT);
        g
    }

    pub fn leaves(&self) -> impl Iterator<Item = &CellId> {
        self.leaves.iter()
    }

    pub fn is_leaf(&self, c: &CellId) -> bool {
        self.leaves.contains(c)
    }

    /// Apply a committed split: the parent leaf becomes four child leaves.
    pub fn split(&mut self, c: CellId) {
        if self.leaves.remove(&c) {
            for child in c.children() {
                self.leaves.insert(child);
            }
        }
    }

    /// Apply a committed merge: four sibling leaves collapse into the parent leaf.
    pub fn merge(&mut self, parent: CellId) {
        for child in parent.children() {
            self.leaves.remove(&child);
        }
        self.leaves.insert(parent);
    }

    /// Resolve a world point to the live authoritative leaf that currently owns it. Walks from the
    /// finest possible cell upward to the first ancestor that is a live leaf, so it is correct even
    /// while a region is mid-split (children not all claimed) — there is always exactly one answer.
    pub fn leaf_at(&self, wx: World, wy: World) -> CellId {
        let mut c = CellId::at_depth_for(MAX_DEPTH, wx, wy);
        loop {
            if self.leaves.contains(&c) {
                return c;
            }
            match c.parent() {
                Some(p) => c = p,
                None => return CellId::ROOT,
            }
        }
    }

    /// The interest set for a player at a point: their leaf plus the live leaves touching their view
    /// radius. Because neighbours can be at different depths (a fine battle cell next to a coarse empty
    /// cell), this resolves each ring direction to whatever live leaf covers it — seamless across the
    /// depth seam.
    pub fn interest(&self, wx: World, wy: World, view_radius: World) -> BTreeSet<CellId> {
        let mut set = BTreeSet::new();
        let here = self.leaf_at(wx, wy);
        set.insert(here);
        let r = view_radius;
        for (ox, oy) in [(-r, -r), (0.0, -r), (r, -r), (-r, 0.0), (r, 0.0), (-r, r), (0.0, r), (r, r)] {
            set.insert(self.leaf_at(wx + ox, wy + oy));
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_parent_roundtrip() {
        let c = CellId { depth: 3, x: 5, y: 6 };
        for ch in c.children() {
            assert_eq!(ch.parent(), Some(c));
        }
    }

    #[test]
    fn point_resolves_into_its_cell_rect() {
        let c = CellId::at_depth_for(10, 1234.0, -5678.0);
        assert!(c.rect().contains(1234.0, -5678.0));
    }

    #[test]
    fn hot_cell_splits_cold_cell_merges() {
        let p = ScalePolicy::default();
        let hot = CellLoad { players: 500, entities: 50, tick_p99_us: 1000, bandwidth_bps: 0, epoch: 1 };
        assert_eq!(hot.verdict(4, &p), Verdict::Split);
        let cold = CellLoad { players: 1, entities: 1, tick_p99_us: 10, bandwidth_bps: 0, epoch: 1 };
        assert_eq!(cold.verdict(4, &p), Verdict::Merge);
        assert_eq!(cold.verdict(0, &p), Verdict::Hold, "root never merges upward");
    }

    #[test]
    fn galaxy_split_then_resolve() {
        let mut g = Galaxy::genesis();
        g.split(CellId::ROOT);
        let leaf = g.leaf_at(1.0, 1.0);
        assert_eq!(leaf.depth, 1, "after splitting root, points resolve to depth-1 leaves");
        assert!(g.is_leaf(&leaf));
    }
}
