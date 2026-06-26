//! The **elastic fleet** — the pool of mesh nodes that host galaxy cells, and the machinery that grows
//! it on demand.
//!
//! Capacity comes from two springs, blended seamlessly:
//!
//! 1. **Donated compute** — the CE thesis. Anyone running `ce` with spare cores auto-enrols as a game
//!    host just by advertising capacity in the atlas; the autoscaler discovers them for free and pays
//!    them per cell-minute in credits. A phone, a gaming PC, a rack — all the same to the fleet.
//! 2. **Cloud burst** — when the donor pool is saturated (no node has headroom for the next split), the
//!    fleet provisions fresh cloud nodes through a pluggable [`CloudProvider`], boots `ce` + the
//!    spacegame cell image on them, lets them join the mesh, and **pays for them out of game revenue**.
//!    When the surge passes, they are drained and destroyed. The galaxy literally rents the planet's
//!    spare computers when a battle erupts and gives them back when it's over.
//!
//! Placement is latency-first and capacity-aware: a cell is hosted on the lowest-RTT node *to the
//! crowd inside it* that still has budget, so players talk to compute that is physically near them.

use crate::galaxy::CellId;

/// A node's id on the mesh (hex NodeId).
pub type NodeId = String;

/// Where a host node came from. Drives billing and lifecycle: donated nodes are never destroyed by us;
/// burst nodes are ours to retire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacitySource {
    /// A volunteer node already on the mesh. Paid per cell-minute; never torn down by the fleet.
    Donated,
    /// A node we provisioned for a surge through `provider`, billed by the provider, recouped from play.
    CloudBurst { provider: String, region: String, instance_id: String },
}

/// A coarse geographic hint so the fleet can place a cell's host near the players inside it. Derived
/// from measured RTT clusters in the netgraph, not from IP geolocation — latency is the truth.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RegionHint(pub String);

impl RegionHint {
    pub const UNKNOWN: &'static str = "unknown";
}

/// One host node the fleet can place cells on.
#[derive(Debug, Clone)]
pub struct HostNode {
    pub node_id: NodeId,
    pub source: CapacitySource,
    pub cores: u32,
    pub mem_mb: u64,
    pub region: RegionHint,
    /// Cells this node currently runs — its occupancy.
    pub cells: u32,
    /// Soft cap on concurrent cells for this node (from cores/mem and a safety margin).
    pub cell_budget: u32,
    /// Smoothed load 0.0..1.0 from the atlas (CPU + mem + the host's own tick headroom report).
    pub load: f32,
    /// Price the node asks per cell-minute, in credits (donated nodes set their own; 0 = altruist).
    pub price_per_min: u64,
    /// Last time the atlas/heartbeat confirmed it alive (unix secs).
    pub last_seen: u64,
}

impl HostNode {
    /// Free cell slots right now.
    pub fn headroom(&self) -> u32 {
        self.cell_budget.saturating_sub(self.cells)
    }
    pub fn is_burst(&self) -> bool {
        matches!(self.source, CapacitySource::CloudBurst { .. })
    }
    /// A node is a candidate to host another cell if it is alive, has a free slot, and isn't redlining.
    pub fn can_take_cell(&self) -> bool {
        self.headroom() > 0 && self.load < 0.85
    }
}

/// Aggregate capacity across the whole fleet — the number the autoscaler watches to decide whether to
/// burst more cloud nodes or drain idle ones.
#[derive(Debug, Clone, Copy)]
pub struct Headroom {
    pub free_slots: u32,
    pub total_slots: u32,
    pub burst_nodes: u32,
    pub donor_nodes: u32,
}

impl Headroom {
    /// Fraction of the fleet that is free (0.0 = full, 1.0 = idle).
    pub fn slack(&self) -> f32 {
        if self.total_slots == 0 {
            0.0
        } else {
            self.free_slots as f32 / self.total_slots as f32
        }
    }
}

/// A pluggable cloud backend. The fleet doesn't care whether it's Hetzner, Fly, a friend's spare box
/// reachable over the mesh, or a Kubernetes burst pool — it just asks for a node and gets one that
/// boots `ce`, joins the mesh, and pulls the spacegame cell image.
pub trait CloudProvider: Send + Sync {
    /// Stable name (`"hetzner"`, `"fly"`, `"any-mesh-rental"`, …) recorded in [`CapacitySource`].
    fn name(&self) -> &str;
    /// Provision `count` nodes near `region`, each booting with `cloud_init` (which installs `ce`,
    /// joins via the relay bootstrap, and starts the spacegame cell image). Returns their `HostNode`s
    /// once they've reported into the atlas. Implementations may stream nodes as they come up.
    fn provision(
        &self,
        count: usize,
        region: &RegionHint,
        cloud_init: &str,
    ) -> Vec<HostNode>;
    /// Tear a burst node down once it has been drained.
    fn destroy(&self, instance_id: &str);
    /// Indicative price per node-minute in credits, so the autoscaler can prefer the cheapest region
    /// that satisfies latency.
    fn price_per_min(&self, region: &RegionHint) -> u64;
}

/// The fleet: a live snapshot of every host plus the cloud providers it may burst into.
pub struct Fleet {
    pub hosts: Vec<HostNode>,
    pub providers: Vec<Box<dyn CloudProvider>>,
    /// The content-addressed cell image / cloud-init the providers boot. Set once at deploy.
    pub cell_boot: String,
}

impl Fleet {
    /// Build the live fleet view from the mesh: join the capacity atlas with the latency netgraph and
    /// the spacegame host advertisements. (`ce.atlas()` × `ce.netgraph()` × `find_service`.) Donor
    /// nodes appear here automatically the moment they advertise capacity — no registration step.
    pub async fn observe(_ce: &CeHandle, _providers: Vec<Box<dyn CloudProvider>>) -> Fleet {
        // Pseudocode of the real pipeline (kept declarative so the intent is the spec):
        //   atlas    = ce.atlas().await           // cores/mem/load/price per node
        //   net      = ce.netgraph().await         // measured RTT edges → region clustering
        //   advert   = ce.find_service("ce-game/spacegame").await  // who already hosts cells
        //   hosts    = atlas.map(|a| HostNode::from(a, region_of(a, net), cells_of(a, advert)))
        unimplemented!("observe(): atlas × netgraph × host adverts → Vec<HostNode>")
    }

    /// Total free/used capacity across donor + burst nodes.
    pub fn headroom(&self) -> Headroom {
        let mut h = Headroom { free_slots: 0, total_slots: 0, burst_nodes: 0, donor_nodes: 0 };
        for n in &self.hosts {
            h.free_slots += n.headroom();
            h.total_slots += n.cell_budget;
            if n.is_burst() {
                h.burst_nodes += 1;
            } else {
                h.donor_nodes += 1;
            }
        }
        h
    }

    /// Pick the best host for `cell` given where its crowd is (`near`): lowest price among the
    /// lowest-RTT nodes-with-headroom to that region. Donated capacity is preferred (cheaper, already
    /// paid into the mesh); burst nodes are the fallback. Returns `None` when the whole fleet is full —
    /// the autoscaler reads that as "burst now".
    pub fn best_host_for(&self, _cell: &CellId, near: &RegionHint) -> Option<NodeId> {
        let mut candidates: Vec<&HostNode> = self
            .hosts
            .iter()
            .filter(|n| n.can_take_cell())
            .collect();
        // Score: region match first (latency), then donor over burst, then price, then emptiness.
        candidates.sort_by(|a, b| {
            let ra = (a.region == *near) as u8;
            let rb = (b.region == *near) as u8;
            rb.cmp(&ra)
                .then((a.is_burst() as u8).cmp(&(b.is_burst() as u8)))
                .then(a.price_per_min.cmp(&b.price_per_min))
                .then(a.cells.cmp(&b.cells))
        });
        candidates.first().map(|n| n.node_id.clone())
    }

    /// Bring `count` fresh nodes online near `region` for a surge, cheapest qualifying provider first.
    /// They boot `ce`, join the mesh, advertise capacity, and become placeable within a minute.
    pub async fn burst(&mut self, count: usize, region: &RegionHint) -> Vec<NodeId> {
        let mut provs: Vec<&Box<dyn CloudProvider>> = self.providers.iter().collect();
        provs.sort_by_key(|p| p.price_per_min(region));
        let mut new_ids = Vec::new();
        for p in provs {
            if new_ids.len() >= count {
                break;
            }
            let want = count - new_ids.len();
            for node in p.provision(want, region, &self.cell_boot) {
                new_ids.push(node.node_id.clone());
                self.hosts.push(node);
            }
        }
        new_ids
    }

    /// Gracefully retire a burst node: stop placing on it, let its cells migrate or merge away, then
    /// destroy it. Donor nodes are never destroyed — only stopped being given new cells.
    pub async fn drain(&mut self, node_id: &NodeId) {
        if let Some(n) = self.hosts.iter_mut().find(|n| n.node_id == *node_id) {
            n.cell_budget = n.cells; // no new cells land here
            if let CapacitySource::CloudBurst { provider, instance_id, .. } = &n.source {
                if let Some(p) = self.providers.iter().find(|p| p.name() == provider) {
                    p.destroy(instance_id); // after cells have drained
                }
            }
        }
        self.hosts.retain(|n| n.node_id != *node_id || !n.is_burst() || n.cells > 0);
    }
}

/// Opaque handle to the local CE node SDK client (`ce_rs::CeClient`), kept abstract here so this
/// module reads as the *design* rather than the plumbing.
pub struct CeHandle;

#[cfg(test)]
mod tests {
    use super::*;

    fn donor(id: &str, region: &str, free: u32, price: u64, load: f32) -> HostNode {
        HostNode {
            node_id: id.into(),
            source: CapacitySource::Donated,
            cores: 8,
            mem_mb: 16_000,
            region: RegionHint(region.into()),
            cells: 0,
            cell_budget: free,
            load,
            price_per_min: price,
            last_seen: 0,
        }
    }

    #[test]
    fn places_in_region_and_prefers_cheap_donor() {
        let fleet = Fleet {
            hosts: vec![
                donor("far", "us-east", 4, 0, 0.1),
                donor("near-cheap", "eu-west", 4, 5, 0.2),
                donor("near-pricey", "eu-west", 4, 50, 0.2),
            ],
            providers: vec![],
            cell_boot: String::new(),
        };
        let pick = fleet.best_host_for(&CellId::ROOT, &RegionHint("eu-west".into()));
        assert_eq!(pick.as_deref(), Some("near-cheap"));
    }

    #[test]
    fn full_fleet_yields_no_host() {
        let mut n = donor("x", "eu", 1, 0, 0.1);
        n.cells = 1; // no headroom
        let fleet = Fleet { hosts: vec![n], providers: vec![], cell_boot: String::new() };
        assert!(fleet.best_host_for(&CellId::ROOT, &RegionHint("eu".into())).is_none());
    }
}
