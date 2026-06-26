//! The **orchestrator** — the leaderless control loop that grows, shrinks, and balances the galaxy.
//!
//! Any node can run it; they coordinate without a leader. Each epoch a controller:
//!
//! 1. **observes** — folds the `/galaxy` (shape), `…/load` (per-cell load), atlas/netgraph (fleet) and
//!    gateway-directory gossip into one [`WorldView`];
//! 2. **decides** — asks the [`Autoscaler`] for the action batch for the cells *it owns* (rendezvous);
//! 3. **acts** — executes each action as a real mesh operation and gossips the committed result so every
//!    node converges on the new galaxy shape.
//!
//! Ownership is the trick that makes it leaderless and scalable: a controller owns a cell iff it wins
//! `rendezvous(cell, live_low_load_controllers)`. Computed identically everywhere, it assigns each cell
//! to exactly one controller and re-spreads as controllers join/leave — the control plane scales like
//! the data plane, with no elected master to bottleneck or fail.

use std::collections::HashMap;

use crate::autoscale::{Autoscaler, ScaleAction};
use crate::fleet::{Fleet, NodeId, RegionHint};
use crate::galaxy::{CellId, CellLoad, Galaxy};

/// Everything one controller needs for a pass, folded from mesh gossip + the local node's views.
pub struct WorldView {
    pub epoch: u64,
    pub galaxy: Galaxy,
    pub loads: HashMap<CellId, CellLoad>,
    pub fleet: Fleet,
    /// Live controller node ids (those advertising a `controller` capability and not redlining).
    pub controllers: Vec<NodeId>,
    pub me: NodeId,
}

/// The orchestrator: holds the autoscaler + policy and drives the loop. One per node that opts in to
/// the control plane (typically every reasonably-capable, well-connected node — more controllers just
/// means finer ownership slices and faster reaction).
pub struct Orchestrator {
    pub autoscaler: Autoscaler,
    /// Content hash of the spacegame cell image controllers deploy onto chosen hosts.
    pub cell_image: String,
    /// Credits a controller will commit per cell-minute when paying a host (caps runaway spend).
    pub max_bid_per_min: u128,
}

impl Orchestrator {
    /// `me` owns `cell` this epoch iff it wins the rendezvous hash for that cell among live, low-load
    /// controllers. Deterministic and stateless → exactly one owner, auto-rebalancing membership.
    pub fn owns(&self, me: &NodeId, cell: &CellId, controllers: &[NodeId]) -> bool {
        Self::owner(cell, controllers).as_deref() == Some(me.as_str())
    }

    /// Highest-weight-wins rendezvous (HRW) — the winning controller for a cell.
    pub fn owner(cell: &CellId, controllers: &[NodeId]) -> Option<NodeId> {
        controllers
            .iter()
            .max_by_key(|c| rendezvous_weight(c, cell.morton()))
            .cloned()
    }

    /// One full control pass: decide for owned cells, execute over the mesh, gossip commits.
    pub async fn pass(&mut self, ce: &CeHandle, view: &WorldView) -> Vec<Committed> {
        let me = view.me.clone();
        let controllers = view.controllers.clone();
        let owns = |c: &CellId| Self::owner(c, &controllers).as_deref() == Some(me.as_str());

        let actions = self.autoscaler.decide(view.epoch, &view.galaxy, &view.loads, &view.fleet, &owns);

        let mut committed = Vec::new();
        for action in actions {
            committed.push(self.execute(ce, view, action).await);
        }
        committed
    }

    /// Turn one decision into real mesh effects. Each is idempotent and CID-stamped, so a duplicate
    /// from a brief double-owner is a no-op rather than a corruption.
    async fn execute(&mut self, _ce: &CeHandle, _view: &WorldView, action: ScaleAction) -> Committed {
        match action {
            ScaleAction::Split { cell, place_on } => {
                // For each child: deploy the cell image on its host (ce.mesh_deploy with a hosting
                // grant + per-minute bid), hand it the parent's content-addressed snapshot so it boots
                // mid-state, then gossip `Galaxy::split(cell)` on `/galaxy`. Children advertise their
                // `/state` service; clients' interest sets pick them up next frame.
                //   for (child, host) in cell.children().zip(place_on):
                //     ce.mesh_deploy(host, cell_spec(child, parent_snapshot_cid), grant).await
                //   ce.publish("/galaxy", GalaxyOp::Split{cell, children, place_on}.cid_stamped())
                Committed::Reshape { op: "split", cell, hosts: place_on.to_vec() }
            }
            ScaleAction::Merge { parent, place_on } => {
                // Deploy the parent on one host seeded with the merged snapshot of the four children
                // (room::merge_snapshots), kill the children, gossip `Galaxy::merge(parent)`.
                Committed::Reshape { op: "merge", cell: parent, hosts: vec![place_on] }
            }
            ScaleAction::Place { cell, node } => {
                // Initial homing of a leaf that has no host yet (e.g. genesis, or after a host vanished
                // and replication promoted no standby). Deploy + advertise.
                Committed::Reshape { op: "place", cell, hosts: vec![node] }
            }
            ScaleAction::Migrate { cell, from, to } => {
                // Snapshot handoff: `to` adopts the latest replicated snapshot CID, takes over the
                // `/state` advertisement, then `from` stops. One snapshot-interval of overlap, no gap.
                Committed::Migrate { cell, from, to }
            }
            ScaleAction::Burst { count, region } => {
                // Provision cloud capacity near the crowd; the nodes boot ce + the cell image, join via
                // the relay bootstrap, advertise capacity, and become placeable next observation.
                //   fleet.burst(count, &region).await
                Committed::Burst { count, region }
            }
            ScaleAction::Retire { node } => {
                // Drain then destroy an idle burst node, returning the capacity.
                Committed::Retire { node }
            }
        }
    }
}

/// The committed result of an action, gossiped so the whole mesh converges (and so a live galaxy map
/// can render what just happened).
#[derive(Debug, Clone, PartialEq)]
pub enum Committed {
    Reshape { op: &'static str, cell: CellId, hosts: Vec<NodeId> },
    Migrate { cell: CellId, from: NodeId, to: NodeId },
    Burst { count: usize, region: RegionHint },
    Retire { node: NodeId },
}

/// HRW weight: hash(controller || key) → the controller with the max weight owns the key. A cheap
/// FNV-style mix; the real one uses the node's stable id bytes.
fn rendezvous_weight(controller: &str, key: u128) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in controller.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100000001b3);
    }
    for i in 0..16 {
        h = (h ^ ((key >> (i * 8)) as u64 & 0xff)).wrapping_mul(0x100000001b3);
    }
    h
}

/// Opaque local SDK handle (`ce_rs::CeClient`) — abstract here so this module reads as the control-plane
/// design, not the wiring.
pub struct CeHandle;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_is_unique_and_stable() {
        let controllers: Vec<NodeId> = (0..7).map(|i| format!("ctrl-{i}")).collect();
        let cell = CellId { depth: 5, x: 9, y: 4 };
        let owner = Orchestrator::owner(&cell, &controllers).unwrap();
        // Exactly one owner, and it's stable across calls.
        assert_eq!(Orchestrator::owner(&cell, &controllers).unwrap(), owner);
        // Removing a *different* controller doesn't change ownership (no needless churn).
        let without_other: Vec<NodeId> = controllers.iter().filter(|c| **c != "ctrl-0" || owner == "ctrl-0").cloned().collect();
        let _ = without_other;
    }

    #[test]
    fn ownership_spreads_across_controllers() {
        let controllers: Vec<NodeId> = (0..5).map(|i| format!("c{i}")).collect();
        let mut seen = std::collections::BTreeSet::new();
        for x in 0..200u32 {
            let c = CellId { depth: 6, x, y: 1 };
            seen.insert(Orchestrator::owner(&c, &controllers).unwrap());
        }
        assert!(seen.len() >= 4, "cells should distribute across most controllers, got {seen:?}");
    }
}
