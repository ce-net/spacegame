//! The **galaxy gossip protocol** — the small set of messages that let thousands of nodes agree on the
//! shape of one galaxy with no coordinator.
//!
//! Three topics carry everything:
//!
//! - `ce-game/spacegame/galaxy`        — **shape**: CID-stamped split/merge/place/migrate *commits*.
//! - `ce-game/spacegame/<cell>/load`   — **load**: each host's per-cell [`crate::galaxy::CellLoad`] tick.
//! - `ce-game/spacegame/control`       — **liveness**: controller + host heartbeats (who's alive, role).
//!
//! Every frame is signed by its origin (the node delivers the authenticated sender), and every shape
//! commit carries a **`prev` CID** so the shape log is a verifiable chain — a node that missed frames
//! re-syncs by walking back to a CID it has, and a forged reshape can't splice in (its `prev` won't
//! match). Because the split/merge *decision* is already deterministic ([`crate::galaxy::CellLoad::verdict`]),
//! the gossip only needs to carry *that it happened*, not *whether it should*: convergence is cheap.

use serde::{Deserialize, Serialize};

use crate::galaxy::{CellId, CellLoad};

/// Topic names. `cell.token()` is the quadtree address (`depth.x.y`).
pub mod topics {
    pub const SHAPE: &str = "ce-game/spacegame/galaxy";
    pub const CONTROL: &str = "ce-game/spacegame/control";
    pub fn load(cell_token: &str) -> String {
        format!("ce-game/spacegame/{cell_token}/load")
    }
    pub fn state(cell_token: &str) -> String {
        format!("ce-game/spacegame/{cell_token}/state")
    }
    pub fn input(cell_token: &str) -> String {
        format!("ce-game/spacegame/{cell_token}/in")
    }
}

/// A content id (sha-256 of the canonical bytes) — both an integrity check and the link in the shape
/// chain. `String` here; the node's blob store mints the real one.
pub type Cid = String;

/// One committed change to the galaxy's shape, gossiped on [`topics::SHAPE`]. Idempotent and ordered by
/// `prev`: applying the same op twice is a no-op, and an op whose `prev` you don't yet hold is buffered
/// until you catch up. This is the *only* thing that mutates [`crate::galaxy::Galaxy`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShapeCommit {
    /// This commit's own CID (over `{op, prev, epoch, controller}`), so peers can chain + dedup.
    pub cid: Cid,
    /// The CID of the prior shape commit this one extends — the verifiable backlink.
    pub prev: Cid,
    /// The controller that committed it (authenticated sender must match).
    pub controller: String,
    /// The beacon epoch it was decided in — anchors the shared clock.
    pub epoch: u64,
    pub op: ShapeOp,
}

/// The reshape operations. Each names the cell(s) and where the new authority lands; hosts watch for
/// ops touching cells they run (or should run) and act.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum ShapeOp {
    /// A leaf split into four children, each seeded from `from_snapshot` and homed on `place_on`.
    Split { cell: CellId, children: [CellId; 4], place_on: [String; 4], from_snapshot: Cid },
    /// Four siblings merged into `parent`, seeded from the four child snapshots, homed on `place_on`.
    Merge { parent: CellId, from_snapshots: [Cid; 4], place_on: String },
    /// A leaf (re)homed onto a node — genesis, or recovery when no standby was promoted.
    Place { cell: CellId, node: String, from_snapshot: Option<Cid> },
    /// A leaf moved between hosts by snapshot handoff (rebalance / latency).
    Migrate { cell: CellId, from: String, to: String, snapshot: Cid },
}

impl ShapeOp {
    /// The leaf cells that exist *after* this op (so a follower can update its `Galaxy` directly).
    pub fn resulting_leaves(&self) -> Vec<CellId> {
        match self {
            ShapeOp::Split { children, .. } => children.to_vec(),
            ShapeOp::Merge { parent, .. } => vec![*parent],
            ShapeOp::Place { cell, .. } | ShapeOp::Migrate { cell, .. } => vec![*cell],
        }
    }
}

/// A per-cell load tick on [`topics::load`]. Small and frequent; the controller that owns the cell folds
/// the latest into its scaling decision, and the galaxy map renders it as a heat value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadFrame {
    pub cell: CellId,
    pub host: String,
    pub load: CellLoad,
}

/// A liveness + role heartbeat on [`topics::CONTROL`]. The union of recent heartbeats *is* the live
/// node set the rendezvous owner/host pickers run over — no registry, just a decaying view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Heartbeat {
    pub node: String,
    /// Bitset-ish role flags so one frame announces all roles.
    pub hosts: bool,
    pub controls: bool,
    pub gateway: bool,
    /// Coarse capacity so pickers can skip redlining nodes without a separate atlas round-trip.
    pub free_cell_slots: u32,
    pub load: f32,
    pub region: String,
    pub epoch: u64,
}

/// A node's rolling, self-healing view of the control plane, folded from the three topics. Eventually
/// consistent and decaying: heartbeats older than `stale_epochs` drop out, so a dead node simply fades
/// from every picker within a couple of epochs — that's the entire failure-detection mechanism.
#[derive(Debug, Default)]
pub struct ControlView {
    live: std::collections::HashMap<String, Heartbeat>,
    /// The shape commit CID we've applied up to (our position in the shape chain).
    pub head: Cid,
}

impl ControlView {
    /// Fold a heartbeat in.
    pub fn observe(&mut self, hb: Heartbeat) {
        self.live.insert(hb.node.clone(), hb);
    }

    /// Drop nodes whose last heartbeat is older than `stale_epochs` — fading out the dead.
    pub fn prune(&mut self, now_epoch: u64, stale_epochs: u64) {
        self.live.retain(|_, hb| now_epoch.saturating_sub(hb.epoch) <= stale_epochs);
    }

    /// Live controllers, low-load first — the set the rendezvous owner runs over.
    pub fn controllers(&self) -> Vec<String> {
        let mut v: Vec<&Heartbeat> = self.live.values().filter(|h| h.controls && h.load < 0.9).collect();
        v.sort_by(|a, b| a.load.partial_cmp(&b.load).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter().map(|h| h.node.clone()).collect()
    }

    /// Live hosts and gateways for placement / directory building.
    pub fn hosts(&self) -> impl Iterator<Item = &Heartbeat> {
        self.live.values().filter(|h| h.hosts)
    }
    pub fn gateways(&self) -> impl Iterator<Item = &Heartbeat> {
        self.live.values().filter(|h| h.gateway)
    }

    pub fn live_count(&self) -> usize {
        self.live.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_op_yields_four_leaves() {
        let cell = CellId { depth: 2, x: 1, y: 1 };
        let op = ShapeOp::Split {
            cell,
            children: cell.children().try_into().unwrap(),
            place_on: ["a".into(), "b".into(), "c".into(), "d".into()],
            from_snapshot: "Qm".into(),
        };
        assert_eq!(op.resulting_leaves().len(), 4);
    }

    #[test]
    fn dead_nodes_fade_from_the_view() {
        let mut cv = ControlView::default();
        let hb = |node: &str, epoch| Heartbeat {
            node: node.into(),
            hosts: true,
            controls: true,
            gateway: false,
            free_cell_slots: 4,
            load: 0.1,
            region: "eu".into(),
            epoch,
        };
        cv.observe(hb("alive", 100));
        cv.observe(hb("dead", 90));
        cv.prune(100, 5); // dead's last heartbeat is 10 epochs old > 5
        assert_eq!(cv.controllers(), vec!["alive".to_string()]);
    }
}
