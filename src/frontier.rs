//! **The Frontier** — the galaxy grows outward as players push into the dark.
//!
//! Beyond the explored cells is the *void*: defined (every cell's contents are a pure function of the
//! seed — see [`crate::worldgen`]) but **not yet simulated**. It costs nothing. The instant a ship
//! crosses into a void cell, that cell is *born* — its contents materialise, a host is assigned (via the
//! same elastic placement as everything else), and the **frontier advances** one ring outward. The map
//! is not authored by us; it is drawn by whoever sails there first.
//!
//! Discovery has teeth:
//! - **First-in names it.** The first ship to enter a cell — or reach a landmark in it — earns the right
//!   to name it. The name is claimed first-wins on the `/frontier` topic and anchored to the chain, so a
//!   star you found and named carries your name for the life of the galaxy. The map is a monument to its
//!   explorers.
//! - **Risk is the gradient.** Danger and reward both climb with distance from the core
//!   ([`crate::worldgen::ring_distance`]), so the frontier always dangles a richer prize one jump deeper
//!   into more lethal space. Greed pulls you out; death pushes you back; the edge is where the game lives.
//! - **Rushes self-scale.** When a rich new cell is charted, explorers pour in — and it splits under the
//!   ordinary autoscaler. Discovery surges are handled by the same machinery as battle surges; the
//!   frontier needs no special capacity plumbing.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::galaxy::CellId;
use crate::worldgen::{CellGenesis, GalaxySeed};

/// A player's unspoofable id (NodeId / in-tab peer id / signed subkey).
pub type PlayerId = String;

/// A named feature on the permanent map. Landmarks are the player-written cartography of the galaxy.
#[derive(Debug, Clone, PartialEq)]
pub struct Landmark {
    pub cell: CellId,
    pub kind: LandmarkKind,
    /// The name its discoverer gave it (or the procedural designation, until someone renames the first
    /// time they arrive).
    pub name: String,
    pub named_by: PlayerId,
    pub epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandmarkKind {
    System,
    Nebula,
    Wormhole,
    Ruin,
    /// A whole region a faction or alliance has claimed and named.
    Territory,
}

/// The event of a player being first into a cell — the atom of exploration, gossiped and chain-anchored.
#[derive(Debug, Clone, PartialEq)]
pub struct Discovery {
    pub cell: CellId,
    pub by: PlayerId,
    pub epoch: u64,
    /// How deep into the frontier this was (bragging rights + leaderboard weight).
    pub ring: u32,
}

/// What happened when a ship entered a cell. Drives both the client (reveal the contents) and the mesh
/// (advance the frontier, place a host, broadcast a first-discovery).
#[derive(Debug, Clone)]
pub enum FrontierEvent {
    /// The cell was already charted — here are its (regenerated) contents, nothing to announce.
    Revealed(Box<CellGenesis>),
    /// This ship is the first ever here. Contents materialise, the discovery is claimed, and the
    /// frontier ring advances to include this cell's unexplored neighbours.
    FirstDiscovery { genesis: Box<CellGenesis>, discovery: Discovery, advanced: Vec<CellId> },
}

/// The live frontier on one node: which cells are charted, and therefore where the edge is. Folded from
/// `/frontier` gossip so every node shares the same map of the known galaxy.
#[derive(Debug, Default)]
pub struct Frontier {
    charted: HashSet<CellId>,
}

impl Frontier {
    /// A fresh galaxy: only the origin cell is charted; everything else is void.
    pub fn genesis() -> Self {
        let mut f = Frontier::default();
        f.charted.insert(CellId::at_depth_for(12, 0.0, 0.0));
        f
    }

    pub fn is_charted(&self, cell: &CellId) -> bool {
        self.charted.contains(cell)
    }

    /// The frontier ring: charted cells that border the void — the edge you push from.
    pub fn ring(&self) -> Vec<CellId> {
        self.charted
            .iter()
            .filter(|c| c.ring().iter().any(|n| !self.charted.contains(n)))
            .copied()
            .collect()
    }

    /// A ship at `by` enters `cell`. If charted, reveal its (regenerated) contents. If not, this is a
    /// first discovery: chart it, regenerate its contents, mint the discovery, and advance the frontier
    /// to surface its still-unexplored neighbours as the new edge.
    pub fn enter(&mut self, seed: &GalaxySeed, cell: CellId, by: &PlayerId, epoch: u64) -> FrontierEvent {
        let genesis = Box::new(CellGenesis::generate(seed, cell));
        if self.charted.contains(&cell) {
            return FrontierEvent::Revealed(genesis);
        }
        self.charted.insert(cell);
        let discovery = Discovery { cell, by: by.clone(), epoch, ring: genesis.ring };
        // The neighbours that are now reachable but still void — the frontier has moved outward.
        let advanced: Vec<CellId> = cell.ring().into_iter().filter(|n| !self.charted.contains(n)).collect();
        FrontierEvent::FirstDiscovery { genesis, discovery, advanced }
    }

    /// Apply a discovery we heard from another node (so all nodes converge on the same charted set).
    pub fn observe(&mut self, d: &Discovery) {
        self.charted.insert(d.cell);
    }

    pub fn charted_count(&self) -> usize {
        self.charted.len()
    }
}

/// The permanent record of who charted what and what they named it — the galaxy's history book, folded
/// from `/frontier` gossip and (for durability + the explorers leaderboard) anchored to the chain.
#[derive(Debug, Default)]
pub struct Cartography {
    /// First discoverer of each cell, first-claim wins (ties broken by lowest epoch then lowest id).
    first_by: HashMap<CellId, Discovery>,
    /// Named landmarks per cell, first-name wins per (cell, kind, slot).
    landmarks: BTreeMap<CellId, Vec<Landmark>>,
    /// Per-explorer tallies for the leaderboard.
    explorers: HashMap<PlayerId, ExplorerRecord>,
}

/// One explorer's standing: how much of the galaxy they put on the map.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ExplorerRecord {
    pub discoveries: u32,
    pub landmarks_named: u32,
    /// The deepest ring they ever reached — the headline frontier-cred number.
    pub deepest_ring: u32,
}

impl Cartography {
    /// Record a discovery, first-claim wins. Returns true if this claim is the canonical first (so the
    /// caller awards naming rights + leaderboard credit; a later duplicate returns false).
    pub fn claim_discovery(&mut self, d: Discovery) -> bool {
        let win = match self.first_by.get(&d.cell) {
            Some(existing) => (d.epoch, &d.by) < (existing.epoch, &existing.by),
            None => true,
        };
        if win {
            self.first_by.insert(d.cell, d.clone());
            let rec = self.explorers.entry(d.by.clone()).or_default();
            rec.discoveries += 1;
            rec.deepest_ring = rec.deepest_ring.max(d.ring);
        }
        win
    }

    /// Name a landmark, first-name wins. Only the cell's first discoverer (or whoever the rules allow)
    /// should call this; enforcement is the app's, the record is first-write.
    pub fn name_landmark(&mut self, lm: Landmark) -> bool {
        let list = self.landmarks.entry(lm.cell).or_default();
        if list.iter().any(|e| e.kind == lm.kind && e.name == lm.name) {
            return false;
        }
        // First name for this (cell, kind) sticks; later attempts on the same feature are rejected.
        let already_named_kind = list.iter().any(|e| e.kind == lm.kind);
        if already_named_kind {
            return false;
        }
        self.explorers.entry(lm.named_by.clone()).or_default().landmarks_named += 1;
        list.push(lm);
        true
    }

    pub fn first_discoverer(&self, cell: &CellId) -> Option<&PlayerId> {
        self.first_by.get(cell).map(|d| &d.by)
    }
    pub fn landmarks(&self, cell: &CellId) -> &[Landmark] {
        self.landmarks.get(cell).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// The explorers leaderboard — most of the galaxy charted, deepest reached. Order-independent and
    /// reproducible from the same claims, so it can be sealed against the chain like the kill board.
    pub fn leaderboard(&self) -> Vec<(PlayerId, ExplorerRecord)> {
        let mut v: Vec<(PlayerId, ExplorerRecord)> = self.explorers.iter().map(|(k, r)| (k.clone(), r.clone())).collect();
        v.sort_by(|a, b| {
            b.1.discoveries
                .cmp(&a.1.discoveries)
                .then(b.1.deepest_ring.cmp(&a.1.deepest_ring))
                .then(a.0.cmp(&b.0))
        });
        v
    }
}

/// Frontier gossip wire (on `ce-game/spacegame/frontier`): the two claim types that converge the map.
pub mod wire {
    use super::{Discovery, Landmark};
    use serde::{Deserialize, Serialize};

    pub const TOPIC: &str = "ce-game/spacegame/frontier";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "t")]
    pub enum FrontierMsg {
        /// "I was first into this cell." Converges the charted set + the explorers board.
        #[serde(rename = "discover")]
        Discover(DiscoveryClaim),
        /// "I name this feature." First-write wins per (cell, kind).
        #[serde(rename = "name")]
        Name(NameClaim),
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DiscoveryClaim {
        pub cell_token: String,
        pub by: String,
        pub epoch: u64,
        pub ring: u32,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct NameClaim {
        pub cell_token: String,
        pub kind: String,
        pub name: String,
        pub by: String,
        pub epoch: u64,
    }

    // (Discovery/Landmark <-> claim conversions live here in the real wiring; omitted from the design cut.)
    pub fn _link(_d: &Discovery, _l: &Landmark) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> GalaxySeed {
        GalaxySeed([3u8; 32])
    }

    #[test]
    fn first_entry_charts_and_advances_the_frontier() {
        let mut f = Frontier::genesis();
        let origin = CellId::at_depth_for(12, 0.0, 0.0);
        // Step into a neighbour of origin — fresh void → first discovery, frontier advances.
        let target = *origin.ring().first().unwrap();
        let ev = f.enter(&seed(), target, &"explorer".into(), 42);
        match ev {
            FrontierEvent::FirstDiscovery { discovery, advanced, .. } => {
                assert_eq!(discovery.by, "explorer");
                assert!(f.is_charted(&target));
                assert!(!advanced.is_empty(), "the frontier surfaced new void neighbours");
            }
            _ => panic!("expected a first discovery into the void"),
        }
        // Re-entering is just a reveal now.
        assert!(matches!(f.enter(&seed(), target, &"other".into(), 43), FrontierEvent::Revealed(_)));
    }

    #[test]
    fn discovery_is_first_claim_wins() {
        let mut carto = Cartography::default();
        let cell = CellId { depth: 12, x: 100, y: 100 };
        assert!(carto.claim_discovery(Discovery { cell, by: "early".into(), epoch: 10, ring: 5 }));
        // A later claim on the same cell loses.
        assert!(!carto.claim_discovery(Discovery { cell, by: "late".into(), epoch: 20, ring: 5 }));
        assert_eq!(carto.first_discoverer(&cell), Some(&"early".to_string()));
        assert_eq!(carto.leaderboard()[0].0, "early");
    }

    #[test]
    fn a_landmark_name_sticks() {
        let mut carto = Cartography::default();
        let cell = CellId { depth: 12, x: 5, y: 5 };
        let lm = Landmark { cell, kind: LandmarkKind::System, name: "Leif's Star".into(), named_by: "leif".into(), epoch: 1 };
        assert!(carto.name_landmark(lm));
        // Nobody can rename that system.
        let steal = Landmark { cell, kind: LandmarkKind::System, name: "Not Leif's".into(), named_by: "thief".into(), epoch: 2 };
        assert!(!carto.name_landmark(steal));
        assert_eq!(carto.landmarks(&cell)[0].name, "Leif's Star");
    }
}
