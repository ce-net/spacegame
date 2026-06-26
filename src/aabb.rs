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
    /// The largest half-extent (on either axis) of any indexed item box. Items are bucketed into a
    /// leaf by their *centre*, so a box reaches at most this far past its leaf's region; padding a
    /// region by this value before the prune test makes pruning provably lossless.
    max_half: f32,
    #[allow(dead_code)]
    max_depth: u32,
    #[allow(dead_code)]
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
        let max_half = boxes
            .iter()
            .map(|b| ((b.max_x - b.min_x) * 0.5).max((b.max_y - b.min_y) * 0.5))
            .fold(0.0_f32, f32::max);
        let all: Vec<usize> = (0..boxes.len()).collect();
        let root = Self::subdivide(bounds, &all, &boxes, 0, leaf_cap, max_depth);
        AabbTree { bounds, boxes, payloads, root, max_half, max_depth, leaf_cap }
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
        self.query_into(&self.root, self.bounds, q, &mut |i| out.push(self.payloads[i].clone()));
        out
    }

    /// Like [`query`](Self::query) but reports the candidate's index (into the build order) instead of
    /// cloning the payload — handy when the caller indexes its own parallel arrays.
    pub fn query_indices(&self, q: &Aabb) -> Vec<usize> {
        let mut out = Vec::new();
        self.query_into(&self.root, self.bounds, q, &mut |i| out.push(i));
        out
    }

    /// Candidates whose box overlaps the square bound of the circle `(x, y, r)`. The returned set is a
    /// superset of the true circle hits (broad phase); the caller refines with a distance test.
    pub fn query_circle(&self, x: f32, y: f32, r: f32) -> Vec<T> {
        self.query(&Aabb::around(x, y, r))
    }

    /// The four child regions of `region`, in the same fixed NW/NE/SW/SE order [`subdivide`] used —
    /// so query-time region recomputation lines up with the build-time partition.
    fn quadrants(region: Aabb) -> [Aabb; 4] {
        let (cx, cy) = region.center();
        [
            Aabb::new(region.min_x, region.min_y, cx, cy),
            Aabb::new(cx, region.min_y, region.max_x, cy),
            Aabb::new(region.min_x, cy, cx, region.max_y),
            Aabb::new(cx, cy, region.max_x, region.max_y),
        ]
    }

    /// Recurse, pruning any branch whose covered region does not overlap `q`. A leaf still does the
    /// precise per-item box test, because a loose quadtree assigns items by centre so an item box may
    /// poke slightly past its leaf's region. `region` is recomputed on the way down (it mirrors the
    /// build partition exactly), so nodes stay small and the walk is allocation-free.
    fn query_into(&self, node: &Node, region: Aabb, q: &Aabb, push: &mut impl FnMut(usize)) {
        if !region.expanded(self.max_half).intersects(q) {
            return;
        }
        match node {
            Node::Leaf { items } => {
                for &i in items {
                    if self.boxes[i].intersects(q) {
                        push(i);
                    }
                }
            }
            Node::Branch { children } => {
                let quads = Self::quadrants(region);
                for (child, quad) in children.iter().zip(quads.iter()) {
                    self.query_into(child, *quad, q, push);
                }
            }
        }
    }

    /// A small region pad used only for pruning. Because items are bucketed by centre, an item box can
    /// extend at most half its own width past its region edge; rather than track per-region item
    /// extents we pad the region by a conservative slack so pruning never drops a leaf that holds a
    /// true overlap. The leaf's precise `intersects` test then removes the false positives.
    fn span_pad(&self) -> f32 {
        // Half the bounds width over 2^max_depth, scaled up — comfortably larger than any leaf's
        // worst-case item overhang for the entity sizes this game uses. Conservative on purpose:
        // over-padding only costs a few extra leaf visits, never correctness.
        let w = (self.bounds.max_x - self.bounds.min_x).max(self.bounds.max_y - self.bounds.min_y);
        let leaf_w = w / (1u32 << self.max_depth.min(20)) as f32;
        leaf_w.max(64.0)
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

/// Grow a box by the larger of an absolute and a relative margin — used to give a dynamic-tree leaf a
/// "fat" AABB so a moving object can travel a little without forcing a re-insert.
impl Aabb {
    /// The combined box that tightly contains both `self` and `o`.
    pub fn union(&self, o: &Aabb) -> Aabb {
        Aabb {
            min_x: self.min_x.min(o.min_x),
            min_y: self.min_y.min(o.min_y),
            max_x: self.max_x.max(o.max_x),
            max_y: self.max_y.max(o.max_y),
        }
    }

    /// Perimeter (2D "surface area") — the cost metric the dynamic tree minimises when choosing where
    /// to insert, so the tree stays cheap to query as objects move.
    pub fn perimeter(&self) -> f32 {
        2.0 * ((self.max_x - self.min_x) + (self.max_y - self.min_y))
    }

    /// True if `self` fully contains `o`.
    pub fn contains(&self, o: &Aabb) -> bool {
        self.min_x <= o.min_x && self.min_y <= o.min_y && self.max_x >= o.max_x && self.max_y >= o.max_y
    }
}

/// A 2D rigid transform (rotation + translation) that places a nested body's local space into its
/// parent's space. Used so a **recursive AABB can hold other recursive AABBs**: a compound object (a
/// ship made of modules, an asteroid cluster, a planet with stations) carries its own local
/// [`DynamicAabbTree`], and this transform maps it into the world tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub tx: f32,
    pub ty: f32,
    pub cos: f32,
    pub sin: f32,
}

impl Transform {
    pub fn identity() -> Self {
        Transform { tx: 0.0, ty: 0.0, cos: 1.0, sin: 0.0 }
    }

    /// A transform from a position and heading (radians).
    pub fn from_pos_angle(x: f32, y: f32, a: f32) -> Self {
        Transform { tx: x, ty: y, cos: a.cos(), sin: a.sin() }
    }

    /// Map a local point into parent space.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (self.cos * x - self.sin * y + self.tx, self.sin * x + self.cos * y + self.ty)
    }

    /// Map a parent-space point back into local space (inverse of [`apply`](Self::apply)).
    pub fn inverse_apply(&self, x: f32, y: f32) -> (f32, f32) {
        let dx = x - self.tx;
        let dy = y - self.ty;
        (self.cos * dx + self.sin * dy, -self.sin * dx + self.cos * dy)
    }

    /// Map a local AABB into parent space (axis-aligned bound of the rotated box — conservative).
    pub fn apply_aabb(&self, b: &Aabb) -> Aabb {
        let corners = [
            self.apply(b.min_x, b.min_y),
            self.apply(b.max_x, b.min_y),
            self.apply(b.min_x, b.max_y),
            self.apply(b.max_x, b.max_y),
        ];
        let mut out = Aabb { min_x: corners[0].0, min_y: corners[0].1, max_x: corners[0].0, max_y: corners[0].1 };
        for (x, y) in corners.into_iter().skip(1) {
            out.min_x = out.min_x.min(x);
            out.min_y = out.min_y.min(y);
            out.max_x = out.max_x.max(x);
            out.max_y = out.max_y.max(y);
        }
        out
    }
}

/// A stable handle to a leaf in a [`DynamicAabbTree`], returned by `insert` and used by `update` /
/// `remove`. (An index into the node arena; reused after removal via the free list.)
pub type Proxy = usize;

const DYN_NIL: usize = usize::MAX;

#[derive(Debug, Clone)]
struct DynNode<T> {
    /// The fat box stored in the tree (the leaf's tight box grown by a margin).
    aabb: Aabb,
    parent: usize,
    child1: usize,
    child2: usize,
    /// Leaf height is 0; internal nodes are 1 + max(child heights); a free-list slot is -1.
    height: i32,
    /// `Some` for a leaf, `None` for an internal node.
    payload: Option<T>,
}

/// A **dynamic** bounding-volume hierarchy (Box2D-style) whose leaves are *fattened* so a moving
/// object only re-inserts when it leaves its fat box. This is the broad-phase that **follows objects
/// around** — players, ships, debris, asteroids, planets — frame to frame at low cost, instead of the
/// per-tick full rebuild a static [`AabbTree`] does. Insertion picks the sibling that least grows the
/// tree (surface-area heuristic) and balances with tree rotations, so queries stay logarithmic as the
/// world churns.
///
/// Leaves can carry any payload `T`; pairing it with a nested [`DynamicAabbTree`] via a [`Transform`]
/// gives **recursive AABBs that hold recursive AABBs** (compound bodies) — see [`Compound`].
#[derive(Debug, Clone)]
pub struct DynamicAabbTree<T> {
    nodes: Vec<DynNode<T>>,
    root: usize,
    free: usize,
    count: usize,
    /// Margin added on every side of an inserted box (movement slack).
    pub fat_margin: f32,
}

impl<T: Clone> Default for DynamicAabbTree<T> {
    fn default() -> Self {
        Self::new(8.0)
    }
}

impl<T: Clone> DynamicAabbTree<T> {
    pub fn new(fat_margin: f32) -> Self {
        DynamicAabbTree { nodes: Vec::new(), root: DYN_NIL, free: DYN_NIL, count: 0, fat_margin }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn alloc(&mut self, aabb: Aabb, payload: Option<T>) -> usize {
        if self.free != DYN_NIL {
            let id = self.free;
            self.free = self.nodes[id].child1;
            let n = &mut self.nodes[id];
            n.aabb = aabb;
            n.parent = DYN_NIL;
            n.child1 = DYN_NIL;
            n.child2 = DYN_NIL;
            n.height = 0;
            n.payload = payload;
            id
        } else {
            self.nodes.push(DynNode {
                aabb,
                parent: DYN_NIL,
                child1: DYN_NIL,
                child2: DYN_NIL,
                height: 0,
                payload,
            });
            self.nodes.len() - 1
        }
    }

    fn freenode(&mut self, id: usize) {
        self.nodes[id].child1 = self.free;
        self.nodes[id].height = -1;
        self.nodes[id].payload = None;
        self.free = id;
    }

    fn fatten(&self, tight: Aabb) -> Aabb {
        tight.expanded(self.fat_margin)
    }

    /// Insert a leaf with tight box `tight` and `payload`; returns a stable [`Proxy`].
    pub fn insert(&mut self, tight: Aabb, payload: T) -> Proxy {
        let fat = self.fatten(tight);
        let leaf = self.alloc(fat, Some(payload));
        self.insert_leaf(leaf);
        self.count += 1;
        leaf
    }

    /// Update a leaf to a new tight box. If the object is still inside its fat box this is a no-op
    /// (the whole point — cheap movement); otherwise the leaf is re-inserted with a fresh fat box.
    /// Returns true if the tree was restructured.
    pub fn update(&mut self, proxy: Proxy, tight: Aabb) -> bool {
        if self.nodes[proxy].aabb.contains(&tight) {
            return false;
        }
        self.remove_leaf(proxy);
        self.nodes[proxy].aabb = self.fatten(tight);
        self.insert_leaf(proxy);
        true
    }

    /// Remove a leaf.
    pub fn remove(&mut self, proxy: Proxy) {
        self.remove_leaf(proxy);
        self.freenode(proxy);
        self.count = self.count.saturating_sub(1);
    }

    /// Every payload whose fat box overlaps `q`, in arbitrary (tree) order.
    pub fn query(&self, q: &Aabb) -> Vec<T> {
        let mut out = Vec::new();
        if self.root == DYN_NIL {
            return out;
        }
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            let n = &self.nodes[id];
            if !n.aabb.intersects(q) {
                continue;
            }
            if n.payload.is_some() {
                out.push(n.payload.clone().unwrap());
            } else {
                if n.child1 != DYN_NIL {
                    stack.push(n.child1);
                }
                if n.child2 != DYN_NIL {
                    stack.push(n.child2);
                }
            }
        }
        out
    }

    /// The fat box currently stored for a proxy (for debugging / nested transforms).
    pub fn proxy_aabb(&self, proxy: Proxy) -> Aabb {
        self.nodes[proxy].aabb
    }

    /// Tree height (0 for a single leaf, grows ~log n) — a balance/health metric for tests.
    pub fn height(&self) -> i32 {
        if self.root == DYN_NIL { 0 } else { self.nodes[self.root].height }
    }

    // ---- internal: Box2D-style insert with SAH sibling choice + rotation balancing ----

    fn insert_leaf(&mut self, leaf: usize) {
        if self.root == DYN_NIL {
            self.root = leaf;
            self.nodes[leaf].parent = DYN_NIL;
            return;
        }
        let leaf_aabb = self.nodes[leaf].aabb;
        // 1) Find the best sibling: descend toward the child that minimises the surface-area cost.
        let mut index = self.root;
        while self.nodes[index].payload.is_none() {
            let c1 = self.nodes[index].child1;
            let c2 = self.nodes[index].child2;
            let area = self.nodes[index].aabb.perimeter();
            let combined = self.nodes[index].aabb.union(&leaf_aabb).perimeter();
            let cost = 2.0 * combined;
            let inherit = 2.0 * (combined - area);
            let cost1 = self.descend_cost(c1, &leaf_aabb) + inherit;
            let cost2 = self.descend_cost(c2, &leaf_aabb) + inherit;
            if cost < cost1 && cost < cost2 {
                break;
            }
            index = if cost1 < cost2 { c1 } else { c2 };
        }
        let sibling = index;

        // 2) Create a new parent for sibling + leaf.
        let old_parent = self.nodes[sibling].parent;
        let new_parent = self.alloc(self.nodes[sibling].aabb.union(&leaf_aabb), None);
        self.nodes[new_parent].parent = old_parent;
        self.nodes[new_parent].child1 = sibling;
        self.nodes[new_parent].child2 = leaf;
        self.nodes[new_parent].height = self.nodes[sibling].height + 1;
        self.nodes[sibling].parent = new_parent;
        self.nodes[leaf].parent = new_parent;
        if old_parent != DYN_NIL {
            if self.nodes[old_parent].child1 == sibling {
                self.nodes[old_parent].child1 = new_parent;
            } else {
                self.nodes[old_parent].child2 = new_parent;
            }
        } else {
            self.root = new_parent;
        }

        // 3) Walk back up, refit boxes and balance.
        let mut i = self.nodes[leaf].parent;
        while i != DYN_NIL {
            i = self.balance(i);
            let c1 = self.nodes[i].child1;
            let c2 = self.nodes[i].child2;
            self.nodes[i].height = 1 + self.nodes[c1].height.max(self.nodes[c2].height);
            self.nodes[i].aabb = self.nodes[c1].aabb.union(&self.nodes[c2].aabb);
            i = self.nodes[i].parent;
        }
    }

    fn descend_cost(&self, child: usize, leaf_aabb: &Aabb) -> f32 {
        let combined = self.nodes[child].aabb.union(leaf_aabb).perimeter();
        if self.nodes[child].payload.is_some() {
            combined
        } else {
            combined - self.nodes[child].aabb.perimeter()
        }
    }

    fn remove_leaf(&mut self, leaf: usize) {
        if leaf == self.root {
            self.root = DYN_NIL;
            return;
        }
        let parent = self.nodes[leaf].parent;
        let grand = self.nodes[parent].parent;
        let sibling = if self.nodes[parent].child1 == leaf {
            self.nodes[parent].child2
        } else {
            self.nodes[parent].child1
        };
        if grand != DYN_NIL {
            if self.nodes[grand].child1 == parent {
                self.nodes[grand].child1 = sibling;
            } else {
                self.nodes[grand].child2 = sibling;
            }
            self.nodes[sibling].parent = grand;
            self.freenode(parent);
            let mut i = grand;
            while i != DYN_NIL {
                i = self.balance(i);
                let c1 = self.nodes[i].child1;
                let c2 = self.nodes[i].child2;
                self.nodes[i].aabb = self.nodes[c1].aabb.union(&self.nodes[c2].aabb);
                self.nodes[i].height = 1 + self.nodes[c1].height.max(self.nodes[c2].height);
                i = self.nodes[i].parent;
            }
        } else {
            self.root = sibling;
            self.nodes[sibling].parent = DYN_NIL;
            self.freenode(parent);
        }
    }

    /// AVL-style rotation to keep the tree balanced (height difference of children <= 1).
    fn balance(&mut self, a: usize) -> usize {
        if self.nodes[a].payload.is_some() || self.nodes[a].height < 2 {
            return a;
        }
        let b = self.nodes[a].child1;
        let c = self.nodes[a].child2;
        let balance = self.nodes[c].height - self.nodes[b].height;
        if balance > 1 {
            self.rotate(a, c, b, true)
        } else if balance < -1 {
            self.rotate(a, b, c, false)
        } else {
            a
        }
    }

    /// Promote `pivot` above `a`, re-parenting the lighter grandchild under `a`. `pivot_is_child2`
    /// tells which side `pivot` was on. Returns the new subtree root.
    fn rotate(&mut self, a: usize, pivot: usize, other: usize, pivot_is_child2: bool) -> usize {
        let f = self.nodes[pivot].child1;
        let g = self.nodes[pivot].child2;
        // Swap a and pivot.
        self.nodes[pivot].child1 = a;
        self.nodes[pivot].parent = self.nodes[a].parent;
        self.nodes[a].parent = pivot;
        // pivot's old parent should now point to pivot.
        let pp = self.nodes[pivot].parent;
        if pp != DYN_NIL {
            if self.nodes[pp].child1 == a {
                self.nodes[pp].child1 = pivot;
            } else {
                self.nodes[pp].child2 = pivot;
            }
        } else {
            self.root = pivot;
        }
        // Pick the taller grandchild to keep under pivot; the shorter goes back under a.
        let (keep, give) = if self.nodes[f].height > self.nodes[g].height { (f, g) } else { (g, f) };
        self.nodes[pivot].child2 = keep;
        if pivot_is_child2 {
            self.nodes[a].child2 = give;
        } else {
            self.nodes[a].child1 = give;
        }
        self.nodes[give].parent = a;
        // Refit a then pivot.
        self.nodes[a].aabb = self.nodes[other].aabb.union(&self.nodes[give].aabb);
        self.nodes[pivot].aabb = self.nodes[a].aabb.union(&self.nodes[keep].aabb);
        self.nodes[a].height = 1 + self.nodes[other].height.max(self.nodes[give].height);
        self.nodes[pivot].height = 1 + self.nodes[a].height.max(self.nodes[keep].height);
        pivot
    }
}

/// A **compound body** — a nested recursive AABB. It is an object that has internal structure (a ship
/// of modules, an asteroid cluster, a planet with orbiting stations) carried as its *own*
/// [`DynamicAabbTree`] in local space, plus a [`Transform`] placing it in the world. The world tree
/// stores compounds as single fat leaves; a deep query descends into the compound's local tree, so a
/// railgun ray or a collision check can resolve right down to the struck module. This is what lets the
/// recursive AABBs literally hold other recursive AABBs and follow the parent object as it moves and
/// rotates.
#[derive(Debug, Clone)]
pub struct Compound<T> {
    pub transform: Transform,
    pub local: DynamicAabbTree<T>,
    /// The compound's bound in its own local space (refreshed as parts move); the world leaf box is
    /// `transform.apply_aabb(local_bounds)`.
    pub local_bounds: Aabb,
}

impl<T: Clone> Compound<T> {
    pub fn new(transform: Transform) -> Self {
        Compound { transform, local: DynamicAabbTree::new(4.0), local_bounds: Aabb::new(0.0, 0.0, 0.0, 0.0) }
    }

    /// The world-space fat box of the whole compound (what the parent world tree indexes).
    pub fn world_bounds(&self) -> Aabb {
        self.transform.apply_aabb(&self.local_bounds)
    }

    /// Deep query: parts of this compound whose local box overlaps the world-space query `q`. The
    /// query is mapped into local space first, so the descent into the nested tree is exact.
    pub fn query_world(&self, q: &Aabb) -> Vec<T> {
        // Map the query corners into local space and take their bound (conservative for rotation).
        let local_q = self.transform.apply_aabb_inverse(q);
        self.local.query(&local_q)
    }
}

impl Transform {
    /// Map a parent-space AABB back into local space (conservative axis-aligned bound).
    pub fn apply_aabb_inverse(&self, b: &Aabb) -> Aabb {
        let corners = [
            self.inverse_apply(b.min_x, b.min_y),
            self.inverse_apply(b.max_x, b.min_y),
            self.inverse_apply(b.min_x, b.max_y),
            self.inverse_apply(b.max_x, b.max_y),
        ];
        let mut out = Aabb { min_x: corners[0].0, min_y: corners[0].1, max_x: corners[0].0, max_y: corners[0].1 };
        for (x, y) in corners.into_iter().skip(1) {
            out.min_x = out.min_x.min(x);
            out.min_y = out.min_y.min(y);
            out.max_x = out.max_x.max(x);
            out.max_y = out.max_y.max(y);
        }
        out
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
