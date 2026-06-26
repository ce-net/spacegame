//! **Lossless cell partition** — how a running cell splits into four without dropping a bullet, and how
//! four merge back into one. This is what makes subdivision *seamless*: a 5,000-ship battle can shard
//! across four machines mid-fight and no player feels a thing.
//!
//! The rule is dead simple and therefore deterministic: every entity belongs to the child cell whose
//! footprint contains its position. Each of the four new hosts adopts the parent snapshot, keeps only
//! the entities inside its quadrant, and runs from there; entities that were straddling the split line
//! are owned by exactly one child (the one containing their centre) and naturally transit to a
//! neighbour next tick if they drift across — the same cross-cell transit the infinite map already uses.
//!
//! Because the assignment is a pure function of position + the (deterministic) `CellId` rects, all four
//! children — and any replica recomputing the split — agree on exactly who got what. No handshake, no
//! "who owns this bullet" negotiation. Merge is the trivial inverse: union the four, dedup by id.

use crate::galaxy::{CellId, World};

/// Anything with a world position can be partitioned. The renderer/sim types (ships, bullets, beams,
/// debris, pickups, mines, faction structures) all expose a centre; this trait keeps the partition
/// logic generic and pure so it's unit-testable without the whole sim.
pub trait Positioned {
    fn pos(&self) -> (World, World);
    /// A stable id, used to dedup on merge (the same entity must not appear twice if it was replicated
    /// into two children near the seam during the split tick).
    fn entity_id(&self) -> u64;
}

/// Assign one entity to the child cell that owns it. `parent.children()` is `[NW, NE, SW, SE]`; we pick
/// by which child rect contains the centre, clamping seam/edge points into the nearest child so nothing
/// is ever orphaned.
pub fn child_for<T: Positioned>(parent: CellId, e: &T) -> CellId {
    let (x, y) = e.pos();
    let r = parent.rect();
    let (cx, cy) = r.center();
    let east = x >= cx;
    let south = y >= cy;
    let kids = parent.children(); // [NW, NE, SW, SE] in (x,y) order from CellId::children
    match (east, south) {
        (false, false) => kids[0], // NW
        (true, false) => kids[1],  // NE
        (false, true) => kids[2],  // SW
        (true, true) => kids[3],   // SE
    }
}

/// Split a parent's entities into the four child buckets. Each child host keeps its bucket and discards
/// the rest. Order within a bucket is preserved (stable), so replicas that recompute the split produce
/// byte-identical child states.
pub fn split_entities<T: Positioned + Clone>(parent: CellId, entities: &[T]) -> [Vec<T>; 4] {
    let kids = parent.children();
    let mut out: [Vec<T>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for e in entities {
        let target = child_for(parent, e);
        let idx = kids.iter().position(|k| *k == target).unwrap_or(0);
        out[idx].push(e.clone());
    }
    out
}

/// Merge four children's entities back into the parent, deduping by id (an entity that was replicated
/// near the seam appears once). Stable order by id so the merged state is reproducible.
pub fn merge_entities<T: Positioned + Clone>(children: [Vec<T>; 4]) -> Vec<T> {
    let mut seen = std::collections::BTreeMap::new();
    for bucket in children {
        for e in bucket {
            seen.entry(e.entity_id()).or_insert(e);
        }
    }
    seen.into_values().collect()
}

/// A quick integrity check used in tests and (cheaply) at runtime: every parent entity lands in exactly
/// one child, and the four buckets sum to the parent count. If this ever fails, the split is unsound.
pub fn is_lossless<T: Positioned + Clone>(parent: CellId, entities: &[T]) -> bool {
    let buckets = split_entities(parent, entities);
    let total: usize = buckets.iter().map(|b| b.len()).sum();
    total == entities.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct E {
        id: u64,
        x: World,
        y: World,
    }
    impl Positioned for E {
        fn pos(&self) -> (World, World) {
            (self.x, self.y)
        }
        fn entity_id(&self) -> u64 {
            self.id
        }
    }

    #[test]
    fn entities_partition_into_their_quadrant() {
        // Split root: children are the four halves of the centred plane.
        let parent = CellId::ROOT;
        let r = parent.rect();
        let (cx, cy) = r.center();
        let nw = E { id: 1, x: cx - 10.0, y: cy - 10.0 };
        let ne = E { id: 2, x: cx + 10.0, y: cy - 10.0 };
        let sw = E { id: 3, x: cx - 10.0, y: cy + 10.0 };
        let se = E { id: 4, x: cx + 10.0, y: cy + 10.0 };
        let kids = parent.children();
        assert_eq!(child_for(parent, &nw), kids[0]);
        assert_eq!(child_for(parent, &ne), kids[1]);
        assert_eq!(child_for(parent, &sw), kids[2]);
        assert_eq!(child_for(parent, &se), kids[3]);
    }

    #[test]
    fn split_is_lossless_and_merge_is_inverse() {
        let parent = CellId { depth: 3, x: 2, y: 5 };
        let r = parent.rect();
        let mut es = Vec::new();
        for i in 0..200u64 {
            // scatter across the parent rect deterministically
            let fx = (i as World * 37.0) % r.span;
            let fy = (i as World * 53.0) % r.span;
            es.push(E { id: i, x: r.x + fx, y: r.y + fy });
        }
        assert!(is_lossless(parent, &es));
        let buckets = split_entities(parent, &es);
        let merged = merge_entities(buckets);
        assert_eq!(merged.len(), es.len(), "merge restores every entity exactly once");
    }
}
