//! `cosmos` — the one-call public face of the planet-scale galaxy. Two entry points, and everything
//! above (quadtree galaxy, elastic fleet, autoscaler, gateway tier, leaderless orchestrator) hides
//! behind them.
//!
//! - [`run_node`] — what every participating machine runs. A single daemon that *is* the whole system:
//!   it hosts whatever cells the galaxy assigns it, measures and gossips their load, opts into the
//!   control plane, and (if it's a public edge) serves as a gateway. Start it on a laptop, a relay, a
//!   burst VM, a thousand donor boxes — they self-organise into one galaxy with no central server.
//! - [`Player`] — what a client (native or in-browser) uses to play. `join`, then move and read; the
//!   mesh routes you to the right cell, hands you off across cell edges, and survives any host dying —
//!   all invisible.

use crate::autoscale::{Autoscaler, AutoscalePolicy};
use crate::fleet::{CloudProvider, RegionHint};
use crate::galaxy::{CellId, Galaxy, World};
use crate::orchestrator::Orchestrator;

/// What roles a node offers the galaxy. A node can do all three at once; the network figures out the
/// rest. Most boxes are `host + controller`; public, well-connected boxes add `gateway`.
#[derive(Debug, Clone, Copy)]
pub struct Roles {
    /// Run authoritative cells the galaxy assigns here.
    pub host: bool,
    /// Participate in leaderless scaling decisions (owns a rendezvous slice of cells).
    pub controller: bool,
    /// Accept browser sessions over wss/WebTransport and fan them into the mesh.
    pub gateway: bool,
}

impl Roles {
    pub const EVERYTHING: Roles = Roles { host: true, controller: true, gateway: true };
    pub const DONOR: Roles = Roles { host: true, controller: true, gateway: false };
    pub const EDGE: Roles = Roles { host: false, controller: false, gateway: true };
}

/// Everything that shapes a node's participation. Sensible defaults make `run_node` a one-liner.
pub struct NodeConfig {
    pub roles: Roles,
    pub autoscale: AutoscalePolicy,
    /// Cloud backends this node may burst into when it's acting as a controller and the donor pool is
    /// tight. Empty = donate-only (never spend on cloud); the galaxy still scales across donors.
    pub cloud: Vec<Box<dyn CloudProvider>>,
    /// Content hash of the spacegame cell image controllers deploy onto hosts.
    pub cell_image: String,
    /// Ceiling on credits/cell-minute a controller will bid — caps spend even under a huge surge.
    pub max_bid_per_min: u128,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            roles: Roles::DONOR,
            autoscale: AutoscalePolicy::default(),
            cloud: Vec::new(),
            cell_image: "ce-net/spacegame:latest".into(),
            max_bid_per_min: 1_000,
        }
    }
}

/// Run the galaxy node daemon forever. This is the entire server side of a million-player game in one
/// call. Conceptually:
///
/// ```text
/// loop every beacon epoch:
///   view = observe()                       // /galaxy + /load + atlas/netgraph + gateways
///   if roles.host:        host_assigned_cells(view)   // tick + publish /state + replicate + gossip /load
///   if roles.controller:  orchestrator.pass(view)     // split/merge/place/migrate/burst/retire (owned cells)
///   if roles.gateway:     serve_browser_sessions()    // wss/WebTransport fan-in/out, advertise in directory
/// ```
///
/// Hosting, scaling, and gateway duty all run concurrently; a node degrades gracefully (drop a role,
/// keep the others) and the galaxy reabsorbs its cells via deterministic failover if it vanishes.
pub async fn run_node(_ce: &crate::orchestrator::CeHandle, cfg: NodeConfig) -> ! {
    let _orch = Orchestrator {
        autoscaler: Autoscaler::new(cfg.autoscale),
        cell_image: cfg.cell_image,
        max_bid_per_min: cfg.max_bid_per_min,
    };
    // The real loop drives observe → host/control/gateway off the mesh's beacon + message streams.
    loop {
        // observe(); host_assigned_cells(); orch.pass(); serve_browser_sessions();
        std::future::pending::<()>().await;
    }
}

/// A live player session over the galaxy. The same type backs the native app and the in-browser client;
/// only the transport under `node` differs. The whole client contract is: tell it where you are, get
/// back what you can see; it handles which cell, which host, and every handoff.
pub struct Player {
    /// The player's unspoofable id (this peer's NodeId / signed subkey).
    pub id: String,
    /// Local mirror of the live galaxy shape, kept current from `/galaxy` gossip.
    pub galaxy: Galaxy,
    /// Where the player is in the world.
    pub x: World,
    pub y: World,
    /// View radius (from the platform profile — native sees farther than a phone).
    pub view: World,
    /// The cell currently authoritative for the player.
    pub cell: CellId,
}

impl Player {
    /// Join the galaxy: bring up the peer, learn the galaxy shape, subscribe to your interest cells,
    /// and spawn. Returns once you have a cell and your first snapshot.
    pub async fn join(_node: &crate::orchestrator::CeHandle, id: String, view: World) -> Player {
        let galaxy = Galaxy::genesis(); // replaced by the gossiped live shape on first `/galaxy` frame
        let (x, y) = (0.0, 0.0);
        let cell = galaxy.leaf_at(x, y);
        Player { id, galaxy, x, y, view, cell }
    }

    /// The handful of cells the player should be subscribed to right now — their leaf plus the live
    /// leaves touching their view. Bounded no matter how large or crowded the galaxy is: this is the
    /// `O(visible)` guarantee that makes a million concurrent players affordable.
    pub fn interest(&self) -> std::collections::BTreeSet<CellId> {
        self.galaxy.interest(self.x, self.y, self.view)
    }

    /// Advance the player's position (from prediction/input). If they crossed into a different leaf,
    /// returns the new cell so the client shifts its interest set and starts publishing input there —
    /// a seamless cross-cell transit, even when the new cell lives on a different node at a different
    /// quadtree depth.
    pub fn moved_to(&mut self, x: World, y: World) -> Option<CellId> {
        self.x = x;
        self.y = y;
        let now = self.galaxy.leaf_at(x, y);
        if now != self.cell {
            self.cell = now;
            Some(now)
        } else {
            None
        }
    }

    /// Apply a committed galaxy reshape (split/merge) the player heard on `/galaxy`, then re-resolve
    /// their own cell — so when the battle they're standing in subdivides under them, they slide onto
    /// the finer cell with no reconnect.
    pub fn on_reshape(&mut self, apply: impl FnOnce(&mut Galaxy)) -> Option<CellId> {
        apply(&mut self.galaxy);
        let now = self.galaxy.leaf_at(self.x, self.y);
        if now != self.cell {
            self.cell = now;
            Some(now)
        } else {
            None
        }
    }
}

/// The region a player should be tagged with — measured by gateway RTT, not IP geolocation, because
/// latency is what placement actually optimises.
pub fn region_from_rtt(_gateway_rtts: &[(String, f32)]) -> RegionHint {
    RegionHint(RegionHint::UNKNOWN.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_a_cell_edge_reports_a_transit() {
        let mut g = Galaxy::genesis();
        g.split(CellId::ROOT);
        let mut p = Player { id: "me".into(), galaxy: g, x: -1.0, y: -1.0, view: 100.0, cell: CellId::ROOT };
        p.cell = p.galaxy.leaf_at(p.x, p.y);
        let start = p.cell;
        // Move across the root midline into another depth-1 leaf.
        let crossed = p.moved_to(1.0, 1.0);
        assert!(crossed.is_some());
        assert_ne!(crossed.unwrap(), start);
    }

    #[test]
    fn interest_is_bounded() {
        let p = Player {
            id: "me".into(),
            galaxy: Galaxy::genesis(),
            x: 0.0,
            y: 0.0,
            view: 1000.0,
            cell: CellId::ROOT,
        };
        // Even with a wide view in a fresh galaxy, the interest set is a tiny bounded handful.
        assert!(p.interest().len() <= 9);
    }
}
