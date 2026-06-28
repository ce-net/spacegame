//! The **autoscaler** — the control plane that keeps the galaxy's compute matched to its crowds.
//!
//! Every ~beacon epoch it runs one pass: **observe** load across the galaxy, **decide** a batch of
//! actions (split / merge / place / migrate / burst / retire), and **act** by issuing them over the
//! mesh. It has three properties that make it safe at planet scale:
//!
//! - **Leaderless.** There is no central scaler. The galaxy is partitioned among controllers by
//!   rendezvous hash: a node *owns* a cell's scaling decisions iff it wins the hash for that cell among
//!   live, low-load nodes. Ownership is computed identically everywhere, so exactly one controller acts
//!   on each cell, and ownership re-spreads automatically as controllers come and go. The control plane
//!   scales the same way the data plane does.
//! - **Deterministic where it matters.** Split/merge verdicts come from gossiped [`CellLoad`] via the
//!   pure [`CellLoad::verdict`], so even two controllers that briefly both think they own a cell reach
//!   the *same* verdict and the action is idempotent (a CID-stamped commit on the `/galaxy` topic).
//! - **Dampened.** Hysteresis + dwell epochs + a global burst cooldown stop the galaxy from oscillating
//!   or stampeding the cloud on a single noisy frame.

use std::collections::HashMap;

use crate::fleet::{Fleet, NodeId, RegionHint};
use crate::galaxy::{CellId, CellLoad, Galaxy, ScalePolicy, Verdict};

/// One thing the autoscaler decides to do this pass. A pass yields a small batch; the orchestrator
/// applies them over the mesh and the results show up in the next observation.
#[derive(Debug, Clone, PartialEq)]
pub enum ScaleAction {
    /// Subdivide a hot leaf into four children and place each on a host.
    Split { cell: CellId, place_on: [NodeId; 4] },
    /// Collapse four cold siblings back into their parent on a single host.
    Merge { parent: CellId, place_on: NodeId },
    /// (Re)home an existing leaf onto a host — initial placement, or rebalancing off a hot/draining node.
    Place { cell: CellId, node: NodeId },
    /// Move a leaf from one host to another (load rebalance / latency improvement) via snapshot handoff.
    Migrate { cell: CellId, from: NodeId, to: NodeId },
    /// Provision fresh cloud capacity near a region because the donor pool can't absorb demand.
    Burst { count: usize, region: RegionHint },
    /// Drain + destroy an idle burst node to give the capacity back.
    Retire { node: NodeId },
}

/// Knobs for the control loop itself (the data-plane knobs live in [`ScalePolicy`]).
#[derive(Debug, Clone, Copy)]
pub struct AutoscalePolicy {
    pub scale: ScalePolicy,
    /// Burst more cloud when fleet slack drops below this (e.g. 0.15 = act while 15% still free, so new
    /// nodes are warming *before* we actually run out).
    pub burst_below_slack: f32,
    /// Retire burst nodes when slack rises above this (give capacity back once the surge passes).
    pub retire_above_slack: f32,
    /// Max new cloud nodes to provision in a single pass — a stampede guard.
    pub max_burst_per_pass: usize,
    /// Epochs to wait between bursts in the same region — lets warming nodes report before bursting more.
    pub burst_cooldown_epochs: u8,
}

impl Default for AutoscalePolicy {
    fn default() -> Self {
        AutoscalePolicy {
            scale: ScalePolicy::default(),
            burst_below_slack: 0.15,
            retire_above_slack: 0.55,
            max_burst_per_pass: 32,
            burst_cooldown_epochs: 4,
        }
    }
}

/// Per-cell debounce state: how many consecutive epochs a cell has wanted the same non-Hold verdict.
#[derive(Default)]
struct Dwell {
    last: Option<Verdict>,
    streak: u8,
}

/// The autoscaler. Stateless about the world (it re-observes every pass) but keeps tiny per-cell dwell
/// counters and per-region burst cooldowns so it doesn't act on noise.
pub struct Autoscaler {
    pub policy: AutoscalePolicy,
    dwell: HashMap<CellId, Dwell>,
    burst_cooldown: HashMap<RegionHint, u64>, // region -> epoch last burst
}

impl Autoscaler {
    pub fn new(policy: AutoscalePolicy) -> Self {
        Autoscaler { policy, dwell: HashMap::new(), burst_cooldown: HashMap::new() }
    }

    /// One control pass. Pure decision function over the observed world — returns the action batch; the
    /// orchestrator executes it. `owns` says which cells this controller is responsible for this epoch
    /// (rendezvous ownership), so two controllers never both act on a cell.
    pub fn decide(
        &mut self,
        epoch: u64,
        galaxy: &Galaxy,
        loads: &HashMap<CellId, CellLoad>,
        fleet: &Fleet,
        owns: &dyn Fn(&CellId) -> bool,
    ) -> Vec<ScaleAction> {
        let mut actions = Vec::new();

        // 1) Per-leaf split/merge, debounced by dwell, only for cells we own.
        for leaf in galaxy.leaves() {
            if !owns(leaf) {
                continue;
            }
            let Some(load) = loads.get(leaf) else { continue };
            let verdict = load.verdict(leaf.depth, &self.policy.scale);
            if !self.debounce(*leaf, verdict) {
                continue;
            }
            match verdict {
                Verdict::Split => {
                    let region = self.region_of(load, fleet);
                    if let Some(place_on) = self.place_four(leaf, &region, fleet) {
                        actions.push(ScaleAction::Split { cell: *leaf, place_on });
                    } else {
                        // No room to land the children — ask for capacity instead of dropping the split.
                        actions.push(ScaleAction::Burst { count: 4, region });
                    }
                }
                Verdict::Merge => {
                    // Only the controller that owns the *parent* commits the merge, and only when all
                    // four siblings agree to merge — avoids half-merges.
                    if let Some(parent) = leaf.parent() {
                        if owns(&parent) && self.all_siblings_cold(parent, loads) {
                            let region = self.region_of(load, fleet);
                            if let Some(node) = fleet.best_host_for(&parent, &region) {
                                actions.push(ScaleAction::Merge { parent, place_on: node });
                            }
                        }
                    }
                }
                Verdict::Hold => {}
            }
        }

        // 2) Fleet-wide elasticity: burst when tight, retire when slack — region by region.
        let h = fleet.headroom();
        if h.slack() < self.policy.burst_below_slack {
            for region in self.tight_regions(fleet) {
                if self.burst_ready(&region, epoch) {
                    let count = self.policy.max_burst_per_pass.min(8);
                    actions.push(ScaleAction::Burst { count, region: region.clone() });
                    self.burst_cooldown.insert(region, epoch);
                }
            }
        } else if h.slack() > self.policy.retire_above_slack {
            for node in self.idle_burst_nodes(fleet) {
                actions.push(ScaleAction::Retire { node });
            }
        }

        // 3) Rebalance: shed cells off any host that is redlining onto cooler near-neighbours.
        for hot in fleet.hosts.iter().filter(|n| n.load > 0.9) {
            if let Some(cell) = self.heaviest_movable_cell(&hot.node_id, galaxy, loads, owns) {
                if let Some(to) = fleet.best_host_for(&cell, &hot.region) {
                    if to != hot.node_id {
                        actions.push(ScaleAction::Migrate { cell, from: hot.node_id.clone(), to });
                    }
                }
            }
        }

        actions
    }

    /// Require `dwell_epochs` consecutive identical non-Hold verdicts before acting; returns true when
    /// the streak is met (and resets, so we don't re-fire every epoch).
    fn debounce(&mut self, cell: CellId, v: Verdict) -> bool {
        if v == Verdict::Hold {
            self.dwell.remove(&cell);
            return false;
        }
        let d = self.dwell.entry(cell).or_default();
        if d.last == Some(v) {
            d.streak += 1;
        } else {
            d.last = Some(v);
            d.streak = 1;
        }
        if d.streak >= self.policy.scale.dwell_epochs {
            d.streak = 0;
            true
        } else {
            false
        }
    }

    /// Place all four children of a splitting cell, preferring to keep them near the crowd. Returns
    /// `None` if the fleet can't seat all four (→ caller bursts).
    fn place_four(&self, cell: &CellId, region: &RegionHint, fleet: &Fleet) -> Option<[NodeId; 4]> {
        let kids = cell.children();
        let mut out: Vec<NodeId> = Vec::with_capacity(4);
        // A throwaway local view so we don't seat two children on the last free slot of one node.
        let mut taken: HashMap<NodeId, u32> = HashMap::new();
        for _k in &kids {
            let pick = fleet
                .hosts
                .iter()
                .filter(|n| n.can_take_cell() && n.headroom() > *taken.get(&n.node_id).unwrap_or(&0))
                .min_by_key(|n| {
                    let region_pen = if n.region == *region { 0 } else { 1 };
                    (region_pen, n.is_burst() as u8, n.price_per_min, n.cells)
                })?;
            *taken.entry(pick.node_id.clone()).or_default() += 1;
            out.push(pick.node_id.clone());
        }
        out.try_into().ok()
    }

    fn all_siblings_cold(&self, parent: CellId, loads: &HashMap<CellId, CellLoad>) -> bool {
        parent.children().iter().all(|c| {
            loads
                .get(c)
                .map(|l| l.verdict(c.depth, &self.policy.scale) == Verdict::Merge)
                .unwrap_or(false)
        })
    }

    fn region_of(&self, _load: &CellLoad, _fleet: &Fleet) -> RegionHint {
        // Real impl: cluster the cell's players by their gateway's RTT region; fall back to the host's.
        RegionHint(RegionHint::UNKNOWN.into())
    }

    fn tight_regions(&self, fleet: &Fleet) -> Vec<RegionHint> {
        // Regions whose local slack is below threshold (so we burst *where* it's tight, not globally).
        let mut by_region: HashMap<RegionHint, (u32, u32)> = HashMap::new();
        for n in &fleet.hosts {
            let e = by_region.entry(n.region.clone()).or_insert((0, 0));
            e.0 += n.headroom();
            e.1 += n.cell_budget;
        }
        by_region
            .into_iter()
            .filter(|(_, (free, total))| *total > 0 && (*free as f32 / *total as f32) < self.policy.burst_below_slack)
            .map(|(r, _)| r)
            .collect()
    }

    fn burst_ready(&self, region: &RegionHint, epoch: u64) -> bool {
        self.burst_cooldown
            .get(region)
            .map(|last| epoch.saturating_sub(*last) >= self.policy.burst_cooldown_epochs as u64)
            .unwrap_or(true)
    }

    fn idle_burst_nodes(&self, fleet: &Fleet) -> Vec<NodeId> {
        fleet.hosts.iter().filter(|n| n.is_burst() && n.cells == 0).map(|n| n.node_id.clone()).collect()
    }

    fn heaviest_movable_cell(
        &self,
        _node: &NodeId,
        _galaxy: &Galaxy,
        _loads: &HashMap<CellId, CellLoad>,
        _owns: &dyn Fn(&CellId) -> bool,
    ) -> Option<CellId> {
        // Real impl: among cells this node hosts that we own, pick the one whose move best lowers the
        // node's projected load without overloading the destination.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::{CapacitySource, HostNode};

    fn loads_one(cell: CellId, l: CellLoad) -> HashMap<CellId, CellLoad> {
        let mut m = HashMap::new();
        m.insert(cell, l);
        m
    }

    fn fleet_with(n: usize) -> Fleet {
        let hosts = (0..n)
            .map(|i| HostNode {
                node_id: format!("h{i}"),
                source: CapacitySource::Donated,
                cores: 8,
                mem_mb: 16000,
                region: RegionHint("eu".into()),
                cells: 0,
                cell_budget: 4,
                load: 0.1,
                price_per_min: 0,
                last_seen: 0,
            })
            .collect();
        Fleet { hosts, providers: vec![], cell_boot: String::new() }
    }

    #[test]
    fn hot_owned_cell_splits_after_dwell() {
        let mut a = Autoscaler::new(AutoscalePolicy::default());
        let mut g = Galaxy::genesis();
        g.split(CellId::ROOT); // make a depth-1 leaf to split
        let leaf = *g.leaves().next().unwrap();
        let hot = CellLoad { players: 9999, entities: 0, tick_p99_us: 0, bandwidth_bps: 0, epoch: 1 };
        let fleet = fleet_with(4);
        let owns = |_: &CellId| true;
        // First passes are swallowed by dwell; the action fires once the streak is met.
        let mut fired = None;
        for e in 1..=5 {
            let acts = a.decide(e, &g, &loads_one(leaf, hot), &fleet, &owns);
            if let Some(ScaleAction::Split { cell, .. }) = acts.into_iter().next() {
                fired = Some(cell);
                break;
            }
        }
        assert_eq!(fired, Some(leaf));
    }
}
