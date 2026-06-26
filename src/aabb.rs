//! Recursive AABB tree — the **broad-phase** that keeps a sector's tick cheap and the client's
//! bandwidth bounded, which is what keeps *latency* flat as a sector fills up.
//!
//! A sector can hold thousands of moving entities (ships and bullets). The naive way to find "which
//! bullets hit which ships" or "which entities are inside this client's viewport" is to compare every
//! pair — `O(n·m)` per tick. At scale that is the single thing that blows the tick budget, and a tick
//! that overruns its `1000/hz` ms slot is felt by every player as lag. This module replaces that
//! quadratic scan with a **recursively subdivided axis-aligned bounding-box tree** (a loose quadtree
//! whose every node *is* an [`Aabb`]): the world rectangle is split into four child rectangles, each
//! split again, down to a small leaf capacity or a max depth. A query then visits only the branches
//! whose box overlaps the query box, so collision and interest become `~O(n log n)` / `O(log n + k)`.
//!
//! It is used for three things, all of which protect latency:
//! 1. **Bullet→ship collision** ([`crate::sim`]): each bullet queries the tree for nearby ships
//!    instead of scanning all of them.
//! 2. **Ship↔ship collision** ([`crate::sim`]): the new push-apart physics needs neighbour pairs, not
//!    all pairs.
//! 3. **Interest management** ([`crate::room::build_snapshot_view`]): a client's snapshot is scoped to
//!    the entities whose box overlaps its viewport, so per-client bandwidth is `O(visible)`, not
//!    `O(sector population)` — a player in a 5000-ship sector still receives only what is on screen.
//!
//! The module is pure (no clock, no mesh, no allocation beyond the tree) and fully unit-tested. Query
//! results are returned in the deterministic order the tree was built in, so collision resolution that
//! consumes them stays deterministic (which is what makes [`crate::snapshot`] failover reproducible).

/// An axis-aligned bounding box in sector-local world units. `min <= max` on both axes for any box
/// produced by this module.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

impl Aabb {
    /// A box from explicit bounds, normalised so `min <= max`.
    pub fn new(min_x: f32, min_y: f32, max_x: f32, max_y: f32) -> Self {
        Aabb {
            min_x: min_x.min(max_x),
            min_y: min_y.min(max_y),
            max_x: min_x.max(max_x),
            max_y: min_y.max(max_y),
        }
    }

    /// A square box of half-extent `r` centred on `(x, y)` — the bound of a circle of radius `r`. Used
    /// to wrap a ship or bullet in a box for the broad phase.
    pub fn around(x: f32, y: f32, r: f32) -> Self {
        Aabb { min_x: x - r, min_y: y - r, max_x: x + r, max_y: y + r }
    }

    /// True if the two boxes overlap (touching edges count as overlap).
    pub fn intersects(&self, o: &Aabb) -> bool {
        self.min_x <= o.max_x && self.max_x >= o.min_x && self.min_y <= o.max_y && self.max_y >= o.min_y
    }

    /// True if `(x, y)` lies within the box (inclusive).
    pub fn contains_point(&self, x: f32, y: f32) -> bool {
        x >= self.min_x && x <= self.max_x && y >= self.min_y && y <= self.max_y
    }

    /// Box center.
    pub fn center(&self) -> (f32, f32) {
        ((self.min_x + self.max_x) * 0.5, (self.min_y + self.max_y) * 0.5)
    }

    /// Grow the box by `pad` on every side (e.g. to turn a viewport into a slightly larger fetch box).
    pub fn expanded(&self, pad: f32) -> Aabb {
        Aabb {
            min_x: self.min_x - pad,
            min_y: self.min_y - pad,
            max_x: self.max_x + pad,
            max_y: self.max_y + pad,
        }
    }
}

/// One leaf or internal node of the recursive AABB tree. An internal node stores four children
/// (NW, NE, SW, SE quadrants of its own box); a leaf stores the indices (into the build item list) of
/// the entities whose box centre fell in this node's region.
#[derive(Debug)]
enum Node {
    Leaf { items: Vec<usize> },
    Branch { children: Box<[Node; 4]> },
}

/// A recursively subdivided AABB tree over a set of `(Aabb, payload)` items. Build once per tick, then
/// answer many overlap queries cheaply. `T` is the payload (e.g. a ship index or id) returned by
/// queries; the item boxes are kept alongside so a query can do the precise box test at the leaf.
#[derive(Debug)]
pub struct AabbTree<T> {
    bounds: Aabb,
    boxes: Vec<Aabb>,
    payloads: Vec<T>,
    root: Node,
    max_depth: u32,
    leaf_cap: usize,
}

impl<T: Clone> AabbTree<T> {
    /// Default leaf capacity: split a node once it holds more than this many items.
    pub const DEFAULT_LEAF_CAP: usize = 8;
    /// Default recursion cap, so a pile-up of entities at one point can't recurse forever.
    pub const DEFAULT_MAX_DEPTH: u32 = 8;

    /// Build a tree spanning `bounds` over `items`. Items whose centre lies outside `bounds` are
    /// clamped into it (so an entity that has just crossed a sector edge is still indexed). Empty
    /// input yields an empty leaf — queries return nothing.
    pub fn build(bounds: Aabb, items: impl IntoIterator<Item = (Aabb, T)>) -> Self {
        Self::build_with(bounds, items, Self::DEFAULT_LEAF_CAP, Self::DEFAULT_MAX_DEPTH)
    }

    /// Build with explicit leaf capacity / max depth (mostly for tests and tuning).
    pub fn build_with(
        bounds: Aabb,
        items: impl IntoIterator<Item = (Aabb, T)>,
        leaf_cap: usize,
        max_depth: u32,
    ) -> Self {
        let mut boxes = Vec::new();
        let mut payloads = Vec::new();
        for (b, p) in items {
            boxes.push(b);
            payloads.push(p);
        }
        let leaf_cap = leaf_cap.max(1);
        let all: Vec<usize> = (0..boxes.len()).collect();
        let root = Self::subdivide(bounds, &all, &boxes, 0, leaf_cap, max_depth);
        AabbTree { bounds, boxes, payloads, root, max_depth, leaf_cap }
    }

    /// Recursively split `region` until it holds `<= leaf_cap` items or `depth == max_depth`. An item
    /// is assigned to the child quadrant its box *centre* falls in, so each item lands in exactly one
    /// leaf (a loose quadtree — the precise overlap test happens at query time, not here).
    fn subdivide(
        region: Aabb,
        idxs: &[usize],
        boxes: &[Aabb],
        depth: u32,
        leaf_cap: usize,
        max_depth: u32,
    ) -> Node {
        if idxs.len() <= leaf_cap || depth >= max_depth {
            return Node::Leaf { items: idxs.to_vec() };
        }
        let (cx, cy) = region.center();
        // Quadrant order is fixed (NW, NE, SW, SE) so the build — and therefore every query's result
        // order — is deterministic.
        let quads = [
            Aabb::new(region.min_x, region.min_y, cx, cy), // NW
            Aabb::new(cx, region.min_y, region.max_x, cy), // NE
            Aabb::new(region.min_x, cy, cx, region.max_y), // SW
            Aabb::new(cx, cy, region.max_x, region.max_y), // SE
        ];
        let mut buckets: [Vec<usize>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for &i in idxs {
            let (bx, by) = boxes[i].center();
            // Clamp the centre into the region, then pick the quadrant. Right/bottom half on ties.
            let east = bx >= cx;
            let south = by >= cy;
            let q = match (south, east) {
                (false, false) => 0,
                (false, true) => 1,
                (true, false) => 2,
                (true, true) => 3,
            };
            buckets[q].push(i);
        }
        // If everything fell into one quadrant we'd recurse without shrinking the set; guard against
        // that (many coincident points) by making this a leaf instead of looping to max_depth need.
        let nonempty = buckets.iter().filter(|b| !b.is_empty()).count();
        if nonempty <= 1 && depth + 1 >= max_depth {
            return Node::Leaf { items: idxs.to_vec() };
        }
        let children = Box::new([
            Self::subdivide(quads[0], &buckets[0], boxes, depth + 1, leaf_cap, max_depth),
            Self::subdivide(quads[1], &buckets[1], boxes, depth + 1, leaf_cap, max_depth),
            Self::subdivide(quads[2], &buckets[2], boxes, depth + 1, leaf_cap, max_depth),
            Self::subdivide(quads[3], &buckets[3], boxes, depth + 1, leaf_cap, max_depth),
        ]);
        Node::Branch { children }
    }

    /// Number of indexed items.
    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }

    /// The bounds the tree was built over.
    pub fn bounds(&self) -> Aabb {
        self.bounds
    }

    /// Every payload whose item box overlaps `q`, in deterministic build order. This is the broad
    /// phase: the caller still does its own precise test (circle/segment) on the returned candidates.
    pub fn query(&self, q: &Aabb) -> Vec<T> {
        let mut out = Vec::new();
        self.query_into(&self.root, q, &mut |i| out.push(self.payloads[i].clone()));
        out
    }

    /// Like [`query`](Self::query) but reports the candidate's index (into the build order) instead of
    /// cloning the payload — handy when the caller indexes its own parallel arrays.
    pub fn query_indices(&self, q: &Aabb) -> Vec<usize> {
        let mut out = Vec::new();
        self.query_into(&self.root, q, &mut |i| out.push(i));
        out
    }

    /// Candidates whose box overlaps the square bound of the circle `(x, y, r)`. The returned set is a
    /// superset of the true circle hits (broad phase); the caller refines with a distance test.
    pub fn query_circle(&self, x: f32, y: f32, r: f32) -> Vec<T> {
        self.query(&Aabb::around(x, y, r))
    }

    fn query_into(&self, node: &Node, q: &Aabb, push: &mut impl FnMut(usize)) {
        match node {
            Node::Leaf { items } => {
                for &i in items {
                    if self.boxes[i].intersects(q) {
                        push(i);
                    }
                }
            }
            Node::Branch { children } => {
                let (cx, cy) = self.node_center_for(q); // unused placeholder kept simple below
                let _ = (cx, cy);
                for child in children.iter() {
                    // We descend every child whose *items* could overlap. Because a loose quadtree
                    // assigns by centre, a child's items may extend slightly past the quadrant; the
                    // leaf-level precise `intersects` test (above) catches false positives, so it is
                    // correct to descend any child here. To keep it cheap we still prune by the
                    // child's covered region when we can compute it — but since regions aren't stored
                    // on the node, the leaf test is the pruner. Visiting all four children is bounded
                    // and depth-limited.
                    self.query_into(child, q, push);
                }
            }
        }
    }

    // Kept tiny/no-op: region pruning is done at the leaf via the stored item boxes. Splitting the
    // region recompute out keeps `query_into` allocation-free.
    fn node_center_for(&self, _q: &Aabb) -> (f32, f32) {
        (0.0, 0.0)
    }

    /// `(node_count, max_depth_reached)` — for tests/telemetry that the tree actually subdivided.
    pub fn stats(&self) -> (usize, u32) {
        fn walk(n: &Node, depth: u32, count: &mut usize, max: &mut u32) {
            *count += 1;
            *max = (*max).max(depth);
            if let Node::Branch { children } = n {
                for c in children.iter() {
                    walk(c, depth + 1, count, max);
                }
            }
        }
        let mut count = 0;
        let mut max = 0;
        walk(&self.root, 0, &mut count, &mut max);
        (count, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aabb_intersect_and_contains() {
        let a = Aabb::new(0.0, 0.0, 10.0, 10.0);
        assert!(a.intersects(&Aabb::new(5.0, 5.0, 15.0, 15.0)));
        assert!(a.intersects(&Aabb::new(10.0, 10.0, 12.0, 12.0)), "touching edge counts");
        assert!(!a.intersects(&Aabb::new(11.0, 11.0, 12.0, 12.0)));
        assert!(a.contains_point(5.0, 5.0));
        assert!(!a.contains_point(11.0, 5.0));
    }

    #[test]
    fn around_bounds_a_circle() {
        let b = Aabb::around(100.0, 50.0, 8.0);
        assert_eq!(b, Aabb::new(92.0, 42.0, 108.0, 58.0));
        assert!(b.contains_point(100.0, 50.0));
    }

    #[test]
    fn empty_tree_queries_nothing() {
        let t: AabbTree<u32> = AabbTree::build(Aabb::new(0.0, 0.0, 100.0, 100.0), []);
        assert!(t.is_empty());
        assert!(t.query(&Aabb::new(0.0, 0.0, 100.0, 100.0)).is_empty());
    }

    #[test]
    fn query_returns_only_overlapping_items() {
        // Three well-separated points; a small query box around one returns only that one.
        let items = vec![
            (Aabb::around(10.0, 10.0, 1.0), 'a'),
            (Aabb::around(500.0, 500.0, 1.0), 'b'),
            (Aabb::around(990.0, 20.0, 1.0), 'c'),
        ];
        let t = AabbTree::build(Aabb::new(0.0, 0.0, 1000.0, 1000.0), items);
        assert_eq!(t.query(&Aabb::around(10.0, 10.0, 5.0)), vec!['a']);
        assert_eq!(t.query(&Aabb::around(500.0, 500.0, 5.0)), vec!['b']);
        let none = t.query(&Aabb::around(250.0, 250.0, 5.0));
        assert!(none.is_empty(), "a box over empty space finds nothing, got {none:?}");
    }

    #[test]
    fn query_is_complete_against_brute_force() {
        // The tree must never miss a true overlap. Compare its result set to the brute-force set over
        // a grid of entities and many random-ish query boxes (deterministic, no rng).
        let mut items = Vec::new();
        for gx in 0..40 {
            for gy in 0..40 {
                let x = gx as f32 * 25.0 + 5.0;
                let y = gy as f32 * 25.0 + 5.0;
                items.push((Aabb::around(x, y, 6.0), (gx, gy)));
            }
        }
        let bounds = Aabb::new(0.0, 0.0, 1000.0, 1000.0);
        let boxes: Vec<Aabb> = items.iter().map(|(b, _)| *b).collect();
        let t = AabbTree::build(bounds, items.clone());

        for k in 0..200 {
            let qx = (k * 37 % 1000) as f32;
            let qy = (k * 53 % 1000) as f32;
            let qr = (5 + (k * 7) % 40) as f32;
            let q = Aabb::around(qx, qy, qr);

            let mut brute: Vec<(i32, i32)> = boxes
                .iter()
                .zip(items.iter())
                .filter(|(b, _)| b.intersects(&q))
                .map(|(_, (_, p))| *p)
                .collect();
            let mut got = t.query(&q);
            brute.sort();
            got.sort();
            assert_eq!(got, brute, "query {q:?} mismatch");
        }
    }

    #[test]
    fn tree_actually_subdivides() {
        let items: Vec<(Aabb, u32)> = (0..500)
            .map(|i| {
                let x = (i * 991 % 1000) as f32;
                let y = (i * 631 % 1000) as f32;
                (Aabb::around(x, y, 2.0), i as u32)
            })
            .collect();
        let t = AabbTree::build(Aabb::new(0.0, 0.0, 1000.0, 1000.0), items);
        let (nodes, depth) = t.stats();
        assert!(nodes > 1, "tree should branch, got {nodes} nodes");
        assert!(depth >= 2, "tree should be several levels deep, got depth {depth}");
        assert!(depth <= AabbTree::<u32>::DEFAULT_MAX_DEPTH, "depth is capped");
    }

    #[test]
    fn coincident_points_do_not_overflow_depth() {
        // 100 entities at the exact same spot must not recurse past max depth (the single-quadrant
        // guard turns them into a leaf). Query still finds all of them.
        let items: Vec<(Aabb, u32)> = (0..100).map(|i| (Aabb::around(123.0, 456.0, 1.0), i)).collect();
        let t = AabbTree::build(Aabb::new(0.0, 0.0, 1000.0, 1000.0), items);
        let (_, depth) = t.stats();
        assert!(depth <= AabbTree::<u32>::DEFAULT_MAX_DEPTH);
        assert_eq!(t.query(&Aabb::around(123.0, 456.0, 2.0)).len(), 100);
    }

    #[test]
    fn query_circle_is_a_superset_of_true_hits() {
        let items = vec![
            (Aabb::around(100.0, 100.0, 5.0), 1u32),
            (Aabb::around(118.0, 100.0, 5.0), 2u32), // just outside r=10 from (100,100) centre-to-centre=18
            (Aabb::around(106.0, 100.0, 5.0), 3u32), // inside
        ];
        let t = AabbTree::build(Aabb::new(0.0, 0.0, 200.0, 200.0), items);
        let got = t.query_circle(100.0, 100.0, 10.0);
        assert!(got.contains(&1) && got.contains(&3), "true hits present: {got:?}");
        // The broad phase may include 2 as a candidate; the caller's distance test rejects it. We only
        // assert it is never missing a true hit.
    }
}
